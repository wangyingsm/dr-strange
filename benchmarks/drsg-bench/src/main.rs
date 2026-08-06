//! Cross-engine benchmark harness for dr-strange (dev-only tooling, not part
//! of the shipped product). Two subcommands:
//!
//! - `gen`  — writes a deterministic synthetic graph + vector dataset and the
//!   query sets to a directory, as plain CSV/txt so every engine (drsg here,
//!   plus SQLite / Kùzu / Neo4j via `benchmarks/compare.py`) loads *identical*
//!   data and runs *identical* queries.
//! - `run`  — loads that dataset into dr-strange (redb file backend) and times
//!   the core operations + vector search, emitting results JSON in the shared
//!   schema the Python driver also produces.
//!
//! The dataset is the single source of truth: `gen` produces the files, and
//! both `run` and the Python engines read them — no engine regenerates data.

/// Same process allocator as the shipped binaries (drsg / drsg-mcp), so the
/// benchmark measures what production runs.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    },
    /// Load the dataset into dr-strange and time the workload.
    Run {
        #[arg(long, default_value = "benchmarks/data")]
        data: PathBuf,
        /// Scratch database file (recreated each run).
        #[arg(long, default_value = "benchmarks/data/drsg.redb")]
        db: PathBuf,
        /// Where to write the results JSON.
        #[arg(long, default_value = "benchmarks/results/dr-strange.json")]
        out: PathBuf,
        /// k for vector top-k.
        #[arg(long, default_value_t = 10)]
        k: u64,
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

    // vectors.csv: id,<space-separated dim floats> (unit-normalized, cosine)
    let mut w = BufWriter::new(File::create(out.join("vectors.csv"))?);
    writeln!(w, "id,vector")?;
    for id in 0..nodes {
        let v = unit_vector(&mut rng, dim);
        write!(w, "{id},")?;
        write_vec(&mut w, &v)?;
        writeln!(w)?;
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
    let mut w = BufWriter::new(File::create(out.join("queries/vector_queries.csv"))?);
    for _ in 0..vec_queries {
        let v = unit_vector(&mut rng, dim);
        write_vec(&mut w, &v)?;
        writeln!(w)?;
    }
    w.flush()?;

    // meta.json — so compare.py knows the shape without re-parsing everything
    fs::write(
        out.join("meta.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "nodes": nodes, "edges": edges, "dim": dim,
            "queries": queries, "vec_queries": vec_queries,
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

fn write_vec(w: &mut impl Write, v: &[f32]) -> Result<()> {
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            write!(w, " ")?;
        }
        write!(w, "{x:.6}")?;
    }
    Ok(())
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
fn run_pass(data: &Path, db_path: &Path, k: u64) -> Result<Vec<OpResult>> {
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
        {
            let mut txn = plane.write()?;
            for line in vec_lines.iter().skip(1) {
                let (id, rest) = line.split_once(',').unwrap();
                let v = parse_vector(rest);
                let mut props: Properties = BTreeMap::new();
                props.insert("embedding".into(), prop(PropValue::Vector(v)));
                txn.create_node_with_key(&format!("v{id}"), &["Item"], props)?;
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
    if med > 0.0 { (hi - lo) / med * 100.0 } else { 0.0 }
}

/// Run `repeat` measurement passes and aggregate: every reported metric is the
/// median across passes; `spread_pct` records the min→max spread of each op's
/// primary metric (latency median, else throughput) so noise stays visible
/// instead of silently baked into a single-shot number.
fn run(data: &Path, db_path: &Path, out: &Path, k: u64, repeat: u32) -> Result<()> {
    let repeat = repeat.max(1);
    let mut passes: Vec<Vec<OpResult>> = Vec::with_capacity(repeat as usize);
    for i in 0..repeat {
        if repeat > 1 {
            println!("pass {}/{repeat}…", i + 1);
        }
        passes.push(run_pass(data, db_path, k)?);
    }

    let per_op = |f: &dyn Fn(&OpResult) -> f64, op_idx: usize| -> Vec<f64> {
        passes.iter().map(|p| f(&p[op_idx])).collect()
    };
    let results: Vec<OpResult> = (0..passes[0].len())
        .map(|i| {
            let first = &passes[0][i];
            let latency = first.median_us.is_some();
            let primary = per_op(&|r| r.median_us.unwrap_or(r.throughput_per_s), i);
            OpResult {
                engine: first.engine.clone(),
                op: first.op.clone(),
                n: first.n,
                total_ms: median_of(per_op(&|r| r.total_ms, i)),
                median_us: latency.then(|| median_of(per_op(&|r| r.median_us.unwrap(), i))),
                p95_us: latency.then(|| median_of(per_op(&|r| r.p95_us.unwrap(), i))),
                throughput_per_s: median_of(per_op(&|r| r.throughput_per_s, i)),
                runs: (repeat > 1).then_some(repeat),
                spread_pct: (repeat > 1).then(|| spread_of(&primary)),
            }
        })
        .collect();

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
        match r.median_us {
            Some(m) => println!(
                "  {:<14} n={:<7} {:>9.2} ms total  median {:>8.2} µs  {:>12.0}/s{spread}",
                r.op, r.n, r.total_ms, m, r.throughput_per_s
            ),
            None => println!(
                "  {:<14} n={:<7} {:>9.2} ms total  {:>12.0}/s{spread}",
                r.op, r.n, r.total_ms, r.throughput_per_s
            ),
        }
    }
    Ok(())
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
        } => generate(&out, nodes, edges, dim, queries, vec_queries),
        Command::Run {
            data,
            db,
            out,
            k,
            repeat,
        } => run(&data, &db, &out, k, repeat),
    }
}
