//! Command handlers for `drsg` (arch/05). Each takes an open `Database` (or a
//! path) and writes to a `&mut dyn Write`, so they are unit-testable without
//! spawning a process.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use dr_strange_core::{
    BulkEdgeById, BulkNode, Database, Dir, LogicalPlan, Metric, NodeId, PlaneHandle, Properties,
};
use serde_json::{Value, json};

use dr_strange_core::json as jsonio;

/// Opens (creating if needed) the database at `path`.
pub fn open(path: &Path) -> Result<Database> {
    Database::open(path).with_context(|| format!("opening database at {}", path.display()))
}

fn plane<'db>(db: &'db Database, name: &str) -> Result<PlaneHandle<'db>> {
    db.plane(name)
        .with_context(|| format!("no such plane '{name}'"))
}

pub fn init(path: &Path, out: &mut dyn Write) -> Result<()> {
    open(path)?;
    writeln!(out, "initialized dr-strange database at {}", path.display())?;
    Ok(())
}

// ---- planes --------------------------------------------------------------

pub fn plane_list(db: &Database, out: &mut dyn Write) -> Result<()> {
    for (id, name) in db.planes()? {
        writeln!(out, "{}\t{}", id.0, name)?;
    }
    Ok(())
}

pub fn plane_create(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let handle = db.create_plane(name, Properties::new())?;
    writeln!(out, "created plane '{name}' (id {})", handle.id().0)?;
    Ok(())
}

pub fn plane_drop(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let id = plane(db, name)?.id();
    db.drop_plane(id)?;
    writeln!(out, "dropped plane '{name}'")?;
    Ok(())
}

pub fn plane_show(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, name)?;
    let props = p.properties()?;
    let cat = p.catalog()?;
    writeln!(
        out,
        "plane '{name}': {} nodes, {} edges",
        cat.node_count, cat.edge_count
    )?;
    if !props.is_empty() {
        writeln!(out, "  properties: {}", jsonio::properties_to_json(&props))?;
    }
    for (label, stats) in &cat.labels {
        writeln!(out, "  label {label}: {} nodes", stats.count)?;
    }
    for (ty, stats) in &cat.edge_types {
        writeln!(out, "  edge {ty}: {} edges", stats.count)?;
    }
    Ok(())
}

// ---- get / query / catalog -----------------------------------------------

/// Resolves a node reference: `@key` looks up an external key, otherwise a
/// numeric id.
fn resolve_node(p: &PlaneHandle, reference: &str) -> Result<Option<NodeId>> {
    if let Some(key) = reference.strip_prefix('@') {
        Ok(p.node_by_key(key)?.map(|n| n.id))
    } else {
        let id: u64 = reference
            .parse()
            .with_context(|| format!("'{reference}' is not a node id or @external-key"))?;
        Ok(Some(NodeId(id)))
    }
}

pub fn get(db: &Database, plane_name: &str, reference: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, plane_name)?;
    let Some(id) = resolve_node(&p, reference)? else {
        bail!("no node with external key {reference}");
    };
    match p.node(id)? {
        Some(node) => writeln!(out, "{}", jsonio::node_to_json(&node))?,
        None => bail!("no node with id {}", id.0),
    }
    Ok(())
}

pub fn query(db: &Database, plane_name: &str, plan_json: &str, out: &mut dyn Write) -> Result<()> {
    let plan: LogicalPlan =
        serde_json::from_str(plan_json).context("parsing the query plan JSON")?;
    run_plan(db, plane_name, plan, out)
}

/// Run a query written in the openCypher-subset language (arch/00 §5): compile
/// it to a `LogicalPlan`, then execute it exactly like the JSON `query` path.
/// `embed` names an embedding provider for a text `SEARCH … NEAR "…"`.
pub fn cypher(
    db: &Database,
    plane_name: &str,
    query: &str,
    embed: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let plan = compile_cypher(query, embed)?;
    run_plan(db, plane_name, plan, out)
}

/// Compile a query, embedding a text `SEARCH … NEAR "…"` when an `embed`
/// provider is given. Embedding lives behind the `digest` feature (which pulls
/// in dr-strange-llm); MATCH / literal-vector queries compile without it.
#[cfg(feature = "digest")]
fn compile_cypher(query: &str, embed: Option<&str>) -> Result<LogicalPlan> {
    // Adapt the LLM provider to the parser's embedder seam (key from the env).
    struct LlmEmbedder(Box<dyn dr_strange_llm::Embedder>);
    impl dr_strange_parser::Embedder for LlmEmbedder {
        fn embed(&self, text: &str) -> std::result::Result<Vec<f32>, String> {
            let reply = self
                .0
                .embed(&[text.to_string()])
                .map_err(|e| e.to_string())?;
            reply
                .vectors
                .into_iter()
                .next()
                .ok_or_else(|| "embedder returned no vector".to_string())
        }
    }
    match embed {
        Some(provider) => {
            let e = LlmEmbedder(Box::new(dr_strange_llm::build_provider(
                provider, None, None, None, true,
            )?));
            dr_strange_parser::parse_with_embedder(query, &e).map_err(|e| anyhow!("{e}"))
        }
        None => dr_strange_parser::parse(query).map_err(|e| anyhow!("{e}")),
    }
}

#[cfg(not(feature = "digest"))]
fn compile_cypher(query: &str, embed: Option<&str>) -> Result<LogicalPlan> {
    if embed.is_some() {
        bail!(
            "text SEARCH embedding needs the `digest` build feature \
             (this binary was built with --no-default-features)"
        );
    }
    dr_strange_parser::parse(query).map_err(|e| anyhow!("{e}"))
}

/// Execute a `LogicalPlan` and print each matched node as a JSON line, tagging
/// the similarity score when the plan produced one.
fn run_plan(db: &Database, plane_name: &str, plan: LogicalPlan, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, plane_name)?;
    for (node, score) in p.query_from_plan(plan).scored_nodes()? {
        let mut obj = jsonio::node_to_json(&node);
        if let (Some(s), Value::Object(map)) = (score, &mut obj) {
            map.insert("score".into(), json!(s));
        }
        writeln!(out, "{obj}")?;
    }
    Ok(())
}

pub fn catalog(db: &Database, plane_name: Option<&str>, out: &mut dyn Write) -> Result<()> {
    let cat = match plane_name {
        Some(name) => plane(db, name)?.catalog()?,
        None => db.catalog()?,
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&cat)?)?;
    Ok(())
}

pub fn index_ensure(
    db: &Database,
    plane_name: &str,
    label: &str,
    property: &str,
    metric: Metric,
    out: &mut dyn Write,
) -> Result<()> {
    plane(db, plane_name)?.ensure_vector_index(label, property, metric)?;
    writeln!(out, "ensured vector index on {label}.{property}")?;
    Ok(())
}

pub fn stats(db: &Database, out: &mut dyn Write) -> Result<()> {
    let planes = db.planes()?;
    let cat = db.catalog()?;
    writeln!(
        out,
        "{} planes, {} nodes, {} edges",
        planes.len(),
        cat.node_count,
        cat.edge_count
    )?;
    Ok(())
}

pub fn check(db: &Database, out: &mut dyn Write) -> Result<()> {
    // A full scan of every plane (via the catalog) exercises decode paths and
    // surfaces corruption as an error. arch/05 §2.
    let mut nodes = 0u64;
    for (_, name) in db.planes()? {
        nodes += db.plane(&name)?.catalog()?.node_count;
    }
    writeln!(out, "ok: {nodes} nodes readable across all planes")?;
    Ok(())
}

// ---- digest (LLM ingest, arch/07) ----------------------------------------

/// Flags for [`digest`]. Chat and embeddings are configured separately so a
/// document can be extracted by one provider and embedded by another (e.g.
/// `--chat deepseek --embed qwen`, since DeepSeek has no embeddings endpoint).
#[cfg(feature = "digest")]
pub struct DigestArgs<'a> {
    pub file: &'a Path,
    pub plane: &'a str,
    pub apply: bool,
    pub chunk_chars: usize,
    pub embed: bool,
    /// Link extracted entities to existing plane nodes via vector retrieval.
    pub link: bool,
    /// Provider preset name (openai/deepseek/qwen/ollama) or a raw base URL.
    pub chat_provider: &'a str,
    pub embed_provider: &'a str,
    pub model: Option<&'a str>,
    pub embed_model: Option<&'a str>,
    pub chat_url: Option<&'a str>,
    pub embed_url: Option<&'a str>,
    pub chat_key_env: Option<&'a str>,
    pub embed_key_env: Option<&'a str>,
}

/// Digests a document into the plane: an LLM extracts entities/relations
/// (labels chosen purely from the document), they're embedded and stamped with
/// provenance, and — only with `apply` — written through the bulk path.
/// Dry-run by default (arch/07 §2: proposals, not mutations).
#[cfg(feature = "digest")]
pub fn digest(db: &Database, args: &DigestArgs, out: &mut dyn Write) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let doc = std::fs::read_to_string(args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let source = args
        .file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.file.display().to_string());

    let chat = dr_strange_llm::build_provider(
        args.chat_provider,
        args.model,
        args.chat_url,
        args.chat_key_env,
        false,
    )?;
    let chat_model = chat.model().to_string();
    let embedder = dr_strange_llm::build_provider(
        args.embed_provider,
        args.embed_model,
        args.embed_url,
        args.embed_key_env,
        args.embed,
    )?;

    let p = plane(db, args.plane)?;
    let run_id = format!(
        "{}-{}",
        source,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let opts = dr_strange_llm::DigestOptions {
        source,
        model: chat_model,
        run_id,
        chunk_chars: args.chunk_chars,
        embed: args.embed,
    };

    let cands = dr_strange_llm::PlaneCandidates::new(&p);
    let candidates = args
        .link
        .then_some(&cands as &dyn dr_strange_llm::CandidateSource);
    let result = dr_strange_llm::digest(&doc, &chat, &embedder, candidates, &opts)?;
    let r = &result.report;
    writeln!(
        out,
        "digest: {} chunks → {} new entities ({} linked to existing), {} relations ({} dangling dropped)",
        r.chunks, r.entities, r.linked, r.relations, r.dropped_relations
    )?;
    writeln!(
        out,
        "  {} chat request(s); tokens {} in / {} out / {} embed",
        r.chat_requests, r.input_tokens, r.output_tokens, r.embed_tokens
    )?;

    if args.apply {
        let mut txn = p.write()?;
        let stats = result.apply(&mut txn)?;
        txn.commit()?;
        writeln!(
            out,
            "applied: wrote {} nodes, {} edges",
            stats.nodes, stats.edges
        )?;
        if args.embed {
            writeln!(
                out,
                "  embeddings stored as `embedding`; `drsg index ensure <label> embedding` for indexed search"
            )?;
        }
    } else {
        for n in result.nodes.iter().take(12) {
            writeln!(out, "  [{}] {} ({} props)", n.label, n.key, n.props.len())?;
        }
        if result.nodes.len() > 12 {
            writeln!(out, "  … and {} more", result.nodes.len() - 12)?;
        }
        writeln!(out, "dry run — re-run with --apply to write")?;
    }
    Ok(())
}

// ---- import / export -----------------------------------------------------

/// An edge endpoint reference in the JSONL: `{prefix}_key` (external key) or
/// `{prefix}` (numeric node id, as `export` emits).
enum Ref {
    Key(String),
    Id(u64),
}

/// Imports JSONL: each line is a node `{"id"?, "labels":[…], "external_key"?,
/// "properties"?}` or an edge `{"src_key"|"src", "dst_key"|"dst", "type",
/// "properties"?}` (an edge line is one carrying `type`).
///
/// Uses the bulk-load fast path: the whole file is buffered, nodes are loaded
/// in one batch, then edge endpoints are resolved — by external key, or by
/// remapping the exported numeric `id` to the node's freshly-assigned one —
/// and edges are bulk-written. Endpoints must resolve within this batch or
/// already exist in the plane; keys are assumed fresh (as bulk load requires).
pub fn import(
    db: &Database,
    plane_name: &str,
    reader: impl BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let p = plane(db, plane_name)?;

    // Buffer the whole file (bulk load needs the batch up front).
    let mut old_ids: Vec<Option<u64>> = Vec::new();
    let mut keys: Vec<Option<String>> = Vec::new();
    let mut labels: Vec<Vec<String>> = Vec::new();
    let mut node_props: Vec<Properties> = Vec::new();
    let mut edges: Vec<(Ref, Ref, String, Properties)> = Vec::new();

    for (lineno, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ctx = || format!("line {}", lineno + 1);
        let value: Value =
            serde_json::from_str(&line).with_context(|| format!("{}: bad JSON", ctx()))?;
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("{}: expected a JSON object", ctx()))?;

        if obj.contains_key("type") {
            let src = parse_ref(obj, "src").with_context(ctx)?;
            let dst = parse_ref(obj, "dst").with_context(ctx)?;
            let ty = obj
                .get("type")
                .and_then(|t| t.as_str())
                .with_context(|| format!("{}: edge missing `type`", ctx()))?
                .to_string();
            edges.push((src, dst, ty, edge_props(obj)?));
        } else {
            old_ids.push(obj.get("id").and_then(Value::as_u64));
            keys.push(
                obj.get("external_key")
                    .and_then(|k| k.as_str())
                    .map(str::to_string),
            );
            labels.push(
                obj.get("labels")
                    .and_then(|l| l.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            node_props.push(edge_props(obj)?);
        }
    }

    let mut txn = p.write()?;

    // Node phase (fast path): one batch, contiguous ids.
    let label_refs: Vec<Vec<&str>> = labels
        .iter()
        .map(|ls| ls.iter().map(String::as_str).collect())
        .collect();
    let bnodes: Vec<BulkNode> = keys
        .iter()
        .zip(&label_refs)
        .zip(node_props)
        .map(|((k, lr), props)| BulkNode {
            external_key: k.as_deref(),
            labels: lr,
            props,
        })
        .collect();
    let n_nodes = bnodes.len() as u64;
    let stats = txn.bulk_load(bnodes, Vec::new())?;

    // Maps from this batch's identifiers to the freshly-assigned node ids.
    let mut old_to_new = std::collections::HashMap::new();
    let mut key_to_new = std::collections::HashMap::new();
    for i in 0..n_nodes as usize {
        let id = NodeId(stats.node_start + i as u64);
        if let Some(o) = old_ids[i] {
            old_to_new.insert(o, id);
        }
        if let Some(k) = &keys[i] {
            key_to_new.insert(k.clone(), id);
        }
    }

    // Resolve + validate every endpoint, then bulk-write the edges by id.
    let mut bedges: Vec<BulkEdgeById> = Vec::with_capacity(edges.len());
    for (src, dst, ty, props) in &edges {
        bedges.push(BulkEdgeById {
            src: resolve(src, &key_to_new, &old_to_new, &p)?,
            dst: resolve(dst, &key_to_new, &old_to_new, &p)?,
            ty,
            props: props.clone(),
        });
    }
    let n_edges = txn.bulk_load_edges(bedges)?;

    txn.commit()?;
    tracing::info!(
        plane = plane_name,
        nodes = n_nodes,
        edges = n_edges,
        "imported JSONL into plane",
    );
    writeln!(out, "imported {n_nodes} nodes, {n_edges} edges")?;
    Ok(())
}

fn edge_props(obj: &serde_json::Map<String, Value>) -> Result<Properties> {
    Ok(obj
        .get("properties")
        .map(jsonio::json_to_properties)
        .transpose()?
        .unwrap_or_default())
}

fn parse_ref(obj: &serde_json::Map<String, Value>, prefix: &str) -> Result<Ref> {
    if let Some(key) = obj.get(&format!("{prefix}_key")).and_then(|v| v.as_str()) {
        Ok(Ref::Key(key.to_string()))
    } else if let Some(id) = obj.get(prefix).and_then(|v| v.as_u64()) {
        Ok(Ref::Id(id))
    } else {
        bail!("edge missing `{prefix}_key` or `{prefix}`")
    }
}

/// Resolves a reference to a node id, validating existence: a batch key/id
/// maps to the freshly-assigned id; otherwise it must already exist in the
/// plane (a committed key, or a live node id).
fn resolve(
    r: &Ref,
    key_to_new: &std::collections::HashMap<String, NodeId>,
    old_to_new: &std::collections::HashMap<u64, NodeId>,
    p: &PlaneHandle,
) -> Result<NodeId> {
    match r {
        Ref::Key(k) => {
            if let Some(&id) = key_to_new.get(k) {
                return Ok(id);
            }
            p.node_by_key(k)?
                .map(|n| n.id)
                .ok_or_else(|| anyhow!("edge references unknown key '{k}'"))
        }
        Ref::Id(o) => {
            if let Some(&id) = old_to_new.get(o) {
                return Ok(id);
            }
            let id = NodeId(*o);
            if p.node(id)?.is_some() {
                Ok(id)
            } else {
                bail!("edge references unknown node id {o}")
            }
        }
    }
}

/// Exports a plane as JSONL: node lines then edge lines (id-based).
pub fn export(db: &Database, plane_name: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, plane_name)?;
    for node in p.query().scan_all().nodes()? {
        writeln!(out, "{}", jsonio::node_to_json(&node))?;
    }
    // Edges: walk every node's out-adjacency, emit each edge once.
    for node in p.query().scan_all().nodes()? {
        for n in p.neighbors(node.id, Dir::Out, None)? {
            if let Some(edge) = p.edge(n.edge)? {
                writeln!(
                    out,
                    "{}",
                    json!({
                        "id": edge.id.0,
                        "src": edge.src.0,
                        "dst": edge.dst.0,
                        "type": edge.ty,
                        "properties": jsonio::properties_to_json(&edge.properties),
                    })
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a handler and returns its captured stdout as a String.
    fn cap(f: impl FnOnce(&mut dyn Write) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    const SAMPLE: &str = concat!(
        r#"{"labels":["Paper"],"external_key":"p1","properties":{"year":2020,"emb":{"$vector":[0.0,0.0]}}}"#,
        "\n",
        r#"{"labels":["Paper"],"external_key":"p2","properties":{"year":2021,"emb":{"$vector":[1.0,0.0]}}}"#,
        "\n",
        r#"{"src_key":"p1","dst_key":"p2","type":"CITES"}"#,
        "\n",
    );

    fn loaded() -> Database {
        let db = Database::in_memory().unwrap();
        cap(|out| import(&db, "startup", SAMPLE.as_bytes(), out));
        db
    }

    #[test]
    fn plane_lifecycle() {
        let db = Database::in_memory().unwrap();
        assert!(cap(|o| plane_create(&db, "scratch", o)).contains("created plane 'scratch'"));
        let list = cap(|o| plane_list(&db, o));
        assert!(list.contains("startup") && list.contains("scratch"));
        assert!(cap(|o| plane_drop(&db, "scratch", o)).contains("dropped"));
        assert!(db.plane("scratch").is_err());
    }

    #[test]
    fn import_then_get_and_stats() {
        let db = loaded();
        let got = cap(|o| get(&db, "startup", "@p1", o));
        assert!(got.contains("\"external_key\":\"p1\""));
        assert!(got.contains("\"year\":2020"));
        // get by numeric id works too
        assert!(cap(|o| get(&db, "startup", "1", o)).contains("\"id\":1"));
        assert!(cap(|o| stats(&db, o)).contains("1 planes, 2 nodes, 1 edges"));
        assert!(cap(|o| check(&db, o)).contains("ok: 2 nodes"));
    }

    #[test]
    fn import_remaps_exported_numeric_edge_ids() {
        // The file's node ids (5, 6) don't match the fresh db's assignments;
        // the numeric edge (src:5 → dst:6) must still connect a → b.
        let jsonl = concat!(
            r#"{"id":5,"external_key":"a","labels":["N"]}"#,
            "\n",
            r#"{"id":6,"external_key":"b","labels":["N"]}"#,
            "\n",
            r#"{"src":5,"dst":6,"type":"E"}"#,
            "\n",
        );
        let db = Database::in_memory().unwrap();
        cap(|o| import(&db, "startup", jsonl.as_bytes(), o));
        let p = db.plane("startup").unwrap();
        let a = p.node_by_key("a").unwrap().unwrap();
        let b = p.node_by_key("b").unwrap().unwrap();
        assert_ne!(a.id.0, 5, "ids are reassigned, not copied from the file");
        let ns = p.neighbors(a.id, Dir::Out, None).unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].node, b.id, "numeric edge remapped to the right node");
    }

    #[test]
    fn query_plan_json() {
        let db = loaded();
        // scan Paper, filter year >= 2021 -> only p2
        let plan = r#"{"source":{"ScanLabel":"Paper"},"steps":[
            {"Filter":{"Compare":{"op":"Ge","lhs":{"Property":"year"},"rhs":{"Literal":{"Int":2021}}}}}]}"#;
        let out = cap(|o| query(&db, "startup", plan, o));
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
    }

    #[test]
    fn cypher_query_over_the_graph() {
        let db = loaded();
        // WHERE pushdown: scan Paper, keep year >= 2021 → only p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (n:Paper) WHERE n.year >= 2021 RETURN n",
                None,
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // Traversal over the CITES edge: p1 → p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (a:Paper)-[:CITES]->(b:Paper) RETURN b",
                None,
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // A vector-literal SEARCH runs the top-k with no embedder: the fixture's
        // Papers carry an `emb` vector, so NEAR [1,0] (cosine) returns p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "SEARCH (n:Paper) ON emb NEAR [1.0, 0.0] TOPK 1 RETURN n",
                None,
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // An unsupported query surfaces the parser's error, not a panic.
        let mut sink = Vec::new();
        let err = cypher(&db, "startup", "MATCH (n)", None, &mut sink).unwrap_err();
        assert!(err.to_string().contains("syntax error"), "{err}");
        // A text SEARCH with no --embed is a clear error, not a panic.
        let mut sink = Vec::new();
        let err = cypher(
            &db,
            "startup",
            "SEARCH (n:Paper) ON emb NEAR \"hi\" RETURN n",
            None,
            &mut sink,
        )
        .unwrap_err();
        assert!(err.to_string().contains("embedding provider"), "{err}");
    }

    #[test]
    fn vector_query_via_declared_index() {
        let db = loaded();
        cap(|o| index_ensure(&db, "startup", "Paper", "emb", Metric::L2, o));
        let plan = r#"{"source":{"VectorTopK":{"label":"Paper","property":"emb",
            "query":[0.0,0.0],"metric":"L2","k":1}},"steps":[]}"#;
        let out = cap(|o| query(&db, "startup", plan, o));
        assert!(out.contains("\"external_key\":\"p1\"")); // nearest [0,0]
        assert!(out.contains("\"score\":")); // score channel projected
    }

    #[test]
    fn catalog_and_show() {
        let db = loaded();
        let cat = cap(|o| catalog(&db, Some("startup"), o));
        assert!(cat.contains("\"Paper\""));
        assert!(cat.contains("\"node_count\": 2"));
        // whole-db roll-up too
        assert!(cap(|o| catalog(&db, None, o)).contains("\"node_count\": 2"));
        assert!(cap(|o| plane_show(&db, "startup", o)).contains("2 nodes, 1 edges"));
    }

    #[test]
    fn export_round_trips_into_a_second_db() {
        let db = loaded();
        let dumped = cap(|o| export(&db, "startup", o));
        // nodes carry keys; the re-import resolves the edge by src_key/dst_key
        let db2 = Database::in_memory().unwrap();
        cap(|o| import(&db2, "startup", dumped.as_bytes(), o));
        assert!(cap(|o| stats(&db2, o)).contains("2 nodes, 1 edges"));
    }

    #[test]
    fn bad_plan_and_missing_node_error() {
        let db = loaded();
        assert!(query(&db, "startup", "not json", &mut Vec::new()).is_err());
        assert!(get(&db, "startup", "9999", &mut Vec::new()).is_err());
        assert!(get(&db, "startup", "@nope", &mut Vec::new()).is_err());
    }
}
