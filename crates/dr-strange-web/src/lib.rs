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
mod methods;
mod rpc;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;

use dr_strange_core::Database;

/// Serves `db` (whose file lives at `db_path`, if on disk) on `addr` until
/// Ctrl-C. Synchronous: it owns a multi-threaded tokio runtime internally so
/// the sync `drsg` CLI can call it without being async itself.
pub fn serve(db: Database, db_path: Option<PathBuf>, addr: SocketAddr) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(server::run(db, db_path, addr))
}
