//! One place to stand up the `tracing` subscriber for dr-strange's binaries
//! (`drsg`, `drsg-mcp`). Library crates (core / llm / web) only *emit* events;
//! they never install a subscriber, so a subscriber lives exactly here and is
//! called once from each binary's `main`.
//!
//! Two sinks, both fed by the same filter:
//! - **stderr** — human-readable console output. Never stdout: the CLI's
//!   command results and the MCP server's JSON-RPC protocol both own stdout,
//!   and a stray log line there would corrupt them.
//! - **a daily-rolling file** under `DRSG_LOG_DIR` (default `./logs`), written
//!   through a non-blocking worker so logging never stalls the hot path.
//!
//! Filtering honours `RUST_LOG`; absent that it defaults to `info`.
//!
//! Document conversion (`anydoc`, via dr-strange-llm's `document`) reports
//! through the `log` crate, bridged in by `tracing-log`, and is deliberately
//! **not** filtered down. Its PDF warnings are per document and are exactly
//! what explains a disappointing digest — "3 of 40 pages need OCR and were not
//! extracted", "broken font encodings detected; extracted text may be garbled".
//! Silencing those to keep the log tidy would hide the answer to the question
//! an operator is about to ask.
//!
//! Volume is not a concern the way it was. `anydoc` warns per *document* —
//! twice at most for a PDF. Its one per-record warning, for a CSV row the
//! parser cannot read at all, is rarer than it looks: ragged rows are padded
//! into a wider table rather than refused. Should a deployment ever meet a file
//! that does flood, `RUST_LOG=info,anydoc=error` pins it.
//!
//! The predecessor, `pdf-extract`, was pinned to `error` here permanently
//! because it warned per *glyph* on every broken font — a different order of
//! noise, and gone with the crate.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Holds the non-blocking file writer's worker alive. Dropping it flushes any
/// buffered log lines and stops the worker, so keep it bound in `main` for the
/// whole process lifetime — drop it too early and late log lines vanish.
#[must_use = "dropping the guard stops file logging; bind it for the process lifetime"]
pub struct LogGuard(#[allow(dead_code)] WorkerGuard);

/// Install the global subscriber. `service` is the log-file name stem (e.g.
/// `drsg` / `drsg-mcp`) so the binaries don't interleave into one file.
///
/// Call once, early in `main`; a second call is a no-op that logs a warning
/// (a global subscriber can only be set once).
pub fn init(service: &str) -> LogGuard {
    let dir = std::env::var_os("DRSG_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("logs"));

    let file_appender = tracing_appender::rolling::daily(&dir, format!("{service}.log"));
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // EnvFilter isn't Clone, so build a fresh one per layer.
    let make_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_filter(make_filter());

    // No ANSI colour codes in the file — it's read as plain text / grep'd.
    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_filter(make_filter());

    let already_set = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .is_err();
    if already_set {
        tracing::warn!("tracing subscriber was already initialized; ignoring re-init");
    }

    LogGuard(guard)
}
