//! The JSON-RPC method implementations (arch/08 §1). Each is a plain
//! synchronous `fn(&Ctx, params) -> Result<Value, RpcError>` that wraps the
//! core `Database` API and serializes through the core's `json` dialect — the
//! same structures the CLI and MCP emit, so all three surfaces agree on the
//! wire shape. Methods are read-only in chunk 1 (arch/08 §2 backend slice).

use std::path::Path;

use dr_strange_core::{Database, Dir, LogicalPlan, Metric, NodeId, NodeRecord, json};
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
