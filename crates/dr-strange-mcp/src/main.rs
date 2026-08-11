//! drsg-mcp — the [`dr_strange_mcp`] tool set served over stdio (arch/06).
//! One process per host, embedding the database file directly: point a host
//! at a path and it works, with nothing to run and nothing to configure.
//!
//! For several agent hosts sharing one database, see `drsg serve`'s `/mcp`
//! endpoint (ROADMAP §10) instead — this binary keeps no client mode of its
//! own, since a host that wants that can point its MCP client straight at
//! the served URL.

/// mimalloc as the process allocator (see the drsg binary for the rationale;
/// binaries choose the allocator, the core library never does).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;

use dr_strange_core::Database;
use dr_strange_mcp::DrStrange;
use rmcp::ServiceExt;

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

    // Local files are allowed here and nowhere else: this process runs on the
    // agent's own machine, as that agent's user, so `digest { path }` reads
    // exactly what the agent could already read for itself. The served `/mcp`
    // leaves the flag off, where the same param would be arbitrary file read.
    let server = DrStrange::new(db).with_local_files(true);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
