//! drsg-mcp — an MCP server embedding `dr-strange-core` (arch/06). stdio
//! transport, JSON-RPC 2.0 via the official `rmcp` SDK; the host process owns
//! the database file. Contains no database logic — every tool is a thin
//! call into the core API.
//!
//! The core is synchronous; each tool runs its database work on a blocking
//! task so a long scan never stalls the async runtime. The `digest` tool
//! (LLM ingestion) is intentionally absent pending its own design session
//! (arch/06, arch/07).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use dr_strange_core::{Database, Dir, LogicalPlan, Metric, NodeId, Properties, json};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json as jval};

#[derive(Clone)]
struct DrStrange {
    db: Arc<Database>,
    tool_router: ToolRouter<Self>,
}

// ---- request payloads ----------------------------------------------------

fn default_plane() -> String {
    "startup".to_string()
}

#[derive(Deserialize, JsonSchema)]
struct PlaneOnly {
    /// Plane to operate in (default `startup`).
    #[serde(default = "default_plane")]
    plane: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetNode {
    #[serde(default = "default_plane")]
    plane: String,
    /// Node id (mutually exclusive with `key`).
    id: Option<u64>,
    /// External key (mutually exclusive with `id`).
    key: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Search {
    #[serde(default = "default_plane")]
    plane: String,
    /// Restrict to a label (omit to search the whole plane).
    label: Option<String>,
    /// The vector property to compare.
    property: String,
    /// The query embedding.
    query: Vec<f32>,
    /// `cosine` (default), `dot`, or `l2`.
    metric: Option<String>,
    /// Number of nearest neighbours (default 10).
    k: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct Traverse {
    #[serde(default = "default_plane")]
    plane: String,
    /// Start node id (or use `from_key`).
    from_id: Option<u64>,
    from_key: Option<String>,
    /// `out` (default), `in`, or `both`.
    direction: Option<String>,
    /// Restrict to an edge type.
    edge_type: Option<String>,
    /// Min hops (default 1) and max hops (default 1) — >1 gives multi-hop.
    min: Option<u32>,
    max: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct Query {
    #[serde(default = "default_plane")]
    plane: String,
    /// A serialized logical plan (same shape the CLI `query` accepts).
    plan: Value,
}

#[derive(Deserialize, JsonSchema)]
struct NodeInput {
    external_key: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    /// Property map in the JSON dialect (`{"$vector":[…]}`, `{"$desc","$value"}`).
    properties: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteNodes {
    #[serde(default = "default_plane")]
    plane: String,
    nodes: Vec<NodeInput>,
}

#[derive(Deserialize, JsonSchema)]
struct EdgeInput {
    src_key: String,
    dst_key: String,
    #[serde(rename = "type")]
    ty: String,
    properties: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
struct WriteEdges {
    #[serde(default = "default_plane")]
    plane: String,
    edges: Vec<EdgeInput>,
}

#[derive(Deserialize, JsonSchema)]
struct CreatePlane {
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct DropPlane {
    name: String,
    /// Must be `true` — dropping a plane deletes everything on it (arch/06 §3).
    #[serde(default)]
    confirm: bool,
}

// ---- helpers -------------------------------------------------------------

fn parse_metric(s: Option<&str>) -> Metric {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("dot") => Metric::Dot,
        Some("l2") => Metric::L2,
        _ => Metric::Cosine,
    }
}

fn parse_dir(s: Option<&str>) -> Dir {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("in") => Dir::In,
        Some("both") => Dir::Both,
        _ => Dir::Out,
    }
}

fn ok(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

impl DrStrange {
    /// Runs sync database work off the async runtime. A core error becomes a
    /// *tool-level* error (the caller sees the message); only a task-join
    /// failure is a protocol error (arch/06: rmcp's two failure modes).
    async fn blocking<F>(&self, f: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&Database) -> AnyResult<Value> + Send + 'static,
    {
        let db = self.db.clone();
        let joined = tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| McpError::internal_error(format!("task join failed: {e}"), None))?;
        Ok(match joined {
            Ok(value) => ok(value),
            Err(e) => tool_error(e.to_string()),
        })
    }
}

// ---- tool logic ----------------------------------------------------------
//
// The database work of each tool lives in a plain `fn (&Database, req) ->
// Value`, so it's unit-testable without the async/rmcp machinery. The `#[tool]`
// methods below are one-line wrappers that run these on a blocking task.

fn scored_rows(rows: &[(dr_strange_core::NodeRecord, Option<f32>)]) -> Value {
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

fn list_planes_logic(db: &Database) -> AnyResult<Value> {
    let mut out = Vec::new();
    for (id, name) in db.planes()? {
        let cat = db.plane(&name)?.catalog()?;
        out.push(jval!({
            "id": id.0, "name": name,
            "nodes": cat.node_count, "edges": cat.edge_count,
        }));
    }
    Ok(Value::Array(out))
}

fn describe_plane_logic(db: &Database, req: PlaneOnly) -> AnyResult<Value> {
    Ok(serde_json::to_value(db.plane(&req.plane)?.catalog()?)?)
}

fn get_node_logic(db: &Database, req: GetNode) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let node = match (req.id, &req.key) {
        (Some(id), _) => p.node(NodeId(id))?,
        (None, Some(key)) => p.node_by_key(key)?,
        (None, None) => anyhow::bail!("provide `id` or `key`"),
    };
    Ok(node.map(|n| json::node_to_json(&n)).unwrap_or(Value::Null))
}

fn search_logic(db: &Database, req: Search) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let metric = parse_metric(req.metric.as_deref());
    let hits = p
        .query()
        .vector_top_k(
            req.label.as_deref(),
            &req.property,
            req.query,
            metric,
            req.k.unwrap_or(10) as u64,
        )
        .scored_nodes()?;
    Ok(scored_rows(&hits))
}

fn traverse_logic(db: &Database, req: Traverse) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let from = match (req.from_id, &req.from_key) {
        (Some(id), _) => NodeId(id),
        (None, Some(key)) => p
            .node_by_key(key)?
            .map(|n| n.id)
            .ok_or_else(|| anyhow::anyhow!("no node with key '{key}'"))?,
        (None, None) => anyhow::bail!("provide `from_id` or `from_key`"),
    };
    let dir = parse_dir(req.direction.as_deref());
    let (min, max) = (req.min.unwrap_or(1), req.max.unwrap_or(1));
    let ids = p
        .query()
        .seek_ids([from])
        .expand_var(dir, req.edge_type.as_deref(), min, max)
        .distinct()
        .ids()?;
    Ok(jval!(ids.iter().map(|i| i.0).collect::<Vec<_>>()))
}

fn query_logic(db: &Database, req: Query) -> AnyResult<Value> {
    let plan: LogicalPlan = serde_json::from_value(req.plan)?;
    let rows = db.plane(&req.plane)?.query_from_plan(plan).scored_nodes()?;
    Ok(scored_rows(&rows))
}

fn write_nodes_logic(db: &Database, req: WriteNodes) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let mut txn = p.write()?;
    let mut ids = Vec::new();
    for node in req.nodes {
        let labels: Vec<&str> = node.labels.iter().map(String::as_str).collect();
        let props = match &node.properties {
            Some(v) => json::json_to_properties(v)?,
            None => Properties::new(),
        };
        let id = match &node.external_key {
            Some(key) => txn.create_node_with_key(key, &labels, props)?,
            None => txn.create_node(&labels, props)?,
        };
        ids.push(id.0);
    }
    txn.commit()?;
    Ok(jval!({ "created": ids }))
}

fn write_edges_logic(db: &Database, req: WriteEdges) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let mut txn = p.write()?;
    let mut count = 0u64;
    for edge in req.edges {
        let src = p
            .node_by_key(&edge.src_key)?
            .ok_or_else(|| anyhow::anyhow!("unknown src_key '{}'", edge.src_key))?;
        let dst = p
            .node_by_key(&edge.dst_key)?
            .ok_or_else(|| anyhow::anyhow!("unknown dst_key '{}'", edge.dst_key))?;
        let props = match &edge.properties {
            Some(v) => json::json_to_properties(v)?,
            None => Properties::new(),
        };
        txn.create_edge(src.id, dst.id, &edge.ty, props)?;
        count += 1;
    }
    txn.commit()?;
    Ok(jval!({ "created": count }))
}

fn create_plane_logic(db: &Database, req: CreatePlane) -> AnyResult<Value> {
    let handle = db.create_plane(&req.name, Properties::new())?;
    Ok(jval!({ "id": handle.id().0, "name": req.name }))
}

fn drop_plane_logic(db: &Database, req: DropPlane) -> AnyResult<Value> {
    let id = db.plane(&req.name)?.id();
    db.drop_plane(id)?;
    Ok(jval!({ "dropped": req.name }))
}

// ---- tools (rmcp wrappers) ------------------------------------------------

#[tool_router(router = tool_router)]
impl DrStrange {
    #[tool(description = "List all planes with their node/edge counts.")]
    async fn list_planes(&self) -> Result<CallToolResult, McpError> {
        self.blocking(list_planes_logic).await
    }

    #[tool(description = "The soft-schema catalog for a plane: labels, property \
        types and descriptions, edge-type connectivity, counts.")]
    async fn describe_plane(
        &self,
        Parameters(req): Parameters<PlaneOnly>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| describe_plane_logic(db, req)).await
    }

    #[tool(description = "Fetch one node by `id` or external `key`.")]
    async fn get_node(
        &self,
        Parameters(req): Parameters<GetNode>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| get_node_logic(db, req)).await
    }

    #[tool(description = "Vector similarity search: the k nodes closest to \
        `query` by their `property` embedding, with similarity scores.")]
    async fn search(
        &self,
        Parameters(req): Parameters<Search>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| search_logic(db, req)).await
    }

    #[tool(description = "Neighbourhood expansion from a node (1 hop by \
        default; set `max` > 1 for multi-hop). Returns the reached node ids.")]
    async fn traverse(
        &self,
        Parameters(req): Parameters<Traverse>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| traverse_logic(db, req)).await
    }

    #[tool(description = "Run a serialized logical query plan and return the \
        matching node records (with scores where present).")]
    async fn query(&self, Parameters(req): Parameters<Query>) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| query_logic(db, req)).await
    }

    #[tool(description = "Create nodes (batched). Each: {external_key?, labels, \
        properties?}. Returns the created ids.")]
    async fn write_nodes(
        &self,
        Parameters(req): Parameters<WriteNodes>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| write_nodes_logic(db, req)).await
    }

    #[tool(description = "Create edges (batched) by endpoint external keys. \
        Each: {src_key, dst_key, type, properties?}.")]
    async fn write_edges(
        &self,
        Parameters(req): Parameters<WriteEdges>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| write_edges_logic(db, req)).await
    }

    #[tool(description = "Create a new empty plane.")]
    async fn create_plane(
        &self,
        Parameters(req): Parameters<CreatePlane>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking(move |db| create_plane_logic(db, req)).await
    }

    #[tool(description = "Delete a plane and everything on it. Requires \
        `confirm: true`.")]
    async fn drop_plane(
        &self,
        Parameters(req): Parameters<DropPlane>,
    ) -> Result<CallToolResult, McpError> {
        if !req.confirm {
            return Ok(tool_error(format!(
                "refusing to drop plane '{}': pass confirm=true (this deletes all its data)",
                req.name
            )));
        }
        self.blocking(move |db| drop_plane_logic(db, req)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DrStrange {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive]; start from the default and set the
        // fields we care about.
        let mut info = ServerInfo::default();
        info.server_info =
            rmcp::model::Implementation::new("dr-strange", env!("CARGO_PKG_VERSION"));
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "dr-strange graph database. Orient with list_planes and \
             describe_plane (the soft-schema catalog), then get_node / \
             search (vector) / traverse / query to read, and write_nodes / \
             write_edges to write. Planes are isolated graph canvases; \
             default 'startup'."
                .to_string(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The database path: first CLI arg, else $DRSG_DB, else graph.drsg.
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("DRSG_DB").ok())
        .unwrap_or_else(|| "graph.drsg".to_string());
    let db = Arc::new(Database::open(PathBuf::from(path))?);

    let server = DrStrange {
        db,
        tool_router: DrStrange::tool_router(),
    };
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::from_value;

    /// A db with two keyed "Doc" nodes (embeddings on a line) and a CITES
    /// edge, plus a declared L2 index — mirrors the CLI/hybrid fixtures.
    fn fixture() -> Database {
        let db = Database::in_memory().unwrap();
        let plane = db.plane("startup").unwrap();
        write_nodes_logic(
            &db,
            from_value(jval!({"nodes": [
                {"external_key": "d0", "labels": ["Doc"], "properties": {"emb": {"$vector": [0.0, 0.0]}, "year": 2020}},
                {"external_key": "d1", "labels": ["Doc"], "properties": {"emb": {"$vector": [1.0, 0.0]}, "year": 2021}}
            ]})).unwrap(),
        )
        .unwrap();
        write_edges_logic(
            &db,
            from_value(jval!({"edges": [{"src_key": "d0", "dst_key": "d1", "type": "CITES"}]}))
                .unwrap(),
        )
        .unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
        db
    }

    #[test]
    fn list_and_describe() {
        let db = fixture();
        let planes = list_planes_logic(&db).unwrap();
        assert_eq!(planes[0]["nodes"], jval!(2));
        assert_eq!(planes[0]["edges"], jval!(1));
        let cat = describe_plane_logic(&db, from_value(jval!({})).unwrap()).unwrap();
        assert_eq!(cat["labels"]["Doc"]["count"], jval!(2));
    }

    #[test]
    fn get_node_by_id_and_key() {
        let db = fixture();
        let by_key = get_node_logic(&db, from_value(jval!({"key": "d0"})).unwrap()).unwrap();
        assert_eq!(by_key["external_key"], jval!("d0"));
        let by_id = get_node_logic(&db, from_value(jval!({"id": 1})).unwrap()).unwrap();
        assert_eq!(by_id["id"], jval!(1));
        // missing → Null; neither id nor key → error
        assert_eq!(
            get_node_logic(&db, from_value(jval!({"id": 999})).unwrap()).unwrap(),
            Value::Null
        );
        assert!(get_node_logic(&db, from_value(jval!({})).unwrap()).is_err());
    }

    #[test]
    fn search_uses_index_and_scores() {
        let db = fixture();
        let rows = search_logic(
            &db,
            from_value(jval!({"property": "emb", "query": [0.0, 0.0], "metric": "l2", "k": 1}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(rows[0]["external_key"], jval!("d0"));
        assert!(rows[0]["score"].is_number());
    }

    #[test]
    fn traverse_and_query() {
        let db = fixture();
        // out from d0 over CITES reaches d1 (id 2)
        let hops = traverse_logic(
            &db,
            from_value(jval!({"from_key": "d0", "edge_type": "CITES"})).unwrap(),
        )
        .unwrap();
        assert_eq!(hops, jval!([2]));

        // a serialized plan: scan Doc, filter year >= 2021 -> d1
        let rows = query_logic(
            &db,
            from_value(jval!({"plan": {
                "source": {"ScanLabel": "Doc"},
                "steps": [{"Filter": {"Compare": {"op": "Ge",
                    "lhs": {"Property": "year"}, "rhs": {"Literal": {"Int": 2021}}}}}]
            }}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["external_key"], jval!("d1"));
    }

    #[test]
    fn write_edges_reports_unknown_endpoint() {
        let db = fixture();
        let err = write_edges_logic(
            &db,
            from_value(jval!({"edges": [{"src_key": "d0", "dst_key": "ghost", "type": "X"}]}))
                .unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn create_and_drop_plane_logic() {
        let db = Database::in_memory().unwrap();
        create_plane_logic(&db, from_value(jval!({"name": "scratch"})).unwrap()).unwrap();
        assert!(db.plane("scratch").is_ok());
        drop_plane_logic(
            &db,
            from_value(jval!({"name": "scratch", "confirm": true})).unwrap(),
        )
        .unwrap();
        assert!(db.plane("scratch").is_err());
    }

    #[test]
    fn helpers_parse_metric_and_dir() {
        assert_eq!(parse_metric(Some("l2")), Metric::L2);
        assert_eq!(parse_metric(Some("DOT")), Metric::Dot);
        assert_eq!(parse_metric(None), Metric::Cosine);
        assert!(matches!(parse_dir(Some("in")), Dir::In));
        assert!(matches!(parse_dir(Some("both")), Dir::Both));
        assert!(matches!(parse_dir(None), Dir::Out));
    }
}
