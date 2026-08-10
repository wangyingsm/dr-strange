//! The JSON-RPC method implementations (arch/08 §1). Each is a plain
//! synchronous `fn(&Ctx, params) -> Result<Value, RpcError>` that wraps the
//! core `Database` API and serializes through the core's `json` dialect — the
//! same structures the CLI and MCP emit, so all three surfaces agree on the
//! wire shape. Most methods are reads; `digest.run`/`digest.write` power the
//! digest page (arch/07), the latter writing through the bulk path.

use std::path::Path;

use dr_strange_core::{
    BulkEdge, BulkNode, Change, ChangeKind, ChangeOp, ChangeSet, Database, Dir, EdgeId, EdgeRecord,
    HybridWeights, Language, LogicalPlan, LouvainOptions, Metric, NodeId, NodeRecord,
    PageRankOptions, PlaneHandle, Properties, ShortestPathOptions, json,
};
// Time-travel address type — ships only with the native backend (ROADMAP §4).
#[cfg(feature = "native-backend")]
use dr_strange_core::AsOf;
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
    /// Server-side `digest.run` defaults (request params override these).
    pub digest: crate::DigestDefaults,
}

// ---- param decoding -------------------------------------------------------

fn params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

/// Core errors are the caller's fault far more often than ours (unknown plane,
/// bad plan), so they ride the server-error code, not `-32603 internal`.
fn app<T>(r: dr_strange_core::Result<T>) -> Result<T, RpcError> {
    r.map_err(|e| match e {
        // The one core error a client should retry unchanged rather than treat
        // as its own fault: it never got the writer, so nothing was attempted.
        dr_strange_core::Error::Timeout(_) => RpcError::timeout(e.to_string()),
        _ => RpcError::server(e.to_string()),
    })
}

/// Optional time-travel address on a read request (ROADMAP §4): pin the read to
/// a past commit `as_of` (sequence) or `as_of_ms` (unix-epoch milliseconds). At
/// most one; `#[serde(flatten)]` this into a request struct.
#[derive(Deserialize, Default)]
pub struct AsOfParams {
    #[serde(default)]
    as_of: Option<u64>,
    #[serde(default)]
    as_of_ms: Option<i64>,
}

/// Resolve a plane handle, applying the request's AS OF address if present.
/// Native backend: pins the historical snapshot.
#[cfg(feature = "native-backend")]
fn plane_at<'a>(
    ctx: &'a Ctx<'a>,
    plane: &str,
    at: &AsOfParams,
) -> Result<PlaneHandle<'a>, RpcError> {
    let handle = app(ctx.db.plane(plane))?;
    match (at.as_of, at.as_of_ms) {
        (Some(_), Some(_)) => Err(RpcError::invalid_params(
            "specify only one of as_of / as_of_ms",
        )),
        (Some(seq), None) => app(handle.as_of(AsOf::Seq(seq))),
        (None, Some(ms)) => app(handle.as_of(AsOf::Time(ms))),
        (None, None) => Ok(handle),
    }
}

/// Non-native backends keep no history: reject an AS OF request outright,
/// otherwise resolve the plane as usual.
#[cfg(not(feature = "native-backend"))]
fn plane_at<'a>(
    ctx: &'a Ctx<'a>,
    plane: &str,
    at: &AsOfParams,
) -> Result<PlaneHandle<'a>, RpcError> {
    if at.as_of.is_some() || at.as_of_ms.is_some() {
        return Err(RpcError::invalid_params(
            "time-travel (as_of / as_of_ms) requires the native backend",
        ));
    }
    app(ctx.db.plane(plane))
}

/// Pin a plane handle to the snapshot a query's `AS OF` clause names — the
/// in-language counterpart of the `as_of` / `as_of_ms` request params.
#[cfg(feature = "native-backend")]
fn pin(
    p: PlaneHandle<'_>,
    at: Option<dr_strange_parser::AsOfSpec>,
) -> Result<PlaneHandle<'_>, RpcError> {
    use dr_strange_parser::AsOfSpec;
    match at {
        None => Ok(p),
        Some(AsOfSpec::Seq(seq)) => app(p.as_of(AsOf::Seq(seq))),
        Some(AsOfSpec::Time(ms)) => app(p.as_of(AsOf::Time(ms))),
    }
}

#[cfg(not(feature = "native-backend"))]
fn pin(
    p: PlaneHandle<'_>,
    at: Option<dr_strange_parser::AsOfSpec>,
) -> Result<PlaneHandle<'_>, RpcError> {
    if at.is_some() {
        return Err(RpcError::invalid_params(
            "AS OF (time-travel) requires the native backend",
        ));
    }
    Ok(p)
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

/// One change-feed entry as JSON (ROADMAP §5): kind/op/id, plus the node's
/// labels and the sanitized record for a create/update (a delete carries id
/// only). Mirrors the `scored_rows` node shape so the UI can plot it directly.
fn change_to_json(c: &Change) -> Value {
    let kind = match c.kind {
        ChangeKind::Node => "node",
        ChangeKind::Edge => "edge",
    };
    let op = match c.op {
        ChangeOp::Created => "created",
        ChangeOp::Updated => "updated",
        ChangeOp::Deleted => "deleted",
    };
    let mut obj = jval!({ "kind": kind, "op": op, "id": c.id });
    if let Value::Object(map) = &mut obj {
        if !c.labels.is_empty() {
            map.insert("labels".into(), jval!(c.labels));
        }
        if let Some(n) = &c.node {
            map.insert("record".into(), json::node_to_json(n));
        } else if let Some(e) = &c.edge {
            map.insert("record".into(), edge_to_json(e));
        }
    }
    obj
}

/// Build the `plane.change` WebSocket notification for a subscriber watching
/// `plane_name`, optionally narrowed to node `label`. Returns `None` when no
/// change in the set matches the filter (nothing to send). A label filter keeps
/// node changes carrying that label; edge changes pass only on an unfiltered
/// (plane-wide) subscription (edges have no label — arch/01).
pub fn change_message(cs: &ChangeSet, plane_name: &str, label: Option<&str>) -> Option<String> {
    let changes: Vec<Value> = cs
        .changes
        .iter()
        .filter(|c| match label {
            None => true,
            Some(l) => c.kind == ChangeKind::Node && c.labels.iter().any(|x| x == l),
        })
        .map(change_to_json)
        .collect();
    if changes.is_empty() {
        return None;
    }
    Some(
        jval!({
            "jsonrpc": "2.0",
            "method": "plane.change",
            "params": {
                "plane": plane_name,
                "seq": cs.seq,
                "truncated": cs.truncated,
                "changes": changes,
            }
        })
        .to_string(),
    )
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

// ---- service description --------------------------------------------------

/// The OpenRPC service description, embedded at build time. It is the source of
/// truth for the language SDKs and the RPC reference; the drift test in `rpc`
/// keeps it in lockstep with the dispatch table.
const OPENRPC: &str = include_str!("../openrpc.json");

/// `rpc.discover` — return the OpenRPC document (the standard discovery method).
pub fn rpc_discover(_ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    serde_json::from_str(OPENRPC).map_err(|e| RpcError::server(format!("bad openrpc doc: {e}")))
}

// ---- methods --------------------------------------------------------------

/// `db.stats` — the dashboard's health panel: plane/node/edge counts, soft-schema
/// breadth (labels, edge types), declared search indexes, the commit sequence,
/// and the file size when the backend is on disk.
pub fn db_stats(ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let planes = app(ctx.db.planes())?;
    let cat = app(ctx.db.catalog())?;
    let commit_seq = app(ctx.db.commit_seq())?;
    // Declared vector + keyword indexes across every plane.
    let mut indexes = 0usize;
    for (_, name) in &planes {
        let plane = app(ctx.db.plane(name))?;
        indexes += plane.vector_indexes().len() + plane.keyword_indexes().len();
    }
    let file_size = ctx
        .db_path
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    Ok(jval!({
        "planes": planes.len(),
        "nodes": cat.node_count,
        "edges": cat.edge_count,
        "labels": cat.labels.len(),
        "edge_types": cat.edge_types.len(),
        "indexes": indexes,
        "commit_seq": commit_seq,
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
    #[serde(flatten)]
    at: AsOfParams,
}

/// `plane.neighbors` — 1-hop expansion as `{node, edge}` id pairs. Chunk 2's
/// plot enriches these into full records; chunk 1 keeps it to the raw hop.
/// `plane.history` — the time-travel window (ROADMAP §4): the oldest and latest
/// commit sequences a read can be pinned to (`as_of` / `as_of_ms` on read
/// methods). The wire method exists on every backend (the OpenRPC contract is
/// uniform), but only the native engine can answer it.
#[cfg(feature = "native-backend")]
pub fn plane_history(ctx: &Ctx<'_>, _p: Value) -> Result<Value, RpcError> {
    let (oldest, latest) = app(ctx.db.history())?;
    Ok(jval!({ "oldest": oldest, "latest": latest }))
}

/// Non-native backends keep no history, so the window is unavailable.
#[cfg(not(feature = "native-backend"))]
pub fn plane_history(_ctx: &Ctx<'_>, _p: Value) -> Result<Value, RpcError> {
    Err(RpcError::invalid_params(
        "time-travel history requires the native backend",
    ))
}

pub fn plane_neighbors(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Neighbors = params(p)?;
    let plane = plane_at(ctx, &req.plane, &req.at)?;
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
    #[serde(flatten)]
    at: AsOfParams,
}

/// `plane.query` — run a serialized logical plan verbatim (the params ride the
/// wire exactly as MCP/CLI send them) and return scored rows.
pub fn plane_query(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: RunPlan = params(p)?;
    let plan: LogicalPlan =
        serde_json::from_value(req.plan).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let rows = app(plane_at(ctx, &req.plane, &req.at)?
        .query_from_plan(plan)
        .scored_nodes())?;
    Ok(scored_rows(&rows))
}

/// Adapts an LLM provider to the parser's `Embedder` seam, so a
/// `SEARCH … NEAR "text"` embeds the text server-side (key from the server
/// environment, never the client) before the top-k runs.
struct LlmEmbedder(Box<dyn Embedder>);
impl dr_strange_parser::Embedder for LlmEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
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

/// Build an embedder from a provider preset/URL (`None` if it can't be
/// configured — e.g. the provider has no embedding model; a text SEARCH then
/// errors clearly, while MATCH / literal-vector queries still work).
fn make_embedder(provider: &str) -> Option<LlmEmbedder> {
    dr_strange_llm::build_provider(provider, None, None, None, true)
        .ok()
        .map(|p| LlmEmbedder(Box::new(p)))
}

#[derive(Deserialize)]
pub struct CypherReq {
    plane: String,
    query: String,
    /// Embedding provider for a text `SEARCH … NEAR "…"` (default `openai`).
    #[serde(default)]
    embed: Option<String>,
    /// Values for `$name` placeholders in the query.
    #[serde(default)]
    params: serde_json::Map<String, Value>,
}

/// Convert a JSON params object to the parser's `Params` (name → PropValue).
fn to_params(map: &serde_json::Map<String, Value>) -> Result<dr_strange_parser::Params, RpcError> {
    map.iter()
        .map(|(k, v)| {
            json::json_to_value(v)
                .map(|pv| (k.clone(), pv))
                .map_err(|e| RpcError::invalid_params(format!("param `{k}`: {e}")))
        })
        .collect()
}

/// `plane.cypher` — run a statement in the query language (reads return
/// `{nodes, edges, count}`; writes return `{write: true, …counts}`). The
/// first-class RPC counterpart of the web-only `POST /cypher`, so SDK clients
/// get the language too. Write-gated at dispatch (the language can mutate).
pub fn plane_cypher(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: CypherReq = params(p)?;
    let params = to_params(&req.params)?;
    cypher_subgraph(
        ctx,
        &req.plane,
        &req.query,
        req.embed.as_deref().unwrap_or("openai"),
        &params,
    )
}

/// Compile an openCypher-subset query (via dr-strange-parser) to a
/// `LogicalPlan`, run it, and return the matching nodes plus the edges induced
/// among exactly that result set — the same `{nodes, edges}` shape as
/// `graph.seed`, so the plot can render a query result as a subgraph. Shared by
/// the web-only `POST /cypher` endpoint and the `plane.cypher` RPC method.
/// `embed_provider` names the embedding provider for a text `SEARCH … NEAR "…"`.
pub fn cypher_subgraph(
    ctx: &Ctx<'_>,
    plane_name: &str,
    query: &str,
    embed_provider: &str,
    params: &dr_strange_parser::Params,
) -> Result<Value, RpcError> {
    let embedder = make_embedder(embed_provider);
    let stmt = dr_strange_parser::parse_statement_full(
        query,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_parser::Embedder),
        params,
    )
    .map_err(|e| RpcError::invalid_params(e.to_string()))?;

    let plane = app(ctx.db.plane(plane_name))?;

    // A write statement mutates the plane and returns its change-counts; the UI
    // shows a status rather than a subgraph.
    let (plane, plan) = match stmt {
        dr_strange_parser::Statement::Read(read) => (pin(plane, read.as_of)?, read.plan),
        dr_strange_parser::Statement::Write(w) => {
            let s = w.apply(&plane).map_err(RpcError::server)?;
            return Ok(jval!({
                "write": true,
                "nodes_created": s.nodes_created,
                "edges_created": s.edges_created,
                "props_set": s.props_set,
                "labels_set": s.labels_set,
                "nodes_deleted": s.nodes_deleted,
                "edges_deleted": s.edges_deleted,
            }));
        }
    };

    let rows = app(plane.query_from_plan(plan).scored_nodes())?;

    let set: std::collections::BTreeSet<u64> = rows.iter().map(|(n, _)| n.id.0).collect();
    let nodes: Vec<Value> = rows
        .iter()
        .map(|(n, s)| {
            let mut obj = json::node_to_json(n);
            if let (Some(score), Value::Object(map)) = (s, &mut obj) {
                map.insert("score".into(), jval!(score));
            }
            obj
        })
        .collect();

    // Induced edges: one pass over each result node's outgoing hops, keeping
    // those whose destination is also in the result set (deduped by edge id).
    let mut seen_edges = std::collections::BTreeSet::new();
    let mut edges = Vec::new();
    for (n, _) in &rows {
        for hop in app(plane.neighbors(n.id, Dir::Out, None))? {
            if set.contains(&hop.node.0)
                && seen_edges.insert(hop.edge.0)
                && let Some(edge) = app(plane.edge(hop.edge))?
            {
                edges.push(edge_to_json(&edge));
            }
        }
    }

    Ok(jval!({ "nodes": nodes, "edges": edges, "count": set.len() }))
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
    /// `"degree"` or `"pagerank"` seed the plane's important nodes rather than
    /// the first ones the scan happens to reach. Anything else (and the
    /// default) keeps scan order, which is cheaper and is what a re-seed of a
    /// small plane wants.
    #[serde(default)]
    order: Option<String>,
    #[serde(flatten)]
    at: AsOfParams,
}

/// `graph.seed` — an initial canvas: up to `limit` nodes (optionally of one
/// label) plus the edges induced among exactly that node set. `total` is the
/// full unfiltered node count so the UI can say how much was left off.
pub fn graph_seed(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Seed = params(p)?;
    let limit = req.limit.unwrap_or(SEED_LIMIT);
    let plane = plane_at(ctx, &req.plane, &req.at)?;

    let all_ids = match &req.label {
        Some(label) => app(plane.query().scan_label(label.clone()).ids())?,
        None => app(plane.query().scan_all().ids())?,
    };
    let total = all_ids.len();

    // Ranked seeding: take the *important* nodes, not the first ones the scan
    // reached. A canvas of two hundred arbitrary nodes is a hairball whatever
    // the layout does with it; the same budget spent on the highest-PageRank
    // nodes is the plane's skeleton, and the caller widens it deliberately.
    let ranked: Option<Vec<(NodeId, f64)>> = match req.order.as_deref() {
        // Degree, not PageRank, is what "the skeleton" means. PageRank on a
        // directed graph flows rank *along* the edges and pools it in sinks, so
        // a hub that points at forty things ranks below the forty — measured on
        // a test plane, a twelve-leaf hub came out under its own leaves. Degree
        // asks the question actually being asked: what is connected to a lot.
        Some("degree") => {
            let mut rows = Vec::with_capacity(all_ids.len());
            for id in &all_ids {
                let d = app(plane.neighbors(*id, Dir::Both, None))?.len();
                rows.push((*id, d as f64));
            }
            // Descending by degree, ties by id so a re-seed is reproducible.
            rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0)));
            Some(rows)
        }
        Some("pagerank") => {
            let mut builder = plane.algo();
            if let Some(label) = &req.label {
                builder = builder.label(label.clone());
            }
            Some(app(builder.pagerank(PageRankOptions::default()))?)
        }
        _ => None,
    };

    let (ids, scores): (Vec<NodeId>, Option<Vec<(u64, f64)>>) = match ranked {
        Some(rows) => {
            let top: Vec<(NodeId, f64)> = rows.into_iter().take(limit as usize).collect();
            (
                top.iter().map(|(id, _)| *id).collect(),
                Some(top.into_iter().map(|(id, s)| (id.0, s)).collect()),
            )
        }
        None => (all_ids.into_iter().take(limit as usize).collect(), None),
    };
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
        // Present only for a ranked seed. The caller gets the scores it just
        // paid for, so sizing a node or weighting an edge by importance costs
        // no second call.
        "scores": scores.map(|rows| {
            rows.into_iter()
                .map(|(id, score)| jval!({ "id": id, "score": score }))
                .collect::<Vec<_>>()
        }),
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
    #[serde(flatten)]
    at: AsOfParams,
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

    let plane = plane_at(ctx, &req.plane, &req.at)?;

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

/// Default number of PageRank/Louvain rows returned when the caller sets no
/// `limit` (whole-plane algorithms can produce very large result sets).
const ALGO_LIMIT: usize = 100;

#[derive(Deserialize)]
pub struct Algo {
    plane: String,
    /// Which algorithm: `pagerank` | `components` | `shortest_path` | `louvain`.
    algo: String,
    /// Restrict to nodes carrying this label (and the edges among them).
    #[serde(default)]
    label: Option<String>,
    /// Top-N rows to return for the ranked/labelled algorithms (default 100).
    #[serde(default)]
    limit: Option<usize>,
    // pagerank
    #[serde(default)]
    damping: Option<f64>,
    #[serde(default)]
    max_iters: Option<u32>,
    #[serde(default)]
    tolerance: Option<f64>,
    // shortest_path
    #[serde(default)]
    src: Option<u64>,
    #[serde(default)]
    dst: Option<u64>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    weight: Option<String>,
    // louvain
    #[serde(default)]
    max_levels: Option<u32>,
    #[serde(default)]
    min_gain: Option<f64>,
}

/// `plane.algo` — run a graph algorithm (ROADMAP §1) over the plane, or one
/// label subset, at a single snapshot. Read-only; results are transient. The
/// `algo` field selects the operation and which extra params apply:
/// - `pagerank` → `{ algo, results: [{id, score}], count }` (top `limit`)
/// - `components` → `{ algo, results: [{id, component}], count }` (component count)
/// - `shortest_path` (needs `src`/`dst`) → `{ algo, found, path: {nodes, edges, cost} }`
/// - `louvain` → `{ algo, results: [{id, community}], count }` (community count)
pub fn plane_algo(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Algo = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let mut builder = plane.algo();
    if let Some(label) = &req.label {
        builder = builder.label(label.clone());
    }
    let limit = req.limit.unwrap_or(ALGO_LIMIT);

    match req.algo.as_str() {
        "pagerank" => {
            let d = PageRankOptions::default();
            let opts = PageRankOptions {
                damping: req.damping.unwrap_or(d.damping),
                max_iters: req.max_iters.unwrap_or(d.max_iters),
                tolerance: req.tolerance.unwrap_or(d.tolerance),
            };
            let scored = app(builder.pagerank(opts))?;
            let count = scored.len();
            let results: Vec<Value> = scored
                .into_iter()
                .take(limit)
                .map(|(id, s)| jval!({ "id": id.0, "score": s }))
                .collect();
            Ok(jval!({ "algo": "pagerank", "results": results, "count": count }))
        }
        "components" => {
            let (rows, count) = app(builder.connected_components())?;
            let results: Vec<Value> = rows
                .into_iter()
                .take(limit)
                .map(|(id, rep)| jval!({ "id": id.0, "component": rep.0 }))
                .collect();
            Ok(jval!({ "algo": "components", "results": results, "count": count }))
        }
        "louvain" => {
            let d = LouvainOptions::default();
            let opts = LouvainOptions {
                max_levels: req.max_levels.unwrap_or(d.max_levels),
                min_gain: req.min_gain.unwrap_or(d.min_gain),
            };
            let (rows, count) = app(builder.louvain(opts))?;
            let results: Vec<Value> = rows
                .into_iter()
                .take(limit)
                .map(|(id, rep)| jval!({ "id": id.0, "community": rep.0 }))
                .collect();
            Ok(jval!({ "algo": "louvain", "results": results, "count": count }))
        }
        "shortest_path" => {
            let (Some(src), Some(dst)) = (req.src, req.dst) else {
                return Err(RpcError::invalid_params(
                    "shortest_path requires `src` and `dst`",
                ));
            };
            let opts = ShortestPathOptions {
                dir: parse_dir(req.dir.as_deref()),
                weight: req.weight.clone(),
            };
            let found = app(builder.shortest_path(NodeId(src), NodeId(dst), &opts))?;
            let path = found.map(|p| {
                jval!({
                    "nodes": p.nodes.iter().map(|n| n.0).collect::<Vec<_>>(),
                    "edges": p.edges.iter().map(|e| e.0).collect::<Vec<_>>(),
                    "cost": p.cost,
                })
            });
            Ok(jval!({ "algo": "shortest_path", "found": path.is_some(), "path": path }))
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown algo `{other}` (expected pagerank|components|shortest_path|louvain)"
        ))),
    }
}

#[derive(Deserialize)]
pub struct Hybrid {
    plane: String,
    /// The query text: embedded for the vector channel, tokenized for keyword.
    q: String,
    /// Label scope (required when the keyword channel is on).
    #[serde(default)]
    label: Option<String>,
    /// Enable the vector channel over this embedding property.
    #[serde(default)]
    vector_prop: Option<String>,
    /// Enable the BM25 keyword channel over this string property.
    #[serde(default)]
    keyword_prop: Option<String>,
    /// Vector metric (default cosine).
    #[serde(default)]
    metric: Option<String>,
    /// Enable the graph-proximity channel with this many hops.
    #[serde(default)]
    graph_hops: Option<u32>,
    /// Per-hop decay for the graph channel (default 0.5).
    #[serde(default)]
    graph_decay: Option<f32>,
    #[serde(default)]
    w_vector: Option<f32>,
    #[serde(default)]
    w_keyword: Option<f32>,
    #[serde(default)]
    w_graph: Option<f32>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    candidates: Option<usize>,
    /// Embedding provider for the vector channel (key from the server env).
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

/// `plane.hybrid` — hybrid retrieval (ROADMAP §2): fuse vector, BM25 keyword,
/// and graph-proximity channels into one ranking. Enable a channel by naming
/// its property (`vector_prop` / `keyword_prop`) or setting `graph_hops`. The
/// vector channel embeds `q` server-side (provider key from the environment).
/// Returns node records with the fused `score` and each channel's raw
/// contribution (`vector`/`keyword`/`graph`).
pub fn plane_hybrid(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Hybrid = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let mut builder = plane.hybrid();
    if let Some(label) = &req.label {
        builder = builder.label(label.clone());
    }
    if let Some(prop) = &req.vector_prop {
        let provider = req.provider.as_deref().unwrap_or("openai");
        let embedder =
            dr_strange_llm::build_provider(provider, req.embed_model.as_deref(), None, None, true)
                .map_err(|e| RpcError::server(format!("embedding provider: {e}")))?;
        let reply = embedder
            .embed(std::slice::from_ref(&req.q))
            .map_err(|e| RpcError::server(format!("embedding failed: {e}")))?;
        let query = reply
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| RpcError::server("embedder returned no vector"))?;
        builder = builder.vector(prop.clone(), query, parse_metric(req.metric.as_deref()));
    }
    if let Some(prop) = &req.keyword_prop {
        builder = builder.keyword(prop.clone(), req.q.clone());
    }
    if let Some(hops) = req.graph_hops {
        builder = builder.graph(hops, req.graph_decay.unwrap_or(0.5));
    }
    if req.w_vector.is_some() || req.w_keyword.is_some() || req.w_graph.is_some() {
        let d = HybridWeights::default();
        builder = builder.weights(HybridWeights {
            vector: req.w_vector.unwrap_or(d.vector),
            keyword: req.w_keyword.unwrap_or(d.keyword),
            graph: req.w_graph.unwrap_or(d.graph),
        });
    }
    if let Some(c) = req.candidates {
        builder = builder.candidates(c);
    }
    builder = builder.k(req.k.unwrap_or(10));

    let hits = app(builder.run())?;
    let mut results = Vec::with_capacity(hits.len());
    for h in &hits {
        let mut obj = match app(plane.node(h.node))? {
            Some(node) => json::node_to_json(&node),
            None => jval!({ "id": h.node.0 }),
        };
        if let Value::Object(map) = &mut obj {
            map.insert("score".into(), jval!(h.score));
            map.insert(
                "channels".into(),
                jval!({ "vector": h.vector, "keyword": h.keyword, "graph": h.graph }),
            );
        }
        results.push(obj);
    }
    Ok(jval!({ "results": results, "count": results.len() }))
}

#[derive(Deserialize)]
pub struct Ask {
    plane: String,
    /// The natural-language question.
    question: String,
    /// Return the generated plan without executing it.
    #[serde(default)]
    dry_run: bool,
    /// Total model attempts including repairs (default 3).
    #[serde(default)]
    max_attempts: Option<u32>,
    /// Safety row cap appended when the plan declares none (default 100).
    #[serde(default)]
    limit: Option<u64>,
    /// Chat provider (preset or base URL); key from the server env.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Embedding provider for the find_edge/find_entity grounding tools; should
    /// match how the plane was embedded. Omit to disable the tools (schema only).
    #[serde(default)]
    embed_provider: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

/// `plane.ask` — natural-language query (ROADMAP §3): an LLM turns `question`
/// into a read-only LogicalPlan, which is run (unless `dry_run`). With
/// `embed_provider` the model can call embedding tools to ground the plan in
/// the real edge types / entity keys. Returns the generated plan for
/// transparency plus the result node records. Keys come from the server env.
pub fn plane_ask(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Ask = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let provider = req.provider.as_deref().unwrap_or("openai");
    let chat = dr_strange_llm::build_provider(provider, req.model.as_deref(), None, None, false)
        .map_err(|e| RpcError::server(format!("chat provider: {e}")))?;
    // Embedding tools are enabled when an embed provider is configured and builds.
    let embedder = req.embed_provider.as_deref().and_then(|ep| {
        dr_strange_llm::build_provider(ep, req.embed_model.as_deref(), None, None, true).ok()
    });
    let opts = dr_strange_llm::AskOptions {
        max_attempts: req.max_attempts.unwrap_or(20),
        dry_run: req.dry_run,
        limit: req.limit.unwrap_or(100),
    };
    let res = dr_strange_llm::ask(
        &chat,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_llm::Embedder),
        &plane,
        &req.question,
        &opts,
    )
    .map_err(|e| RpcError::server(e.to_string()))?;
    let plans = serde_json::to_value(&res.plans).map_err(|e| RpcError::server(e.to_string()))?;
    // The matched subgraph: nodes + the edges among them (union of all plans),
    // so the answer plots connected, not as disconnected endpoints.
    let results: Vec<Value> = res.nodes.iter().map(json::node_to_json).collect();
    let edges: Vec<Value> = res.edges.iter().map(edge_to_json).collect();
    Ok(jval!({
        "plans": plans,
        "ran": res.ran,
        "attempts": res.attempts,
        "results": results,
        "edges": edges,
        "count": results.len(),
        "trace": res.trace,
    }))
}

#[derive(Deserialize)]
pub struct PlaneIndexes {
    plane: String,
}

fn metric_name(m: Metric) -> &'static str {
    match m {
        Metric::Cosine => "cosine",
        Metric::Dot => "dot",
        Metric::L2 => "l2",
    }
}

#[derive(Deserialize)]
pub struct EnsureIndex {
    plane: String,
    label: String,
    property: String,
    /// `keyword` (default, BM25) or `vector` (embedding similarity).
    #[serde(default)]
    kind: Option<String>,
    /// Vector metric (default cosine).
    #[serde(default)]
    metric: Option<String>,
    /// Keyword analyzer language (default english).
    #[serde(default)]
    language: Option<String>,
}

/// `index.ensure` — declare (and build) a search index on `(label, property)`
/// from the dashboard, so a UI need never send the user to the CLI (ROADMAP §2).
/// `kind` selects the index type. Idempotent; errors if one already exists with
/// different settings.
pub fn index_ensure(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: EnsureIndex = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    match req.kind.as_deref().unwrap_or("keyword") {
        "vector" => {
            app(plane.ensure_vector_index(
                &req.label,
                &req.property,
                parse_metric(req.metric.as_deref()),
            ))?;
            Ok(jval!({ "kind": "vector", "label": req.label, "property": req.property }))
        }
        "keyword" => {
            let language: Language = req
                .language
                .as_deref()
                .unwrap_or("english")
                .parse()
                .map_err(|e: dr_strange_core::Error| RpcError::invalid_params(e.to_string()))?;
            app(plane.ensure_keyword_index(&req.label, &req.property, language))?;
            Ok(jval!({ "kind": "keyword", "label": req.label, "property": req.property }))
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown index kind `{other}` (expected keyword|vector)"
        ))),
    }
}

/// `plane.indexes` — the search indexes declared on a plane, so a UI can offer
/// only the channels that actually exist (ROADMAP §2). Returns the vector and
/// keyword indexes as `{label, property, …}`; the keyword channel can only
/// search a `(label, property)` that appears under `keyword`.
pub fn plane_indexes(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PlaneIndexes = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let vector: Vec<Value> = plane
        .vector_indexes()
        .into_iter()
        .map(|(label, property, metric)| {
            jval!({ "label": label, "property": property, "metric": metric_name(metric) })
        })
        .collect();
    let keyword: Vec<Value> = plane
        .keyword_indexes()
        .into_iter()
        .map(|(label, property, language)| {
            jval!({ "label": label, "property": property, "language": format!("{language:?}").to_lowercase() })
        })
        .collect();
    Ok(jval!({ "vector": vector, "keyword": keyword }))
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
    #[serde(flatten)]
    at: AsOfParams,
}

/// `graph.expand` — hub-safe neighbourhood expansion around one node: the
/// neighbour node records plus the connecting edge records, capped at `limit`
/// hops. `total` is the full incident count so the UI can offer "N more…".
pub fn graph_expand(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Expand = params(p)?;
    let limit = req.limit.unwrap_or(EXPAND_LIMIT) as usize;
    let plane = plane_at(ctx, &req.plane, &req.at)?;
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
    /// `reasoning_effort` to send on the extraction chat calls (e.g. `"none"`
    /// to disable reasoning on models that would otherwise spend the output
    /// budget on thinking tokens and truncate the extraction JSON). Unset ⇒
    /// not sent, so the provider's own default applies.
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    no_embed: bool,
    /// Link extracted entities to existing graph nodes via vector retrieval
    /// (default true). Off ⇒ every entity is proposed as new.
    #[serde(default)]
    link: Option<bool>,
    /// Per-chunk extraction chat calls to run concurrently. Omit to use the
    /// server default (`[digest].concurrency`, else 8).
    #[serde(default)]
    concurrency: Option<usize>,
    /// Target chunk size in characters. Omit to use the server default
    /// (`[digest].chunk_chars`, else 4000).
    #[serde(default)]
    chunk_chars: Option<usize>,
    /// How thoroughly to clean up the extraction: `coarse` reconciles the
    /// label and edge-type vocabularies, `fine` (the default) also merges
    /// entities naming the same thing, `super` also re-reads every entity
    /// against all the passages mentioning it.
    #[serde(default)]
    mode: Option<String>,
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
    // Opt-in only: unset leaves the request body byte-for-byte what it was, so
    // providers with no such field are unaffected. Embedding calls never carry
    // it — there is nothing to reason about.
    let chat = match req.reasoning_effort.as_deref() {
        Some(effort) => chat.with_reasoning_effort(effort),
        None => chat,
    };
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
        chunk_chars: req.chunk_chars.unwrap_or(ctx.digest.chunk_chars),
        embed,
        concurrency: req.concurrency.unwrap_or(ctx.digest.concurrency),
        mode: match req.mode.as_deref() {
            None => dr_strange_llm::DigestMode::default(),
            Some(m) => dr_strange_llm::DigestMode::parse(m).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown digest mode `{m}` — expected coarse, fine or super"
                ))
            })?,
        },
        refine_max_entities: None,
        refine_max_context: None,
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

// ---- granular mutations (arch/09 §3) --------------------------------------
//
// Each method is one write transaction, committed atomically — a single-op
// unit of change. (Cross-op atomicity is the batch-`mutate` shape we did not
// take.) Every one is gated `Access::Write` at dispatch, so an unauthorized
// caller never reaches the core.

/// A node reference in a request body: either a numeric `id` or an external
/// `key`. Used for edge endpoints, which take one field each.
#[derive(Deserialize)]
#[serde(untagged)]
enum NodeRef {
    Id(u64),
    Key(String),
}

impl NodeRef {
    /// Resolve to a concrete [`NodeId`] in `plane`. A key that names no node is
    /// an error; a numeric id is trusted (the core validates it on use).
    fn resolve(&self, plane: &PlaneHandle<'_>) -> Result<NodeId, RpcError> {
        match self {
            NodeRef::Id(id) => Ok(NodeId(*id)),
            NodeRef::Key(key) => app(plane.node_by_key(key))?
                .map(|n| n.id)
                .ok_or_else(|| RpcError::server(format!("no node with key '{key}'"))),
        }
    }
}

/// Resolve an `id`-or-`key` node selector (the `node.*` request shape) to a
/// `NodeId`, without asserting existence for the numeric path.
fn resolve_node(
    plane: &PlaneHandle<'_>,
    id: Option<u64>,
    key: Option<&str>,
) -> Result<NodeId, RpcError> {
    match (id, key) {
        (Some(id), _) => Ok(NodeId(id)),
        (None, Some(k)) => app(plane.node_by_key(k))?
            .map(|n| n.id)
            .ok_or_else(|| RpcError::server(format!("no node with key '{k}'"))),
        (None, None) => Err(RpcError::invalid_params("provide `id` or `key`")),
    }
}

#[derive(Deserialize)]
pub struct CreateNode {
    plane: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    properties: Value,
}

/// `node.create` — add a node with optional stable external key + labels.
/// Returns the created node record. Errors (as a conflict) if the key is
/// already bound in this plane.
pub fn node_create(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: CreateNode = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let props = props_of(&req.properties)?;
    let labels: Vec<&str> = req.labels.iter().map(String::as_str).collect();

    let mut txn = app(plane.write())?;
    let id = match &req.key {
        Some(k) => app(txn.create_node_with_key(k, &labels, props))?,
        None => app(txn.create_node(&labels, props))?,
    };
    app(txn.commit())?;

    Ok(app(plane.node(id))?
        .map(|n| json::node_to_json(&n))
        .unwrap_or(Value::Null))
}

#[derive(Deserialize)]
pub struct UpdateNode {
    plane: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    key: Option<String>,
    /// Properties to insert or overwrite (core JSON dialect).
    #[serde(default)]
    set: Value,
    /// Property keys to remove.
    #[serde(default)]
    unset: Vec<String>,
    /// When present, replaces the node's entire label set.
    #[serde(default)]
    labels: Option<Vec<String>>,
}

/// `node.update` — patch a node's properties (`set`/`unset`) and, when `labels`
/// is present, replace its label set. Returns the updated record.
pub fn node_update(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: UpdateNode = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let id = resolve_node(&plane, req.id, req.key.as_deref())?;
    let set = props_of(&req.set)?;

    let mut txn = app(plane.write())?;
    for (k, pd) in set {
        app(txn.set_prop(id, &k, pd))?;
    }
    for k in &req.unset {
        app(txn.remove_prop(id, k))?;
    }
    if let Some(labels) = &req.labels {
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        app(txn.set_labels(id, &refs))?;
    }
    app(txn.commit())?;

    Ok(app(plane.node(id))?
        .map(|n| json::node_to_json(&n))
        .unwrap_or(Value::Null))
}

/// Serialize a plane to JSONL — node lines, then id-based edge lines — the
/// exact format `drsg import` reads back. Backs the Dashboard's per-plane
/// Export download. Not an RPC method (it returns a file, not JSON-RPC data);
/// the `/export` HTTP endpoint calls it directly.
pub fn export_plane(ctx: &Ctx<'_>, plane_name: &str) -> Result<String, RpcError> {
    let plane = app(ctx.db.plane(plane_name))?;
    let mut out = String::new();
    for node in app(plane.query().scan_all().nodes())? {
        out.push_str(&json::node_to_json(&node).to_string());
        out.push('\n');
    }
    // Edges: walk each node's out-adjacency and emit each edge once.
    for node in app(plane.query().scan_all().nodes())? {
        for hop in app(plane.neighbors(node.id, Dir::Out, None))? {
            if let Some(edge) = app(plane.edge(hop.edge))? {
                out.push_str(&edge_to_json(&edge).to_string());
                out.push('\n');
            }
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct DeleteNode {
    plane: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    key: Option<String>,
}

/// `node.delete` — remove a node and cascade to its incident edges. Reports
/// whether a node was actually present (`deleted`), so a redundant delete is a
/// clean no-op rather than an error.
pub fn node_delete(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DeleteNode = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let existing = match (req.id, &req.key) {
        (Some(id), _) => app(plane.node(NodeId(id)))?,
        (None, Some(k)) => app(plane.node_by_key(k))?,
        (None, None) => return Err(RpcError::invalid_params("provide `id` or `key`")),
    };
    let Some(node) = existing else {
        return Ok(jval!({ "deleted": false }));
    };

    let mut txn = app(plane.write())?;
    app(txn.delete_node(node.id))?;
    app(txn.commit())?;
    Ok(jval!({ "deleted": true, "id": node.id.0 }))
}

#[derive(Deserialize)]
pub struct CreateEdge {
    plane: String,
    src: NodeRef,
    dst: NodeRef,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    properties: Value,
}

/// `edge.create` — add a directed edge between two existing nodes (each named
/// by id or key). Both endpoints must exist in the plane. Returns the created
/// edge record.
pub fn edge_create(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: CreateEdge = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let src = req.src.resolve(&plane)?;
    let dst = req.dst.resolve(&plane)?;
    let props = props_of(&req.properties)?;

    let mut txn = app(plane.write())?;
    let id = app(txn.create_edge(src, dst, &req.ty, props))?;
    app(txn.commit())?;

    Ok(app(plane.edge(id))?
        .map(|e| edge_to_json(&e))
        .unwrap_or(Value::Null))
}

#[derive(Deserialize)]
pub struct UpdateEdge {
    plane: String,
    edge: u64,
    #[serde(default)]
    set: Value,
    #[serde(default)]
    unset: Vec<String>,
    /// When present, changes the edge's type.
    #[serde(rename = "type", default)]
    ty: Option<String>,
}

/// `edge.update` — patch an edge's properties (`set`/`unset`) and, when `type`
/// is present, change its type. Returns the updated edge record.
pub fn edge_update(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: UpdateEdge = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let id = EdgeId(req.edge);
    let set = props_of(&req.set)?;

    let mut txn = app(plane.write())?;
    for (k, pd) in set {
        app(txn.set_edge_prop(id, &k, pd))?;
    }
    for k in &req.unset {
        app(txn.remove_edge_prop(id, k))?;
    }
    if let Some(ty) = &req.ty {
        app(txn.set_edge_type(id, ty))?;
    }
    app(txn.commit())?;

    Ok(app(plane.edge(id))?
        .map(|e| edge_to_json(&e))
        .unwrap_or(Value::Null))
}

#[derive(Deserialize)]
pub struct DeleteEdge {
    plane: String,
    edge: u64,
}

/// `edge.delete` — remove one edge. Reports whether it was present, so a
/// redundant delete is a clean no-op.
pub fn edge_delete(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DeleteEdge = params(p)?;
    let plane = app(ctx.db.plane(&req.plane))?;
    let id = EdgeId(req.edge);
    if app(plane.edge(id))?.is_none() {
        return Ok(jval!({ "deleted": false }));
    }

    let mut txn = app(plane.write())?;
    app(txn.delete_edge(id))?;
    app(txn.commit())?;
    Ok(jval!({ "deleted": true, "id": id.0 }))
}

// ---- plane administration (arch/09 §3) ------------------------------------

#[derive(Deserialize)]
pub struct CreatePlane {
    name: String,
    #[serde(default)]
    properties: Value,
}

/// `plane.create` — make a new, empty plane. Errors if the name is taken.
pub fn plane_create(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: CreatePlane = params(p)?;
    let props = props_of(&req.properties)?;
    let handle = app(ctx.db.create_plane(&req.name, props))?;
    Ok(jval!({ "id": handle.id().0, "name": req.name }))
}

#[derive(Deserialize)]
pub struct RenamePlane {
    plane: String,
    to: String,
}

/// `plane.rename` — rename an existing plane. Errors if the new name is taken
/// or the target is the always-present `startup` plane.
pub fn plane_rename(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: RenamePlane = params(p)?;
    let handle = app(ctx.db.plane(&req.plane))?;
    app(handle.rename(&req.to))?;
    Ok(jval!({ "id": handle.id().0, "name": req.to }))
}

#[derive(Deserialize)]
pub struct SetPlaneProps {
    plane: String,
    #[serde(default)]
    properties: Value,
}

/// `plane.set_props` — replace a plane's own property map (provenance,
/// description, …). Returns the plane's new properties.
pub fn plane_set_props(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: SetPlaneProps = params(p)?;
    let props = props_of(&req.properties)?;
    let handle = app(ctx.db.plane(&req.plane))?;
    app(handle.set_properties(props))?;
    let now = app(handle.properties())?;
    Ok(jval!({
        "id": handle.id().0,
        "name": req.plane,
        "properties": json::properties_to_json(&now),
    }))
}

#[derive(Deserialize)]
pub struct DeletePlane {
    plane: String,
}

/// `plane.delete` — drop a plane and everything on it. Reports whether one was
/// present (an absent name is a clean no-op); the `startup` plane cannot be
/// dropped (the core rejects it).
pub fn plane_delete(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DeletePlane = params(p)?;
    let found = app(ctx.db.planes())?
        .into_iter()
        .find(|(_, name)| name == &req.plane);
    let Some((id, _)) = found else {
        return Ok(jval!({ "deleted": false }));
    };
    app(ctx.db.drop_plane(id))?;
    Ok(jval!({ "deleted": true, "id": id.0 }))
}

#[cfg(test)]
mod change_feed_tests {
    use super::*;
    use dr_strange_core::{Database, PlaneHandle, Properties};
    use std::sync::{Arc, Mutex};

    /// Run `build` under a registered change observer and return the one
    /// ChangeSet it commits.
    fn one_change_set(build: impl FnOnce(&PlaneHandle<'_>)) -> ChangeSet {
        let db = Database::in_memory().unwrap();
        let plane = db.create_plane("p", Properties::new()).unwrap();
        let sink: Arc<Mutex<Option<ChangeSet>>> = Arc::new(Mutex::new(None));
        let into = sink.clone();
        db.on_change(move |cs| *into.lock().unwrap() = Some(cs));
        build(&plane);
        sink.lock()
            .unwrap()
            .take()
            .expect("a change set was produced")
    }

    fn changes_of(msg: &str) -> Value {
        serde_json::from_str::<Value>(msg).unwrap()
    }

    #[test]
    fn label_filter_keeps_matching_nodes_and_drops_others() {
        let cs = one_change_set(|plane| {
            let mut w = plane.write().unwrap();
            w.create_node_with_key("a", &["Person"], Properties::new())
                .unwrap();
            w.create_node_with_key("b", &["Company"], Properties::new())
                .unwrap();
            w.commit().unwrap();
        });

        // Watching "Person" → only the Person change, framed as a notification.
        let v = changes_of(&change_message(&cs, "p", Some("Person")).unwrap());
        assert_eq!(v["method"], "plane.change");
        assert_eq!(v["params"]["plane"], "p");
        let arr = v["params"]["changes"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["labels"][0], "Person");
        assert_eq!(arr[0]["op"], "created");

        // A label with no matching change → nothing to send.
        assert!(change_message(&cs, "p", Some("Nope")).is_none());

        // Plane-wide → both changes.
        let v = changes_of(&change_message(&cs, "p", None).unwrap());
        assert_eq!(v["params"]["changes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn edges_pass_only_on_an_unfiltered_watch() {
        let cs = one_change_set(|plane| {
            let mut w = plane.write().unwrap();
            let a = w.create_node(&["N"], Properties::new()).unwrap();
            let b = w.create_node(&["N"], Properties::new()).unwrap();
            w.create_edge(a, b, "LINKS", Properties::new()).unwrap();
            w.commit().unwrap();
        });

        // Plane-wide: 2 nodes + 1 edge.
        let v = changes_of(&change_message(&cs, "p", None).unwrap());
        assert_eq!(v["params"]["changes"].as_array().unwrap().len(), 3);

        // Label "N": only the two nodes; the edge is dropped.
        let v = changes_of(&change_message(&cs, "p", Some("N")).unwrap());
        let arr = v["params"]["changes"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr.iter().all(|c| c["kind"] == "node"));
    }

    #[test]
    fn deleted_change_carries_id_only() {
        let cs = one_change_set(|plane| {
            let id = {
                let mut w = plane.write().unwrap();
                let id = w.create_node(&["N"], Properties::new()).unwrap();
                w.commit().unwrap();
                id
            };
            let mut w = plane.write().unwrap();
            w.delete_node(id).unwrap();
            w.commit().unwrap();
        });
        let v = changes_of(&change_message(&cs, "p", None).unwrap());
        let c = &v["params"]["changes"][0];
        assert_eq!(c["op"], "deleted");
        assert!(c.get("record").is_none(), "a delete carries no record");
    }
}
