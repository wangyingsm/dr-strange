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
mod extract;
mod methods;
mod rpc;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;

use dr_strange_core::Database;

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

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            addr: DEFAULT_ADDR.parse().expect("valid default addr"),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            tls: None,
            digest: DigestDefaults::default(),
        }
    }
}

/// Serves `db` (whose file lives at `db_path`, if on disk) per `opts` until a
/// shutdown signal (Ctrl-C / SIGTERM). Synchronous: it owns a multi-threaded
/// tokio runtime internally so the sync `drsg` CLI can call it without being
/// async itself.
pub fn serve(db: Database, db_path: Option<PathBuf>, opts: ServeOptions) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::run(db, db_path, opts))
}
