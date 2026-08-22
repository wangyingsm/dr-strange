//! `drsg serve` — a thin local web backend for dr-strange (arch/08).
//!
//! Embeds `dr-strange-core` and serves two things from one process: a
//! **JSON-RPC 2.0** API (`/rpc` over HTTP, `/ws` over WebSocket) whose methods
//! map 1:1 to the public core API, and a bundled **single-page dashboard**.
//! The JSON-RPC surface is the project-wide wire protocol (00-overview §2), so
//! this backend doubles as the first draft of the eventual network server.
//!
//! Chunk 1 (this milestone slice) ships the server + a read-only method set +
//! a minimal dashboard; the WebGL graph-plot views (arch/08 §2.2) land next.

mod assets;
mod auth;
pub mod fetch;
// The client half of raw-WAL replication (arch/01 §9) needs
// `Database::apply_replicated`, which only the native engine provides.
#[cfg(feature = "native-backend")]
mod follow;
mod methods;
mod rpc;
mod server;

pub use server::ServeOutcome;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use dr_strange_core::Database;

/// How a `serve --follow` replica reaches the master it follows. Kept
/// ungated (unlike the `follow` module itself) so `ServeOptions` has one
/// shape regardless of backend feature — a non-native build can still parse
/// `--follow`, it just refuses to start with it set (arch/01 §9 replication
/// mirrors the native engine's own WAL).
#[derive(Debug, Clone)]
pub struct FollowOptions {
    /// The master's `drsg serve` address, e.g. `ws://host:7700` or
    /// `wss://host:7700` — *without* a path; `/ws/wal` and `/snapshot` are
    /// appended by the follower.
    pub upstream: String,
    /// Bearer token presented to the master. Distinct from this replica's own
    /// `DRSG_TOKEN`, which still gates *its* downstream clients.
    pub token: Option<String>,
}

/// Default listen address when neither the CLI nor a config file specifies one.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7700";
/// Default ceiling on requests in flight at once. Generous — a slow `digest`
/// run legitimately holds a slot for minutes, so the cap only exists to bound
/// pathological fan-out, not to throttle normal use.
pub const DEFAULT_MAX_CONCURRENT: usize = 1024;

/// How to serve — the knobs a `config.toml` (or the CLI) can set. Secrets and
/// the Origin allow-list still travel through the process environment (so the
/// auth/LLM plumbing stays a single source); this carries only what the
/// listener itself needs.
pub struct ServeOptions {
    /// Address to bind.
    pub addr: SocketAddr,
    /// Maximum requests processed concurrently; excess requests queue.
    pub max_concurrent: usize,
    /// When set, serve HTTPS with this certificate/key instead of plain HTTP.
    pub tls: Option<TlsOptions>,
    /// Server-side defaults for `digest.run` when the request omits them.
    pub digest: DigestDefaults,
    /// Policy and budgets for the URL fetcher (ROADMAP §9).
    pub fetch: FetchDefaults,
    /// How long a write transaction waits for the single writer slot before
    /// failing with a retryable timeout; `None` waits forever.
    ///
    /// Bounded here, unlike an embedded `Database`, because a server has other
    /// clients: one long `bulk_load` or `digest` holds the slot for its whole
    /// transaction, and an unbounded wait would leave every other writer
    /// blocked with nothing to report.
    pub write_timeout: Option<Duration>,
    /// How long a single request's queries may run before stopping with a
    /// retryable timeout; `None` runs to completion.
    ///
    /// Bounded for the same reason as [`ServeOptions::write_timeout`]: the
    /// query is someone else's, and since `/mcp` shipped an agent's runaway
    /// `MATCH` had nothing bounding it at all.
    pub query_timeout: Option<Duration>,
    /// When set, `/mcp`'s `write_nodes` embeds the nodes it writes —
    /// `(preset-or-url, model, key-env)`. Configured by the operator rather
    /// than asked for per call, so an agent cannot write silently unsearchable
    /// nodes by forgetting a flag. The key is read from the environment at call
    /// time, never carried here.
    pub embed_provider: Option<(String, Option<String>, Option<String>)>,
    /// The source tree behind the graph, when one is attached — enables the
    /// MCP `grep` tool. `serve watch` sets it to its `--dir`.
    pub source_root: Option<std::path::PathBuf>,
    /// Run once on a background thread after the server is up, with a handle
    /// to the same database the server writes through — how `drsg serve
    /// watch` runs its repository watcher against the one open database
    /// rather than fighting it for the file lock. The server neither waits
    /// on it nor restarts it.
    pub on_start: Option<Box<dyn FnOnce(std::sync::Arc<Database>) + Send>>,
    /// When set, this server is a read-only replica of another `drsg serve`
    /// (`serve --follow`, arch/01 §9): every write RPC is refused regardless
    /// of token, and a background task bootstraps from the master's
    /// `/snapshot` then tails its `/ws/wal`.
    pub follow: Option<FollowOptions>,
}

/// A PEM certificate chain + private key for native TLS.
pub struct TlsOptions {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Default digest tuning applied by `digest.run` unless the request overrides
/// it (precedence: request param → these → the built-in constants).
#[derive(Debug, Clone, Copy)]
pub struct DigestDefaults {
    /// Per-chunk extraction chat calls to run concurrently.
    pub concurrency: usize,
    /// Target chunk size in characters (paragraph-aware).
    pub chunk_chars: usize,
}

impl Default for DigestDefaults {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_DIGEST_CONCURRENCY,
            chunk_chars: DEFAULT_DIGEST_CHUNK_CHARS,
        }
    }
}

/// Built-in digest defaults (used when neither the request nor config sets them).
pub const DEFAULT_DIGEST_CONCURRENCY: usize = 8;
pub const DEFAULT_DIGEST_CHUNK_CHARS: usize = 4000;

/// Policy for the URL fetcher (ROADMAP §9), from the `[fetch]` config section.
///
/// Fetching ships **enabled**: a database that must be reconfigured before it
/// can read a URL will not be used to read URLs. That default carries none of
/// the security — the address guard in [`fetch::guard`] is not a setting, and
/// `allow_private` is the one deliberate exception an operator can make.
#[derive(Debug, Clone)]
pub struct FetchDefaults {
    /// Whether `/digest/fetch` will fetch anything at all.
    pub enabled: bool,
    /// Ceiling on pages kept in one crawl, the root included.
    pub max_pages: usize,
    /// Ceiling on link-following depth — the most a request may ask for, not
    /// what it gets by default. A request that names no depth follows one hop.
    /// The page budget bounds the crawl regardless of how deep it is allowed
    /// to go.
    pub max_depth: usize,
    /// Requests in flight at once.
    pub concurrency: usize,
    /// CIDR blocks the operator has deliberately re-permitted, e.g. to read an
    /// intranet wiki. Everything not listed here that is not publicly routable
    /// stays refused.
    pub allow_private: Vec<String>,
}

impl Default for FetchDefaults {
    fn default() -> Self {
        Self {
            enabled: true,
            max_pages: 10,
            max_depth: 3,
            concurrency: 4,
            allow_private: Vec::new(),
        }
    }
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            addr: DEFAULT_ADDR.parse().expect("valid default addr"),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            tls: None,
            digest: DigestDefaults::default(),
            fetch: FetchDefaults::default(),
            write_timeout: Some(DEFAULT_WRITE_TIMEOUT),
            query_timeout: Some(DEFAULT_QUERY_TIMEOUT),
            embed_provider: None,
            source_root: None,
            on_start: None,
            follow: None,
        }
    }
}

/// Generous for an interactive agent query, and far short of "forever".
/// A deliberately heavy analytical query can raise it per deployment.
const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Long enough that an ordinary import or digest is waited out, short enough
/// that a client learns the writer is busy rather than hanging on it.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Serves `db` (whose file lives at `db_path`, if on disk) per `opts` until a
/// shutdown signal (Ctrl-C / SIGTERM) — or, in `--follow` mode, until the
/// replication stream from the master is lost, in which case the caller
/// should reopen a fresh `db` (see [`Database::open_read_only`]) and call
/// this again to resync from scratch (`ServeOutcome::ResyncNeeded`).
/// Synchronous: it owns a multi-threaded tokio runtime internally so the
/// sync `drsg` CLI can call it without being async itself.
pub fn serve(
    db: Database,
    db_path: Option<PathBuf>,
    opts: ServeOptions,
) -> anyhow::Result<ServeOutcome> {
    // Applied before the first request: unlike an embedded caller, this process
    // has several clients competing for the one writer slot.
    db.set_write_timeout(opts.write_timeout);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::run(db, db_path, opts))
}
