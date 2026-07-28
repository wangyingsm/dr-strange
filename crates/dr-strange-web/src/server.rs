//! The axum HTTP + WebSocket server (arch/08 §1). Two live endpoints —
//! `POST /rpc` for request/response and `GET /ws` for streaming — plus the
//! embedded SPA on every other path. The core is synchronous, so every
//! database call runs on a blocking task; the async runtime is never stalled
//! by a long scan.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use dr_strange_core::Database;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::assets::static_handler;
use crate::methods::{self, Ctx};
use crate::rpc;

/// How often a WebSocket connection pushes a fresh `db.stats` snapshot. The
/// dashboard renders these live (arch/08 §2.1).
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// Upload ceiling for `/digest/extract` and `/rpc`. axum defaults to 2 MiB,
/// which rejects real PDFs (and digest.write payloads carrying embeddings)
/// with a plain-text "Failed to buffer the request body" before the handler
/// ever runs. 64 MiB is generous for a document + its vectors.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Everything the request handlers share. `Arc`-wrapped and cheap to clone
/// into each blocking task.
pub struct AppState {
    pub db: Arc<Database>,
    pub db_path: Option<PathBuf>,
}

impl AppState {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            db: self.db.as_ref(),
            db_path: self.db_path.as_deref(),
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/rpc", post(rpc_http))
        .route("/ws", get(ws_upgrade))
        .route("/digest/extract", post(extract_http))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .fallback(static_handler)
        .with_state(state)
}

#[derive(serde::Deserialize)]
struct ExtractQuery {
    /// Filename — the extension selects the extractor.
    #[serde(default)]
    name: String,
}

/// `POST /digest/extract?name=doc.pdf` — the raw file bytes in the body,
/// extracted text out (arch/07 digest page). No DB access; the (potentially
/// slow) PDF/docx parsing runs on a blocking task.
///
/// The response is a stream of newline-delimited JSON objects so the digest
/// page can show a progress bar during a long PDF extraction:
///   `{"progress":{"page":3,"total":42}}`  — zero or more, as pages are parsed
///   `{"chars":12345,"text":"…"}`          — the final result, or
///   `{"error":"…"}`                       — a terminal failure
async fn extract_http(Query(q): Query<ExtractQuery>, body: Bytes) -> Response {
    // Bounded channel: `blocking_send` applies backpressure if the client reads
    // slowly, rather than buffering the whole document's progress in memory.
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);

    tokio::task::spawn_blocking(move || {
        let send = |v: serde_json::Value| {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            // Err ⇒ the client hung up; nothing left to do but stop trying.
            tx.blocking_send(Ok(Bytes::from(line))).is_ok()
        };
        // Scope the progress closure so its borrow of `send` ends before the
        // final message is sent.
        let result = {
            let mut on_page = |page, total| {
                send(json!({ "progress": { "page": page, "total": total } }));
            };
            crate::extract::extract_text_with_progress(&q.name, &body, &mut on_page)
        };
        match result {
            Ok(text) => send(json!({ "chars": text.chars().count(), "text": text })),
            Err(e) => send(json!({ "error": e.to_string() })),
        };
    });

    Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap()
}

/// Runs the server until Ctrl-C. Owns the tokio runtime setup's payload; the
/// synchronous `serve` wrapper in `lib.rs` drives it with `block_on`.
pub async fn run(db: Database, db_path: Option<PathBuf>, addr: SocketAddr) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        db: Arc::new(db),
        db_path,
    });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    println!("drsg serve: dashboard + JSON-RPC on http://{bound}");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ---- HTTP JSON-RPC --------------------------------------------------------

async fn rpc_http(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let out = tokio::task::spawn_blocking(move || rpc::handle(&state.ctx(), &body)).await;
    match out {
        // A notification (or all-notification batch) owes no response body.
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Ok(Some(value)) => Json(value).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "jsonrpc": "2.0",
                "error": { "code": -32603, "message": "request task panicked" },
                "id": null,
            })),
        )
            .into_response(),
    }
}

// ---- WebSocket ------------------------------------------------------------

async fn ws_upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_task(socket, state))
}

/// One WebSocket connection: answers JSON-RPC requests framed as text, and
/// every [`STATS_INTERVAL`] pushes a `db.stats` notification for the live
/// dashboard. The first interval tick fires immediately, so a client sees
/// stats the moment it connects.
async fn ws_task(mut socket: WebSocket, state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(note) = stats_notification(&state).await
                    && socket.send(Message::Text(note.into())).await.is_err()
                {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let body = text.as_bytes().to_vec();
                        let st = state.clone();
                        let resp = tokio::task::spawn_blocking(move || rpc::handle(&st.ctx(), &body))
                            .await
                            .ok()
                            .flatten();
                        if let Some(value) = resp
                            && socket.send(Message::Text(value.to_string().into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    // Ping/pong are handled by axum; ignore binary frames.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Computes a `db.stats` notification off-thread, or `None` if the snapshot
/// failed (a transient read error shouldn't tear down the socket).
async fn stats_notification(state: &Arc<AppState>) -> Option<String> {
    let st = state.clone();
    let stats = tokio::task::spawn_blocking(move || methods::db_stats(&st.ctx()).ok())
        .await
        .ok()
        .flatten()?;
    Some(json!({ "jsonrpc": "2.0", "method": "db.stats", "params": stats }).to_string())
}
