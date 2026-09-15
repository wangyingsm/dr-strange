//! The JSON-RPC method implementations (arch/08 §1). Each is a plain
//! synchronous `fn(&Ctx, params) -> Result<Value, RpcError>` that wraps the
//! core `Database` API and serializes through the core's `json` dialect — the
//! same structures the CLI and MCP emit, so all three surfaces agree on the
//! wire shape. Most methods are reads; `digest.run`/`digest.write` power the
//! digest page (arch/07), the latter writing through the bulk path.

use std::collections::HashSet;
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
use dr_strange_parser::{Connection, EdgeInfo, Expect, Kind, LabelInfo, Vocab};
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
    /// When this request's queries must stop. `None` ⇒ run to completion.
    pub deadline: Option<std::time::Instant>,
    /// How many queries the history keeps.
    pub history_limit: usize,
    /// The one provider the operator configured by name or URL (`[server]
    /// embed_provider`), if any. The only non-preset provider a request may
    /// name — see [`provider_for`].
    pub configured_provider: Option<&'a str>,
    /// How many commits back time-travel reaches (`[server] retain_commits`);
    /// `None` is unbounded. Reported by `db.stats` so a client can say how
    /// deep the history it offers goes — `plane.history` already spans no
    /// further than this, since the core's window starts at the retained
    /// floor.
    pub retain_commits: Option<u64>,
}

impl Ctx<'_> {
    /// Resolve a plane, carrying this request's deadline onto the handle.
    ///
    /// Every method goes through here rather than `ctx.db.plane` so a new
    /// method cannot ship unbounded by forgetting to apply it — the same
    /// reason `Access` is named at every dispatch arm.
    fn plane(&self, name: &str) -> Result<dr_strange_core::PlaneHandle<'_>, RpcError> {
        let p = app(self.db.plane(name))?;
        Ok(match self.deadline {
            Some(d) => p.with_deadline(d),
            None => p,
        })
    }
}

// ---- param decoding -------------------------------------------------------

fn params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|e| RpcError::invalid_params(e.to_string()))
}

/// Core errors are the caller's fault far more often than ours (unknown plane,
/// bad plan), so they ride the server-error code, not `-32603 internal`.
pub(crate) fn app<T>(r: dr_strange_core::Result<T>) -> Result<T, RpcError> {
    r.map_err(core_err)
}

/// The client-facing form of a core error. Client-fault variants (unknown
/// plane, bad plan, conflict) name the caller's own inputs and go through
/// verbatim; the storage-side ones carry an `io::Error` with the database's
/// path or a backend's internals, which the operator wants and the client
/// has no business seeing — those become an [`opaque`] reference.
pub(crate) fn core_err(e: dr_strange_core::Error) -> RpcError {
    use dr_strange_core::Error as E;
    match e {
        // The one core error a client should retry unchanged rather than treat
        // as its own fault: it never got the writer, so nothing was attempted.
        E::Timeout(_) => RpcError::timeout(e.to_string()),
        E::Io(_) | E::Backend(_) | E::Corrupt(_) => opaque("storage error", format!("{e:#}")),
        _ => RpcError::server(e.to_string()),
    }
}

/// Resolve the provider a request may use. A provider name is either one of
/// the llm crate's presets or exactly the one the operator configured; any
/// other string is a base URL this process would POST to from its own
/// network, on behalf of whoever holds a read credential — a server-side
/// request forgery (the audit's third finding). `build_provider` itself takes
/// a URL because the CLI at an operator's terminal legitimately means one;
/// the wire surface must never hand it one. Every call site that turns a
/// request field into a provider goes through here — the same
/// single-chokepoint reasoning as [`Ctx::plane`] and the `Access` at every
/// dispatch arm. `None` means the request named nothing and gets `openai`.
pub(crate) fn provider_for<'a>(
    ctx: &Ctx<'a>,
    requested: Option<&'a str>,
) -> Result<&'a str, RpcError> {
    let name = requested.unwrap_or("openai");
    if dr_strange_llm::is_preset(name) || ctx.configured_provider == Some(name) {
        return Ok(name);
    }
    Err(RpcError::invalid_params(format!(
        "provider must be a preset ({}) or the server's configured provider; \
         a base URL is not accepted over the wire",
        dr_strange_llm::PRESET_NAMES.join(", ")
    )))
}

/// An error whose text is for the operator, not the client: filesystem
/// paths, upstream bodies, backend internals. The detail goes to the log
/// under a reference the client message carries, so a user can quote the
/// message and the operator can find the line, and nothing about the
/// server's disk layout or its provider's reply crosses the wire.
pub(crate) fn opaque(kind: &str, detail: impl std::fmt::Display) -> RpcError {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // A counter rather than a random id: it only has to be unique within one
    // process's log, and it must not be guessable *into* anything.
    let reference = format!("{:06x}", NEXT.fetch_add(1, Ordering::Relaxed));
    tracing::warn!(reference = %reference, error = %detail, "{kind}");
    RpcError::server(format!("{kind} (ref {reference})"))
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
    let handle = ctx.plane(plane)?;
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
    ctx.plane(plane)
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
/// How a node reaches the dashboard: **lean**, always.
///
/// A vector property comes back as the marker `$vector(N dims, omitted)`
/// rather than N floats. Nothing in the dashboard draws an embedding — the
/// inspector shows a button, the plot a circle, the query table a summary
/// line — and a thousand floats per node is the difference between a
/// hundred-kilobyte answer and a hundred-megabyte one. The values themselves
/// are one call away: `node.get` with `lean: false` returns the node whole.
fn node_json(n: &NodeRecord) -> Value {
    json::node_to_json_lean(n)
}

/// What every `lean` flag defaults to, now that nothing wants the other
/// answer unasked.
fn lean() -> bool {
    true
}

/// An edge record as a JSON object — the counterpart to `json::node_to_json`
/// (which the core provides), kept here since the core's dialect has no edge
/// form yet. The plot merges these into its graph model alongside nodes.
fn edge_to_json(e: &EdgeRecord) -> Value {
    jval!({
        "id": e.id.0,
        "src": e.src.0,
        "dst": e.dst.0,
        "type": e.ty,
        // Lean, like the nodes it joins: an edge is far less likely to carry a
        // vector, and just as unlikely to want one inlined if it does.
        "properties": json::properties_to_json_lean(&e.properties),
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
            map.insert("record".into(), node_json(n));
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
                let mut obj = node_json(n);
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
    // Summary counters, maintained transactionally with every mutation —
    // a point lookup per plane, where the catalog would scan everything
    // (arch/03 §5). This is what keeps the dashboard at ms whatever the
    // graph grows to.
    let counters = app(ctx.db.counters())?;
    let commit_seq = app(ctx.db.commit_seq())?;
    // Declared vector + keyword indexes across every plane.
    let mut indexes = 0usize;
    for (_, name) in &planes {
        let plane = ctx.plane(name)?;
        indexes += plane.vector_indexes().len() + plane.keyword_indexes().len();
    }
    let file_size = ctx.db_path.map(on_disk_bytes);
    Ok(jval!({
        "planes": planes.len(),
        "nodes": counters.nodes,
        "edges": counters.edges,
        "labels": counters.labels.len(),
        "edge_types": counters.edge_types.len(),
        "indexes": indexes,
        "commit_seq": commit_seq,
        "persistent": ctx.db_path.is_some(),
        "file_size": file_size,
        "rss_bytes": resident_bytes(),
        "plugin_bytes": dr_strange_llm::plugin_memory_bytes(),
        // The time-travel depth the operator keeps; null when every version
        // is retained. What the dashboard's slider can reach is bounded by
        // it, and this is how the dashboard says so.
        "retain_commits": ctx.retain_commits,
    }))
}

/// The process's resident set, in bytes — what the whole server holds right
/// now, plugins and page cache of the database included. Read the way each
/// platform reports it: Linux from `/proc/self/status`, macOS from
/// `proc_pidinfo`, Windows from `GetProcessMemoryInfo` (the working set,
/// which is the figure Task Manager shows). Anywhere else it is `None`,
/// which the dashboard shows as a dash rather than a zero that would read as
/// a measurement.
fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `proc_pidinfo` writes at most `size` bytes into `info`, a
        // plain C struct of exactly that size, and returns how many it wrote;
        // anything short of the whole struct is treated as no answer.
        let got = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast::<libc::c_void>(),
                size,
            )
        };
        (got == size).then_some(info.pti_resident_size)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        counters.cb = cb;
        // SAFETY: the current process's pseudo-handle needs no closing, and
        // `counters` is a plain C struct whose `cb` states its size, which is
        // all `GetProcessMemoryInfo` reads before writing into it.
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb) };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

/// Bytes the database occupies on disk. The native backend's "path" is a
/// directory — `fs::metadata` on one reports the inode (4 KB, famously
/// wrong on the dashboard) — so a directory is walked and its files summed.
/// The vector and keyword sidecars sit *beside* the path and count too.
fn on_disk_bytes(path: &std::path::Path) -> u64 {
    fn du(p: &std::path::Path) -> u64 {
        let Ok(meta) = std::fs::metadata(p) else {
            return 0;
        };
        if meta.is_file() {
            return meta.len();
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            return 0;
        };
        entries.flatten().map(|e| du(&e.path())).sum()
    }
    let mut total = du(path);
    for sidecar in ["hnsw", "bm25"] {
        let mut os = path.as_os_str().to_owned();
        os.push(".");
        os.push(sidecar);
        total += du(std::path::Path::new(&os));
    }
    total
}

/// `plugin.list` — the installed preprocessor plugins, exactly the records
/// `drsg plugin list --json` prints: one shape for agents whichever surface
/// they read (arch/07, ROADMAP §11).
pub fn plugin_list(_ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let store = plugin_store()?;
    let plugins = store.list().map_err(plug)?;
    serde_json::to_value(plugins).map_err(|e| RpcError::server(e.to_string()))
}

/// How long the dashboard's copy of the catalog may be reused before the
/// server asks GitHub again.
///
/// The panel refreshes on every page load; the file behind it changes when
/// someone cuts a plugin release. An hour is far below the first rate and far
/// above the second, and the on-disk copy is shared with the CLI, so a
/// `drsg plugin install` a moment ago already warmed it.
const CATALOG_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// `plugin.catalog` — the official plugins, read from the extensions
/// repository's `catalog.json` rather than compiled into this binary, so a
/// plugin release needs no drsg release (ROADMAP §11).
///
/// Each entry carries the version it was cut from, the artifact's SHA-256, and
/// what it claims; entries this build cannot run are returned too, tagged with
/// why, rather than filtered out. The dashboard joins the list against
/// `plugin.list` to mark each one installed/upgradable/absent.
///
/// The answer says where it came from. `stale: true` means the fetch failed
/// and this is the last copy the store kept — a catalog is only worth trusting
/// as far as it says how current it is.
pub fn plugin_catalog(_ctx: &Ctx<'_>) -> Result<Value, RpcError> {
    let store = plugin_store()?;

    // Whatever is on disk answers *now*, and the refresh happens behind the
    // response. A panel must not sit on a request to GitHub: on a network
    // where that host is slow or unreachable, a blocking fetch would stall
    // the Extensions panel for the fetch timeout, once per TTL, to end up
    // showing this same copy anyway.
    if let Some((catalog, age)) = dr_strange_llm::cached_catalog(&store) {
        let stale = age > CATALOG_TTL;
        if stale {
            refresh_catalog_once(|| {
                let Ok(store) = dr_strange_llm::PluginStore::open_default() else {
                    return;
                };
                dr_strange_llm::refresh_cache(&store, |url| {
                    crate::fetch::fetch_bytes(url, dr_strange_llm::CATALOG_DOWNLOAD_CAP, &[])
                });
            });
        }
        return catalog_value(&catalog, stale, age_note(age, stale));
    }

    // Nothing cached: this one request has to wait, because there is nothing
    // else to show. No private-range allowances — a server following a
    // redirect into its own network is exactly what the guard is for, and the
    // catalog lives on the public internet.
    let fetched = dr_strange_llm::load_catalog_within(&store, CATALOG_TTL, |url| {
        crate::fetch::fetch_bytes(url, dr_strange_llm::CATALOG_DOWNLOAD_CAP, &[])
    })
    .map_err(plug)?;
    let stale = fetched.source.is_stale();
    let source = serde_json::to_value(&fetched.source).unwrap_or(Value::Null);
    catalog_value(&fetched.catalog, stale, source)
}

/// Set while one background catalog refresh is running (see
/// [`refresh_catalog_once`]).
static CATALOG_REFRESHING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Run `refresh` in the background unless a refresh is already running;
/// returns whether this call started one.
///
/// Single-flight, because a stale catalog is stale for *every* request until
/// the refresh lands: a dashboard polling the Extensions panel, or several
/// tabs opening it at once, would otherwise start one fetch per request, all
/// racing GitHub for the same bytes and all rewriting the same cache file.
/// The work goes on the runtime's blocking pool when there is one — it is a
/// synchronous HTTP fetch, and the pool is where the server puts every other
/// blocking unit of work, bounded with them — and on a plain thread only when
/// no runtime is present (an embedding caller running the handler directly).
/// The flag is cleared by a guard so a panicking fetch cannot wedge refreshes
/// off for the life of the process.
fn refresh_catalog_once(refresh: impl FnOnce() + Send + 'static) -> bool {
    use std::sync::atomic::Ordering;
    if CATALOG_REFRESHING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return false;
    }
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            CATALOG_REFRESHING.store(false, Ordering::Release);
        }
    }
    let work = move || {
        let _clear = Clear;
        refresh();
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(work);
        }
        Err(_) => {
            std::thread::spawn(work);
        }
    }
    true
}

/// Where a cached answer came from, in the shape `Source` serializes to.
fn age_note(age: std::time::Duration, stale: bool) -> Value {
    serde_json::json!({
        "source": "cache",
        "age": {"secs": age.as_secs(), "nanos": age.subsec_nanos()},
        "why": if stale { Some("older than the server's cache window — a refresh is running") } else { None },
    })
}

fn catalog_value(
    catalog: &dr_strange_llm::Catalog,
    stale: bool,
    source: Value,
) -> Result<Value, RpcError> {
    serde_json::to_value(serde_json::json!({
        "source": source,
        "stale": stale,
        "schema": catalog.schema,
        "plugins": catalog.current(),
    }))
    .map_err(|e| RpcError::server(e.to_string()))
}

#[derive(Deserialize)]
pub struct PlaneVectorize {
    plane: String,
    /// Embedding provider preset or base URL; the key comes from the
    /// server's environment, mirroring `digest.run`.
    #[serde(default)]
    embed: Option<String>,
    #[serde(default)]
    embed_model: Option<String>,
    /// `cosine` (default), `dot`, or `l2`.
    #[serde(default)]
    metric: Option<String>,
}

/// `plane.vectorize` — embed every node in a plane and ensure its vector
/// indexes: the dashboard's per-plane button, same engine as `drsg vec`.
/// Incremental by meaning, so pressing it twice costs one pass.
pub fn plane_vectorize(_ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PlaneVectorize = params(p)?;
    let metric = match req.metric.as_deref() {
        None | Some("cosine") => dr_strange_core::Metric::Cosine,
        Some("dot") => dr_strange_core::Metric::Dot,
        Some("l2") => dr_strange_core::Metric::L2,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "unknown metric `{other}` — cosine, dot or l2"
            )));
        }
    };
    let embedder = dr_strange_llm::build_provider(
        provider_for(_ctx, req.embed.as_deref())?,
        req.embed_model.as_deref(),
        None,
        None,
        true,
    )
    .map_err(build_err)?;
    let stats =
        dr_strange_llm::vectorize_plane(_ctx.db, &req.plane, &embedder, metric).map_err(llm_err)?;
    serde_json::to_value(stats).map_err(|e| RpcError::server(e.to_string()))
}

#[derive(Deserialize)]
pub struct PluginInstall {
    /// A URL to download the component from. Server-local file paths are
    /// deliberately not accepted over RPC: a remote caller naming paths on
    /// the server's disk is a probe, not a workflow.
    url: String,
}

/// `plugin.install` — download, validate, hash-pin and store a plugin.
/// Write-gated; the URL passes the same resolved-address network policy as
/// every other fetch (public addresses only).
pub fn plugin_install(_ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PluginInstall = params(p)?;
    if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
        return Err(RpcError::invalid_params(
            "plugin.install takes an http(s) URL".to_string(),
        ));
    }
    const CAP: usize = 256 << 20;
    let bytes = crate::fetch::fetch_bytes(&req.url, CAP, &[])
        .map_err(|e| opaque("plugin download failed", format!("{e:#}")))?;
    let store = plugin_store()?;
    let (entry, replaced) = store.install(&bytes, &req.url).map_err(plug)?;
    Ok(jval!({ "installed": entry, "replaced": replaced }))
}

#[derive(Deserialize)]
pub struct PluginRemove {
    name: String,
}

/// `plugin.remove` — uninstall by name. Write-gated.
pub fn plugin_remove(_ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PluginRemove = params(p)?;
    let store = plugin_store()?;
    let entry = store.remove(&req.name).map_err(plug)?;
    Ok(jval!({ "removed": entry }))
}

fn plugin_store() -> Result<dr_strange_llm::PluginStore, RpcError> {
    dr_strange_llm::PluginStore::open_default().map_err(plug)
}

/// Plugin-store errors name the store directory and the files in it.
fn plug(e: anyhow::Error) -> RpcError {
    opaque("plugin store error", format!("{e:#}"))
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
        let plane = ctx.plane(&name)?;
        let counters = app(plane.counters())?;
        let props = app(plane.properties())?;
        out.push(jval!({
            "id": id.0,
            "name": name,
            "nodes": counters.nodes,
            "edges": counters.edges,
            "properties": json::properties_to_json(&props),
        }));
    }
    Ok(Value::Array(out))
}

#[derive(Deserialize)]
pub struct PlaneOnly {
    plane: String,
}

/// The vocabulary a completion ranks by: this plane's catalog, read as the
/// labels and edge types a query may name.
///
/// The catalog is the same soft schema `plane.catalog` serves — computed by
/// scanning the plane, which is why the caller caches what comes back rather
/// than asking on every keystroke. Properties are ordered by how many of the
/// label's nodes carry them, so the commonest is offered first; the catalog
/// itself is alphabetical, which is nobody's idea of a ranking.
pub fn plane_vocab(ctx: &Ctx<'_>, plane: &str) -> Result<Vocab, RpcError> {
    let cat = app(ctx.plane(plane)?.catalog())?;
    Ok(Vocab {
        labels: cat
            .labels
            .iter()
            .map(|(name, stats)| {
                let mut properties: Vec<(&String, u64)> =
                    stats.properties.iter().map(|(p, s)| (p, s.count)).collect();
                properties
                    .sort_by_key(|(name, count)| (std::cmp::Reverse(*count), (*name).clone()));
                LabelInfo {
                    name: name.clone(),
                    count: stats.count,
                    properties: properties.into_iter().map(|(p, _)| p.clone()).collect(),
                }
            })
            .collect(),
        edges: cat
            .edge_types
            .iter()
            .map(|(name, stats)| EdgeInfo {
                name: name.clone(),
                count: stats.count,
                connections: stats
                    .connections
                    .iter()
                    .map(|c| Connection {
                        src: c.src_label.clone(),
                        dst: c.dst_label.clone(),
                        count: c.count,
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// What may follow `prefix`, as JSON: the best guess, every candidate, and
/// what the caret sits at — said in words, since a caller showing the list
/// wants a heading for it and should not have to know the grammar to write
/// one.
pub fn completion(prefix: &str, vocab: &Vocab) -> Value {
    let c = dr_strange_parser::complete(prefix, vocab);
    jval!({
        "best": c.best,
        "word": c.word,
        "expects": expects_tag(&c.expects),
        "about": expects_about(&c.expects),
        "suggestions": c.suggestions.iter().map(|s| jval!({
            "text": s.text,
            "insert": s.insert,
            "detail": s.detail,
            "kind": kind_tag(s.kind),
        })).collect::<Vec<_>>(),
    })
}

/// What the caret sits at, as a stable tag a caller can branch on.
fn expects_tag(e: &Expect) -> &'static str {
    match e {
        Expect::Statement => "statement",
        Expect::Node { .. } => "node",
        Expect::NodeVar { .. } => "node-var",
        Expect::Label => "label",
        Expect::NodeEnd => "node-end",
        Expect::Hop { .. } => "hop",
        Expect::EdgeType { .. } => "edge-type",
        Expect::RelEnd { .. } => "rel-end",
        Expect::Arrow { .. } => "arrow",
        Expect::Predicate { .. } => "predicate",
        Expect::Projection { .. } => "projection",
        Expect::Property { .. } => "property",
        Expect::Value => "value",
        Expect::SortKey { .. } => "sort-key",
        Expect::Argument { .. } => "argument",
        Expect::Clause { .. } => "clause",
        Expect::Nothing => "nothing",
    }
}

/// The same, in words, with the context that made the guess possible — what a
/// list of candidates should be headed with.
fn expects_about(e: &Expect) -> String {
    match e {
        Expect::Statement => "how the query starts".into(),
        Expect::Node { .. } => "a node to match".into(),
        Expect::NodeVar { .. } => "a name for this node".into(),
        Expect::Label | Expect::NodeEnd => "which label".into(),
        // Both directions are offered here, so neither verb would do.
        Expect::Hop { from: Some(l), .. } => format!("what {} connects to", a(l)),
        Expect::Hop { from: None, .. } => "what this node connects to".into(),
        Expect::EdgeType {
            from: Some(l),
            incoming,
            ..
        } => format!(
            "what {} {}",
            if *incoming { "reaches" } else { "leaves" },
            a(l)
        ),
        Expect::EdgeType { from: None, .. } => "which edge type".into(),
        Expect::RelEnd { ranged: true, .. } => "how many hops".into(),
        Expect::RelEnd { .. } | Expect::Arrow { .. } => "closing the relationship".into(),
        Expect::Predicate { .. } => "what to filter on".into(),
        Expect::Projection { .. } => "what to return".into(),
        Expect::Property {
            var,
            label: Some(l),
        } => format!("properties of {var}, {}", a(l)),
        Expect::Property { var, label: None } => format!("properties of {var}"),
        Expect::Value => "a value only you know".into(),
        Expect::SortKey { .. } => "what to sort on".into(),
        Expect::Argument { .. } => "what the call takes".into(),
        Expect::Clause { .. } => "what comes after".into(),
        Expect::Nothing => "inside a string".into(),
    }
}

/// A label with its article: `a Function`, `an UnresolvedRef`. By the sound
/// of the first letter, which for a type name is near enough its spelling.
fn a(label: &str) -> String {
    let vowel = label
        .chars()
        .next()
        .is_some_and(|c| "aeiouAEIOU".contains(c));
    format!("{} {label}", if vowel { "an" } else { "a" })
}

fn kind_tag(k: Kind) -> &'static str {
    match k {
        Kind::Keyword => "keyword",
        Kind::Label => "label",
        Kind::EdgeType => "edge-type",
        Kind::Property => "property",
        Kind::Variable => "variable",
        Kind::Snippet => "snippet",
    }
}

/// `plane.catalog` — one plane's soft schema (labels, property descriptions,
/// edge-type connectivity, counts).
pub fn plane_catalog(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: PlaneOnly = params(p)?;
    let cat = app(ctx.plane(&req.plane)?.catalog())?;
    serde_json::to_value(cat).map_err(|e| RpcError::server(e.to_string()))
}

#[derive(Deserialize)]
pub struct GetNode {
    plane: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    key: Option<String>,
    /// Vector properties as markers rather than floats — **the default**, and
    /// what every other read returns. `lean: false` is the one way to ask for
    /// an embedding itself: the dashboard's "show vector" button is this call.
    #[serde(default = "lean")]
    lean: bool,
}

/// `node.get` — one node by id or external key; `null` if absent.
pub fn node_get(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: GetNode = params(p)?;
    let plane = ctx.plane(&req.plane)?;
    let node = match (req.id, &req.key) {
        (Some(id), _) => app(plane.node(NodeId(id)))?,
        (None, Some(key)) => app(plane.node_by_key(key))?,
        (None, None) => return Err(RpcError::invalid_params("provide `id` or `key`")),
    };
    let render = if req.lean {
        json::node_to_json_lean
    } else {
        json::node_to_json
    };
    Ok(node.map(|n| render(&n)).unwrap_or(Value::Null))
}

#[derive(Deserialize)]
pub struct Neighbors {
    plane: String,
    id: u64,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default, rename = "type")]
    edge_type: Option<String>,
    /// Return full records instead of id pairs: each hop becomes
    /// `{node: {…}, edge: {id, type, properties}}` — the composition that
    /// lets `node.get` + `neighbors` answer "who calls this?" in two calls,
    /// call-site lines included, with no per-id follow-ups. Off by default:
    /// existing callers see the exact id pairs they always did.
    #[serde(default)]
    hydrate: bool,
    /// With `hydrate`, vector properties as markers rather than floats —
    /// the default; `lean: false` returns the embeddings themselves.
    #[serde(default = "lean")]
    lean: bool,
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
    if !req.hydrate {
        return Ok(Value::Array(
            hops.iter()
                .map(|n| jval!({ "node": n.node.0, "edge": n.edge.0 }))
                .collect(),
        ));
    }
    let render = if req.lean {
        json::node_to_json_lean
    } else {
        json::node_to_json
    };
    let mut out = Vec::with_capacity(hops.len());
    for n in &hops {
        let node = match app(plane.node(n.node))? {
            Some(record) => render(&record),
            None => jval!({ "id": n.node.0 }),
        };
        let edge = match app(plane.edge(n.edge))? {
            Some(e) => jval!({
                "id": e.id.0,
                "type": e.ty,
                "properties": if req.lean {
                    json::properties_to_json_lean(&e.properties)
                } else {
                    json::properties_to_json(&e.properties)
                },
            }),
            None => jval!({ "id": n.edge.0 }),
        };
        out.push(jval!({ "node": node, "edge": edge }));
    }
    Ok(Value::Array(out))
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
    let plane = ctx.plane(&req.plane)?;
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
    read_result(plane_at(ctx, &req.plane, &req.at)?.query_from_plan(plan))
}

/// Render what a read query returns: a table when its plan projects, its
/// scored nodes otherwise.
fn read_result(q: dr_strange_core::QueryBuilder<'_>) -> Result<Value, RpcError> {
    match q.plan().project.is_some() {
        true => Ok(json::table_to_json_lean(&app(q.table())?)),
        false => Ok(scored_rows(&app(q.scored_nodes())?)),
    }
}

/// Adapts an LLM provider to the parser's `Embedder` seam, so a
/// `SEARCH … NEAR "text"` embeds the text server-side (key from the server
/// environment, never the client) before the top-k runs.
struct LlmEmbedder(Box<dyn Embedder>);
impl dr_strange_parser::Embedder for LlmEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        // The parser folds this text into its own error, which reaches the
        // client verbatim: keep the upstream body out of it.
        let reply = self
            .0
            .embed(&[text.to_string()])
            .map_err(|e| opaque("embedding failed", format!("{e:#}")).message)?;
        reply
            .vectors
            .into_iter()
            .next()
            .ok_or_else(|| "embedder returned no vector".to_string())
    }
}

/// Build an embedder from a request's provider name (`None` if it can't be
/// configured — e.g. the provider has no embedding model; a text SEARCH then
/// errors clearly, while MATCH / literal-vector queries still work). A name
/// that is neither a preset nor the configured provider is an error, not a
/// `None`: silently running the query without it would hide the refusal.
fn make_embedder(ctx: &Ctx<'_>, provider: Option<&str>) -> Result<Option<LlmEmbedder>, RpcError> {
    let provider = provider_for(ctx, provider)?;
    Ok(
        dr_strange_llm::build_provider(provider, None, None, None, true)
            .ok()
            .map(|p| LlmEmbedder(Box::new(p))),
    )
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
    /// Vector properties as markers rather than floats — the default;
    /// `lean: false` returns the embeddings themselves.
    #[serde(default = "lean")]
    lean: bool,
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
        req.embed.as_deref(),
        &params,
        req.lean,
        // An SDK caller asked for a query, not for a screenful of it.
        Page::all(),
    )
}

/// Notes a query that ran, so it can be run again.
///
/// **Best-effort, and deliberately so.** Recording is a write, and a read
/// query that could not be written down still ran: failing it because the note
/// did not land would be answering the wrong question. A failure is logged and
/// forgotten.
fn remember(ctx: &Ctx<'_>, plane: &str, query: &str) {
    if let Err(e) = ctx.db.record_query(plane, query, ctx.history_limit) {
        tracing::debug!(plane = %plane, error = %e, "could not record the query");
    }
}

/// The queries this database has run, newest first.
pub fn query_history(ctx: &Ctx<'_>, limit: usize) -> Result<Value, RpcError> {
    let rows = app(ctx.db.query_history(limit))?;
    Ok(Value::Array(rows.iter().map(history_json).collect()))
}

/// One recorded query, or `null` when it has been purged or never was.
pub fn recorded_query(ctx: &Ctx<'_>, id: u64) -> Result<Option<Value>, RpcError> {
    Ok(app(ctx.db.recorded_query(id))?.as_ref().map(history_json))
}

fn history_json(r: &dr_strange_core::QueryRecord) -> Value {
    jval!({ "id": r.id, "at": r.at, "plane": r.plane, "query": r.query })
}

/// One page of a result: where to start, and how many rows to carry.
///
/// A query says how many rows there are; a reader looks at a screenful. The
/// two were the same number until a pattern over a real codebase answered with
/// four thousand functions and every one of them carrying its own source —
/// eleven megabytes to ship, parse and lay out, for a table nobody scrolls to
/// the end of.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page {
    pub offset: usize,
    /// `None` takes everything from `offset` on, which is what a caller that
    /// asked for no page gets.
    pub limit: Option<usize>,
}

impl Page {
    /// Everything, from the top — what a caller that never asked for a page
    /// has always received.
    pub fn all() -> Self {
        Self::default()
    }

    fn of<T>(self, rows: Vec<T>) -> Vec<T> {
        let mut rows = rows;
        if self.offset > 0 {
            rows.drain(..self.offset.min(rows.len()));
        }
        if let Some(limit) = self.limit {
            rows.truncate(limit);
        }
        rows
    }
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
    embed_provider: Option<&str>,
    params: &dr_strange_parser::Params,
    lean: bool,
    page: Page,
) -> Result<Value, RpcError> {
    let embedder = make_embedder(ctx, embed_provider)?;
    let stmt = dr_strange_parser::parse_statement_full(
        query,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_parser::Embedder),
        params,
    )
    .map_err(|e| RpcError::invalid_params(e.to_string()))?;

    let plane = ctx.plane(plane_name)?;

    // A write statement mutates the plane and returns its change-counts; the UI
    // shows a status rather than a subgraph.
    let (plane, plan) = match stmt {
        dr_strange_parser::Statement::Read(read) => (pin(plane, read.as_of)?, read.plan),
        dr_strange_parser::Statement::Write(w) => {
            let s = w.apply(&plane).map_err(RpcError::server)?;
            remember(ctx, plane_name, query);
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

    // A projecting query has no induced subgraph to plot.
    let q = plane.query_from_plan(plan);
    if q.plan().project.is_some() {
        let table = app(q.table())?;
        remember(ctx, plane_name, query);
        let total = table.rows.len();
        // A projected column is as able to be an embedding as a property is:
        // `RETURN m.embedding` asks for one by name.
        let mut out = if lean {
            json::table_to_json_lean(&table)
        } else {
            json::table_to_json(&table)
        };
        if let Value::Object(map) = &mut out {
            let rows = map.get_mut("rows").and_then(Value::as_array_mut);
            if let Some(rows) = rows {
                *rows = page.of(std::mem::take(rows));
            }
            map.insert("total".into(), jval!(total));
            map.insert("offset".into(), jval!(page.offset));
        }
        return Ok(out);
    }
    // A subgraph has each node once. A pattern reaches the same function
    // through every call that lands on it — two thousand matches over nine
    // hundred functions — and the answer to `RETURN m` as a *subgraph* is the
    // functions, in the order the query first reached them. (A caller who
    // wants the matches themselves projects: `RETURN key(m)` is a table, and
    // a table has rows.)
    //
    // Reported distinct all along — `count` was the size of this set — while
    // the list beside it carried the duplicates. On a keyed table that is not
    // untidy but fatal: repeated keys abort the render, and the page sits on
    // "Running…" with no result and no error.
    let mut seen = std::collections::BTreeSet::new();
    let ran = app(q.scored_nodes())?;
    remember(ctx, plane_name, query);
    let all: Vec<_> = ran
        .into_iter()
        .filter(|(n, _)| seen.insert(n.id.0))
        .collect();
    let total = all.len();
    let rows = page.of(all);

    let set: std::collections::BTreeSet<u64> = rows.iter().map(|(n, _)| n.id.0).collect();
    let nodes: Vec<Value> = rows
        .iter()
        .map(|(n, s)| {
            let mut obj = if lean {
                json::node_to_json_lean(n)
            } else {
                json::node_to_json(n)
            };
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

    Ok(jval!({
        "nodes": nodes,
        "edges": edges,
        "count": set.len(),
        "total": total,
        "offset": page.offset,
    }))
}

// ---- graph-plot subgraph methods (chunk 2, arch/08 §2.2) ------------------

/// The default node cap for a seeded view — the plot never asks the core for
/// an unbounded dump (arch/08 §2.2, "cursors throughout").
const SEED_LIMIT: u64 = 200;
/// The default fan-out cap for one click-to-expand (hub-safe expansion).
const EXPAND_LIMIT: u64 = 100;
/// Text search stops after examining this many nodes in its node pass, and
/// again after visiting this many nodes (or examining this many edges) in
/// its edge pass — there is no text index, so `plane.find` is a linear scan;
/// the caps keep a huge plane responsive.
pub(crate) const FIND_SCAN_CAP: usize = 20_000;
/// Default number of matches `plane.find` returns.
const FIND_LIMIT: usize = 50;
/// How many node *records* one page of a linear scan loads. `plane.find` and
/// a degree-ordered `graph.seed` walk the plane a page at a time and stop
/// the moment they have what they came for, so a small plane costs one page
/// of records and a hit early in a huge one costs one page — never every
/// record of the plane. What each page still costs is the core's id scan:
/// the executor collects the plane's node ids (8 bytes each) before its
/// skip/limit steps apply (core `compute/exec.rs` `source_rows`), so a page
/// is O(plane) in ids and O(page) in records, and a walk to the cap is at
/// most `cap / SCAN_PAGE` such id scans. A resumable id cursor in the core
/// would remove that term; until then the caps are what bound a keystroke.
pub(crate) const SCAN_PAGE: u64 = 2_000;
/// A degree-ordered seed measures the degree of at most this many nodes. The
/// measurement is a neighbour lookup per node, so on a plane of millions it
/// would otherwise be the most expensive thing a header click can trigger.
const SEED_SCAN_CAP: usize = 20_000;

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

    // `total` comes from the transactional counters (arch/03 §5) — a point
    // read — not from materialising every id of the plane on each seed.
    let counters = app(plane.counters())?;
    let total = match &req.label {
        Some(label) => counters.labels.get(label).copied().unwrap_or(0),
        None => counters.nodes,
    } as usize;
    let scan = || match &req.label {
        Some(label) => plane.query().scan_label(label.clone()),
        None => plane.query().scan_all(),
    };

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
        Some("degree") => Some(top_by_degree(&plane, scan, limit as usize)?),
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
        // Scan order: ask for `limit` ids and no more — the executor stops
        // at the limit instead of this handler discarding the rest.
        None => (app(scan().limit(limit).ids())?, None),
    };
    let set: std::collections::BTreeSet<u64> = ids.iter().map(|n| n.0).collect();

    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(node) = app(plane.node(*id))? {
            nodes.push(node_json(&node));
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

/// The `limit` highest-degree nodes among the first [`SEED_SCAN_CAP`] the
/// scan reaches, descending by degree and ascending by id within a degree so
/// a re-seed is reproducible.
///
/// The core keeps no per-node degree, so degree is a neighbour lookup per
/// node; what this bounds is everything around it. Ids arrive a page at a
/// time (each page an id scan in the core, see [`SCAN_PAGE`]; never every
/// record), the scan ends at the cap, and the ranking is a bounded min-heap
/// of `limit` entries rather than a sort of every node — so the cost is
/// `cap` lookups, at most `cap / SCAN_PAGE` id scans, and `limit` memory,
/// whatever the plane's size.
fn top_by_degree<'db>(
    plane: &PlaneHandle<'db>,
    scan: impl Fn() -> dr_strange_core::QueryBuilder<'db>,
    limit: usize,
) -> Result<Vec<(NodeId, f64)>, RpcError> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    // Ordered so the heap's top is the *weakest* candidate: lowest degree,
    // and among equals the highest id (which the final order puts last).
    let mut heap: BinaryHeap<Reverse<(usize, Reverse<u64>)>> = BinaryHeap::with_capacity(limit + 1);
    let mut examined = 0usize;
    let mut skip = 0u64;
    'scan: loop {
        let page = app(scan().skip(skip).limit(SCAN_PAGE).ids())?;
        let short = (page.len() as u64) < SCAN_PAGE;
        for id in page {
            if examined >= SEED_SCAN_CAP {
                break 'scan;
            }
            examined += 1;
            let d = app(plane.neighbors(id, Dir::Both, None))?.len();
            let entry = Reverse((d, Reverse(id.0)));
            if heap.len() < limit {
                heap.push(entry);
            } else if let Some(weakest) = heap.peek()
                && entry < *weakest
            {
                heap.pop();
                heap.push(entry);
            }
        }
        if short {
            break;
        }
        skip += SCAN_PAGE;
    }
    let mut rows: Vec<(NodeId, f64)> = heap
        .into_iter()
        .map(|Reverse((d, Reverse(id)))| (NodeId(id), d as f64))
        .collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0)));
    Ok(rows)
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
/// linear scan capped at [`FIND_SCAN_CAP`] nodes examined (node pass), nodes
/// visited and edges examined (edge pass), and [`limit`] results each;
/// `truncated` says whether any cap cut the results short.
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
        let provider = provider_for(ctx, req.provider.as_deref())?;
        match semantic_find(&plane, &req, provider, limit) {
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
            // The note is shown in the dashboard, so it gets the same
            // operator/client split as an error would.
            Err(e) => {
                let why = opaque("semantic search unavailable", format!("{e:#}")).message;
                note = Some(format!("{why} — showing text matches"));
            }
        }
    }

    let needle = req.q.trim().to_lowercase();
    // `total` is the counters' figure (a point read), not the length of a
    // vector holding every node — the scan below never builds one.
    let total = app(plane.counters())?.nodes as usize;

    // ---- nodes ----
    // A page at a time, stopping at `limit` hits or [`FIND_SCAN_CAP`] nodes.
    // This runs on every header keystroke: a match near the front of the
    // plane costs one page of records, and a miss costs the cap — never
    // every record of the plane (each page's id scan is the core's cost,
    // see [`SCAN_PAGE`]).
    let mut node_hits = Vec::new();
    let mut examined = 0usize;
    // Ids the node pass loaded, kept so the edge pass below need not read
    // the same records twice; past this prefix it fetches ids alone.
    let mut walked: Vec<NodeId> = Vec::new();
    let mut skip = 0u64;
    // True only once a page came back short with every node on it walked;
    // an early break (limit reached, cap hit) leaves it false so the edge
    // pass knows the remainder of that page is still unvisited.
    let mut exhausted = false;
    'nodes: loop {
        let page = app(plane.query().scan_all().skip(skip).limit(SCAN_PAGE).nodes())?;
        let short = (page.len() as u64) < SCAN_PAGE;
        for n in &page {
            if examined >= FIND_SCAN_CAP {
                break 'nodes;
            }
            examined += 1;
            walked.push(n.id);
            if let Some(hint) = match_node(n, &needle) {
                let mut obj = node_json(n);
                if let Value::Object(map) = &mut obj {
                    map.insert("match".into(), Value::String(hint));
                }
                node_hits.push(obj);
                if node_hits.len() >= limit {
                    break 'nodes;
                }
            }
        }
        if short {
            exhausted = true;
            break;
        }
        skip += SCAN_PAGE;
    }
    let nodes_truncated = examined < total;

    // ---- edges ----
    // The core has no edge scan, so walk each node's outgoing hops (as
    // `graph.seed` does), dedup by edge id, and match the edge record. The
    // walk covers the nodes the pass above loaded first, then continues from
    // where it stopped with ids alone, until `limit` edge hits or the cap.
    let mut edge_hits = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut edges_examined = 0usize;
    let mut edges_truncated = false;
    let mut sources = walked;
    let mut next_skip = sources.len() as u64;
    // Nodes whose out-edges the walk has looked up. A node with no out-edges
    // examines no edge, so without this second cap a needle matching no edge
    // on a plane of mostly leaves would look up the neighbours of every node
    // — the whole plane per keystroke, which is what the cap exists to
    // prevent.
    let mut sources_visited = 0usize;
    'walk: loop {
        for n in &sources {
            if sources_visited >= FIND_SCAN_CAP {
                // Truncated only if the plane holds nodes the walk never
                // reached; a cap met exactly at the end missed nothing.
                edges_truncated = sources_visited < total;
                break 'walk;
            }
            sources_visited += 1;
            for hop in app(plane.neighbors(*n, Dir::Out, None))? {
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
        if exhausted {
            break;
        }
        sources = app(plane
            .query()
            .scan_all()
            .skip(next_skip)
            .limit(SCAN_PAGE)
            .ids())?;
        exhausted = (sources.len() as u64) < SCAN_PAGE;
        next_skip += sources.len() as u64;
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;
    let mut builder = plane.hybrid();
    if let Some(label) = &req.label {
        builder = builder.label(label.clone());
    }
    if let Some(prop) = &req.vector_prop {
        let provider = provider_for(ctx, req.provider.as_deref())?;
        let embedder =
            dr_strange_llm::build_provider(provider, req.embed_model.as_deref(), None, None, true)
                .map_err(build_err)?;
        let reply = embedder
            .embed(std::slice::from_ref(&req.q))
            .map_err(llm_err)?;
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
            Some(node) => node_json(&node),
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
    /// Total model attempts including repairs — default and ceiling
    /// `ASK_DEFAULT_ATTEMPTS` (20): each attempt is a chat call on the
    /// server's key, so a request cannot ask for more than the default.
    #[serde(default)]
    max_attempts: Option<u32>,
    /// Safety row cap appended when the plan declares none (default
    /// `ASK_DEFAULT_LIMIT`, 100; at most `ASK_MAX_LIMIT`, 1000).
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

/// The attempt and row budgets a `plane.ask` actually gets: the llm crate's
/// defaults when the request is silent, and never more than its ceilings —
/// `ASK_DEFAULT_ATTEMPTS` doubles as the attempt ceiling because every
/// attempt is a chat call on the server's key.
pub(crate) fn ask_knobs(max_attempts: Option<u32>, limit: Option<u64>) -> (u32, u64) {
    use dr_strange_llm::{ASK_DEFAULT_ATTEMPTS, ASK_DEFAULT_LIMIT, ASK_MAX_LIMIT};
    (
        max_attempts
            .unwrap_or(ASK_DEFAULT_ATTEMPTS)
            .min(ASK_DEFAULT_ATTEMPTS),
        limit.unwrap_or(ASK_DEFAULT_LIMIT).min(ASK_MAX_LIMIT),
    )
}

/// `plane.ask` — natural-language query (ROADMAP §3): an LLM turns `question`
/// into a read-only LogicalPlan, which is run (unless `dry_run`). With
/// `embed_provider` the model can call embedding tools to ground the plan in
/// the real edge types / entity keys. Returns the generated plan for
/// transparency plus the result node records. Keys come from the server env.
pub fn plane_ask(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: Ask = params(p)?;
    let plane = ctx.plane(&req.plane)?;
    let provider = provider_for(ctx, req.provider.as_deref())?;
    let chat = dr_strange_llm::build_provider(provider, req.model.as_deref(), None, None, false)
        .map_err(build_err)?;
    // Embedding tools are enabled when an embed provider is named and builds.
    // The name is validated even though a build failure is tolerated: a
    // refused URL is the client's error, not a missing model.
    let embedder = match req.embed_provider.as_deref() {
        Some(ep) => {
            let ep = provider_for(ctx, Some(ep))?;
            dr_strange_llm::build_provider(ep, req.embed_model.as_deref(), None, None, true).ok()
        }
        None => None,
    };
    let (max_attempts, limit) = ask_knobs(req.max_attempts, req.limit);
    let opts = dr_strange_llm::AskOptions {
        max_attempts,
        dry_run: req.dry_run,
        limit,
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
    .map_err(llm_err)?;
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;
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
    provider: &str,
    limit: usize,
) -> anyhow::Result<Vec<Value>> {
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
            let mut obj = node_json(n);
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
            nodes.push(node_json(&node));
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

/// A provider *call* failed: the chain carries the upstream reply body,
/// which may quote the request, the account, or whatever the provider felt
/// like saying. Operator-facing.
fn llm_err(e: anyhow::Error) -> RpcError {
    opaque("provider request failed", format!("{e:#}"))
}

/// A provider could not be *built*: no key in the environment, no embedding
/// model, unknown name. Decided before any network call from strings this
/// process composed, and the fix is the client's (or the operator's env), so
/// the message goes through as written.
fn build_err(e: anyhow::Error) -> RpcError {
    RpcError::server(format!("provider: {e:#}"))
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
    /// server default (`[digest].concurrency`, else 8). Capped at
    /// `DIGEST_MAX_CONCURRENCY` or the server default, whichever is larger.
    #[serde(default)]
    concurrency: Option<usize>,
    /// Target chunk size in characters. Omit to use the server default
    /// (`[digest].chunk_chars`, else 4000). Capped at `DIGEST_MAX_CHUNK_CHARS`
    /// or the server default, whichever is larger.
    #[serde(default)]
    chunk_chars: Option<usize>,
    /// How thoroughly to clean up the extraction: `coarse` reconciles the
    /// label and edge-type vocabularies, `fine` (the default) also merges
    /// entities naming the same thing, `super` also re-reads every entity
    /// against all the passages mentioning it.
    #[serde(default)]
    mode: Option<String>,
}

/// The concurrency and chunk size a `digest.run` actually gets. A request
/// may lower either below the server default freely; raising them is bounded
/// by [`crate::DIGEST_MAX_CONCURRENCY`] / [`crate::DIGEST_MAX_CHUNK_CHARS`]
/// (or the operator's own default, if they set it higher — their config is
/// their ceiling). Both spend the server's provider key and memory, which
/// is why a read credential does not get to name them freely; zero is
/// rounded up to one because neither means anything at zero.
pub(crate) fn digest_knobs(
    defaults: &crate::DigestDefaults,
    concurrency: Option<usize>,
    chunk_chars: Option<usize>,
) -> (usize, usize) {
    let conc_cap = defaults.concurrency.max(crate::DIGEST_MAX_CONCURRENCY);
    let chunk_cap = defaults.chunk_chars.max(crate::DIGEST_MAX_CHUNK_CHARS);
    (
        concurrency
            .unwrap_or(defaults.concurrency)
            .clamp(1, conc_cap),
        chunk_chars
            .unwrap_or(defaults.chunk_chars)
            .clamp(1, chunk_cap),
    )
}

/// `digest.run` — extract a proposal from text (LLM, dry-run). Provider API
/// keys come from the server's environment, never params. Blocking work runs
/// on the /rpc handler's blocking task.
pub fn digest_run(ctx: &Ctx<'_>, p: Value) -> Result<Value, RpcError> {
    let req: DigestRun = params(p)?;
    let chat_provider = provider_for(ctx, req.chat.as_deref())?;
    let embed_provider = provider_for(ctx, req.embed.as_deref().or(Some(chat_provider)))?;
    let embed = !req.no_embed;
    let link = req.link.unwrap_or(true);

    let chat =
        dr_strange_llm::build_provider(chat_provider, req.model.as_deref(), None, None, false)
            .map_err(build_err)?;
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
    .map_err(build_err)?;

    let (concurrency, chunk_chars) = digest_knobs(&ctx.digest, req.concurrency, req.chunk_chars);
    let opts = dr_strange_llm::DigestOptions {
        source: req.source.unwrap_or_else(|| "web-digest".into()),
        model: chat_model,
        run_id: format!("web-{}", now_secs()),
        chunk_chars,
        embed,
        concurrency,
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;

    // Only nodes whose key is genuinely free get written.
    //
    // `bulk_load` writes the external-key index unconditionally: a proposed key
    // that already exists overwrites the index entry, leaving the original node
    // in place but reachable only by id. Every `key(...)` read against it then
    // returns empty while the data sits there untouched. Observed, not
    // theorised: one run shadowed two nodes an application addressed by key,
    // and every key-filtered read for them went empty at the same moment —
    // no error, no log line. `node.create` has always rejected a taken key;
    // this path was the way around it.
    //
    // Skipping rather than failing the batch: a distillation proposes every
    // entity as new (`link: false`), so naming something already known is the
    // normal case, not an error — rejecting the batch would drop the run
    // exactly when it had learned something. Edges are kept either way:
    // `resolve` falls back to the plane's existing keys, so an edge onto a
    // skipped key attaches to the node that was already there, which is what
    // "this distillation mentions something we know" ought to mean.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut fresh: Vec<&WriteNode> = Vec::with_capacity(req.nodes.len());
    let mut skipped: Vec<&str> = Vec::new();
    for n in &req.nodes {
        // Intra-batch duplicates would make bulk_load reject the whole call.
        if !seen.insert(n.key.as_str()) || app(plane.node_by_key(&n.key))?.is_some() {
            skipped.push(n.key.as_str());
            continue;
        }
        fresh.push(n);
    }

    let mut node_props = Vec::with_capacity(fresh.len());
    for n in &fresh {
        node_props.push(props_of(&n.properties)?);
    }
    let label_slots: Vec<[&str; 1]> = fresh
        .iter()
        .map(|n| {
            [if n.label.is_empty() {
                "Entity"
            } else {
                n.label.as_str()
            }]
        })
        .collect();
    let bnodes: Vec<BulkNode> = fresh
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
    // `skipped_keys` is reported, not just counted: a silent skip is how the
    // client-side guard managed to do nothing for weeks without anyone noticing.
    Ok(jval!({
        "nodes_written": stats.nodes,
        "edges_written": stats.edges,
        "nodes_skipped": skipped.len(),
        "skipped_keys": skipped,
    }))
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
    let plane = ctx.plane(&req.plane)?;
    let props = props_of(&req.properties)?;
    let labels: Vec<&str> = req.labels.iter().map(String::as_str).collect();

    let mut txn = app(plane.write())?;
    let id = match &req.key {
        Some(k) => app(txn.create_node_with_key(k, &labels, props))?,
        None => app(txn.create_node(&labels, props))?,
    };
    app(txn.commit())?;

    Ok(app(plane.node(id))?
        .map(|n| node_json(&n))
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
    let plane = ctx.plane(&req.plane)?;
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
        .map(|n| node_json(&n))
        .unwrap_or(Value::Null))
}

/// Serialize a plane to JSONL — node lines, then id-based edge lines — the
/// exact format `drsg import` reads back. Backs the Dashboard's per-plane
/// Export download. Not an RPC method (it returns a file, not JSON-RPC data);
/// the `/export` HTTP endpoint calls it directly.
///
/// Two phases, so the endpoint can answer a bad plane name with a 400 and
/// stream a good one: resolving the plane returns a [`PlaneExport`], and
/// [`PlaneExport::write_to`] emits the lines into `out` as it walks. What
/// it holds is the plane's node *ids* (8 bytes each, one scan) and one node
/// record at a time — never every record, and never the output as a string.
pub fn export_plane<'a>(ctx: &'a Ctx<'a>, plane_name: &str) -> Result<PlaneExport<'a>, RpcError> {
    Ok(PlaneExport(ctx.plane(plane_name)?))
}

/// A plane resolved for export — see [`export_plane`].
pub struct PlaneExport<'a>(PlaneHandle<'a>);

impl PlaneExport<'_> {
    /// Write the plane as JSONL. An `io::Error` from `out` means the reader
    /// went away; it is returned as a server error for the log and nothing
    /// else, since there is no one left to tell.
    pub fn write_to(&self, out: &mut dyn std::io::Write) -> Result<(), RpcError> {
        let plane = &self.0;
        let gone = |e: std::io::Error| RpcError::server(format!("export stream closed: {e}"));
        // One id scan for both passes, then a record at a time: `.nodes()`
        // would clone every record of the plane into one vector before the
        // first line went out (and again for the edges), which for a large
        // plane is the plane in memory twice over — the cost streaming the
        // output was meant to remove. Ids are 8 bytes each; a node deleted
        // between the scan and its read is simply skipped.
        let ids = app(plane.query().scan_all().ids())?;
        for id in &ids {
            if let Some(node) = app(plane.node(*id))? {
                serde_json::to_writer(&mut *out, &node_json(&node)).map_err(|e| gone(e.into()))?;
                out.write_all(b"\n").map_err(gone)?;
            }
        }
        // Edges after every node line, since `drsg import` resolves an edge's
        // endpoints against nodes it has already read: walk each node's
        // out-adjacency and emit each edge once.
        for id in &ids {
            for hop in app(plane.neighbors(*id, Dir::Out, None))? {
                if let Some(edge) = app(plane.edge(hop.edge))? {
                    serde_json::to_writer(&mut *out, &edge_to_json(&edge))
                        .map_err(|e| gone(e.into()))?;
                    out.write_all(b"\n").map_err(gone)?;
                }
            }
        }
        Ok(())
    }
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;
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
    let plane = ctx.plane(&req.plane)?;
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
    let handle = ctx.plane(&req.plane)?;
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
    let handle = ctx.plane(&req.plane)?;
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

#[cfg(test)]
mod guard_tests {
    //! The request-side guards: which provider a request may name, how far
    //! it may raise a cost knob, and what an internal error is allowed to say.
    use super::*;

    fn ctx<'a>(db: &'a Database, configured: Option<&'a str>) -> Ctx<'a> {
        Ctx {
            db,
            db_path: None,
            digest: crate::DigestDefaults::default(),
            deadline: None,
            history_limit: Database::DEFAULT_HISTORY,
            configured_provider: configured,
            retain_commits: None,
        }
    }

    #[test]
    fn provider_for_accepts_a_preset_and_defaults_to_openai() {
        let db = Database::in_memory().unwrap();
        let c = ctx(&db, None);
        assert_eq!(provider_for(&c, None).unwrap(), "openai");
        for name in dr_strange_llm::PRESET_NAMES {
            assert_eq!(provider_for(&c, Some(name)).unwrap(), *name);
        }
    }

    #[test]
    fn provider_for_rejects_a_raw_url_as_the_clients_error() {
        let db = Database::in_memory().unwrap();
        let c = ctx(&db, None);
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "https://internal.corp:8443/v1",
            "http://localhost:11434/v1",
        ] {
            let err = provider_for(&c, Some(url)).unwrap_err();
            assert_eq!(err.code, -32602, "{url} must be invalid params");
            assert!(err.message.contains("preset"), "{}", err.message);
            // The message names what is allowed, never echoes the URL.
            assert!(!err.message.contains(url));
        }
    }

    #[test]
    fn provider_for_accepts_exactly_the_configured_provider() {
        let db = Database::in_memory().unwrap();
        let c = ctx(&db, Some("http://embed.internal:8080/v1"));
        assert_eq!(
            provider_for(&c, Some("http://embed.internal:8080/v1")).unwrap(),
            "http://embed.internal:8080/v1"
        );
        // Same host, different path: not the configured value, not allowed.
        assert_eq!(
            provider_for(&c, Some("http://embed.internal:8080/v2"))
                .unwrap_err()
                .code,
            -32602
        );
        // Presets still work alongside a configured URL.
        assert_eq!(provider_for(&c, Some("ollama")).unwrap(), "ollama");
    }

    #[test]
    fn digest_knobs_are_capped_and_never_zero() {
        let d = crate::DigestDefaults::default();
        assert_eq!(digest_knobs(&d, None, None), (d.concurrency, d.chunk_chars));
        assert_eq!(
            digest_knobs(&d, Some(10_000), Some(usize::MAX)),
            (crate::DIGEST_MAX_CONCURRENCY, crate::DIGEST_MAX_CHUNK_CHARS)
        );
        assert_eq!(digest_knobs(&d, Some(0), Some(0)), (1, 1));
        assert_eq!(digest_knobs(&d, Some(2), Some(500)), (2, 500));
        // An operator default above the built-in ceiling is its own ceiling.
        let big = crate::DigestDefaults {
            concurrency: 64,
            chunk_chars: 100_000,
        };
        assert_eq!(
            digest_knobs(&big, Some(1_000), Some(1_000_000)),
            (64, 100_000)
        );
        assert_eq!(digest_knobs(&big, None, None), (64, 100_000));
    }

    #[test]
    fn ask_knobs_follow_the_llm_crate_constants() {
        use dr_strange_llm::{ASK_DEFAULT_ATTEMPTS, ASK_DEFAULT_LIMIT, ASK_MAX_LIMIT};
        assert_eq!(
            ask_knobs(None, None),
            (ASK_DEFAULT_ATTEMPTS, ASK_DEFAULT_LIMIT)
        );
        assert_eq!(
            ask_knobs(Some(u32::MAX), Some(u64::MAX)),
            (ASK_DEFAULT_ATTEMPTS, ASK_MAX_LIMIT)
        );
        assert_eq!(ask_knobs(Some(2), Some(5)), (2, 5));
    }

    #[test]
    fn a_storage_error_crosses_the_wire_without_its_path() {
        let io = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/srv/secret/graph.drsg: permission denied",
        );
        let err = core_err(dr_strange_core::Error::Io(io));
        assert_eq!(err.code, -32000);
        assert!(!err.message.contains("/srv/secret"), "{}", err.message);
        assert!(
            err.message.starts_with("storage error (ref "),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_client_fault_keeps_its_message() {
        let err = core_err(dr_strange_core::Error::NotFound("plane `nope`".into()));
        assert_eq!(err.message, "not found: plane `nope`");
        let err = core_err(dr_strange_core::Error::Timeout("writer busy".into()));
        assert_eq!(err.code, -32002);
    }

    #[test]
    fn opaque_references_are_distinct_and_carry_no_detail() {
        let a = opaque(
            "provider request failed",
            "POST /v1/chat → HTTP 402: {\"error\":\"billing\"}",
        );
        let b = opaque("provider request failed", "same");
        assert_ne!(a.message, b.message);
        assert!(!a.message.contains("billing"));
        assert!(!a.message.contains("HTTP 402"));
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// Two stale hits while a refresh is in flight start one refresh, not
    /// two; once it finishes, the next stale hit starts another.
    #[test]
    fn a_catalog_refresh_is_single_flight() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        assert!(refresh_catalog_once(move || {
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        }));
        // Still running: a second stale hit does not start another.
        assert!(!refresh_catalog_once(|| unreachable!(
            "a second refresh must not start"
        )));
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the refresh runs to completion");
        // The flag clears when the work returns, so the next stale hit may
        // refresh again. The guard drops after `done_tx` fires, so poll.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if refresh_catalog_once(|| {}) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the flag never cleared"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
