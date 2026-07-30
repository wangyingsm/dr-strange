//! The axum HTTP + WebSocket server (arch/08 §1). Two live endpoints —
//! `POST /rpc` for request/response and `GET /ws` for streaming — plus the
//! embedded SPA on every other path. The core is synchronous, so every
//! database call runs on a blocking task; the async runtime is never stalled
//! by a long scan.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use dr_strange_core::Database;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::ServeOptions;
use crate::assets::static_handler;
use crate::auth::{Access, AllowedOrigins, Auth, Authorizer, Credentials, SharedToken};
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
    /// Write-authorization backend (v1: a single shared `DRSG_TOKEN`).
    pub authorizer: Arc<dyn Authorizer>,
    /// Browser-`Origin` allow-list — the CSRF guard.
    pub origins: AllowedOrigins,
    /// The shared token, echoed into the served SPA so the local UI can
    /// authenticate (see [`crate::assets`]). `None` when unset. Same value the
    /// `authorizer` checks against, so the injected token always works.
    pub bootstrap_token: Option<String>,
}

impl AppState {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            db: self.db.as_ref(),
            db_path: self.db_path.as_deref(),
        }
    }
}

/// Pull the bearer token from an `Authorization: Bearer …` header, if present.
fn bearer_of(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Resolve the caller's [`Credentials`], enforcing the **Origin guard**: a
/// request carrying a *disallowed* `Origin` (a cross-site browser) is refused
/// with 403 before it can act. A request with no `Origin` (a native client /
/// SDK) passes through here, to be gated by the token at dispatch instead.
///
/// `ws_token` carries the WebSocket's `?token=` query value — browsers can't
/// set an `Authorization` header on a WS handshake, so the token rides the URL
/// there.
fn resolve_credentials(
    state: &AppState,
    headers: &HeaderMap,
    ws_token: Option<String>,
) -> Result<Credentials, Box<Response>> {
    let local_ui = match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(origin) if state.origins.allows(origin) => true,
        Some(_) => {
            return Err(Box::new(
                (
                    StatusCode::FORBIDDEN,
                    "cross-origin request refused (Origin not allowed)",
                )
                    .into_response(),
            ));
        }
        None => false,
    };
    Ok(Credentials {
        bearer: bearer_of(headers).or(ws_token),
        local_ui,
    })
}

fn router(state: Arc<AppState>, max_concurrent: usize) -> Router {
    // Outermost → innermost: catch panics so a bug becomes a 500 (not a dropped
    // connection), then cap total requests in flight, then stamp defensive
    // headers, then bound the body size. The cap counts a request as in flight
    // until its response is produced — a slow `digest` holds a slot the whole
    // time, which is exactly the exhaustion we want to bound.
    let hardening = ServiceBuilder::new()
        .layer(CatchPanicLayer::new())
        .layer(GlobalConcurrencyLimitLayer::new(max_concurrent))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY));
    Router::new()
        .route("/rpc", post(rpc_http))
        .route("/ws", get(ws_upgrade))
        .route("/digest/extract", post(extract_http))
        // POST (not GET): browsers omit the Origin header on same-origin GETs,
        // so the local-UI Origin check can't see it and a tokenless server
        // would 401 its own UI. POST always carries Origin.
        .route("/export", post(export_http))
        // POST: the query text is the body; kept off /rpc (and thus the OpenRPC
        // schema / SDKs) as a web-only surface, like /export.
        .route("/cypher", post(cypher_http))
        // Unauthenticated liveness probe for load balancers / orchestrators.
        .route("/health", get(health))
        // `.layer` after `.fallback` so the SPA (served by the fallback) is
        // wrapped too — axum only applies a layer to routes registered before
        // it, and the fallback is registered here.
        .fallback(static_handler)
        .layer(hardening)
        .with_state(state)
}

/// `GET /health` — a cheap, unauthenticated liveness check. Deliberately does
/// no database work so a probe can't be starved by a busy server.
async fn health() -> Response {
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
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
async fn extract_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ExtractQuery>,
    body: Bytes,
) -> Response {
    // Extraction touches no DB state, but the whole surface is authenticated:
    // apply the Origin guard, then require a credential (read level).
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
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

#[derive(serde::Deserialize)]
struct ExportQuery {
    #[serde(default)]
    plane: String,
}

/// Keep only filename-safe characters — also stops CRLF/quote injection into
/// the `Content-Disposition` header.
fn safe_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "plane".to_string() } else { s }
}

/// `POST /export?plane=startup` — the plane serialized as JSONL, returned as a
/// file download (`drsg import` reads the same format). Read-gated like the
/// rest of the surface; the DB scan runs on a blocking task.
async fn export_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<ExportQuery>,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let plane = q.plane;
    let built = tokio::task::spawn_blocking({
        let state = state.clone();
        let plane = plane.clone();
        move || methods::export_plane(&state.ctx(), &plane)
    })
    .await;

    match built {
        Ok(Ok(jsonl)) => {
            tracing::info!(plane = %plane, bytes = jsonl.len(), "exported plane as JSONL");
            Response::builder()
                .header("content-type", "application/x-ndjson")
                .header(
                    "content-disposition",
                    format!("attachment; filename=\"{}.jsonl\"", safe_filename(&plane)),
                )
                .body(Body::from(jsonl))
                .unwrap()
        }
        Ok(Err(e)) => {
            tracing::warn!(plane = %plane, error = %e.message, "export failed");
            (StatusCode::BAD_REQUEST, e.message).into_response()
        }
        Err(_) => {
            tracing::error!(plane = %plane, "export task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "export task failed").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct CypherQuery {
    #[serde(default)]
    plane: String,
    /// Embedding provider for a text `SEARCH … NEAR "…"` (preset or base URL);
    /// the server env supplies the key. Defaults to `openai`.
    #[serde(default)]
    embed: Option<String>,
}

/// `POST /cypher?plane=startup` — the query text in the body, run against the
/// plane. A read returns `{nodes, edges, count}` (the result set + induced
/// edges) for the plot; a write (`CREATE`, …) mutates and returns its
/// change-counts. **Write-gated**: the language can mutate, so this needs write
/// authorization even for a read query (the single-token model collapses the
/// levels anyway; the browser UI is write-capable). Runs on a blocking task; a
/// parse/compile error comes back as 400 with the message.
async fn cypher_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CypherQuery>,
    body: Bytes,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Write, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let query = match String::from_utf8(body.to_vec()) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "query body must be UTF-8").into_response(),
    };
    let plane = q.plane;
    let embed = q.embed.unwrap_or_else(|| "openai".to_string());

    let built = tokio::task::spawn_blocking({
        let state = state.clone();
        let plane = plane.clone();
        // The SPA doesn't send params; plane.cypher does (methods::plane_cypher).
        move || methods::cypher_subgraph(&state.ctx(), &plane, &query, &embed, &Default::default())
    })
    .await;

    match built {
        Ok(Ok(value)) => {
            tracing::debug!(plane = %plane, "cypher query ok");
            Json(value).into_response()
        }
        Ok(Err(e)) => {
            tracing::debug!(plane = %plane, code = e.code, error = %e.message, "cypher query rejected");
            (StatusCode::BAD_REQUEST, e.message).into_response()
        }
        Err(_) => {
            tracing::error!(plane = %plane, "cypher task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "cypher task failed").into_response()
        }
    }
}

/// The Eye-of-Agamotto seal (the same square + diamond + tick-ring emblem as
/// the web UI's SVG logo), rendered in text for the startup banner.
const LOGO: &str = r#"
                   ooooooooo
              ooo             ooo
           oo                     oo
         oo          /   \          oo
       oo    ++----//-----\\----++    oo
      oo     |   //         \\   |     oo
     oo      | //             \\ |      oo
    oo       //                 \\       oo
    oo     //|        ***        |\\     oo
    o        |       *****       |        o
    oo     \\|        ***        |//     oo
    oo       \\                 //       oo
     oo      | \\             // |      oo
      oo     |   \\         //   |     oo
       oo    ++----\\-----//----++    oo
         oo          \   /          oo
           oo                     oo
              ooo             ooo
                   ooooooooo"#;

/// Prints the emblem + tagline to stderr at startup. Purely decorative, so it
/// bypasses `tracing` (whose timestamps/levels would mangle the art); ANSI
/// colour is used only when stderr is a real terminal.
fn startup_banner() {
    let (gold, bold, reset) = if std::io::stderr().is_terminal() {
        ("\x1b[38;5;178m", "\x1b[1m", "\x1b[0m")
    } else {
        ("", "", "")
    };
    eprintln!("{gold}{LOGO}{reset}");
    eprintln!(
        "    {gold}{bold}Dr STRANGE{reset}{gold}, an AI-native embedded graph database  v{}{reset}\n",
        env!("CARGO_PKG_VERSION"),
    );
}

/// Runs the server until Ctrl-C. Owns the tokio runtime setup's payload; the
/// synchronous `serve` wrapper in `lib.rs` drives it with `block_on`.
pub async fn run(db: Database, db_path: Option<PathBuf>, opts: ServeOptions) -> anyhow::Result<()> {
    startup_banner();
    // Read the secret once so the checker (`authorizer`) and the SPA's injected
    // copy (`bootstrap_token`) can never disagree.
    let token = std::env::var("DRSG_TOKEN").ok().filter(|t| !t.is_empty());
    let authorizer = SharedToken::new(token.clone());
    if authorizer.is_configured() {
        tracing::info!(
            "auth ENABLED — every request requires DRSG_TOKEN (Authorization: Bearer <token>; WebSocket via ?token=<token>)"
        );
    } else {
        tracing::warn!(
            "no DRSG_TOKEN set; the API is reachable only from the local browser UI. Set DRSG_TOKEN to allow programmatic (SDK / curl) access."
        );
    }
    let state = Arc::new(AppState {
        db: Arc::new(db),
        db_path,
        authorizer: Arc::new(authorizer),
        origins: AllowedOrigins::from_env(),
        bootstrap_token: token,
    });
    let app = router(state, opts.max_concurrent);
    let listener = tokio::net::TcpListener::bind(opts.addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        %bound,
        max_concurrent = opts.max_concurrent,
        "drsg serve: dashboard + JSON-RPC listening on http://{bound}"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves when the process is asked to stop — Ctrl-C (interactive) or SIGTERM
/// (containers / `systemctl stop`), so orchestrated deployments drain cleanly
/// instead of being killed mid-request.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received; draining");
}

// ---- HTTP JSON-RPC --------------------------------------------------------

async fn rpc_http(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    let out = tokio::task::spawn_blocking(move || {
        let auth = Auth::new(state.authorizer.as_ref(), creds);
        rpc::handle(&state.ctx(), &auth, &body)
    })
    .await;
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

/// The WebSocket carries its bearer token in the query string (`/ws?token=…`)
/// because the browser WebSocket API can't set request headers.
#[derive(serde::Deserialize)]
struct WsQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, q.token) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    // The socket's stats push is a read, and reads are authenticated too — an
    // unauthorized client gets no socket at all.
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    ws.on_upgrade(move |socket| ws_task(socket, state, creds))
}

/// One WebSocket connection: answers JSON-RPC requests framed as text, and
/// every [`STATS_INTERVAL`] pushes a `db.stats` notification for the live
/// dashboard. The first interval tick fires immediately, so a client sees
/// stats the moment it connects.
async fn ws_task(mut socket: WebSocket, state: Arc<AppState>, creds: Credentials) {
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
                        let creds = creds.clone();
                        let resp = tokio::task::spawn_blocking(move || {
                            let auth = Auth::new(st.authorizer.as_ref(), creds);
                            rpc::handle(&st.ctx(), &auth, &body)
                        })
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
