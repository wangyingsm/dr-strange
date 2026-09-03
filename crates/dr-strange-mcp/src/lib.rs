//! The `dr-strange` MCP tool set (arch/06), transport-independent (ROADMAP
//! §10). [`DrStrange`] is the `rmcp` service: every tool is a thin call into
//! the core API, run on a blocking task so a long scan never stalls whatever
//! async runtime is driving the transport. Contains no database logic of its
//! own and no transport of its own — two hosts drive it today:
//!
//! - `drsg-mcp` (the `drsg-mcp` binary in this crate) serves it over stdio,
//!   one process per host, embedding the database file directly.
//! - `drsg serve` (`dr-strange-web`) mounts it at `/mcp` over Streamable
//!   HTTP, so several agent hosts can share one `Database` instead of each
//!   opening the file themselves.
//!
//! The `digest` tool (LLM ingestion, arch/07) reads provider API keys from
//! the server process's environment, never from params, regardless of host.
//!
//! [`relay`] is how the two hosts avoid contending: when a repository already
//! runs the second one, `drsg-mcp` forwards to it rather than opening the
//! same database, which one process at a time may do.

pub mod relay;

use std::sync::Arc;

use tokio::sync::Semaphore;

use anyhow::Result as AnyResult;
use dr_strange_core::{
    Database, Dir, HybridWeights, LogicalPlan, LouvainOptions, Metric, NodeId, PageRankOptions,
    PlaneHandle, PropDesc, PropValue, Properties, ShortestPathOptions, json,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json as jval};

/// The `rmcp` service: one instance per MCP session, each holding its own
/// clone of the shared `Arc<Database>` (cheap — the tool router is
/// stateless, built fresh from the same static table every time).
#[derive(Clone)]
pub struct DrStrange {
    db: Arc<Database>,
    tool_router: ToolRouter<Self>,
    digest: DigestTuning,
    /// Ceiling on tool bodies running at once — see [`DrStrange::with_tool_gate`].
    tools: Arc<Semaphore>,
    /// Embedding provider for writes, when the host configured one — see
    /// [`DrStrange::with_embed_provider`]. `None` leaves `write_nodes` writing
    /// exactly what it was given.
    embed: Option<EmbedProvider>,
    /// Whether `digest` may read a `path` the caller names — see
    /// [`DrStrange::with_local_files`]. Off unless the host says otherwise.
    local_files: bool,
    /// The source tree behind the graph, when the host attached one — what
    /// the `grep` tool searches. `serve watch` attaches its `--dir`.
    source_root: Option<std::path::PathBuf>,
}

/// How the host reaches an embedding provider. Only the *names* live here; the
/// key is read from the process environment when a call is made, never carried
/// in a request or in this struct.
#[derive(Debug, Clone)]
pub struct EmbedProvider {
    /// Preset name or base URL.
    pub provider: String,
    /// Embedding model, when the preset's default is not wanted.
    pub model: Option<String>,
    /// Environment variable holding the key.
    pub key_env: Option<String>,
}

/// Digest knobs the host resolved, applied to the `digest` tool.
///
/// The stdio binary embeds its own database and has no config file, so it
/// keeps [`DigestTuning::default`]. A host that does have one — `drsg serve`,
/// whose `[digest]` section already steers `digest.run` over `/rpc` — passes
/// it through [`DrStrange::with_digest`], so the same server does not honour
/// an operator's `concurrency` on one surface and ignore it on the other.
#[derive(Debug, Clone, Copy)]
pub struct DigestTuning {
    /// Target chunk size in characters (paragraph-aware).
    pub chunk_chars: usize,
    /// Per-chunk extraction chat calls to run concurrently.
    pub concurrency: usize,
}

impl Default for DigestTuning {
    fn default() -> Self {
        Self {
            chunk_chars: 4000,
            concurrency: 8,
        }
    }
}

/// Tool bodies allowed to run at once when the host sets no explicit ceiling.
///
/// Every tool is a `spawn_blocking` unit of real work — a full-graph scan, a
/// bulk write, an LLM-fanning digest — so the bound is deliberately far below
/// the HTTP request ceiling, which counts cheap requests too.
pub const DEFAULT_TOOL_CONCURRENCY: usize = 16;

impl DrStrange {
    pub fn new(db: Arc<Database>) -> Self {
        Self::with_digest(db, DigestTuning::default())
    }

    pub fn with_digest(db: Arc<Database>, digest: DigestTuning) -> Self {
        Self {
            db,
            tool_router: Self::tool_router(),
            digest,
            tools: Arc::new(Semaphore::new(DEFAULT_TOOL_CONCURRENCY)),
            embed: None,
            local_files: false,
            source_root: None,
        }
    }

    /// Let `digest` read a document path the caller names.
    ///
    /// For the stdio server this is right: it runs on the agent's own machine,
    /// as that agent's user, and "digest this file" is the obvious thing to ask.
    /// For a shared `drsg serve` it would be an arbitrary-file-read primitive —
    /// an authenticated remote agent could name any path the server process can
    /// open, digest it into the graph, and read it back with a query. Off unless
    /// the host opts in, so the network transport cannot acquire it by accident.
    pub fn with_local_files(mut self, allowed: bool) -> Self {
        self.local_files = allowed;
        self
    }

    /// Embed nodes as they are written, using `provider`.
    ///
    /// Configured by the host rather than asked for per call, because an agent
    /// that forgets a flag writes silently unsearchable nodes — the failure
    /// this removes. The text is [`dr_strange_llm::entity_text`] and the vector
    /// lands in `embedding`, the same recipe and property `digest` uses, so an
    /// agent's writes and a digest's share one vector space and one index.
    ///
    /// A node that already carries `embedding` is left alone.
    pub fn with_embed_provider(mut self, provider: EmbedProvider) -> Self {
        self.embed = Some(provider);
        self
    }

    /// Attach the source tree the graph was parsed from, enabling the
    /// `grep` tool: literal text is the one question a graph should not
    /// pretend to answer, and with the tree attached it need not.
    pub fn with_source_root(mut self, root: std::path::PathBuf) -> Self {
        self.source_root = Some(root);
        self
    }

    /// Share one tool-concurrency gate across every session.
    ///
    /// A host serving several sessions **must** pass the same `gate` to all of
    /// them: the per-instance default bounds one session, and MCP puts no limit
    /// on sessions, so an unshared gate bounds nothing in aggregate.
    ///
    /// Needed because the transport does not bound this. A tool call is
    /// answered as soon as it is *queued* — `create_stream` returns after
    /// `push_message`, so the HTTP response future resolves and tower's
    /// concurrency permit is released before the tool body has run at all.
    /// Without this gate an authenticated caller can pipeline unlimited
    /// concurrent scans or digests and `max_concurrent` bounds none of them.
    pub fn with_tool_gate(mut self, gate: Arc<Semaphore>) -> Self {
        self.tools = gate;
        self
    }
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
    /// Name of the environment variable the server reads the provider key
    /// from. A preset defaults to its own; a base URL has none, so name it
    /// here when the endpoint needs a key. The key never travels in params.
    #[serde(default)]
    key_env: Option<String>,
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
    /// Name of the environment variable the server reads the chat key from. A
    /// preset defaults to its own; a base URL has none, so name it here when
    /// the endpoint needs a key. The key never travels in params.
    #[serde(default)]
    key_env: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Embedding provider for the find_edge/find_entity grounding tools (should
    /// match how the plane was embedded). Omit to disable them (schema only).
    #[serde(default)]
    embed_provider: Option<String>,
    /// The same, for the embedding provider.
    #[serde(default)]
    embed_key_env: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct Digest {
    /// The document text to digest into a graph. Give this **or** `path`.
    #[serde(default)]
    text: String,
    /// Path to a document the *server* can read — Word, PowerPoint, Excel,
    /// OpenDocument, RTF, EPUB, CSV, PDF, Markdown or plain text — converted to
    /// Markdown before digesting.
    ///
    /// May also be a **directory**, walked and routed per file: source files
    /// are parsed into facts and documents become prose (ROADMAP §11). Reading
    /// a checkout this way is what makes it worth parsing — a plugin follows
    /// imports across the tree — which is also why it stays stdio-only.
    ///
    /// Only honoured by the stdio server, which runs on the same machine as the
    /// agent that spawned it. A shared `drsg serve` refuses it: reading any path
    /// the caller names would let an authenticated remote agent pull arbitrary
    /// server files into the graph and query them back out.
    #[serde(default)]
    path: Option<String>,
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
    /// Name of the environment variable the server reads the chat key from. A
    /// preset defaults to its own; a base URL has none, so name it here when
    /// the endpoint needs a key. The key never travels in params.
    #[serde(default)]
    key_env: Option<String>,
    /// The same, for the embedding provider.
    #[serde(default)]
    embed_key_env: Option<String>,
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
    /// How thoroughly to clean up the extraction: `coarse` reconciles the
    /// label and edge-type vocabularies, `fine` (the default) also merges
    /// entities naming the same thing, `super` also re-reads every entity
    /// against all the passages mentioning it — most accurate, most costly.
    #[serde(default)]
    mode: Option<String>,
    /// `path` only: force a preprocessor by name (e.g. `rust`) instead of
    /// routing by file extension. A router that guesses is worse than one that
    /// asks. Ignored for `text`, which is always read as prose.
    #[serde(default)]
    handler: Option<String>,
    /// `path` only: store each parsed function's own source on its node, for
    /// retrieval. Default false — it is roughly a copy of the code in the graph.
    #[serde(default)]
    plugin_source: Option<bool>,
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
    /// Name of the environment variable the server reads the embedding key
    /// from. A preset defaults to its own; a base URL has none, so name it
    /// here when the endpoint needs a key. The key never travels in params.
    #[serde(default)]
    embed_key_env: Option<String>,
    /// Embedding model. A preset supplies its own; a base URL has none, so it
    /// is required there for a text `SEARCH … NEAR "…"`.
    #[serde(default)]
    embed_model: Option<String>,
    /// Values for `$name` placeholders in the query, e.g. `{"min": 18}`.
    #[serde(default)]
    #[schemars(with = "crate::JsonObject")]
    params: serde_json::Map<String, Value>,
}

/// Free-form JSON object. Rendered as a full Schema object (`{"type":
/// "object"}`) so Gemini's strict converter accepts it — schemars renders
/// `serde_json::Map<String, Value>` as `additionalProperties: true`, a bare
/// boolean schema that Gemini rejects with `400 … Schema, true`. `with` here
/// changes only the *schema*; serde still deserializes `params` as a map.
struct JsonObject;

impl schemars::JsonSchema for JsonObject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("JsonObject")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::try_from(serde_json::json!({
            "type": "object",
            "description": "Free-form JSON object for `$name` placeholders, e.g. {\"min\": 18}",
        }))
        .expect("static JSON schema is valid")
    }

    fn inline_schema() -> bool {
        true
    }
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
    /// Property map in the JSON dialect (`{"$vector":[…]}`, `{"$desc","$value"}`).
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
        // Held for the tool's whole body: this is the only thing bounding tool
        // work, since the transport releases its own permit once the call is
        // merely queued (see `with_tool_gate`). Queues rather than rejects — a
        // busy server should make an agent wait, not fail it.
        let _permit = self
            .tools
            .acquire()
            .await
            .map_err(|_| McpError::internal_error("tool gate closed", None))?;
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
                tracing::warn!(tool, error = format!("{e:#}"), "mcp tool failed");
                // The whole chain, not the outermost context: "embedding the
                // query" without "DASHSCOPE_API_KEY is not set" behind it
                // tells an agent nothing it can act on.
                tool_error(format!("{e:#}"))
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
                let mut obj = json::node_to_json_lean(n);
                if let (Some(score), Value::Object(map)) = (s, &mut obj) {
                    map.insert("score".into(), jval!(score));
                }
                obj
            })
            .collect(),
    )
}

fn list_planes_logic(db: &Database) -> AnyResult<Value> {
    let names: Vec<String> = db.planes()?.into_iter().map(|(_, name)| name).collect();
    let mut out = Vec::new();
    for (id, name) in db.planes()? {
        let plane = db.plane(&name)?;
        let cat = plane.catalog()?;
        let props = plane.properties()?;
        let mut row = jval!({
            "id": id.0, "name": name,
            "nodes": cat.node_count, "edges": cat.edge_count,
            "properties": json::properties_to_json(&props),
        });
        // Which plane holds what, said rather than left to be inferred from a
        // name. The pairing is not a guess: a digest of a git checkout writes
        // the history beside the code plane under exactly this name, so both
        // ends of it are the host's own doing.
        let suffix = dr_strange_core::compact::HISTORY_SUFFIX;
        let kind = match name.strip_suffix(suffix) {
            Some(code) if names.iter().any(|n| n == code) => Some(format!(
                "the commit history of plane `{code}` — commits, branches, \
                 tags, merges and rebases; the `history` tool reads it"
            )),
            _ if names.iter().any(|n| n == &format!("{name}{suffix}")) => Some(format!(
                "source code; this repository's history is in plane \
                 `{name}{suffix}`"
            )),
            _ => None,
        };
        if let (Some(kind), Some(obj)) = (kind, row.as_object_mut()) {
            obj.insert("holds".into(), Value::String(kind));
        }
        out.push(row);
    }
    Ok(Value::Array(out))
}

fn describe_plane_logic(db: &Database, req: PlaneOnly) -> AnyResult<Value> {
    Ok(serde_json::to_value(db.plane(&req.plane)?.catalog()?)?)
}

/// The compact agent verbs' shared request: a fuzzy name and a plane.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SymbolReq {
    #[serde(default = "default_plane")]
    plane: String,
    /// Symbol name: an exact key, a `::name`/`.name` suffix, or a substring.
    name: String,
}

/// `grep`'s request: literal text over the attached source tree.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GrepReq {
    /// Literal text to find (no regex — what you type is what is matched).
    pattern: String,
    /// Case-insensitive matching (default false).
    #[serde(default)]
    ignore_case: Option<bool>,
    /// Max matching lines returned (default 50, capped at 200).
    #[serde(default)]
    max_results: Option<usize>,
}

/// `trace`'s request: two symbols, fuzzy like everywhere else.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TraceReq {
    #[serde(default = "default_plane")]
    plane: String,
    /// Where the flow starts.
    from: String,
    /// What it should reach.
    to: String,
}

/// `impact`'s request: one symbol and how far to look.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ImpactReq {
    #[serde(default = "default_plane")]
    plane: String,
    name: String,
    /// Hops of incoming structural edges to walk (default 3, max 6).
    #[serde(default)]
    depth: Option<usize>,
}

/// `fathom`'s request: one symbol and how wide a region around it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FathomReq {
    #[serde(default = "default_plane")]
    plane: String,
    name: String,
    /// Hops to walk, out and in (default 2, max 6).
    #[serde(default)]
    depth: Option<usize>,
}

/// `history`'s request: which repository, and how much of it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct HistoryReq {
    /// The history plane, or the code plane beside it — naming `myrepo` finds
    /// `myrepo_git`, because the first is what a reader has in mind.
    #[serde(default = "default_plane")]
    plane: String,
    /// Commits to list, newest first (default 15).
    #[serde(default)]
    limit: Option<usize>,
}

/// `snippet`'s request: one symbol's source.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SnippetReq {
    #[serde(default = "default_plane")]
    plane: String,
    name: String,
    /// Lines of source returned from the declaration down (default 40).
    #[serde(default)]
    lines: Option<usize>,
}

/// `search`'s request: free text and a plane.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchReq {
    #[serde(default = "default_plane")]
    plane: String,
    /// What to look for, in plain words — no identifier needed.
    query: String,
    /// How many hits (default 8).
    #[serde(default)]
    k: Option<u64>,
}

/// Bounded literal search under `root`: build dirs and binaries skipped,
/// results and line lengths capped, paths relative to the root.
fn grep_tree(root: &std::path::Path, req: &GrepReq) -> anyhow::Result<String> {
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "dist",
        "build",
        "__pycache__",
        ".venv",
        ".codegraph",
        ".codebase-memory",
    ];
    const MAX_FILE: u64 = 2 * 1024 * 1024;
    const MAX_LINE: usize = 300;
    let cap = req.max_results.unwrap_or(50).clamp(1, 200);
    let fold = req.ignore_case.unwrap_or(false);
    let needle = if fold {
        req.pattern.to_lowercase()
    } else {
        req.pattern.clone()
    };
    if needle.trim().is_empty() {
        anyhow::bail!("an empty pattern matches everything and helps no one");
    }

    let mut out = String::new();
    let mut hits = 0usize;
    let mut capped = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            if meta.len() > MAX_FILE {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes[..bytes.len().min(4096)].contains(&0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            for (i, line) in text.lines().enumerate() {
                let hay = if fold {
                    std::borrow::Cow::Owned(line.to_lowercase())
                } else {
                    std::borrow::Cow::Borrowed(line)
                };
                if !hay.contains(needle.as_str()) {
                    continue;
                }
                if hits == cap {
                    capped = true;
                    break;
                }
                let mut shown = line.trim_end();
                if shown.len() > MAX_LINE {
                    let mut end = MAX_LINE;
                    while !shown.is_char_boundary(end) {
                        end -= 1;
                    }
                    shown = &shown[..end];
                }
                out.push_str(&format!("{rel}:{}: {shown}\n", i + 1));
                hits += 1;
            }
            if capped {
                break;
            }
        }
        if capped {
            break;
        }
    }
    if hits == 0 {
        out.push_str("no matches\n");
    } else if capped {
        out.push_str(&format!(
            "… capped at {cap} matches — narrow the pattern or raise max_results\n"
        ));
    }
    Ok(out)
}

fn compact_logic(
    db: &Database,
    req: SymbolReq,
    render: fn(&dr_strange_core::PlaneHandle<'_>, &str) -> dr_strange_core::Result<String>,
) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
    Ok(Value::String(render(&plane, &req.name)?))
}

fn get_node_logic(db: &Database, req: GetNode) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;
    let node = match (req.id, &req.key) {
        (Some(id), _) => p.node(NodeId(id))?,
        (None, Some(key)) => p.node_by_key(key)?,
        (None, None) => anyhow::bail!("provide `id` or `key`"),
    };
    Ok(node
        .map(|n| json::node_to_json_lean(&n))
        .unwrap_or(Value::Null))
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
    read_result(db.plane(&req.plane)?.query_from_plan(plan))
}

/// Render what a read query returns: a table when its plan projects, its
/// nodes otherwise.
fn read_result(q: dr_strange_core::QueryBuilder<'_>) -> AnyResult<Value> {
    match q.plan().project.is_some() {
        true => Ok(json::table_to_json(&q.table()?)),
        false => Ok(scored_rows(&q.scored_nodes()?)),
    }
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
            req.key_env.as_deref(),
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
            Some(node) => json::node_to_json_lean(&node),
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
    let chat = dr_strange_llm::build_provider(
        provider,
        req.model.as_deref(),
        None,
        req.key_env.as_deref(),
        false,
    )?;
    let embedder = req.embed_provider.as_deref().and_then(|ep| {
        dr_strange_llm::build_provider(
            ep,
            req.embed_model.as_deref(),
            None,
            req.embed_key_env.as_deref(),
            true,
        )
        .ok()
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
    let results: Vec<Value> = res.nodes.iter().map(json::node_to_json_lean).collect();
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

/// Pin a plane handle to the snapshot a query's `AS OF` clause names
/// (ROADMAP §4); the native backend is the only one that keeps history.
#[cfg(feature = "native-backend")]
fn pin(p: PlaneHandle<'_>, at: Option<dr_strange_parser::AsOfSpec>) -> AnyResult<PlaneHandle<'_>> {
    use dr_strange_core::AsOf;
    use dr_strange_parser::AsOfSpec;
    Ok(match at {
        None => p,
        Some(AsOfSpec::Seq(seq)) => p.as_of(AsOf::Seq(seq))?,
        Some(AsOfSpec::Time(ms)) => p.as_of(AsOf::Time(ms))?,
    })
}

#[cfg(not(feature = "native-backend"))]
fn pin(p: PlaneHandle<'_>, at: Option<dr_strange_parser::AsOfSpec>) -> AnyResult<PlaneHandle<'_>> {
    if at.is_some() {
        anyhow::bail!("AS OF (time-travel) requires the native backend");
    }
    Ok(p)
}

fn cypher_logic(db: &Database, req: Cypher) -> AnyResult<Value> {
    let provider = req.embed.as_deref().unwrap_or("openai");
    // Built eagerly because the parser needs it up front, but tolerantly: most
    // queries never embed anything, and a plain MATCH must not require a
    // provider. The reason is kept rather than dropped — without it a query
    // that *does* need an embedder reports only "needs an embedding provider
    // (set the API key)", which misdirects when the real cause was the
    // provider spec (an unusable base URL, or a model the endpoint requires).
    let built = dr_strange_llm::build_provider(
        provider,
        req.embed_model.as_deref(),
        None,
        req.embed_key_env.as_deref(),
        true,
    );
    let why_no_embedder = built.as_ref().err().map(|e| e.to_string());
    let embedder = built.ok().map(|p| LlmEmbedder(Box::new(p)));
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
    .map_err(|e| match &why_no_embedder {
        Some(cause) => anyhow::anyhow!("{e} — embedding provider '{provider}': {cause}"),
        None => anyhow::anyhow!("{e}"),
    })?;
    let plane = db.plane(&req.plane)?;
    match stmt {
        dr_strange_parser::Statement::Read(read) => {
            read_result(pin(plane, read.as_of)?.query_from_plan(read.plan))
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

/// The property an auto-generated embedding is written to — the same one
/// `digest` writes and `search` reads, so an agent's writes and a digest's land
/// in one index rather than two.
const EMBED_PROP: &str = "embedding";

fn write_nodes_logic(
    db: &Database,
    req: WriteNodes,
    embedder: Option<&dyn dr_strange_llm::Embedder>,
) -> AnyResult<Value> {
    let p = db.plane(&req.plane)?;

    // Decode before embedding: a batch that cannot decode should fail before it
    // costs a provider call.
    let mut decoded: Vec<(NodeInput, Properties)> = Vec::with_capacity(req.nodes.len());
    for node in req.nodes {
        let props = match &node.properties {
            Some(v) => json::json_to_properties(v)?,
            None => Properties::new(),
        };
        decoded.push((node, props));
    }

    let mut embedded = 0usize;
    if let Some(embedder) = embedder {
        // A node that already carries a vector is left alone: a caller who
        // supplied their own embedding always wins over the server's guess.
        let targets: Vec<usize> = decoded
            .iter()
            .enumerate()
            .filter(|(_, (_, props))| !props.contains_key(EMBED_PROP))
            .map(|(i, _)| i)
            .collect();
        if !targets.is_empty() {
            let texts: Vec<String> = targets
                .iter()
                .map(|&i| {
                    let (node, props) = &decoded[i];
                    dr_strange_llm::entity_text(
                        node.external_key.as_deref().unwrap_or(""),
                        &node.labels,
                        props,
                    )
                    .trim()
                    .to_string()
                })
                .collect();
            // One call for the whole batch, not one per node: these are network
            // round-trips on a blocking thread, so a fifty-node write should
            // cost one of them rather than fifty.
            let reply = embedder.embed(&texts)?;
            // Positional: vector `n` belongs to text `n`. A provider returning a
            // different count would silently mis-assign every vector after the
            // gap, so refuse rather than zip and hope.
            if reply.vectors.len() != texts.len() {
                anyhow::bail!(
                    "embedder returned {} vectors for {} texts; refusing to guess which is which",
                    reply.vectors.len(),
                    texts.len()
                );
            }
            for (&i, vector) in targets.iter().zip(reply.vectors) {
                decoded[i]
                    .1
                    .insert(EMBED_PROP.into(), PropDesc::new(PropValue::Vector(vector)));
                embedded += 1;
            }
        }
    }

    let mut txn = p.write()?;
    let mut ids = Vec::new();
    for (node, props) in decoded {
        let labels: Vec<&str> = node.labels.iter().map(String::as_str).collect();
        let id = match &node.external_key {
            Some(key) => txn.create_node_with_key(key, &labels, props)?,
            None => txn.create_node(&labels, props)?,
        };
        ids.push(id.0);
    }
    txn.commit()?;
    Ok(jval!({ "created": ids, "embedded": embedded }))
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

fn digest_logic(
    db: &Database,
    req: Digest,
    tuning: DigestTuning,
    local_files: bool,
) -> AnyResult<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Built only on the branch that routes: a text digest never needs the
    // plugin store, and must not fail because something in it is broken.
    let load_plugins = || -> AnyResult<dr_strange_llm::Plugins> {
        let mut options = std::collections::BTreeMap::new();
        if req.plugin_source.unwrap_or(false) {
            options.insert(
                "rust".to_string(),
                vec![("include_source".to_string(), "true".to_string())],
            );
        }
        dr_strange_llm::Plugins::load(&dr_strange_llm::PluginConfig {
            options,
            ..Default::default()
        })
    };
    let handler = req.handler.as_deref();

    // Resolve the input before anything else: a refused `path` should cost
    // no provider call.
    let mut facts = match &req.path {
        // Text sent over the wire stays prose, deliberately. Preprocessing is
        // for reading a checkout the agent already has — it is worth its cost
        // when a plugin can pull the files around the one it was handed, and
        // that pull is exactly what a shared server must not offer. Routing it
        // here would hand every caller a handler whose only reachable input is
        // the server's own filesystem.
        None => dr_strange_llm::Preprocessed::prose_only("text", req.text.clone()),
        Some(_) if !local_files => anyhow::bail!(
            "this server does not read local files — send the document as `text`. \
             (`path` is honoured only by the stdio server, which runs on your own machine.)"
        ),
        Some(path) => {
            let p = std::path::Path::new(path);
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            // A directory is a legal source since ROADMAP §11: the preprocessor
            // pulls what it needs, so "digest this project" needs no file list.
            if p.is_dir() {
                let host = dr_strange_llm::LocalFiles::new(p)
                    .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
                let plugins = load_plugins()?;
                dr_strange_llm::route_tree(&host, handler, &plugins)?
            } else {
                let bytes =
                    std::fs::read(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
                let host = dr_strange_llm::LocalFiles::new(
                    p.parent().unwrap_or(std::path::Path::new(".")),
                )
                .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
                let plugins = load_plugins()?;
                dr_strange_llm::route_document(&name, &bytes, handler, &host, &plugins)
                    .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?
            }
        }
    };
    if facts.nodes.is_empty() && !facts.needs_model() {
        anyhow::bail!("nothing to digest — give `text`, or `path` on a stdio server");
    }

    let chat_provider = req.chat.as_deref().unwrap_or("openai");
    let embed_provider = req.embed.as_deref().unwrap_or(chat_provider);
    let embed = !req.no_embed;
    let link = req.link.unwrap_or(true);

    // Parsed before anything expensive: a typo should not cost a provider call.
    let mode = match req.mode.as_deref() {
        None => dr_strange_llm::DigestMode::default(),
        Some(m) => dr_strange_llm::DigestMode::parse(m)
            .ok_or_else(|| anyhow::anyhow!("unknown digest mode `{m}`"))?,
    };

    let p = db.plane(&req.plane)?;
    let run_id = format!(
        "mcp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let source = req.source.unwrap_or_else(|| "mcp-digest".into());
    dr_strange_llm::stamp_run(&mut facts, &source, &run_id);

    // The §11 headline: an input that yields only facts is digested with **no
    // model call at all** — no provider constructed, no key read from the
    // environment, no request made.
    let result = if facts.needs_model() {
        // Provider keys come from the server's environment (never tool params) —
        // `key_env` names the variable, it does not carry the key.
        let chat = dr_strange_llm::build_provider(
            chat_provider,
            req.model.as_deref(),
            None,
            req.key_env.as_deref(),
            false,
        )?;
        let embedder = dr_strange_llm::build_provider(
            embed_provider,
            req.embed_model.as_deref(),
            None,
            req.embed_key_env.as_deref(),
            embed,
        )?;
        let opts = dr_strange_llm::DigestOptions {
            source,
            model: chat.model().to_string(),
            run_id,
            chunk_chars: tuning.chunk_chars,
            embed,
            concurrency: tuning.concurrency,
            mode,
            refine_max_entities: None,
            refine_max_context: None,
        };

        let cands = dr_strange_llm::PlaneCandidates::new(&p);
        let plane_source = link.then_some(&cands as &dyn dr_strange_llm::CandidateSource);
        // Grounded whether or not `link` is on: without this the model is told
        // the facts this very run parsed are new, and proposes a duplicate of
        // what the parser just established.
        let grounded = dr_strange_llm::FactsAndPlane::new(&facts, plane_source);
        let extracted =
            dr_strange_llm::digest(&facts.prose, &chat, &embedder, Some(&grounded), &opts)?;
        dr_strange_llm::fold(facts, extracted)
    } else {
        dr_strange_llm::fold(facts, dr_strange_llm::DigestResult::default())
    };
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
            // What a preprocessor skipped or could not resolve. An agent
            // reading a thinner graph than it expected should be able to see
            // why here, rather than re-running the ingest to find out.
            "notes": r.notes,
        },
    });

    if req.apply {
        let mut txn = p.write()?;
        let stats = result.apply(&p, &mut txn)?;
        txn.commit()?;
        out["nodes_written"] = jval!(stats.written.nodes);
        out["edges_written"] = jval!(stats.written.edges);
        // Named, not just counted: an agent that proposed an entity the plane
        // already knew should be told, or it will keep re-proposing it.
        out["nodes_skipped"] = jval!(stats.skipped.len());
        out["skipped_keys"] = jval!(stats.skipped);
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
                    "properties": json::properties_to_json_lean(&n.props),
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

/// `snippet`'s body. `fallback_root` is the tree the process was started with,
/// used only when the plane does not record one of its own.
fn snippet_logic(
    db: &Database,
    fallback_root: Option<&std::path::Path>,
    req: SnippetReq,
) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
    let node = match dr_strange_core::compact::resolve(&plane, &req.name)? {
        dr_strange_core::compact::Resolved::One(n) => n,
        dr_strange_core::compact::Resolved::Many(hits) => {
            anyhow::bail!(
                "`{}` is ambiguous — {} matches; use an exact key",
                req.name,
                hits.len()
            );
        }
        dr_strange_core::compact::Resolved::None => {
            anyhow::bail!("no symbol matches `{}` in this plane", req.name);
        }
    };
    // The digest's own copy first — exact by construction.
    if let Some(dr_strange_core::PropValue::Str(src)) =
        node.properties.get("source").map(|d| &d.value)
    {
        return Ok(Value::String(src.clone()));
    }
    let file =
        ["file", "path"]
            .iter()
            .find_map(|k| match node.properties.get(*k).map(|d| &d.value) {
                Some(dr_strange_core::PropValue::Str(f)) => Some(f.clone()),
                _ => None,
            });
    let line = match node.properties.get("line").map(|d| &d.value) {
        Some(dr_strange_core::PropValue::Int(l)) => Some(*l as usize),
        _ => None,
    };
    // `file` is relative to whatever directory *this plane* was parsed from,
    // which the digest records on the plane as `synced_root`. The process's own
    // tree is only a fallback: once a server holds a second code plane, using it
    // reads another repository's file at the same relative path — a plausible
    // answer with no error, which is worse than failing.
    let plane_root = plane.properties().ok().and_then(|props| {
        match props.get("synced_root").map(|d| &d.value) {
            Some(dr_strange_core::PropValue::Str(r)) => Some(std::path::PathBuf::from(r)),
            _ => None,
        }
    });
    let (Some(root), Some(file), Some(line)) =
        (plane_root.as_deref().or(fallback_root), file, line)
    else {
        anyhow::bail!(
            "no stored source, and no source tree for this plane — the plane \
             records no `synced_root` and none is attached to this server; \
             digest with include_source, or attach the tree (`serve watch` \
             does; [server] source_root otherwise)"
        );
    };
    let text = std::fs::read_to_string(root.join(&file))
        .map_err(|e| anyhow::anyhow!("reading {file}: {e}"))?;
    let want = req.lines.unwrap_or(40).clamp(1, 200);
    let start = line.saturating_sub(1);
    let slice: Vec<&str> = text.lines().skip(start).take(want).collect();
    let mut out = format!("{file}:{line} ({} lines)\n", slice.len());
    for (i, l) in slice.iter().enumerate() {
        out.push_str(&format!("{:>5} | {l}\n", start + i + 1));
    }
    Ok(Value::String(out))
}

// ---- tools (rmcp wrappers) ------------------------------------------------

#[tool_router(router = tool_router)]
impl DrStrange {
    #[tool(description = "List all planes with their node/edge counts and \
        properties — including `synced_root` (the canonical source \
        directory) and `synced_commit` when the plane was created by \
        `digest`/`serve watch`. Match `synced_root` against the caller's \
        cwd to find the right plane instead of relying on any tool's \
        default.")]
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

    #[tool(description = "The primary code-context tool: one symbol's whole \
        neighborhood in a single call — definition, signature, doc comment, \
        fields, callers with call sites, callees, containment and every \
        other edge, as compact text. Accepts a fuzzy name (exact key, \
        `::name`/`.name` suffix, or substring); ambiguity returns the \
        candidates. Use this first for any what-is/who-calls/what-calls \
        question.")]
    async fn context(
        &self,
        Parameters(req): Parameters<SymbolReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("context", move |db| {
            compact_logic(db, req, dr_strange_core::compact::context)
        })
        .await
    }

    #[tool(description = "Semantic lookup when no identifier is known: embeds \
        the query text and returns the closest symbols by meaning (cosine \
        over the plane's `embedding` vectors), best hit expanded. Requires \
        the plane to be vectorized and the server to have an embed provider \
        configured.")]
    async fn search(
        &self,
        Parameters(req): Parameters<SearchReq>,
    ) -> Result<CallToolResult, McpError> {
        let embed = self.embed.clone();
        self.blocking("search", move |db| {
            let Some(cfg) = &embed else {
                // The section is `[digest]`, and saying `[server]` was worse
                // than saying nothing: `[server]` denies unknown fields, so an
                // operator who followed this hint got a server that would not
                // start.
                anyhow::bail!(
                    "no embed provider configured on this server — set \
                     `[digest] embed_provider` in drsg.toml (with \
                     `embed_key_env` naming the environment variable that \
                     holds the key), then restart; `grep` searches the tree by \
                     text without one"
                );
            };
            let embedder = dr_strange_llm::build_provider(
                &cfg.provider,
                cfg.model.as_deref(),
                None,
                cfg.key_env.as_deref(),
                true,
            )?;
            let text = dr_strange_llm::semantic_search(
                db,
                &req.plane,
                &req.query,
                &embedder,
                req.k.unwrap_or(8),
            )?;
            Ok(Value::String(text))
        })
        .await
    }

    #[tool(description = "Literal text search over the source tree behind \
        the graph (the watched directory). One matching line per result, \
        `file:line: text`. For log messages, config values, comments — \
        anything the graph deliberately does not model. No regex.")]
    async fn grep(&self, Parameters(req): Parameters<GrepReq>) -> Result<CallToolResult, McpError> {
        let root = self.source_root.clone();
        self.blocking("grep", move |_db| {
            let Some(root) = root else {
                anyhow::bail!(
                    "no source tree attached to this server — `serve watch` \
                     attaches its --dir; a plain serve has only the graph, \
                     so run grep locally instead"
                );
            };
            Ok(Value::String(grep_tree(&root, &req)?))
        })
        .await
    }

    #[tool(description = "How one symbol reaches another: the shortest \
        recorded CALLS path, one hop per line; tries the reverse direction \
        and says so when the forward holds nothing. Fuzzy names.")]
    async fn trace(
        &self,
        Parameters(req): Parameters<TraceReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("trace", move |db| {
            let plane = db.plane(&req.plane)?;
            Ok(Value::String(dr_strange_core::compact::trace(
                &plane, &req.from, &req.to,
            )?))
        })
        .await
    }

    #[tool(description = "Blast radius: everything reaching this symbol \
        through incoming structural edges (CALLS, REFERENCES, INSTANTIATES, \
        IMPORTS, EXTENDS, IMPLEMENTS), grouped by distance with exact \
        counts. Fuzzy name; depth defaults to 3.")]
    async fn impact(
        &self,
        Parameters(req): Parameters<ImpactReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("impact", move |db| {
            let plane = db.plane(&req.plane)?;
            Ok(Value::String(dr_strange_core::compact::impact(
                &plane,
                &req.name,
                req.depth.unwrap_or(3),
            )?))
        })
        .await
    }

    #[tool(description = "Read one region of the graph closely: everything \
        within `depth` hops of this symbol, out and in, reported as its \
        makeup rather than a listing — node counts by label, edge counts by \
        type with each direction, how many nodes each hop added, and the \
        hubs that hold the region together (by their edges inside it). Use \
        it to size up an unfamiliar corner before reading it: `context` \
        answers about one symbol and `impact` names what reaches it, while \
        this says what kind of place the symbol sits in. Fuzzy name; depth \
        defaults to 2. The walk is bounded by depth and by a node budget, \
        and the reply says which bound it hit — counts are always exact \
        over what it walked.")]
    async fn fathom(
        &self,
        Parameters(req): Parameters<FathomReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("fathom", move |db| {
            let plane = db.plane(&req.plane)?;
            Ok(Value::String(dr_strange_core::compact::fathom(
                &plane,
                &req.name,
                req.depth.unwrap_or(2),
            )?))
        })
        .await
    }

    #[tool(description = "A repository's history at a glance: where HEAD is, \
        what the branches and tags point at, which branches were rebased \
        (and what each replaced), and the newest commits — as compact text. \
        Reads the `<plane>_git` plane a digest of a git checkout writes; \
        naming the code plane finds it. Use this for when/who/why questions \
        — when something changed, who changed it, what a release contains, \
        whether history was rewritten — where `context` answers what the \
        code is now. Rebases come from the reflog, which is local to one \
        clone and expires, so the answer says what it can and cannot know.")]
    async fn history(
        &self,
        Parameters(req): Parameters<HistoryReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("history", move |db| {
            // Naming the code plane finds the history beside it; naming the
            // history plane is taken at its word.
            let history = dr_strange_core::compact::history_plane_name(&req.plane);
            let plane = match db.plane(&history) {
                Ok(p)
                    if !req
                        .plane
                        .ends_with(dr_strange_core::compact::HISTORY_SUFFIX) =>
                {
                    p
                }
                _ => db.plane(&req.plane)?,
            };
            Ok(Value::String(dr_strange_core::compact::history(
                &plane, req.limit,
            )?))
        })
        .await
    }

    #[tool(description = "One symbol's source text: from the graph when the \
        digest stored it, else read at the symbol's recorded file:line from \
        the tree that plane was parsed from (its `synced_root`), falling back \
        to the tree attached to this server. Fuzzy name.")]
    async fn snippet(
        &self,
        Parameters(req): Parameters<SnippetReq>,
    ) -> Result<CallToolResult, McpError> {
        let root = self.source_root.clone();
        self.blocking("snippet", move |db| snippet_logic(db, root.as_deref(), req))
            .await
    }

    #[tool(
        description = "One symbol's content as `prop: value` text lines         (signature, doc comment, …; vectors elided). Accepts a fuzzy name."
    )]
    async fn describe(
        &self,
        Parameters(req): Parameters<SymbolReq>,
    ) -> Result<CallToolResult, McpError> {
        self.blocking("describe", move |db| {
            compact_logic(db, req, dr_strange_core::compact::describe)
        })
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
        language — the whole engine through one surface, and the preferred \
        alternative to the raw `query` plan. Reads return the matching node \
        records; score() carries the seed's relevance or an algorithm's \
        per-node result. Every source binds one node pattern and may continue \
        with a typed hop (put hops in the pattern, before WHERE): \
        MATCH one linear path (labels, ->/<-/-, bounded *m..n); \
        SEARCH (v:L) [ON prop] NEAR \"text\"|[..] [METRIC m] [TOPK k] — vector \
        top-k, where ON defaults to the `embedding` property; \
        SEARCH (v:L) ON prop MATCHING \"text\" [TOPK k] — BM25 keyword \
        search (label and ON both required); HYBRID (v:L) [VECTOR [ON p] NEAR q [WEIGHT w]] \
        [KEYWORD ON p MATCHING \"text\" [WEIGHT w]] [GRAPH HOPS h [DECAY d] \
        [WEIGHT w]] [CANDIDATES n] [TOPK k] — fused retrieval; \
        CALL pagerank|components|shortest_path|louvain(args) ON (v[:L]) — graph \
        algorithms. Then BEAM similarity traversal, WHERE (property/label \
        tests, key(n) for a node's external key, x IN [a,b]), \
        RETURN [DISTINCT] <var>|* — the pattern's last variable, whole \
        records — or a projection: `RETURN f.file, count(*) AS calls` reads \
        any bound variable, takes AS aliases, and folds with \
        count/sum/avg/min/max/collect (each optionally DISTINCT), grouped by \
        every column that is not a fold; a projected query answers with \
        {columns, rows} instead of nodes. ORDER BY/SKIP/LIMIT take \
        expressions (`ORDER BY f.line`) and, when the query projects, name a \
        returned column (`ORDER BY calls DESC`); a trailing \
        AS OF <seq|\"RFC-3339\"|TIME ms> reads a past snapshot. \
        Writes (CREATE/MERGE/SET/REMOVE/DELETE) mutate the plane and return \
        change-counts. Examples: \
        `MATCH (n:Person) WHERE n.age >= 30 RETURN n ORDER BY n.age DESC LIMIT 5`; \
        `MATCH (n)-[:KNOWS]->(m) WHERE key(n) = \"alice\" RETURN m`; \
        `MATCH (f:Fn)-[:CALLS]->(g:Fn) RETURN f.file, count(*) AS calls \
        ORDER BY calls DESC LIMIT 10`; \
        `SEARCH (d:Doc) ON body MATCHING \"graph database\" TOPK 5 RETURN d`; \
        `CALL pagerank() ON (n:Paper) RETURN n ORDER BY score() DESC LIMIT 10`; \
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
        let embed = self.embed.clone();
        self.blocking("write_nodes", move |db| {
            // Built inside the blocking body: constructing it reads the
            // environment, and `embed` itself is a blocking HTTP call (the LLM
            // layer is sync, on ureq), so it belongs on this thread and not on
            // the async runtime.
            let embedder = match &embed {
                None => None,
                Some(cfg) => Some(dr_strange_llm::build_provider(
                    &cfg.provider,
                    cfg.model.as_deref(),
                    None,
                    cfg.key_env.as_deref(),
                    true,
                )?),
            };
            write_nodes_logic(
                db,
                req,
                embedder
                    .as_ref()
                    .map(|e| e as &dyn dr_strange_llm::Embedder),
            )
        })
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
        let tuning = self.digest;
        let local_files = self.local_files;
        self.blocking("digest", move |db| {
            digest_logic(db, req, tuning, local_files)
        })
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
             default 'startup'.\n\
             A digested repository has two planes. `<name>` holds the code as \
             it is now — ask `context`, `trace`, `impact`, `fathom`, \
             `snippet`, `grep`. \
             `<name>_git` holds the same repository's history — Commit (also \
             Merge), Branch, Tag and Rebase nodes, joined by PARENT (with \
             `order`, so a merge's first parent is the line it was made on), \
             TIP, TAGS, ONTO, REPLACED, PRODUCED, RESULT and ON. Ask \
             `history` for an overview, then `cypher`/`query`/`traverse` over \
             that plane for anything specific. Use it for when/who/why: when \
             a change landed, who made it, what a release contains, whether a \
             branch was rebased and what it replaced. Two caveats it will \
             repeat itself: rebases are reconstructed from the reflog, which \
             is local to one clone and expires, so their absence is not \
             evidence; and commits marked `reachable: false` are what a \
             rewrite left behind."
                .to_string(),
        );
        info
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use serde_json::from_value;

    /// Counts calls and texts so a test can prove the batch is one round-trip.
    struct CountingEmbedder {
        calls: std::sync::atomic::AtomicUsize,
        texts: Mutex<Vec<String>>,
    }

    impl CountingEmbedder {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                texts: Mutex::new(Vec::new()),
            }
        }
    }

    impl dr_strange_llm::Embedder for CountingEmbedder {
        fn embed(&self, texts: &[String]) -> AnyResult<dr_strange_llm::EmbedReply> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.texts.lock().unwrap().extend_from_slice(texts);
            Ok(dr_strange_llm::EmbedReply {
                // Distinct per text, so a mis-assignment would be visible.
                vectors: (0..texts.len()).map(|i| vec![i as f32, 1.0]).collect(),
                tokens: texts.len() as u64,
            })
        }
    }

    /// Embedding a write is one provider round-trip for the whole batch, not
    /// one per node: these are network calls on a blocking thread.
    #[test]
    fn write_nodes_embeds_the_batch_in_one_call() {
        let db = Database::in_memory().unwrap();
        let em = CountingEmbedder::new();
        let out = write_nodes_logic(
            &db,
            from_value(jval!({"nodes": [
                {"external_key": "a", "labels": ["Doc"], "properties": {"title": "graph"}},
                {"external_key": "b", "labels": ["Doc"], "properties": {"title": "sql"}},
                {"external_key": "c", "labels": ["Doc"], "properties": {"title": "vector"}}
            ]}))
            .unwrap(),
            Some(&em),
        )
        .unwrap();

        assert_eq!(out["embedded"], 3);
        assert_eq!(
            em.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "three nodes must cost one round-trip, not three"
        );
        // The text is the shared recipe: identity first, then properties.
        let texts = em.texts.lock().unwrap().clone();
        assert!(texts[0].starts_with("a (Doc)"), "got {:?}", texts[0]);
        assert!(texts[0].contains("title: graph"), "got {:?}", texts[0]);

        // Vectors landed positionally, in the property `digest` and `search` use.
        let p = db.plane("startup").unwrap();
        for (key, want) in [("a", 0.0), ("b", 1.0), ("c", 2.0)] {
            let node = p.node_by_key(key).unwrap().unwrap();
            match &node.properties.get(EMBED_PROP).unwrap().value {
                PropValue::Vector(v) => assert_eq!(v[0], want, "{key} got the wrong vector"),
                other => panic!("{key}: expected a vector, got {other:?}"),
            }
        }
    }

    /// A caller who supplied their own embedding always wins.
    #[test]
    fn write_nodes_leaves_a_supplied_vector_alone() {
        let db = Database::in_memory().unwrap();
        let em = CountingEmbedder::new();
        let out = write_nodes_logic(
            &db,
            from_value(jval!({"nodes": [
                {"external_key": "mine", "labels": ["Doc"],
                 "properties": {"embedding": {"$vector": [9.0, 9.0]}}},
                {"external_key": "theirs", "labels": ["Doc"], "properties": {"title": "x"}}
            ]}))
            .unwrap(),
            Some(&em),
        )
        .unwrap();

        assert_eq!(out["embedded"], 1, "only the node without a vector");
        assert_eq!(
            em.texts.lock().unwrap().len(),
            1,
            "the supplied vector must not even be sent for embedding"
        );
        let p = db.plane("startup").unwrap();
        let mine = p.node_by_key("mine").unwrap().unwrap();
        match &mine.properties.get(EMBED_PROP).unwrap().value {
            PropValue::Vector(v) => {
                assert_eq!(v, &vec![9.0, 9.0], "the caller's vector was replaced")
            }
            other => panic!("expected a vector, got {other:?}"),
        }
    }

    /// With no provider configured, a write is exactly what was asked for.
    #[test]
    fn write_nodes_without_a_provider_embeds_nothing() {
        let db = Database::in_memory().unwrap();
        let out = write_nodes_logic(
            &db,
            from_value(jval!({"nodes": [
                {"external_key": "a", "labels": ["Doc"], "properties": {"title": "graph"}}
            ]}))
            .unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(out["embedded"], 0);
        let p = db.plane("startup").unwrap();
        let node = p.node_by_key("a").unwrap().unwrap();
        assert!(!node.properties.contains_key(EMBED_PROP));
    }

    /// A served MCP endpoint must not read paths its caller names — that is an
    /// arbitrary-file-read primitive dressed as an ingestion feature.
    #[test]
    fn digest_refuses_a_path_unless_the_host_allows_local_files() {
        let db = Database::in_memory().unwrap();
        let req: Digest = from_value(jval!({"path": "/etc/passwd"})).unwrap();
        let err = digest_logic(&db, req, DigestTuning::default(), false)
            .expect_err("a networked server must refuse a caller-named path");
        let msg = err.to_string();
        assert!(msg.contains("does not read local files"), "got: {msg}");
        // It must say what to do instead, or an agent just retries the same call.
        assert!(
            msg.contains("text"),
            "should point at the alternative: {msg}"
        );
    }

    /// Refused before any provider call — a rejected path should cost nothing.
    #[test]
    fn digest_with_neither_text_nor_path_is_refused() {
        let db = Database::in_memory().unwrap();
        let req: Digest = from_value(jval!({"text": "   "})).unwrap();
        let err = digest_logic(&db, req, DigestTuning::default(), true)
            .expect_err("an empty document must not reach a provider");
        assert!(err.to_string().contains("nothing to digest"), "{err}");
    }

    /// Every tool body must pass the gate. Nothing else bounds them: the
    /// transport answers a call as soon as it is queued, releasing its own
    /// concurrency permit long before the work runs, so without this an
    /// authenticated caller could pipeline unlimited scans and digests.
    #[tokio::test]
    async fn a_tool_cannot_run_while_the_gate_is_held() {
        let db = Arc::new(Database::in_memory().unwrap());
        let gate = Arc::new(Semaphore::new(1));
        let svc = DrStrange::new(db).with_tool_gate(gate.clone());

        // Hold the only permit, as a tool already in flight would.
        let held = gate.clone().acquire_owned().await.unwrap();
        let blocked = tokio::time::timeout(Duration::from_millis(250), svc.list_planes()).await;
        assert!(
            blocked.is_err(),
            "a tool ran while the gate was exhausted — nothing is bounding tool work"
        );

        // Releasing lets it through, so the gate queues rather than rejects:
        // a busy server should make an agent wait, not fail it.
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), svc.list_planes())
            .await
            .expect("the tool should run once a permit frees")
            .expect("list_planes");
    }

    /// The gate is only useful if every session shares one — MCP puts no limit
    /// on how many sessions a client opens.
    #[tokio::test]
    async fn the_gate_is_shared_between_instances() {
        let db = Arc::new(Database::in_memory().unwrap());
        let gate = Arc::new(Semaphore::new(1));
        let a = DrStrange::new(db.clone()).with_tool_gate(gate.clone());
        let b = DrStrange::new(db).with_tool_gate(gate.clone());

        let held = gate.clone().acquire_owned().await.unwrap();
        for (name, svc) in [("a", &a), ("b", &b)] {
            assert!(
                tokio::time::timeout(Duration::from_millis(150), svc.list_planes())
                    .await
                    .is_err(),
                "session {name} ran a tool despite the shared gate being exhausted"
            );
        }
        drop(held);
    }

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
            None,
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

    /// `list_planes` must surface plane properties like `synced_root` so an
    /// agent can match its cwd against the real source directory instead of
    /// relying on any tool's "startup" default.
    #[test]
    fn list_planes_surfaces_properties() {
        let db = fixture();
        let plane = db.plane("startup").unwrap();
        let mut props = plane.properties().unwrap();
        props.insert(
            "synced_root".into(),
            PropDesc::described(
                "directory the facts were parsed from",
                PropValue::Str("/home/wangying/workspace/dr-strange".into()),
            ),
        );
        plane.set_properties(props).unwrap();

        let planes = list_planes_logic(&db).unwrap();
        assert_eq!(
            planes[0]["properties"]["synced_root"]["$value"],
            jval!("/home/wangying/workspace/dr-strange")
        );
    }

    /// Gemini's tool-schema converter rejects a *bare boolean* where a Schema
    /// object is required — e.g. `"properties": true` rendered by schemars for
    /// a doc-comment-less `Option<Value>` (the sub2api `400 Invalid value at
    /// 'request.tools[...].parameters.properties[...].value' ... Schema, true`
    /// failure). Walk a rendered schema and fail on any boolean in a
    /// schema-bearing position.
    fn assert_no_boolean_schema(v: &serde_json::Value, path: &str) {
        if v.is_boolean() {
            panic!("bare boolean schema at {path}: {v}");
        }
        let Some(map) = v.as_object() else {
            return;
        };
        for (k, val) in map {
            let p = format!("{path}.{k}");
            match k.as_str() {
                "properties" | "$defs" | "definitions" | "patternProperties" => {
                    if let Some(objs) = val.as_object() {
                        for (name, sub) in objs {
                            assert_no_boolean_schema(sub, &format!("{p}.{name}"));
                        }
                    }
                }
                "items"
                | "additionalProperties"
                | "additionalItems"
                | "contains"
                | "not"
                | "if"
                | "then"
                | "else"
                | "propertyNames" => assert_no_boolean_schema(val, &p),
                "anyOf" | "allOf" | "oneOf" | "prefixItems" => {
                    if let Some(arr) = val.as_array() {
                        for (i, it) in arr.iter().enumerate() {
                            assert_no_boolean_schema(it, &format!("{p}[{i}]"));
                        }
                    }
                }
                // keywords whose values are not schemas (type/description/
                // default/required/enum/…): nothing to descend into
                _ => {}
            }
        }
    }

    #[test]
    fn tool_schemas_have_no_bare_boolean_schema_values() {
        // Regression: `EdgeInput.properties: Option<Value>` had no doc comment
        // and schemars rendered it as a bare `true`, which Gemini's strict
        // converter rejects. Every struct that carries a free-form
        // `Value`/`Map` field is checked here so a future one cannot regress.
        for v in [
            schemars::schema_for!(WriteEdges),
            schemars::schema_for!(WriteNodes),
            schemars::schema_for!(Cypher),
        ] {
            assert_no_boolean_schema(&serde_json::to_value(v).unwrap(), "$");
        }
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
    fn compact_search_ranks_by_vector_and_expands_best() {
        use dr_strange_core::{PropDesc, PropValue, Properties};
        let db = Database::in_memory().unwrap();
        let plane = db.create_plane("v", Properties::new()).unwrap();
        let mut txn = plane.write().unwrap();
        for (key, vec) in [("near", vec![0.0, 0.1]), ("far", vec![1.0, 1.0])] {
            let mut props = Properties::new();
            props.insert(
                "embedding".into(),
                PropDesc::described("v", PropValue::Vector(vec)),
            );
            props.insert(
                "doc_comment".into(),
                PropDesc::described("d", PropValue::Str(format!("about {key}"))),
            );
            txn.create_node_with_key(key, &["Doc"], props).unwrap();
        }
        txn.commit().unwrap();
        let out = dr_strange_core::compact::search(&plane, &[0.0, 0.0], 2).unwrap();
        assert!(out.lines().next().unwrap().contains("near"), "{out}");
        assert!(out.contains("best match:"), "{out}");
        assert!(out.contains("about near"), "{out}");
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

#[cfg(test)]
mod grep_tests {
    use super::*;

    fn req(pattern: &str) -> GrepReq {
        GrepReq {
            pattern: pattern.into(),
            ignore_case: None,
            max_results: None,
        }
    }

    #[test]
    fn finds_lines_skips_build_dirs_and_binaries() {
        let dir = std::env::temp_dir().join(format!("drsg-grep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {\n    // needle here\n}\n").unwrap();
        std::fs::write(dir.join("target/b.rs"), "// needle in build output\n").unwrap();
        std::fs::write(dir.join("blob.bin"), b"nee\0dle").unwrap();

        let out = grep_tree(&dir, &req("needle")).unwrap();
        assert!(out.contains("src/a.rs:2:"), "{out}");
        assert!(!out.contains("target/"), "build dirs are skipped: {out}");
        assert!(!out.contains("blob.bin"), "binaries are skipped: {out}");

        let none = grep_tree(&dir, &req("absent-text")).unwrap();
        assert!(none.contains("no matches"));

        let folded = grep_tree(
            &dir,
            &GrepReq {
                pattern: "NEEDLE".into(),
                ignore_case: Some(true),
                max_results: Some(1),
            },
        )
        .unwrap();
        assert!(folded.contains("src/a.rs:2:"), "{folded}");

        assert!(
            grep_tree(&dir, &req("  ")).is_err(),
            "empty pattern refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod snippet_tests {
    use dr_strange_core::{PropDesc, PropValue, Properties};
    use serde_json::from_value;

    use super::*;

    /// Two trees, same relative path, different content — the shape that makes
    /// reading the wrong one silent rather than an error.
    fn two_repos(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("drsg-snippet-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("repo-a"), base.join("repo-b"));
        for (dir, body) in [
            (&a, "fn f() { \"from repo A\" }"),
            (&b, "fn f() { \"from repo B\" }"),
        ] {
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(dir.join("src/lib.rs"), format!("{body}\n")).unwrap();
        }
        (a, b)
    }

    /// A plane whose facts were parsed from `root`, holding one symbol at
    /// `src/lib.rs:1`. `root = None` leaves the plane without a sync point,
    /// as a hand-built plane has.
    fn plane_with(db: &Database, name: &str, root: Option<&std::path::Path>) {
        db.create_plane(name, Properties::new()).unwrap();
        write_nodes_logic(
            db,
            from_value(jval!({"plane": name, "nodes": [
                {"external_key": "f", "labels": ["Function"],
                 "properties": {"file": "src/lib.rs", "line": 1}}
            ]}))
            .unwrap(),
            None,
        )
        .unwrap();
        if let Some(root) = root {
            let plane = db.plane(name).unwrap();
            let mut props = plane.properties().unwrap();
            props.insert(
                "synced_root".into(),
                PropDesc::described(
                    "directory the facts were parsed from",
                    PropValue::Str(root.display().to_string()),
                ),
            );
            plane.set_properties(props).unwrap();
        }
    }

    fn snippet(db: &Database, fallback: Option<&std::path::Path>, plane: &str) -> String {
        snippet_logic(
            db,
            fallback,
            from_value(jval!({"plane": plane, "name": "f"})).unwrap(),
        )
        .unwrap()
        .as_str()
        .unwrap()
        .to_string()
    }

    /// The bug this guards: `snippet` resolved the *node* in the requested
    /// plane but read the *file* from whichever tree the process was started
    /// with. With a second code plane on the same server that returns another
    /// repository's file at the same relative path — plausible text, no error.
    /// So the assertion is on content, not on success.
    #[test]
    fn reads_the_planes_own_root_not_the_processs() {
        let (a, b) = two_repos("own-root");
        let db = Database::in_memory().unwrap();
        plane_with(&db, "repo-b", Some(&b));

        // The process is watching repo-a — the wrong tree for this plane.
        let out = snippet(&db, Some(&a), "repo-b");
        assert!(
            out.contains("from repo B"),
            "read the wrong repository's file: {out}"
        );
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
    }

    /// A plane with no recorded root still works off the attached tree: that is
    /// the plain `serve` + `[server] source_root` case, and every plane digested
    /// before `synced_root` existed.
    #[test]
    fn falls_back_to_the_attached_tree() {
        let (a, _b) = two_repos("fallback");
        let db = Database::in_memory().unwrap();
        plane_with(&db, "hand-built", None);

        let out = snippet(&db, Some(&a), "hand-built");
        assert!(out.contains("from repo A"), "{out}");

        // No plane root and no attached tree is an error, not a wrong answer.
        let db2 = Database::in_memory().unwrap();
        plane_with(&db2, "hand-built", None);
        assert!(
            snippet_logic(
                &db2,
                None,
                from_value(jval!({"plane": "hand-built", "name": "f"})).unwrap()
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
    }
}
