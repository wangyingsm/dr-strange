//! Cross-engine benchmark harness for dr-strange (dev-only tooling, not part
//! of the shipped product). Two subcommands:
//!
//! - `gen`  — writes a deterministic synthetic graph + vector dataset and the
//!   query sets to a directory, as plain CSV/txt so every engine (drsg here,
//!   plus SQLite / Kùzu / Neo4j via `benchmarks/compare.py`) loads *identical*
//!   data and runs *identical* queries. It also writes the exact (brute-force)
//!   top-K answer for a sample of the vector queries, the shared oracle every
//!   engine's recall@k is scored against.
//! - `run`  — loads that dataset into dr-strange (the native LSM backend, the
//!   shipping default) and times the core operations + vector search, scores
//!   the ANN results against the oracle, and emits results JSON in the shared
//!   schema the Python driver also produces.
//!
//! The dataset is the single source of truth: `gen` produces the files, and
//! both `run` and the Python engines read them — no engine regenerates data.

/// Same process allocator as the shipped binaries (drsg / drsg-mcp), so the
/// benchmark measures what production runs.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use ahash::AHashMap;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dr_strange_core::{BulkEdge, BulkNode, Database, Dir, Metric, PropDesc, PropValue, Properties};
use serde::Serialize;

// A few labels / edge types so the catalog and colouring have variety; kept
// small so distributions stay dense enough for meaningful traversal.
const LABELS: [&str; 4] = ["Person", "Company", "Paper", "Topic"];
const EDGE_TYPES: [&str; 4] = ["KNOWS", "WORKS_AT", "CITES", "ABOUT"];

/// Depth of the exact top-K oracle `gen` writes per sampled vector query. A
/// run with any `k <= EXACT_K` scores recall against a prefix of the same
/// list, so one dataset serves every reasonable k without regeneration.
const EXACT_K: usize = 100;
/// Name of the oracle file under `queries/`; `compare.py` reads the same one.
const EXACT_FILE: &str = "queries/vector_exact_topk.txt";

#[derive(Parser)]
#[command(
    name = "drsg-bench",
    about = "dr-strange cross-engine benchmark harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the shared dataset + query files.
    Gen {
        #[arg(long, default_value = "benchmarks/data")]
        out: PathBuf,
        #[arg(long, default_value_t = 100_000)]
        nodes: u64,
        #[arg(long, default_value_t = 500_000)]
        edges: u64,
        #[arg(long, default_value_t = 128)]
        dim: usize,
        /// Number of point-lookup / expansion queries.
        #[arg(long, default_value_t = 10_000)]
        queries: u64,
        /// Number of vector top-k queries.
        #[arg(long, default_value_t = 1_000)]
        vec_queries: u64,
        /// How many of the vector queries (the first ones) get an exact
        /// brute-force top-K answer written for recall scoring. Brute force
        /// is O(nodes × dim) per query, so this is a sample, not the set.
        #[arg(long, default_value_t = 100)]
        recall_queries: u64,
    },
    /// Load the dataset into dr-strange and time the workload.
    Run {
        #[arg(long, default_value = "benchmarks/data")]
        data: PathBuf,
        /// Scratch database (a directory for the native backend; recreated
        /// each run).
        #[arg(long, default_value = "benchmarks/data/drsg.db")]
        db: PathBuf,
        /// Where to write the results JSON.
        #[arg(long, default_value = "benchmarks/results/dr-strange.json")]
        out: PathBuf,
        /// k for vector top-k.
        #[arg(long, default_value_t = 10)]
        k: u64,
        /// How many vector queries (the first ones) to score for recall@k
        /// against the exact answer. Capped by what `gen` wrote an oracle for
        /// when the dataset carries one; otherwise brute-forced here.
        #[arg(long, default_value_t = 100)]
        recall_queries: u64,
        /// Measurement passes: the whole load + query workload runs this many
        /// times (fresh database each pass) and every reported figure is the
        /// median across passes, with the min→max spread printed alongside.
        /// One pass is a machine-state lottery; three make the noise visible.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
}

// ---- deterministic PRNG (SplitMix64) --------------------------------------

struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    /// A float in [-1, 1).
    fn unit(&mut self) -> f32 {
        (self.next_u64() as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
    }
}

// ---- results schema (shared with compare.py) ------------------------------

#[derive(Serialize)]
struct OpResult {
    engine: String,
    op: String,
    n: u64,
    total_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    median_us: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p95_us: Option<f64>,
    throughput_per_s: f64,
    /// Measurement passes aggregated into this row (absent = single pass).
    #[serde(skip_serializing_if = "Option::is_none")]
    runs: Option<u32>,
    /// (max − min) / median of the primary metric across passes, in percent —
    /// the honest error bar on the numbers above.
    #[serde(skip_serializing_if = "Option::is_none")]
    spread_pct: Option<f64>,
    /// Mean recall@k over the sampled vector queries (the `vector_recall`
    /// row only): |ANN top-k ∩ exact top-k| / k, averaged.
    #[serde(skip_serializing_if = "Option::is_none")]
    recall: Option<f64>,
    /// The k that `recall` was scored at.
    #[serde(skip_serializing_if = "Option::is_none")]
    k: Option<u64>,
}

fn stat(mut micros: Vec<f64>) -> (f64, f64) {
    micros.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = micros[micros.len() / 2];
    let p95 = micros[((micros.len() as f64) * 0.95) as usize].min(*micros.last().unwrap());
    (median, p95)
}

// ---- gen ------------------------------------------------------------------

fn generate(
    out: &Path,
    nodes: u64,
    edges: u64,
    dim: usize,
    queries: u64,
    vec_queries: u64,
    recall_queries: u64,
) -> Result<()> {
    fs::create_dir_all(out)?;
    fs::create_dir_all(out.join("queries"))?;
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);

    // nodes.csv: id,key,label,name,value
    let mut w = BufWriter::new(File::create(out.join("nodes.csv"))?);
    writeln!(w, "id,key,label,name,value")?;
    for id in 0..nodes {
        let label = LABELS[(id as usize) % LABELS.len()];
        let value = rng.below(1_000_000);
        writeln!(w, "{id},n{id},{label},name_{id},{value}")?;
    }
    w.flush()?;

    // edges.csv: src_key,dst_key,type  (no self-loops). Edges reference the
    // string key — every engine makes that key its primary key, so bulk load
    // and lookups use each engine's PK index (fair across engines: Kùzu only
    // indexes the PK, so a non-PK key lookup would be an unfair full scan).
    let mut w = BufWriter::new(File::create(out.join("edges.csv"))?);
    writeln!(w, "src_key,dst_key,type")?;
    let mut made = 0u64;
    while made < edges {
        let src = rng.below(nodes);
        let dst = rng.below(nodes);
        if src == dst {
            continue;
        }
        let ty = EDGE_TYPES[(rng.next_u64() as usize) % EDGE_TYPES.len()];
        writeln!(w, "n{src},n{dst},{ty}")?;
        made += 1;
    }
    w.flush()?;

    // vectors.csv: id,<space-separated dim floats> (unit-normalized, cosine).
    // The oracle below is computed from the *written* (6-decimal) values, so
    // every engine and the oracle see bit-identical vectors.
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(nodes as usize);
    let mut w = BufWriter::new(File::create(out.join("vectors.csv"))?);
    writeln!(w, "id,vector")?;
    for id in 0..nodes {
        let v = unit_vector(&mut rng, dim);
        write!(w, "{id},")?;
        let text = write_vec(&mut w, &v)?;
        writeln!(w)?;
        vectors.push(parse_vector(&text));
    }
    w.flush()?;

    // queries/lookup_keys.txt — random existing keys
    let mut w = BufWriter::new(File::create(out.join("queries/lookup_keys.txt"))?);
    for _ in 0..queries {
        writeln!(w, "n{}", rng.below(nodes))?;
    }
    w.flush()?;

    // queries/expand_keys.txt — random node keys (expansion resolves the
    // start node by its PK first, as every engine must).
    let mut w = BufWriter::new(File::create(out.join("queries/expand_keys.txt"))?);
    for _ in 0..queries {
        writeln!(w, "n{}", rng.below(nodes))?;
    }
    w.flush()?;

    // queries/vector_queries.csv — random query vectors
    let mut queries_v: Vec<Vec<f32>> = Vec::new();
    let mut w = BufWriter::new(File::create(out.join("queries/vector_queries.csv"))?);
    for _ in 0..vec_queries {
        let v = unit_vector(&mut rng, dim);
        let text = write_vec(&mut w, &v)?;
        writeln!(w)?;
        queries_v.push(parse_vector(&text));
    }
    w.flush()?;

    // queries/vector_exact_topk.txt — one line per sampled query (the first
    // `recall_queries`), the ids of its exact cosine top-EXACT_K, nearest
    // first. Row i answers vector query i. Every engine's recall@k is
    // |its top-k ∩ this line's first k| / k.
    let recall_queries = (recall_queries as usize).min(queries_v.len());
    let exact_k = EXACT_K.min(vectors.len());
    let mut w = BufWriter::new(File::create(out.join(EXACT_FILE))?);
    for q in queries_v.iter().take(recall_queries) {
        let ids = brute_force_top_k(&vectors, q, exact_k);
        let line: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
        writeln!(w, "{}", line.join(" "))?;
    }
    w.flush()?;

    // meta.json — so compare.py knows the shape without re-parsing everything
    fs::write(
        out.join("meta.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "nodes": nodes, "edges": edges, "dim": dim,
            "queries": queries, "vec_queries": vec_queries,
            "recall_queries": recall_queries, "exact_k": exact_k,
        }))?,
    )?;

    println!(
        "generated {nodes} nodes, {edges} edges, dim {dim} → {}",
        out.display()
    );
    Ok(())
}

fn unit_vector(rng: &mut Rng, dim: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dim).map(|_| rng.unit()).collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in &mut v {
        *x /= norm;
    }
    v
}

/// Writes `v` as space-separated 6-decimal floats and returns the exact text
/// written, so callers can keep the rounded values the readers will see.
fn write_vec(w: &mut impl Write, v: &[f32]) -> Result<String> {
    let mut text = String::with_capacity(v.len() * 10);
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        text.push_str(&format!("{x:.6}"));
    }
    w.write_all(text.as_bytes())?;
    Ok(text)
}

// ---- recall oracle --------------------------------------------------------

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt()).max(1e-12)
}

/// Exact cosine top-k over `vectors`: the row indices of the k most similar
/// to `q`, nearest first. O(n · dim) — the ground truth the ANN index is
/// scored against, never the thing being timed.
fn brute_force_top_k(vectors: &[Vec<f32>], q: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(f32, usize)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (cosine(v, q), i))
        .collect();
    // Descending similarity; index ascending on exact ties so the answer is
    // deterministic. NaN cannot occur (finite inputs, clamped norm).
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(k);
    scored.into_iter().map(|(_, i)| i).collect()
}

/// recall@k of one answer: the share of the exact top-k the ANN returned.
/// Scored against `exact.len()`, not `got.len()`, so an index that returns
/// fewer than k rows is penalised rather than rewarded.
fn recall_at_k(exact: &[usize], got: &[usize]) -> f64 {
    if exact.is_empty() {
        return 1.0;
    }
    let hits = got.iter().filter(|g| exact.contains(g)).count();
    hits as f64 / exact.len() as f64
}

/// Loads the oracle `gen` wrote, if the dataset has one: one `Vec` of ids per
/// sampled query. `None` for datasets generated before the oracle existed.
fn read_exact_topk(data: &Path) -> Result<Option<Vec<Vec<usize>>>> {
    let path = data.join(EXACT_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for (ln, line) in read_lines(&path)?.iter().enumerate() {
        let ids = line
            .split_whitespace()
            .map(|t| t.parse::<usize>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("{}:{}: bad id", path.display(), ln + 1))?;
        out.push(ids);
    }
    Ok(Some(out))
}

// ---- run (dr-strange) -----------------------------------------------------

fn prop(value: PropValue) -> PropDesc {
    PropDesc {
        description: None,
        value,
    }
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    BufReader::new(f)
        .lines()
        .collect::<std::io::Result<_>>()
        .map_err(Into::into)
}

fn parse_vector(s: &str) -> Vec<f32> {
    s.split_whitespace().map(|t| t.parse().unwrap()).collect()
}

/// One full measurement pass: fresh database, load, then every query set.
fn run_pass(data: &Path, db_path: &Path, k: u64, recall_queries: u64) -> Result<Vec<OpResult>> {
    let engine = "dr-strange".to_string();
    let mut results: Vec<OpResult> = Vec::new();

    // Fresh database each run. The native backend's db is a directory
    // (WAL + SSTs), legacy redb's a single file — clear either shape.
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_dir_all(db_path);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let db = Database::open(db_path)?;

    // ---- load nodes + edges via the bulk fast path (one write txn) --------
    // Every comparator loads through a bulk path (Kùzu COPY / SQLite
    // executemany / Neo4j UNWIND), so drsg's `bulk_load` is the apples-to-
    // apples loader. Timed as one "load" (nodes+edges), parsing included to
    // match how SQLite's timed executemany parses its rows lazily.
    let node_lines = read_lines(&data.join("nodes.csv"))?;
    let edge_lines = read_lines(&data.join("edges.csv"))?;
    let n_nodes = (node_lines.len() - 1) as u64; // minus header
    let n_edges = (edge_lines.len() - 1) as u64;

    let t = Instant::now();
    {
        // Owned buffers keep the &str borrows in BulkNode/BulkEdge alive for
        // the bulk_load call.
        let mut nkeys: Vec<String> = Vec::with_capacity(n_nodes as usize);
        let mut nlabels: Vec<String> = Vec::with_capacity(n_nodes as usize);
        let mut nprops: Vec<Properties> = Vec::with_capacity(n_nodes as usize);
        for line in node_lines.iter().skip(1) {
            let mut f = line.splitn(5, ',');
            let _id = f.next().unwrap();
            nkeys.push(f.next().unwrap().to_string());
            nlabels.push(f.next().unwrap().to_string());
            let name = f.next().unwrap();
            let value: i64 = f.next().unwrap().parse().unwrap();
            let mut props: Properties = BTreeMap::new();
            props.insert("name".into(), prop(PropValue::Str(name.into())));
            props.insert("value".into(), prop(PropValue::Int(value)));
            nprops.push(props);
        }
        let label_slots: Vec<[&str; 1]> = nlabels.iter().map(|l| [l.as_str()]).collect();

        let mut esrc: Vec<String> = Vec::with_capacity(n_edges as usize);
        let mut edst: Vec<String> = Vec::with_capacity(n_edges as usize);
        let mut etype: Vec<String> = Vec::with_capacity(n_edges as usize);
        for line in edge_lines.iter().skip(1) {
            let mut f = line.splitn(3, ',');
            esrc.push(f.next().unwrap().to_string());
            edst.push(f.next().unwrap().to_string());
            etype.push(f.next().unwrap().to_string());
        }

        let bnodes: Vec<BulkNode> = nkeys
            .iter()
            .zip(&label_slots)
            .zip(nprops)
            .map(|((k, ls), props)| BulkNode {
                external_key: Some(k),
                labels: ls,
                props,
            })
            .collect();
        let bedges: Vec<BulkEdge> = (0..n_edges as usize)
            .map(|i| BulkEdge {
                src_key: &esrc[i],
                dst_key: &edst[i],
                ty: &etype[i],
                props: Properties::new(),
            })
            .collect();

        let plane = db.plane("startup")?;
        let mut txn = plane.write()?;
        txn.bulk_load(bnodes, bedges)?;
        txn.commit()?;
    }
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    results.push(throughput_result(
        &engine,
        "load",
        n_nodes + n_edges,
        load_ms,
    ));

    // ---- point lookup by external key -------------------------------------
    let lookup_keys = read_lines(&data.join("queries/lookup_keys.txt"))?;
    {
        let plane = db.plane("startup")?;
        let mut micros = Vec::with_capacity(lookup_keys.len());
        let t = Instant::now();
        for key in &lookup_keys {
            let s = Instant::now();
            let _ = plane.node_by_key(key)?;
            micros.push(s.elapsed().as_secs_f64() * 1e6);
        }
        results.push(latency_result(&engine, "lookup", &micros, t.elapsed()));
    }

    // ---- 1-hop expansion + 2-hop traversal --------------------------------
    // Each query resolves the start node by its key first (as every engine
    // must), then expands — so the timing is the realistic "from key X, get
    // its neighbourhood" cost.
    let expand_keys = read_lines(&data.join("queries/expand_keys.txt"))?;
    {
        let plane = db.plane("startup")?;

        let mut micros = Vec::with_capacity(expand_keys.len());
        let t = Instant::now();
        for key in &expand_keys {
            let s = Instant::now();
            let start = plane.node_by_key(key)?.unwrap().id;
            let _ = plane.neighbors(start, Dir::Out, None)?;
            micros.push(s.elapsed().as_secs_f64() * 1e6);
        }
        results.push(latency_result(&engine, "expand_1hop", &micros, t.elapsed()));

        // 2-hop reachable set (variable-length expand, 1..=2 hops), distinct.
        // Fewer queries — each touches far more of the graph.
        let sample = expand_keys.len().min(2_000);
        let mut micros = Vec::with_capacity(sample);
        let t = Instant::now();
        for key in expand_keys.iter().take(sample) {
            let s = Instant::now();
            let start = plane.node_by_key(key)?.unwrap().id;
            let _ = plane
                .query()
                .seek_ids([start])
                .expand_var(Dir::Out, None, 1, 2)
                .distinct()
                .ids()?;
            micros.push(s.elapsed().as_secs_f64() * 1e6);
        }
        results.push(latency_result(
            &engine,
            "traverse_2hop",
            &micros,
            t.elapsed(),
        ));
    }

    // ---- vectors: separate plane, index build, top-k ----------------------
    let vec_lines = read_lines(&data.join("vectors.csv"))?;
    let n_vecs = (vec_lines.len() - 1) as u64;
    {
        // Load embedding nodes into a dedicated plane (keeps vector cost out
        // of the graph-load numbers).
        if db.plane("vec").is_err() {
            db.create_plane("vec", Properties::new())?;
        }
        let plane = db.plane("vec")?;
        // Row i of vectors.csv ↔ the node it became, so ANN results can be
        // scored against the oracle's row indices. The vectors themselves
        // are kept only to brute-force an oracle when the dataset has none.
        let mut row_of: AHashMap<dr_strange_core::NodeId, usize> =
            AHashMap::with_capacity(n_vecs as usize);
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(n_vecs as usize);
        {
            let mut txn = plane.write()?;
            for (row, line) in vec_lines.iter().skip(1).enumerate() {
                let (id, rest) = line.split_once(',').unwrap();
                let v = parse_vector(rest);
                vectors.push(v.clone());
                let mut props: Properties = BTreeMap::new();
                props.insert("embedding".into(), prop(PropValue::Vector(v)));
                let nid = txn.create_node_with_key(&format!("v{id}"), &["Item"], props)?;
                row_of.insert(nid, row);
            }
            txn.commit()?;
        }

        // Build the vector index (HNSW) and time it.
        let t = Instant::now();
        db.plane("vec")?
            .ensure_vector_index("Item", "embedding", Metric::Cosine)?;
        results.push(throughput_result(
            &engine,
            "vector_build",
            n_vecs,
            t.elapsed().as_secs_f64() * 1000.0,
        ));

        // Top-k queries.
        let qs = read_lines(&data.join("queries/vector_queries.csv"))?;
        let plane = db.plane("vec")?;
        let mut micros = Vec::with_capacity(qs.len());
        let t = Instant::now();
        for line in &qs {
            let q = parse_vector(line);
            let s = Instant::now();
            let _ = plane
                .query()
                .vector_top_k(Some("Item"), "embedding", q, Metric::Cosine, k)
                .scored_nodes()?;
            micros.push(s.elapsed().as_secs_f64() * 1e6);
        }
        results.push(latency_result(&engine, "vector_topk", &micros, t.elapsed()));

        // Recall@k on a sample of the same queries, untimed: the ANN answer
        // above is only worth its latency if it is also right. The oracle
        // is the dataset's exact top-K when `gen` wrote one (shared with
        // compare.py), else brute-forced here from the loaded vectors.
        let sample = (recall_queries as usize).min(qs.len());
        let exact = match read_exact_topk(data)? {
            Some(e) => e,
            None => qs
                .iter()
                .take(sample)
                .map(|line| brute_force_top_k(&vectors, &parse_vector(line), k as usize))
                .collect(),
        };
        let sample = sample.min(exact.len());
        if sample > 0 {
            let t = Instant::now();
            let mut sum = 0.0;
            for (line, exact_ids) in qs.iter().zip(&exact).take(sample) {
                let q = parse_vector(line);
                let got: Vec<usize> = plane
                    .query()
                    .vector_top_k(Some("Item"), "embedding", q, Metric::Cosine, k)
                    .scored_nodes()?
                    .iter()
                    .filter_map(|(n, _)| row_of.get(&n.id).copied())
                    .collect();
                let want = &exact_ids[..(k as usize).min(exact_ids.len())];
                sum += recall_at_k(want, &got);
            }
            let mut r = throughput_result(
                &engine,
                "vector_recall",
                sample as u64,
                t.elapsed().as_secs_f64() * 1000.0,
            );
            r.recall = Some(sum / sample as f64);
            r.k = Some(k);
            results.push(r);
        }
    }

    Ok(results)
}

/// Median of a small sample (by value; passes are few, cloning is fine).
fn median_of(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    vals[vals.len() / 2]
}

/// (max − min) / median, as a percentage — the printed error bar.
fn spread_of(vals: &[f64]) -> f64 {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in vals {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let med = median_of(vals.to_vec());
    if med > 0.0 {
        (hi - lo) / med * 100.0
    } else {
        0.0
    }
}

/// Run `repeat` measurement passes and aggregate: every reported metric is the
/// median across passes; `spread_pct` records the min→max spread of each op's
/// primary metric (latency median, else throughput) so noise stays visible
/// instead of silently baked into a single-shot number.
fn run(
    data: &Path,
    db_path: &Path,
    out: &Path,
    k: u64,
    recall_queries: u64,
    repeat: u32,
) -> Result<()> {
    let repeat = repeat.max(1);
    let mut passes: Vec<Vec<OpResult>> = Vec::with_capacity(repeat as usize);
    for i in 0..repeat {
        if repeat > 1 {
            println!("pass {}/{repeat}…", i + 1);
        }
        passes.push(run_pass(data, db_path, k, recall_queries)?);
    }
    let results = aggregate_passes(&passes, repeat);

    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out, serde_json::to_vec_pretty(&results)?)?;
    println!("wrote {} results → {}", results.len(), out.display());
    for r in &results {
        let spread = r
            .spread_pct
            .map(|s| format!("  ±{s:.1}%"))
            .unwrap_or_default();
        match (r.median_us, r.recall) {
            (_, Some(rc)) => println!(
                "  {:<14} n={:<7} {:>9.2} ms total  recall@{} {:.4}{spread}",
                r.op,
                r.n,
                r.total_ms,
                r.k.unwrap_or(0),
                rc
            ),
            (Some(m), None) => println!(
                "  {:<14} n={:<7} {:>9.2} ms total  median {:>8.2} µs  {:>12.0}/s{spread}",
                r.op, r.n, r.total_ms, m, r.throughput_per_s
            ),
            (None, None) => println!(
                "  {:<14} n={:<7} {:>9.2} ms total  {:>12.0}/s{spread}",
                r.op, r.n, r.total_ms, r.throughput_per_s
            ),
        }
    }
    Ok(())
}

/// Median across passes per op; `spread_pct` is on each op's primary metric
/// (latency median, else recall, else throughput).
fn aggregate_passes(passes: &[Vec<OpResult>], repeat: u32) -> Vec<OpResult> {
    let per_op = |f: &dyn Fn(&OpResult) -> f64, op_idx: usize| -> Vec<f64> {
        passes.iter().map(|p| f(&p[op_idx])).collect()
    };
    (0..passes[0].len())
        .map(|i| {
            let first = &passes[0][i];
            let latency = first.median_us.is_some();
            let recall = first.recall.is_some();
            let primary = per_op(
                &|r| r.median_us.or(r.recall).unwrap_or(r.throughput_per_s),
                i,
            );
            OpResult {
                engine: first.engine.clone(),
                op: first.op.clone(),
                n: first.n,
                total_ms: median_of(per_op(&|r| r.total_ms, i)),
                median_us: latency.then(|| median_of(per_op(&|r| r.median_us.unwrap_or(0.0), i))),
                p95_us: latency.then(|| median_of(per_op(&|r| r.p95_us.unwrap_or(0.0), i))),
                throughput_per_s: median_of(per_op(&|r| r.throughput_per_s, i)),
                runs: (repeat > 1).then_some(repeat),
                spread_pct: (repeat > 1).then(|| spread_of(&primary)),
                recall: recall.then(|| median_of(per_op(&|r| r.recall.unwrap_or(0.0), i))),
                k: first.k,
            }
        })
        .collect()
}

fn throughput_result(engine: &str, op: &str, n: u64, total_ms: f64) -> OpResult {
    OpResult {
        engine: engine.to_string(),
        op: op.to_string(),
        n,
        total_ms,
        median_us: None,
        p95_us: None,
        throughput_per_s: if total_ms > 0.0 {
            n as f64 / (total_ms / 1000.0)
        } else {
            0.0
        },
        runs: None,
        spread_pct: None,
        recall: None,
        k: None,
    }
}

fn latency_result(engine: &str, op: &str, micros: &[f64], total: std::time::Duration) -> OpResult {
    let (median, p95) = stat(micros.to_vec());
    let total_ms = total.as_secs_f64() * 1000.0;
    OpResult {
        engine: engine.to_string(),
        op: op.to_string(),
        n: micros.len() as u64,
        total_ms,
        median_us: Some(median),
        p95_us: Some(p95),
        throughput_per_s: micros.len() as f64 / total.as_secs_f64(),
        runs: None,
        spread_pct: None,
        recall: None,
        k: None,
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Gen {
            out,
            nodes,
            edges,
            dim,
            queries,
            vec_queries,
            recall_queries,
        } => generate(
            &out,
            nodes,
            edges,
            dim,
            queries,
            vec_queries,
            recall_queries,
        ),
        Command::Run {
            data,
            db,
            out,
            k,
            recall_queries,
            repeat,
        } => run(&data, &db, &out, k, recall_queries, repeat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[f32]) -> Vec<f32> {
        xs.to_vec()
    }

    #[test]
    fn brute_force_ranks_by_cosine_not_magnitude() {
        // Row 2 points exactly along q but is tiny; row 0 is long but off-axis.
        // Cosine must rank 2 first, then 0, then 1 (orthogonal), then 3
        // (opposite).
        let vectors = vec![
            v(&[10.0, 1.0]),
            v(&[0.0, 1.0]),
            v(&[0.01, 0.0]),
            v(&[-1.0, 0.0]),
        ];
        assert_eq!(brute_force_top_k(&vectors, &[1.0, 0.0], 3), vec![2, 0, 1]);
        assert_eq!(brute_force_top_k(&vectors, &[1.0, 0.0], 10).len(), 4);
    }

    #[test]
    fn brute_force_breaks_ties_by_row() {
        let vectors = vec![v(&[1.0, 0.0]), v(&[2.0, 0.0]), v(&[0.0, 1.0])];
        assert_eq!(brute_force_top_k(&vectors, &[1.0, 0.0], 2), vec![0, 1]);
    }

    #[test]
    fn recall_counts_hits_against_the_exact_set() {
        let exact = [1usize, 2, 3, 4];
        assert_eq!(recall_at_k(&exact, &[4, 3, 2, 1]), 1.0);
        assert_eq!(recall_at_k(&exact, &[5, 6, 7, 8]), 0.0);
        assert_eq!(recall_at_k(&exact, &[1, 2, 9, 9]), 0.5);
        // Returning fewer rows than k is a miss, not a free pass.
        assert_eq!(recall_at_k(&exact, &[1]), 0.25);
        assert_eq!(recall_at_k(&[], &[]), 1.0);
    }

    fn recall_row(recall: f64, total_ms: f64) -> OpResult {
        let mut r = throughput_result("e", "vector_recall", 10, total_ms);
        r.recall = Some(recall);
        r.k = Some(10);
        r
    }

    #[test]
    fn aggregate_takes_the_median_recall_and_spreads_on_it() {
        // Throughput varies wildly across passes; recall barely. The spread
        // must follow recall (the row's primary metric), not throughput.
        let passes = vec![
            vec![recall_row(0.90, 1.0)],
            vec![recall_row(0.95, 100.0)],
            vec![recall_row(1.00, 10.0)],
        ];
        let agg = aggregate_passes(&passes, 3);
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].recall, Some(0.95));
        assert_eq!(agg[0].k, Some(10));
        assert_eq!(agg[0].runs, Some(3));
        let spread = agg[0].spread_pct.unwrap_or(f64::NAN);
        assert!((spread - (0.10 / 0.95 * 100.0)).abs() < 1e-9, "{spread}");
        // A row without recall keeps recall absent in the aggregate.
        let plain = vec![vec![throughput_result("e", "load", 5, 2.0)]];
        assert_eq!(aggregate_passes(&plain, 1)[0].recall, None);
    }

    #[test]
    fn recall_serialises_only_on_the_row_that_has_it() {
        let json = serde_json::to_value(recall_row(0.5, 1.0)).unwrap_or_default();
        assert_eq!(json["recall"], 0.5);
        assert_eq!(json["k"], 10);
        let json = serde_json::to_value(throughput_result("e", "load", 5, 2.0)).unwrap_or_default();
        assert!(json.get("recall").is_none());
        assert!(json.get("k").is_none());
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            Scratch(
                std::env::temp_dir()
                    .join(format!("drsg-bench-{tag}-{}-{nanos}", std::process::id())),
            )
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// End to end on a tiny dataset: `gen` writes an oracle that agrees with
    /// brute force over the written vectors, `run` scores recall against it,
    /// and a dataset without the oracle file is scored identically by the
    /// in-run brute force.
    #[test]
    fn run_reports_recall_against_the_generated_oracle() -> Result<()> {
        let scratch = Scratch::new("recall");
        let data = scratch.0.join("data");
        generate(&data, 300, 600, 16, 50, 20, 8)?;

        let oracle = read_exact_topk(&data)?.context("oracle missing")?;
        assert_eq!(oracle.len(), 8);
        assert!(oracle.iter().all(|row| row.len() == EXACT_K.min(300)));
        let vectors: Vec<Vec<f32>> = read_lines(&data.join("vectors.csv"))?
            .iter()
            .skip(1)
            .map(|l| parse_vector(l.split_once(',').map(|x| x.1).unwrap_or("")))
            .collect();
        let queries = read_lines(&data.join("queries/vector_queries.csv"))?;
        assert_eq!(
            oracle[3],
            brute_force_top_k(&vectors, &parse_vector(&queries[3]), 100)
        );

        let with_oracle = run_pass(&data, &scratch.0.join("db"), 10, 8)?;
        let row = with_oracle
            .iter()
            .find(|r| r.op == "vector_recall")
            .context("no vector_recall row")?;
        assert_eq!(row.n, 8);
        assert_eq!(row.k, Some(10));
        let recall = row.recall.context("recall missing")?;
        assert!((0.0..=1.0).contains(&recall), "{recall}");
        // 300 vectors is small enough that HNSW should be essentially exact;
        // anything far below that means the scoring is mis-keyed.
        assert!(recall > 0.5, "recall {recall} is implausibly low");

        fs::remove_file(data.join(EXACT_FILE))?;
        let brute = run_pass(&data, &scratch.0.join("db"), 10, 8)?;
        let row2 = brute
            .iter()
            .find(|r| r.op == "vector_recall")
            .context("no vector_recall row without oracle")?;
        assert_eq!(row2.recall, Some(recall));
        Ok(())
    }
}
