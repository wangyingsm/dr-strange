//! The JSON-RPC method implementations (arch/08 §1). Each is a plain
//! synchronous `fn(&Ctx, params) -> Result<Value, RpcError>` that wraps the
//! core `Database` API and serializes through the core's `json` dialect — the
//! same structures the CLI and MCP emit, so all three surfaces agree on the
//! wire shape. Most methods are reads; `digest.run`/`digest.write` power the
//! digest page (arch/07), the latter writing through the bulk path.

use std::path::Path;

use dr_strange_core::{
    BulkEdge, BulkNode, Database, Dir, EdgeRecord, LogicalPlan, Metric, NodeId, NodeRecord,
    Properties, json,
};
use dr_strange_llm::Embedder; // brings `.embed()` into scope for semantic_find
use serde::Deserialize;
use serde_json::{Value, json as jval};

use crate::rpc::RpcError;

/// What a method needs from the running server: the database and, when the
/// backend is file-backed, its path (for `db.stats` file size — the core is
/// deliberately stateless about its own on-disk footprint, arch/08 §5).
pub struct Ctx<'a> {
    pub db: &'a Database,
    pub db_path: Option<&'a Path>,
}

// ---- param decoding -------------------------------------------------------

fn params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

/// Core errors are the caller's fault far more often than ours (unknown plane,
/// bad plan), so they ride the server-error code, not `-32603 internal`.
fn app<T>(r: dr_strange_core::Result<T>) -> Result<T, RpcError> {
    r.map_err(|e| RpcError::server(e.to_string()))
}

fn parse_metric(s: Option<&str>) -> Metric {
    match s {
        Some("dot") | Some("Dot") => Metric::Dot,
        Some("l2") | Some("L2") => Metric::L2,
        _ => Metric::Cosine,
    }
}

fn parse_dir(s: Option<&str>) -> Dir {
    match s {
        Some("in") | Some("In") => Dir::In,
        Some("both") | Some("Both") => Dir::Both,
        _ => Dir::Out,
    }
}

/// Node records with an optional similarity/traversal score folded in as a
/// `score` field — mirrors the MCP `scored_rows` shape so plots (chunk 2) can
/// size/colour by score without a second call.
/// An edge record as a JSON object — the counterpart to `json::node_to_json`
/// (which the core provides), kept here since the core's dialect has no edge
/// form yet. The plot merges these into its graph model alongside nodes.
fn edge_to_json(e: &EdgeRecord) -> Value {
    jval!({
        "id": e.id.0,
        "src": e.src.0,
        "dst": e.dst.0,
        "type": e.ty,
        "properties": json::properties_to_json(&e.properties),
    })
}

fn scored_rows(rows: &[(NodeRecord, Option<f32>)]) -> Value {
    Value::Array(
        rows.iter()
            .map(|(n, s)| {
                let mut obj = json::node_to_json(n);
                if let (Some(score), Value::Object(map)) = (s, &mut obj) {
                    map.insert("score".into(), jval!(score));
                }
                obj
            })
            .collect(),
    )
}

// ---- methods --------------------------------------------------------------

/// `db.stats` — the dashboard's health panel: plane/node/edge counts plus the
/// file size when the backend is on disk.
pub fn db_stats(ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let planes = app(ctx.db.planes())?;
    let cat = app(ctx.db.catalog())?;
    let file_size = ctx
        .db_path
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    Ok(jval!({
        "planes": planes.len(),
        "nodes": cat.node_count,
        "edges": cat.edge_count,
        "persistent": ctx.db_path.is_some(),
        "file_size": file_size,
    }))
}

/// `db.catalog` — the soft schema across every plane.
pub fn db_catalog(ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let cat = app(ctx.db.catalog())?;
    serde_json::to_value(cat).map_err(|e| RpcError::server(e.to_string()))
}

/// `plane.list` — plane cards: id, name, counts, and any plane properties.
pub fn plane_list(ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let mut out = Vec::new();
    for (id, name) in app(ctx.db.planes())? {
        let plane = app(ctx.db.plane(&name))?;
        let cat = app(plane.catalog())?;
        let props = app(plane.properties())?;
        out.push(jval!({
            "id": id.0,
            "name": name,
            "nodes": cat.node_count,
            "edges": cat.edge_count,
            "properties": json::properties_to_json(&props),
        }));
    }
    Ok(Value::Array(out))
}

#[derive(Deserialize)]
pub struct PlaneOnly {
    plane: String,
}

/// `plane.catalog` — one plane's soft schema (labels, property descriptions,
/// edge-type connectivity, counts).
pub fn plane_catalog(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PlaneOnly = params(p)?;
    let cat = app(app(ctx.db.plane(&req.plane))?.catalog())?;
    serde_json::to_value(cat).map_err(|e| RpcError::server(e.to_string()))
}

#[derive(Deserialize)]
pub struct GetNode {
    plane: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    key: Option<String>,
}

/// `node.get` — one node by id or external key; `null` if absent.
pub fn node_get(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: GetNode = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let node = match (req.id, &req.key) {
        (Some(id), _) => app(plane.node(NodeId(id)))?,
        (None, Some(key)) => app(plane.node_by_key(key))?,
        (None, None) => return Err(RpcError::invalid_params("provide `id` or `key`")),
    };
    Ok(node.map(|n| json::node_to_json(&n)).unwrap_or(Value::Null))
}

#[derive(Deserialize)]
pub struct Neighbors {
    plane: String,
    id: u64,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default, rename = "type")]
    edge_type: Option<String>,
}

/// `plane.neighbors` — 1-hop expansion as `{node, edge}` id pairs. Chunk 2's
/// plot enriches these into full records; chunk 1 keeps it to the raw hop.
pub fn plane_neighbors(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Neighbors = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let dir = parse_dir(req.direction.as_deref());
    let hops = app(plane.neighbors(NodeId(req.id), dir, req.edge_type.as_deref()))?;
    Ok(Value::Array(
        hops.iter()
            .map(|n| jval!({ "node": n.node.0, "edge": n.edge.0 }))
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct Search {
    plane: String,
    property: String,
    query: Vec<f32>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    k: Option<u64>,
    #[serde(default)]
    metric: Option<String>,
}

/// `plane.search` — vector top-k, returning scored node records.
pub fn plane_search(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Search = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let hits = app(plane
        .query()
        .vector_top_k(
            req.label.as_deref(),
            &req.property,
            req.query,
            parse_metric(req.metric.as_deref()),
            req.k.unwrap_or(10),
        )
        .scored_nodes())?;
    Ok(scored_rows(&hits))
}

#[derive(Deserialize)]
pub struct RunPlan {
    plane: String,
    plan: Value,
}

/// `plane.query` — run a serialized logical plan verbatim (the params ride the
/// wire exactly as MCP/CLI send them) and return scored rows.
pub fn plane_query(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: RunPlan = params(p)?;
    let plan: LogicalPlan =
        serde_json::from_value(req.plan).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let rows = app(app(ctx.db.plane(&req.plane))?
        .query_from_plan(plan)
        .scored_nodes())?;
    Ok(scored_rows(&rows))
}

// ---- graph-plot subgraph methods (chunk 2, arch/08 §2.2) ------------------

/// The default node cap for a seeded view — the plot never asks the core for
/// an unbounded dump (arch/08 §2.2, "cursors throughout").
const SEED_LIMIT: u64 = 200;
/// The default fan-out cap for one click-to-expand (hub-safe expansion).
const EXPAND_LIMIT: u64 = 100;
/// Text search stops after examining this many nodes — there is no text index,
/// so `plane.find` is a linear scan; the cap keeps a huge plane responsive.
const FIND_SCAN_CAP: usize = 20_000;
/// Default number of matches `plane.find` returns.
const FIND_LIMIT: usize = 50;

#[derive(Deserialize)]
pub struct Seed {
    plane: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `graph.seed` — an initial canvas: up to `limit` nodes (optionally of one
/// label) plus the edges induced among exactly that node set. `total` is the
/// full unfiltered node count so the UI can say how much was left off.
pub fn graph_seed(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Seed = params(p)?;
    let limit = req.limit.unwrap_or(SEED_LIMIT);
    let plane = app(ctx.db.plane(&req.plane))?;

    let all_ids = match &req.label {
        Some(label) => app(plane.query().scan_label(label.clone()).ids())?,
        None => app(plane.query().scan_all().ids())?,
    };
    let total = all_ids.len();
    let ids: Vec<NodeId> = all_ids.into_iter().take(limit as usize).collect();
    let set: std::collections::BTreeSet<u64> = ids.iter().map(|n| n.0).collect();

    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(node) = app(plane.node(*id))? {
            nodes.push(json::node_to_json(&node));
        }
    }

    // Induced edges: walk each in-set node's outgoing hops and keep those
    // whose destination is also in the set (each undirected edge is captured
    // exactly once, from its source). Dedup by edge id defensively.
    let mut seen_edges = std::collections::BTreeSet::new();
    let mut edges = Vec::new();
    for id in &ids {
        for hop in app(plane.neighbors(*id, Dir::Out, None))? {
            if set.contains(&hop.node.0)
                && seen_edges.insert(hop.edge.0)
                && let Some(edge) = app(plane.edge(hop.edge))?
            {
                edges.push(edge_to_json(&edge));
            }
        }
    }

    Ok(jval!({
        "nodes": nodes,
        "edges": edges,
        "total": total,
        "truncated": total > ids.len(),
    }))
}

#[derive(Deserialize)]
pub struct Find {
    plane: String,
    q: String,
    #[serde(default)]
    limit: Option<usize>,
    /// Rank nodes by embedding similarity instead of substring matching.
    #[serde(default)]
    semantic: bool,
    /// Embedding provider for semantic mode (preset or base URL); server env
    /// supplies the key. Must match the model the plane was embedded with.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

/// `plane.find` — text search over the plane. Nodes match on external key,
/// labels, and string property values; edges match on type and string property
/// values. Both hit `match` hints (which field matched) so the UI can show
/// *why* something surfaced. There is no text index (arch/03), so this is a
/// linear scan capped at [`FIND_SCAN_CAP`] nodes / edges and [`limit`] results
/// each; `truncated` says whether either cap cut the results short.
pub fn plane_find(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Find = params(p)?;
    let limit = req.limit.unwrap_or(FIND_LIMIT).min(FIND_LIMIT);
    if req.q.trim().is_empty() {
        return Ok(
            jval!({ "nodes": [], "edges": [], "mode": "text", "scanned": 0, "total": 0, "truncated": false }),
        );
    }

    let plane = app(ctx.db.plane(&req.plane))?;

    // Semantic mode: embed the query and rank nodes by vector similarity. Any
    // failure — no key, provider error, or a plane with no embeddings — falls
    // back to the text scan below, surfacing why via `note`.
    let mut note: Option<String> = None;
    if req.semantic {
        match semantic_find(&plane, &req, limit) {
            Ok(hits) if !hits.is_empty() => {
                let n = hits.len();
                return Ok(jval!({
                    "nodes": hits,
                    "edges": [],
                    "mode": "semantic",
                    "scanned": n,
                    "total": n,
                    "truncated": false,
                }));
            }
            Ok(_) => note = Some("no embedded nodes in this plane — showing text matches".into()),
            Err(e) => note = Some(format!("semantic unavailable ({e}) — showing text matches")),
        }
    }

    let needle = req.q.trim().to_lowercase();
    let all = app(plane.query().scan_all().nodes())?;
    let total = all.len();

    // ---- nodes ----
    let mut node_hits = Vec::new();
    let mut examined = 0usize;
    for n in &all {
        if examined >= FIND_SCAN_CAP {
            break;
        }
        examined += 1;
        if let Some(hint) = match_node(n, &needle) {
            let mut obj = json::node_to_json(n);
            if let Value::Object(map) = &mut obj {
                map.insert("match".into(), Value::String(hint));
            }
            node_hits.push(obj);
            if node_hits.len() >= limit {
                break;
            }
        }
    }
    let nodes_truncated = examined < total;

    // ---- edges ----
    // The core has no edge scan, so walk each node's outgoing hops (as
    // `graph.seed` does), dedup by edge id, and match the edge record.
    let mut edge_hits = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut edges_examined = 0usize;
    let mut edges_truncated = false;
    'walk: for n in &all {
        for hop in app(plane.neighbors(n.id, Dir::Out, None))? {
            if !seen.insert(hop.edge.0) {
                continue;
            }
            if edges_examined >= FIND_SCAN_CAP {
                edges_truncated = true;
                break 'walk;
            }
            edges_examined += 1;
            if let Some(edge) = app(plane.edge(hop.edge))?
                && let Some(hint) = match_edge(&edge, &needle)
            {
                let mut obj = edge_to_json(&edge);
                if let Value::Object(map) = &mut obj {
                    map.insert("match".into(), Value::String(hint));
                }
                edge_hits.push(obj);
                if edge_hits.len() >= limit {
                    edges_truncated = true;
                    break 'walk;
                }
            }
        }
    }

    Ok(jval!({
        "nodes": node_hits,
        "edges": edge_hits,
        "mode": "text",
        "note": note,
        "scanned": examined,
        "total": total,
        "truncated": nodes_truncated || edges_truncated,
    }))
}

/// Semantic search: embed the query with the requested provider (key from the
/// server env) and return the plane's most vector-similar nodes, each carrying
/// a `score` and a `match` hint. Errors (no key, provider down, no embed model)
/// and an empty result (a plane with no embeddings, or embeddings of a
/// different dimension) let the caller fall back to text.
fn semantic_find(
    plane: &dr_strange_core::PlaneHandle<'_>,
    req: &Find,
    limit: usize,
) -> anyhow::Result<Vec<Value>> {
    let provider = req.provider.as_deref().unwrap_or("openai");
    let embedder =
        dr_strange_llm::build_provider(provider, req.embed_model.as_deref(), None, None, true)?;
    let reply = embedder.embed(std::slice::from_ref(&req.q))?;
    let query = reply
        .vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;

    let hits = plane
        .query()
        .vector_top_k(None, "embedding", query, Metric::Cosine, limit as u64)
        .scored_nodes()
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(hits
        .iter()
        .map(|(n, score)| {
            let mut obj = json::node_to_json(n);
            if let Value::Object(map) = &mut obj {
                match score {
                    Some(s) => {
                        map.insert("score".into(), jval!(s));
                        map.insert(
                            "match".into(),
                            Value::String(format!("semantic · {:.0}%", s * 100.0)),
                        );
                    }
                    None => {
                        map.insert("match".into(), Value::String("semantic".into()));
                    }
                }
            }
            obj
        })
        .collect())
}

/// Returns a short "matched in …" hint if `needle` (already lowercased) occurs
/// in the node's key, a label, or a string property value; `None` otherwise.
/// Key is checked first, then labels, then properties — most-specific first.
fn match_node(n: &NodeRecord, needle: &str) -> Option<String> {
    if n.external_key
        .as_deref()
        .is_some_and(|k| k.to_lowercase().contains(needle))
    {
        return Some("key".into());
    }
    if let Some(l) = n.labels.iter().find(|l| l.to_lowercase().contains(needle)) {
        return Some(format!("label: {l}"));
    }
    for (k, pd) in &n.properties {
        if let dr_strange_core::PropValue::Str(s) = &pd.value
            && s.to_lowercase().contains(needle)
        {
            return Some(format!("{k}: {}", snippet(s)));
        }
    }
    None
}

/// Like [`match_node`] but for an edge: matches its type, then string property
/// values. `None` if `needle` (already lowercased) occurs in neither.
fn match_edge(e: &EdgeRecord, needle: &str) -> Option<String> {
    if e.ty.to_lowercase().contains(needle) {
        return Some("type".into());
    }
    for (k, pd) in &e.properties {
        if let dr_strange_core::PropValue::Str(s) = &pd.value
            && s.to_lowercase().contains(needle)
        {
            return Some(format!("{k}: {}", snippet(s)));
        }
    }
    None
}

/// Trim a matched property value for display (single line, bounded length).
fn snippet(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 80 {
        format!("{}…", one_line.chars().take(80).collect::<String>())
    } else {
        one_line
    }
}

#[derive(Deserialize)]
pub struct Expand {
    plane: String,
    id: u64,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default, rename = "type")]
    edge_type: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `graph.expand` — hub-safe neighbourhood expansion around one node: the
/// neighbour node records plus the connecting edge records, capped at `limit`
/// hops. `total` is the full incident count so the UI can offer "N more…".
pub fn graph_expand(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Expand = params(p)?;
    let limit = req.limit.unwrap_or(EXPAND_LIMIT) as usize;
    let plane = app(ctx.db.plane(&req.plane))?;
    let dir = parse_dir(req.direction.as_deref());

    let hops = app(plane.neighbors(NodeId(req.id), dir, req.edge_type.as_deref()))?;
    let total = hops.len();

    let mut seen_nodes = std::collections::BTreeSet::new();
    let mut seen_edges = std::collections::BTreeSet::new();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for hop in hops.into_iter().take(limit) {
        if seen_nodes.insert(hop.node.0)
            && let Some(node) = app(plane.node(hop.node))?
        {
            nodes.push(json::node_to_json(&node));
        }
        if seen_edges.insert(hop.edge.0)
            && let Some(edge) = app(plane.edge(hop.edge))?
        {
            edges.push(edge_to_json(&edge));
        }
    }

    Ok(jval!({
        "nodes": nodes,
        "edges": edges,
        "total": total,
        "truncated": total > limit,
    }))
}

// ---- digest (LLM ingest, arch/07 via the web page) ------------------------

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn llm_err(e: anyhow::Error) -> RpcError {
    RpcError::server(e.to_string())
}

#[derive(Deserialize)]
pub struct DigestRun {
    /// Target plane — only read here, to retrieve existing entities as reuse
    /// candidates for linking (the write happens in `digest.write`).
    plane: String,
    /// The document text to digest.
    text: String,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    embed: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    no_embed: bool,
    /// Link extracted entities to existing graph nodes via vector retrieval
    /// (default true). Off ⇒ every entity is proposed as new.
    #[serde(default)]
    link: Option<bool>,
}

/// `digest.run` — extract a proposal from text (LLM, dry-run). Provider API
/// keys come from the server's environment, never params. Blocking work runs
/// on the /rpc handler's blocking task.
pub fn digest_run(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DigestRun = params(p)?;
    let chat_provider = req.chat.as_deref().unwrap_or("openai");
    let embed_provider = req.embed.as_deref().unwrap_or(chat_provider);
    let embed = !req.no_embed;
    let link = req.link.unwrap_or(true);

    let chat =
        dr_strange_llm::build_provider(chat_provider, req.model.as_deref(), None, None, false)
            .map_err(llm_err)?;
    let chat_model = chat.model().to_string();
    let embedder = dr_strange_llm::build_provider(
        embed_provider,
        req.embed_model.as_deref(),
        None,
        None,
        embed,
    )
    .map_err(llm_err)?;

    let opts = dr_strange_llm::DigestOptions {
        source: req.source.unwrap_or_else(|| "web-digest".into()),
        model: chat_model,
        run_id: format!("web-{}", now_secs()),
        chunk_chars: 4000,
        embed,
    };
    let plane = app(ctx.db.plane(&req.plane))?;
    let cands = dr_strange_llm::PlaneCandidates::new(&plane);
    let candidates = link.then_some(&cands as &dyn dr_strange_llm::CandidateSource);
    let result =
        dr_strange_llm::digest(&req.text, &chat, &embedder, candidates, &opts).map_err(llm_err)?;

    let r = &result.report;
    Ok(jval!({
        "report": {
            "chunks": r.chunks,
            "entities": r.entities,
            "relations": r.relations,
            "linked": r.linked,
            "dropped_relations": r.dropped_relations,
            "chat_requests": r.chat_requests,
            "input_tokens": r.input_tokens,
            "output_tokens": r.output_tokens,
            "embed_tokens": r.embed_tokens,
        },
        "nodes": result.nodes.iter().map(|n| jval!({
            "key": n.key,
            "label": n.label,
            "properties": json::properties_to_json(&n.props),
        })).collect::<Vec<_>>(),
        "edges": result.edges.iter().map(|e| jval!({
            "src": e.src,
            "type": e.ty,
            "dst": e.dst,
            "properties": json::properties_to_json(&e.props),
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
pub struct DigestWrite {
    plane: String,
    nodes: Vec<WriteNode>,
    #[serde(default)]
    edges: Vec<WriteEdge>,
}

#[derive(Deserialize)]
struct WriteNode {
    key: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    properties: Value,
}

#[derive(Deserialize)]
struct WriteEdge {
    src: String,
    #[serde(rename = "type")]
    ty: String,
    dst: String,
    #[serde(default)]
    properties: Value,
}

fn props_of(v: &Value) -> Result<Properties, RpcError> {
    if v.is_null() {
        Ok(Properties::new())
    } else {
        json::json_to_properties(v).map_err(|e| RpcError::invalid_params(e.to_string()))
    }
}

/// `digest.write` — write a previously-computed proposal into the plane via the
/// bulk path. No LLM call: it re-materializes the nodes/edges `digest.run`
/// returned (embeddings included), so the review-then-write flow costs one LLM
/// pass, not two.
pub fn digest_write(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DigestWrite = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;

    let mut node_props = Vec::with_capacity(req.nodes.len());
    for n in &req.nodes {
        node_props.push(props_of(&n.properties)?);
    }
    let label_slots: Vec<[&str; 1]> = req
        .nodes
        .iter()
        .map(|n| {
            [if n.label.is_empty() {
                "Entity"
            } else {
                n.label.as_str()
            }]
        })
        .collect();
    let bnodes: Vec<BulkNode> = req
        .nodes
        .iter()
        .zip(&label_slots)
        .zip(node_props)
        .map(|((n, ls), props)| BulkNode {
            external_key: Some(&n.key),
            labels: ls,
            props,
        })
        .collect();

    let mut edge_props = Vec::with_capacity(req.edges.len());
    for e in &req.edges {
        edge_props.push(props_of(&e.properties)?);
    }
    let bedges: Vec<BulkEdge> = req
        .edges
        .iter()
        .zip(edge_props)
        .map(|(e, props)| BulkEdge {
            src_key: &e.src,
            dst_key: &e.dst,
            ty: &e.ty,
            props,
        })
        .collect();

    let mut txn = app(plane.write())?;
    let stats = app(txn.bulk_load(bnodes, bedges))?;
    app(txn.commit())?;
    Ok(jval!({ "nodes_written": stats.nodes, "edges_written": stats.edges }))
}
