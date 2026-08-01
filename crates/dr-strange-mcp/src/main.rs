//! drsg-mcp — an MCP server embedding `dr-strange-core` (arch/06). stdio
//! transport, JSON-RPC 2.0 via the official `rmcp` SDK; the host process owns
//! the database file. Contains no database logic — every tool is a thin
//! call into the core API.
//!
//! The core is synchronous; each tool runs its database work on a blocking
//! task so a long scan never stalls the async runtime. The `digest` tool
//! (LLM ingestion, arch/07) reads provider API keys from the server's
//! environment, never from params.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use dr_strange_core::{
    Database, Dir, HybridWeights, LogicalPlan, LouvainOptions, Metric, NodeId, PageRankOptions,
    Properties, ShortestPathOptions, json,
};
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
struct Algo {
    /// Algorithm to run: `pagerank` | `components` | `shortest_path` | `louvain`.
    algo: String,
    #[serde(default = "default_plane")]
    plane: String,
    /// Restrict the run to nodes carrying this label (and edges among them).
    #[serde(default)]
    label: Option<String>,
    /// Max rows to return for pagerank/components/louvain (default 100).
    #[serde(default)]
    limit: Option<usize>,
    /// PageRank damping factor (default 0.85).
    #[serde(default)]
    damping: Option<f64>,
    /// PageRank iteration cap (default 20).
    #[serde(default)]
    max_iters: Option<u32>,
    /// PageRank convergence tolerance (default 1e-6).
    #[serde(default)]
    tolerance: Option<f64>,
    /// shortest_path: source node id (required for that algo).
    #[serde(default)]
    src: Option<u64>,
    /// shortest_path: destination node id (required for that algo).
    #[serde(default)]
    dst: Option<u64>,
    /// shortest_path: edge direction `out` | `in` | `both` (default `out`).
    #[serde(default)]
    dir: Option<String>,
    /// shortest_path: numeric edge property used as weight (default unit).
    #[serde(default)]
    weight: Option<String>,
    /// Louvain aggregation-level cap (default 10).
    #[serde(default)]
    max_levels: Option<u32>,
    /// Louvain minimum modularity gain to move a node.
    #[serde(default)]
    min_gain: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct Hybrid {
    /// Query text: embedded for the vector channel, tokenized for keyword.
    query: String,
    #[serde(default = "default_plane")]
    plane: String,
    /// Label scope (required when the keyword channel is used).
    #[serde(default)]
    label: Option<String>,
    /// Enable the vector channel over this embedding property.
    #[serde(default)]
    vector_prop: Option<String>,
    /// Enable the BM25 keyword channel over this string property.
    #[serde(default)]
    keyword_prop: Option<String>,
    /// Vector metric: `cosine` (default) | `dot` | `l2`.
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
    /// Embedding provider for the vector channel (key from the server env).
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Ask {
    /// A natural-language question about the graph.
    question: String,
    #[serde(default = "default_plane")]
    plane: String,
    /// Return the generated plan without executing it.
    #[serde(default)]
    dry_run: bool,
    /// Total model turns including tool calls and repairs (default 20).
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
    /// Embedding provider for the find_edge/find_entity grounding tools (should
    /// match how the plane was embedded). Omit to disable them (schema only).
    #[serde(default)]
    embed_provider: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Digest {
    /// The document text to digest into a graph.
    text: String,
    #[serde(default = "default_plane")]
    plane: String,
    /// Write the result. Default `false` — a dry-run that returns the proposed
    /// nodes/edges for inspection (arch/07 §2: proposals, not mutations).
    #[serde(default)]
    apply: bool,
    /// Chat provider: preset (`openai`/`deepseek`/`qwen`/`ollama`) or a base
    /// URL. API keys are read from the server's environment, never params.
    #[serde(default)]
    chat: Option<String>,
    /// Embedding provider preset or base URL (defaults to the chat provider).
    #[serde(default)]
    embed: Option<String>,
    /// Chat model override.
    #[serde(default)]
    model: Option<String>,
    /// Embedding model override.
    #[serde(default)]
    embed_model: Option<String>,
    /// Provenance: what the document is (recorded on every node/edge).
    #[serde(default)]
    source: Option<String>,
    /// Skip embedding generation.
    #[serde(default)]
    no_embed: bool,
    /// Link extracted entities to existing plane nodes via vector retrieval
    /// (default true). Off ⇒ every entity is proposed as new.
    #[serde(default)]
    link: Option<bool>,
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
struct Cypher {
    #[serde(default = "default_plane")]
    plane: String,
    /// A query in the openCypher-subset language, e.g.
    /// `MATCH (n:Person) WHERE n.age >= 30 RETURN n ORDER BY n.age DESC LIMIT 5`.
    query: String,
    /// Embedding provider for a text `SEARCH … NEAR "…"` (preset or base URL);
    /// the server environment supplies the key. Defaults to `openai`.
    #[serde(default)]
    embed: Option<String>,
    /// Values for `$name` placeholders in the query, e.g. `{"min": 18}`.
    #[serde(default)]
    params: serde_json::Map<String, Value>,
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
    async fn blocking<F>(&self, tool: &'static str, f: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&Database) -> AnyResult<Value> + Send + 'static,
    {
        let db = self.db.clone();
        let joined = tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| McpError::internal_error(format!("task join failed: {e}"), None))?;
        Ok(match joined {
            Ok(value) => {
                tracing::debug!(tool, "mcp tool ok");
                ok(value)
            }
            Err(e) => {
                tracing::warn!(tool, error = %e, "mcp tool failed");
                tool_error(e.to_string())
            }
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

/// Default result cap for the whole-graph algorithms (they can produce a row
/// per node); shortest_path is unaffected.
const ALGO_LIMIT: usize = 100;

fn algo_logic(db: &Database, req: Algo) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
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
            let scored = builder.pagerank(opts)?;
            let count = scored.len();
            let results: Vec<Value> = scored
                .into_iter()
                .take(limit)
                .map(|(id, s)| jval!({ "id": id.0, "score": s }))
                .collect();
            Ok(jval!({ "algo": "pagerank", "results": results, "count": count }))
        }
        "components" => {
            let (rows, count) = builder.connected_components()?;
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
            let (rows, count) = builder.louvain(opts)?;
            let results: Vec<Value> = rows
                .into_iter()
                .take(limit)
                .map(|(id, rep)| jval!({ "id": id.0, "community": rep.0 }))
                .collect();
            Ok(jval!({ "algo": "louvain", "results": results, "count": count }))
        }
        "shortest_path" => {
            let (Some(src), Some(dst)) = (req.src, req.dst) else {
                anyhow::bail!("shortest_path requires `src` and `dst`");
            };
            let opts = ShortestPathOptions {
                dir: parse_dir(req.dir.as_deref()),
                weight: req.weight.clone(),
            };
            let found = builder.shortest_path(NodeId(src), NodeId(dst), &opts)?;
            let path = found.map(|p| {
                jval!({
                    "nodes": p.nodes.iter().map(|n| n.0).collect::<Vec<_>>(),
                    "edges": p.edges.iter().map(|e| e.0).collect::<Vec<_>>(),
                    "cost": p.cost,
                })
            });
            Ok(jval!({ "algo": "shortest_path", "found": path.is_some(), "path": path }))
        }
        other => anyhow::bail!(
            "unknown algo `{other}` (expected pagerank|components|shortest_path|louvain)"
        ),
    }
}

fn hybrid_logic(db: &Database, req: Hybrid) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
    let mut b = plane.hybrid();
    if let Some(label) = &req.label {
        b = b.label(label.clone());
    }
    if let Some(prop) = &req.vector_prop {
        let provider = req.provider.as_deref().unwrap_or("openai");
        let embedder: Box<dyn dr_strange_llm::Embedder> = Box::new(dr_strange_llm::build_provider(
            provider,
            req.embed_model.as_deref(),
            None,
            None,
            true,
        )?);
        let reply = embedder.embed(std::slice::from_ref(&req.query))?;
        let vector = reply
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
        b = b.vector(prop.clone(), vector, parse_metric(req.metric.as_deref()));
    }
    if let Some(prop) = &req.keyword_prop {
        b = b.keyword(prop.clone(), req.query.clone());
    }
    if let Some(hops) = req.graph_hops {
        b = b.graph(hops, req.graph_decay.unwrap_or(0.5));
    }
    if req.w_vector.is_some() || req.w_keyword.is_some() || req.w_graph.is_some() {
        let d = HybridWeights::default();
        b = b.weights(HybridWeights {
            vector: req.w_vector.unwrap_or(d.vector),
            keyword: req.w_keyword.unwrap_or(d.keyword),
            graph: req.w_graph.unwrap_or(d.graph),
        });
    }
    let hits = b.k(req.k.unwrap_or(10)).run()?;
    let mut results = Vec::with_capacity(hits.len());
    for h in &hits {
        let mut obj = match plane.node(h.node)? {
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

fn ask_logic(db: &Database, req: Ask) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
    let provider = req.provider.as_deref().unwrap_or("openai");
    let chat = dr_strange_llm::build_provider(provider, req.model.as_deref(), None, None, false)?;
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
    )?;
    // The matched subgraph: nodes + edges among them (source + traversal).
    let results: Vec<Value> = res.nodes.iter().map(json::node_to_json).collect();
    let edges: Vec<Value> = res
        .edges
        .iter()
        .map(|e| {
            jval!({
                "id": e.id.0, "src": e.src.0, "dst": e.dst.0, "type": e.ty,
                "properties": json::properties_to_json(&e.properties),
            })
        })
        .collect();
    Ok(jval!({
        "plans": serde_json::to_value(&res.plans)?,
        "ran": res.ran,
        "attempts": res.attempts,
        "results": results,
        "edges": edges,
        "count": results.len(),
    }))
}

/// Adapts an LLM provider to the parser's `Embedder` seam so a text
/// `SEARCH … NEAR "…"` embeds server-side (key from the process environment,
/// never tool params).
struct LlmEmbedder(Box<dyn dr_strange_llm::Embedder>);
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

fn cypher_logic(db: &Database, req: Cypher) -> AnyResult<Value> {
    let provider = req.embed.as_deref().unwrap_or("openai");
    let embedder = dr_strange_llm::build_provider(provider, None, None, None, true)
        .ok()
        .map(|p| LlmEmbedder(Box::new(p)));
    // Resolve `$name` placeholders from the params object.
    let mut params = dr_strange_parser::Params::new();
    for (k, v) in &req.params {
        params.insert(k.clone(), json::json_to_value(v)?);
    }
    let stmt = dr_strange_parser::parse_statement_full(
        &req.query,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_parser::Embedder),
        &params,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let plane = db.plane(&req.plane)?;
    match stmt {
        dr_strange_parser::Statement::Read(plan) => {
            Ok(scored_rows(&plane.query_from_plan(plan).scored_nodes()?))
        }
        dr_strange_parser::Statement::Write(w) => {
            let s = w.apply(&plane).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(jval!({
                "nodes_created": s.nodes_created,
                "edges_created": s.edges_created,
                "props_set": s.props_set,
                "labels_set": s.labels_set,
                "nodes_deleted": s.nodes_deleted,
                "edges_deleted": s.edges_deleted,
            }))
        }
    }
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

fn digest_logic(db: &Database, req: Digest) -> AnyResult<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let chat_provider = req.chat.as_deref().unwrap_or("openai");
    let embed_provider = req.embed.as_deref().unwrap_or(chat_provider);
    let embed = !req.no_embed;
    let link = req.link.unwrap_or(true);

    // Provider keys come from the server's environment (never tool params).
    let chat =
        dr_strange_llm::build_provider(chat_provider, req.model.as_deref(), None, None, false)?;
    let chat_model = chat.model().to_string();
    let embedder = dr_strange_llm::build_provider(
        embed_provider,
        req.embed_model.as_deref(),
        None,
        None,
        embed,
    )?;

    let p = db.plane(&req.plane)?;
    let run_id = format!(
        "mcp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let opts = dr_strange_llm::DigestOptions {
        source: req.source.unwrap_or_else(|| "mcp-digest".into()),
        model: chat_model,
        run_id,
        chunk_chars: 4000,
        embed,
    };

    let cands = dr_strange_llm::PlaneCandidates::new(&p);
    let candidates = link.then_some(&cands as &dyn dr_strange_llm::CandidateSource);
    let result = dr_strange_llm::digest(&req.text, &chat, &embedder, candidates, &opts)?;
    let r = &result.report;
    let mut out = jval!({
        "applied": req.apply,
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
    });

    if req.apply {
        let mut txn = p.write()?;
        let stats = result.apply(&mut txn)?;
        txn.commit()?;
        out["nodes_written"] = jval!(stats.nodes);
        out["edges_written"] = jval!(stats.edges);
    } else {
        // Dry-run: return the proposed graph (capped) so the agent can inspect
        // before a second call with apply=true.
        let nodes: Vec<Value> = result
            .nodes
            .iter()
            .take(50)
            .map(|n| {
                jval!({
                    "key": n.key,
                    "label": n.label,
                    "properties": json::properties_to_json(&n.props),
                })
            })
            .collect();
        let edges: Vec<Value> = result
            .edges
            .iter()
            .take(100)
            .map(|e| jval!({ "src": e.src, "type": e.ty, "dst": e.dst }))
            .collect();
        out["proposal"] = jval!({ "nodes": nodes, "edges": edges });
    }
    Ok(out)
}

// ---- tools (rmcp wrappers) ------------------------------------------------

#[tool_router(router = tool_router)]
impl DrStrange {
    #[tool(description = "List all planes with their node/edge counts.")]
    async fn list_planes(&self) -> Result<CallToolResult, McpError> {
        self.blocking("list_planes", list_planes_logic).await
    }

    #[tool(description = "The soft-schema catalog for a plane: labels, property \
        types and descriptions, edge-type connectivity, counts.")]
    async fn describe_plane(
        &self,
        Parameters(req): Parameters<PlaneOnly>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("describe_plane", move |db| describe_plane_logic(db, req))
            .await
    }

    #[tool(description = "Fetch one node by `id` or external `key`.")]
    async fn get_node(
        &self,
        Parameters(req): Parameters<GetNode>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("get_node", move |db| get_node_logic(db, req))
            .await
    }

    #[tool(description = "Vector similarity search: the k nodes closest to \
        `query` by their `property` embedding, with similarity scores.")]
    async fn search(
        &self,
        Parameters(req): Parameters<Search>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("search", move |db| search_logic(db, req))
            .await
    }

    #[tool(description = "Neighbourhood expansion from a node (1 hop by \
        default; set `max` > 1 for multi-hop). Returns the reached node ids.")]
    async fn traverse(
        &self,
        Parameters(req): Parameters<Traverse>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("traverse", move |db| traverse_logic(db, req))
            .await
    }

    #[tool(description = "Run a serialized logical query plan and return the \
        matching node records (with scores where present).")]
    async fn query(&self, Parameters(req): Parameters<Query>) -> Result<CallToolResult, McpError> {
        self.blocking("query", move |db| query_logic(db, req)).await
    }

    #[tool(description = "Run a graph algorithm over a plane (or one `label` \
        subset), read-only at a single snapshot. Set `algo` to one of: \
        `pagerank` (importance; returns [{id, score}] + count), `components` \
        (weakly connected; [{id, component}] where component is the smallest \
        member id, + component count), `shortest_path` (needs `src`/`dst`; \
        weighted Dijkstra with optional `dir` out|in|both and numeric edge \
        `weight` property; returns {found, path:{nodes, edges, cost}}), or \
        `louvain` (community detection; [{id, community}] + community count). \
        `limit` caps the ranked/labelled rows (default 100).")]
    async fn algo(&self, Parameters(req): Parameters<Algo>) -> Result<CallToolResult, McpError> {
        self.blocking("algo", move |db| algo_logic(db, req)).await
    }

    #[tool(
        description = "Hybrid retrieval (ROADMAP §2): fuse vector similarity, \
        BM25 keyword, and graph-proximity into one ranking. Enable a channel by \
        naming its property — `vector_prop` (embedding; `query` is embedded \
        server-side), `keyword_prop` (BM25 over a declared keyword index; needs \
        `label`) — and/or set `graph_hops` to boost neighbours of the strongest \
        hits. Optional per-channel weights (`w_vector`/`w_keyword`/`w_graph`), \
        `metric`, and `k`. Returns node records with the fused `score` and each \
        channel's raw contribution. Embedding keys come from the server env."
    )]
    async fn hybrid(
        &self,
        Parameters(req): Parameters<Hybrid>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("hybrid", move |db| hybrid_logic(db, req))
            .await
    }

    #[tool(
        description = "Natural-language query (ROADMAP §3): ask a question in \
        plain language and an LLM turns it into a read-only query plan over this \
        plane's schema, runs it, and returns the matching node records. Grounded \
        in the plane catalog; repairs its own plan on error. Set `dry_run` to get \
        the generated plan WITHOUT executing it. Read-only — it can never mutate \
        the graph. Chat provider key comes from the server env, never params."
    )]
    async fn ask(&self, Parameters(req): Parameters<Ask>) -> Result<CallToolResult, McpError> {
        self.blocking("ask", move |db| ask_logic(db, req)).await
    }

    #[tool(description = "Run a statement in the openCypher-subset query \
        language. Reads (MATCH one linear path with labels/->/<-/- and bounded \
        *m..n, SEARCH vector top-k, BEAM similarity traversal, WHERE, RETURN \
        [DISTINCT]/ORDER BY/SKIP/LIMIT) return the matching node records. \
        Writes (CREATE) mutate the plane and return change-counts. Prefer this \
        over the raw `query` plan for readability. Examples: \
        `MATCH (n:Person) WHERE n.age >= 30 RETURN n ORDER BY n.age DESC LIMIT 5`; \
        `CREATE (a:Person {key:\"alice\", age:30})-[:KNOWS]->(b:Person {key:\"bob\"})`.")]
    async fn cypher(
        &self,
        Parameters(req): Parameters<Cypher>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("cypher", move |db| cypher_logic(db, req))
            .await
    }

    #[tool(description = "Create nodes (batched). Each: {external_key?, labels, \
        properties?}. Returns the created ids.")]
    async fn write_nodes(
        &self,
        Parameters(req): Parameters<WriteNodes>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("write_nodes", move |db| write_nodes_logic(db, req))
            .await
    }

    #[tool(description = "Create edges (batched) by endpoint external keys. \
        Each: {src_key, dst_key, type, properties?}.")]
    async fn write_edges(
        &self,
        Parameters(req): Parameters<WriteEdges>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("write_edges", move |db| write_edges_logic(db, req))
            .await
    }

    #[tool(description = "Create a new empty plane.")]
    async fn create_plane(
        &self,
        Parameters(req): Parameters<CreatePlane>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("create_plane", move |db| create_plane_logic(db, req))
            .await
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
        self.blocking("drop_plane", move |db| drop_plane_logic(db, req))
            .await
    }

    #[tool(
        description = "Digest a document into the plane's graph via an LLM: extract typed \
        entities + relations (labels chosen purely from the document), link them to existing \
        plane nodes via vector retrieval (set link=false to propose everything as new), embed \
        them, and — \
        only when apply=true — write them with provenance. Dry-run (the default) returns the \
        proposed nodes/edges for review; call again with apply=true to commit. Provider API keys \
        come from the server's environment (e.g. OPENAI_API_KEY / DEEPSEEK_API_KEY / \
        DASHSCOPE_API_KEY), never from params."
    )]
    async fn digest(
        &self,
        Parameters(req): Parameters<Digest>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("digest", move |db| digest_logic(db, req))
            .await
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
    // Logs go to stderr + a rolling file — never stdout, which carries the
    // stdio JSON-RPC protocol. Hold the guard so the writer flushes on exit.
    let _log = dr_strange_log::init("drsg-mcp");

    // The database path: first CLI arg, else $DRSG_DB, else graph.drsg.
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("DRSG_DB").ok())
        .unwrap_or_else(|| "graph.drsg".to_string());
    let db = Arc::new(Database::open(PathBuf::from(&path))?);
    tracing::info!(db = %path, "drsg-mcp: database opened; serving MCP over stdio");

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
    fn algo_runs_over_the_fixture() {
        let db = fixture(); // d0 (id 1) —CITES→ d1 (id 2)

        let pr = algo_logic(&db, from_value(jval!({"algo": "pagerank"})).unwrap()).unwrap();
        assert_eq!(pr["algo"], jval!("pagerank"));
        assert_eq!(pr["count"], jval!(2));
        assert_eq!(pr["results"][0]["id"], jval!(2)); // d1 is the cited hub

        let comp = algo_logic(&db, from_value(jval!({"algo": "components"})).unwrap()).unwrap();
        assert_eq!(comp["count"], jval!(1));

        let sp = algo_logic(
            &db,
            from_value(jval!({"algo": "shortest_path", "src": 1, "dst": 2})).unwrap(),
        )
        .unwrap();
        assert_eq!(sp["found"], jval!(true));
        assert_eq!(sp["path"]["cost"], jval!(1.0));

        // Missing endpoints and unknown algo are tool-level errors.
        assert!(algo_logic(&db, from_value(jval!({"algo": "shortest_path"})).unwrap()).is_err());
        assert!(algo_logic(&db, from_value(jval!({"algo": "nope"})).unwrap()).is_err());
    }

    #[test]
    fn hybrid_keyword_channel_fuses_and_reports() {
        use dr_strange_core::{Language, PropDesc, PropValue, Properties};

        let db = Database::in_memory().unwrap();
        {
            let plane = db.plane("startup").unwrap();
            let mut txn = plane.write().unwrap();
            let mk = |b: &str| -> Properties {
                [("body".to_string(), PropDesc::new(PropValue::Str(b.into())))]
                    .into_iter()
                    .collect()
            };
            txn.create_node_with_key("d0", &["Doc"], mk("graph databases store data"))
                .unwrap();
            txn.create_node_with_key("d1", &["Doc"], mk("graph graph graph queries"))
                .unwrap();
            txn.commit().unwrap();
        }
        db.plane("startup")
            .unwrap()
            .ensure_keyword_index("Doc", "body", Language::English)
            .unwrap();

        let out = hybrid_logic(
            &db,
            from_value(jval!({"query": "graph", "label": "Doc", "keyword_prop": "body"})).unwrap(),
        )
        .unwrap();
        assert_eq!(out["count"], jval!(2));
        assert_eq!(out["results"][0]["external_key"], jval!("d1")); // graph-dense doc
        assert!(out["results"][0]["channels"]["keyword"].is_number());
        assert!(out["results"][0]["channels"]["vector"].is_null());

        // Keyword channel without a label is a tool-level error.
        assert!(
            hybrid_logic(
                &db,
                from_value(jval!({"query": "x", "keyword_prop": "body"})).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn cypher_compiles_and_runs() {
        let db = fixture();
        // all Docs
        let all = cypher_logic(
            &db,
            from_value(jval!({"query": "MATCH (n:Doc) RETURN n"})).unwrap(),
        )
        .unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2);
        // traversal d0 -CITES-> d1
        let hop = cypher_logic(
            &db,
            from_value(jval!({"query": "MATCH (a:Doc)-[:CITES]->(b:Doc) RETURN b"})).unwrap(),
        )
        .unwrap();
        assert_eq!(hop.as_array().unwrap().len(), 1);
        assert_eq!(hop[0]["external_key"], jval!("d1"));
        // a malformed query surfaces the parser error, not a panic
        assert!(cypher_logic(&db, from_value(jval!({"query": "MATCH (n)"})).unwrap()).is_err());
    }

    #[test]
    fn cypher_create_writes() {
        let db = Database::in_memory().unwrap();
        let out = cypher_logic(
            &db,
            from_value(jval!({"query": r#"CREATE (a:Person {key:"x", age:40})"#})).unwrap(),
        )
        .unwrap();
        assert_eq!(out["nodes_created"], jval!(1));
        let n = db
            .plane("startup")
            .unwrap()
            .node_by_key("x")
            .unwrap()
            .unwrap();
        assert!(n.labels.iter().any(|l| l == "Person"));
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
