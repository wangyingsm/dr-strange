//! The axum HTTP + WebSocket server (arch/08 §1). Two live endpoints —
//! `POST /rpc` for request/response and `GET /ws` for streaming — plus the
//! embedded SPA on every other path. The core is synchronous, so every
//! database call runs on a blocking task; the async runtime is never stalled
//! by a long scan.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use dr_strange_core::{ChangeSet, Database, PlaneId, ReplicatedBatch};
use dr_strange_mcp::DrStrange;
use dr_strange_parser::Vocab;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceBuilder;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::ServeOptions;
use crate::assets::static_handler;
use crate::auth::{
    Access, AllowedOrigins, Auth, Authorizer, Credentials, ReadOnlyAuthorizer, SharedToken,
};
#[cfg(feature = "native-backend")]
use crate::follow;
use crate::methods::{self, Ctx};
use crate::rpc;

/// How often a WebSocket connection pushes a fresh `db.stats` snapshot. The
/// dashboard renders these live (arch/08 §2.1).
const STATS_INTERVAL: Duration = Duration::from_secs(2);

/// Buffered commits in the change-feed broadcast channel (ROADMAP §5). A
/// subscriber slower than this many commits behind loses the overflow (a
/// `Lagged` skip) rather than stalling writers — best-effort delivery.
const CHANGE_FEED_CAPACITY: usize = 1024;

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
    /// Commit-time change feed (ROADMAP §5): the core observer publishes each
    /// committed `ChangeSet` here, and every `/ws` subscriber that ran
    /// `plane.watch` drains its own receiver. Best-effort — a lagging consumer
    /// drops events rather than stalling writers.
    pub changes: broadcast::Sender<Arc<ChangeSet>>,
    /// Raw-WAL replication feed (`serve --follow`, arch/01 §9): the core
    /// observer publishes each committed batch here, and `/ws/wal` forwards
    /// it to every follower — no filtering, unlike `changes`, since a
    /// follower mirrors the whole database.
    pub wal_changes: broadcast::Sender<Arc<ReplicatedBatch>>,
    /// Server-side `digest.run` defaults (from `[digest]` config / built-ins).
    pub digest: crate::DigestDefaults,
    /// URL-fetch policy and budgets (from `[fetch]` config / built-ins).
    pub fetch: crate::FetchDefaults,
    /// Per-request query budget (`[server] query_timeout_secs`); `None` runs
    /// to completion.
    pub query_timeout: Option<Duration>,
    /// The completion vocabulary last built, and what it was built from —
    /// see [`AppState::vocab`].
    vocab_cache: Mutex<Option<CachedVocab>>,
}

/// A plane's completion vocabulary, and the state of the database it was read
/// from.
struct CachedVocab {
    plane: String,
    commit_seq: u64,
    vocab: Arc<Vocab>,
}

impl AppState {
    /// This plane's completion vocabulary, rebuilt only when the database has
    /// moved since the last one.
    ///
    /// Building it *scans the plane* — the catalog is computed, not stored —
    /// and completion is asked for on every pause in typing, so the answer is
    /// kept until a commit could have changed it. The commit sequence is the
    /// whole database's, which is conservative: a write to another plane
    /// rebuilds this one needlessly, and needlessly is far better than
    /// stale. One entry, because a person writes a query in one plane at a
    /// time.
    ///
    /// Blocking, like everything that reads the graph.
    fn vocab(&self, plane: &str) -> Result<Arc<Vocab>, rpc::RpcError> {
        let seq = self
            .db
            .commit_seq()
            .map_err(|e| rpc::RpcError::server(e.to_string()))?;
        if let Ok(cache) = self.vocab_cache.lock()
            && let Some(hit) = cache
                .as_ref()
                .filter(|c| c.plane == plane && c.commit_seq == seq)
        {
            return Ok(hit.vocab.clone());
        }
        let vocab = Arc::new(methods::plane_vocab(&self.ctx(), plane)?);
        if let Ok(mut cache) = self.vocab_cache.lock() {
            *cache = Some(CachedVocab {
                plane: plane.to_string(),
                commit_seq: seq,
                vocab: vocab.clone(),
            });
        }
        Ok(vocab)
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            db: self.db.as_ref(),
            db_path: self.db_path.as_deref(),
            digest: self.digest,
            // Stamped per request, not per process: the budget is how long
            // *this* call may run, so it starts when the call does.
            deadline: self.query_timeout.map(|d| Instant::now() + d),
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

/// Gates `/mcp` the same way `cypher_http` gates `/cypher`: **write** level,
/// since several tools mutate (`write_nodes`, `digest` with `apply`, …) and
/// the v1 single-token model doesn't distinguish finer than that (ROADMAP
/// §10 "Authentication" — a per-tool split is future work). Runs as an axum
/// middleware, not a handler check, because the `/mcp` route is a raw tower
/// service ([`StreamableHttpService`]) with no handler body of its own to put
/// the check in.
async fn mcp_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let creds = match resolve_credentials(&state, request.headers(), None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Write, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
}

/// How long an MCP session may sit idle before the server tears it down. A
/// host that is SIGKILLed (an editor restarting its MCP child is routine)
/// never sends `DELETE /mcp`, so without this its session worker, its
/// [`DrStrange`], and its buffered SSE messages would live as long as the
/// process — and this endpoint exists precisely to be a long-running shared
/// server. Pinned rather than inherited from `SessionConfig::default()` so
/// the deployment shape stops depending on an upstream default.
///
/// Ten minutes rather than five, because rmcp 2.2.0's worker counts a *running*
/// tool as idle: the keep-alive timer is only reset by an event, and a tool
/// call emits none between dispatch and its result, so a long `digest` on an
/// otherwise quiet session is torn down mid-flight. Ten minutes buys room for a
/// slow digest without abandoning the reaping that stops a SIGKILLed host
/// leaking its session. The real fix belongs upstream — the timer should not
/// count in-flight work — and this constant drops back when that lands.
///
/// Reaping has a second upstream wart we accept for now: the worker exits but
/// its map entry stays, so the next request sees `has_session` true, misses
/// the spec's 404, and gets a 500 from `create_stream` instead — a client that
/// would have re-initialized on 404 does not. A longer window makes it rarer.
const MCP_SESSION_IDLE: Duration = Duration::from_secs(600);
/// How long a session may exist before its `initialize` must arrive. Bounds
/// the same leak for a connection that opens and then goes silent.
const MCP_SESSION_INIT: Duration = Duration::from_secs(60);

/// How long shutdown waits for in-flight connections before giving up. Both
/// listeners use it, so Ctrl-C behaves the same with and without TLS.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// How many MCP tool bodies may run at once, given the HTTP request ceiling.
///
/// Not `max_concurrent` itself: that counts requests, most of which are cheap,
/// and defaults to 1024 — while every MCP tool is a `spawn_blocking` unit of
/// real work. Clamped rather than fixed so an operator who deliberately runs a
/// small server still gets a proportionally small tool ceiling.
fn mcp_tool_concurrency(max_concurrent: usize) -> usize {
    max_concurrent.clamp(1, dr_strange_mcp::DEFAULT_TOOL_CONCURRENCY)
}

/// The MCP endpoint (ROADMAP §10): the same [`DrStrange`] tool set
/// `drsg-mcp` serves over stdio, mounted here over Streamable HTTP so several
/// agent hosts can share this process's `Database` instead of each opening
/// the file directly. One [`DrStrange`] instance per MCP session — cheap,
/// since all real state lives in the shared `Arc<Database>` each clone
/// points at; `write_gate` inside it, not anything here, is what serializes
/// concurrent writers.
///
/// The digest tuning resolved from `[digest]` is handed to every session, so
/// the `digest` tool and `POST /rpc digest.run` obey the same `concurrency`
/// and `chunk_chars` — an operator lowering `concurrency` to stay under a
/// provider's rate limit should not find one of the two surfaces ignoring it.
fn mcp_router(
    state: Arc<AppState>,
    max_concurrent: usize,
    embed: Option<(String, Option<String>, Option<String>)>,
    source_root: Option<std::path::PathBuf>,
) -> Router<Arc<AppState>> {
    let db = state.db.clone();
    let digest = dr_strange_mcp::DigestTuning {
        chunk_chars: state.digest.chunk_chars,
        concurrency: state.digest.concurrency,
    };
    // One gate for the whole process, cloned into every session. Per-session
    // would bound nothing: MCP puts no limit on how many sessions a client
    // opens, so N sessions would multiply the ceiling by N.
    let tools = Arc::new(tokio::sync::Semaphore::new(mcp_tool_concurrency(
        max_concurrent,
    )));
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = Some(MCP_SESSION_IDLE);
    sessions.session_config.init_timeout = Some(MCP_SESSION_INIT);
    let service = StreamableHttpService::new(
        move || {
            let mut svc = DrStrange::with_digest(db.clone(), digest).with_tool_gate(tools.clone());
            if let Some(root) = &source_root {
                svc = svc.with_source_root(root.clone());
            }
            if let Some((provider, model, key_env)) = &embed {
                svc = svc.with_embed_provider(dr_strange_mcp::EmbedProvider {
                    provider: provider.clone(),
                    model: model.clone(),
                    key_env: key_env.clone(),
                });
            }
            Ok(svc)
        },
        Arc::new(sessions),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .route_service("/mcp", service)
        // `DefaultBodyLimit` cannot cover this route: axum implements it as a
        // request extension that only extractors calling `with_limited_body`
        // consult, and `route_service` hands the raw `Request<Body>` to
        // `StreamableHttpService`, which buffers it itself. Limiting the body
        // rather than the extractor is what actually bounds it — otherwise an
        // authenticated caller can POST gigabytes here that `/rpc` would have
        // refused at 64 MiB, and OOM the process holding the database.
        .route_layer(RequestBodyLimitLayer::new(MAX_BODY))
        .route_layer(middleware::from_fn_with_state(state, mcp_auth))
}

fn router(
    state: Arc<AppState>,
    max_concurrent: usize,
    embed: Option<(String, Option<String>, Option<String>)>,
    source_root: Option<std::path::PathBuf>,
) -> Router {
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
        .merge(mcp_router(
            state.clone(),
            max_concurrent,
            embed,
            source_root,
        ))
        .route("/rpc", post(rpc_http))
        .route("/ws", get(ws_upgrade))
        .route("/digest/extract", post(extract_http))
        // POST for the same reason as /export below: the Origin header the
        // local-UI check needs is omitted on same-origin GETs.
        .route("/digest/fetch", post(fetch_http))
        // POST (not GET): browsers omit the Origin header on same-origin GETs,
        // so the local-UI Origin check can't see it and a tokenless server
        // would 401 its own UI. POST always carries Origin.
        .route("/export", post(export_http))
        // `serve --follow` (arch/01 §9): the one-shot bootstrap bundle and
        // the live WAL tail. GET is fine for both — unlike /export/cypher,
        // callers here are always a follower process carrying an explicit
        // bearer token, never a same-origin browser GET relying on the
        // Origin-based local-UI bypass.
        .route("/snapshot", get(snapshot_http))
        .route("/ws/wal", get(ws_wal_upgrade))
        // POST: the query text is the body; kept off /rpc (and thus the OpenRPC
        // schema / SDKs) as a web-only surface, like /export.
        .route("/cypher", post(cypher_http))
        .route("/cypher/complete", post(complete_http))
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
        // No progress messages: conversion is a single fast call now (anydoc
        // is milliseconds where page-by-page PDF extraction was seconds), so
        // the page shows an indeterminate loading state rather than a bar that
        // would jump straight to 100. The stream stays NDJSON because the
        // crawl endpoint shares this reader and does still report progress.
        match dr_strange_llm::to_markdown(&q.name, &body) {
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
struct FetchQuery {
    /// The address to read. A bare `example.com/x` is read as https.
    #[serde(default)]
    url: String,
    /// Sharpens what the crawl counts as relevant, beyond what the root page
    /// says about itself.
    topic: Option<String>,
    /// Per-request budgets, each bounded by the server's configured ceiling —
    /// a client may ask for less, never for more.
    pages: Option<usize>,
    depth: Option<usize>,
}

/// `POST /digest/fetch?url=…` — read a page and, under a budget, the pages it
/// links to (ROADMAP §9). No DB access; the crawl runs on a blocking task.
///
/// Newline-delimited JSON so the digest page can show progress through what is
/// a genuinely slow operation:
///   `{"progress":{"done":2,"total":8,"url":"…"}}` — zero or more
///   `{"pages":[…],"dropped":[…]}`                 — the final result, or
///   `{"error":"…"}`                               — a terminal failure
///
/// Every page is returned with its own Markdown block and relevance score; the
/// caller chooses which to keep and joins their blocks. Nothing here decides
/// what the model reads — it decides what the reader is offered.
async fn fetch_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<FetchQuery>,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    // Read level: fetching touches no graph state. It does spend the server's
    // network, which is why it is authenticated at all.
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if !state.fetch.enabled {
        return (
            StatusCode::FORBIDDEN,
            "URL fetching is disabled on this server ([fetch] enabled = false)",
        )
            .into_response();
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let cfg = state.fetch.clone();
    tokio::task::spawn_blocking(move || {
        let send = |v: serde_json::Value| {
            let mut line = serde_json::to_vec(&v).unwrap_or_default();
            line.push(b'\n');
            tx.blocking_send(Ok(Bytes::from(line))).is_ok()
        };
        let opts = match fetch_options(&cfg, &q) {
            Ok(o) => o,
            Err(e) => {
                send(json!({ "error": e.to_string() }));
                return;
            }
        };
        let result = {
            let mut on_page = |p: crate::fetch::Progress| {
                send(json!({ "progress": p }));
            };
            crate::fetch::fetch_with_progress(&q.url, &opts, &mut on_page)
        };
        match result {
            Ok(got) => send(json!({ "pages": got.pages, "dropped": got.dropped })),
            Err(e) => send(json!({ "error": format!("{e:#}") })),
        };
    });

    Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap()
}

/// How far a crawl follows links when the request does not say. One hop: the
/// material a page points at is almost always one step away, and further hops
/// multiply requests for a rapidly thinning return.
const DEFAULT_FETCH_DEPTH: usize = 1;

/// Merge the request's budgets with the server's, clamping rather than
/// trusting: a client may ask for a smaller crawl, never a larger one.
fn fetch_options(
    cfg: &crate::FetchDefaults,
    q: &FetchQuery,
) -> anyhow::Result<crate::fetch::FetchOptions> {
    let allow_private = cfg
        .allow_private
        .iter()
        .map(|s| crate::fetch::Prefix::parse(s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let topic = q
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok(crate::fetch::FetchOptions {
        topic,
        max_pages: q.pages.unwrap_or(cfg.max_pages).clamp(1, cfg.max_pages),
        max_depth: q.depth.unwrap_or(DEFAULT_FETCH_DEPTH).min(cfg.max_depth),
        concurrency: cfg.concurrency,
        allow_private,
        ..Default::default()
    })
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

/// `GET /snapshot` — the one-shot bootstrap bundle for `serve --follow`
/// (arch/01 §9): the whole database, id-faithful, at one commit sequence
/// (`Database::snapshot`, ROADMAP §6 — unchanged, just given a wire). Same
/// full-buffer-then-respond shape as `export_http`: this project's scale
/// doesn't yet need chunked transfer.
async fn snapshot_http(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let built = tokio::task::spawn_blocking({
        let state = state.clone();
        move || -> Result<Vec<u8>, dr_strange_core::Error> {
            let mut buf = Vec::new();
            state.db.snapshot(&mut buf)?;
            Ok(buf)
        }
    })
    .await;
    match built {
        Ok(Ok(bytes)) => {
            tracing::info!(bytes = bytes.len(), "served a replication snapshot");
            Response::builder()
                .header("content-type", "application/octet-stream")
                .body(Body::from(bytes))
                .unwrap()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "snapshot export failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(_) => {
            tracing::error!("snapshot export task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "snapshot export task failed",
            )
                .into_response()
        }
    }
}

/// `GET /ws/wal` — the live tail for `serve --follow` (arch/01 §9): every
/// commit's raw ops, forwarded verbatim from [`AppState::wal_changes`] to
/// whichever follower is subscribed. No bootstrap data ever rides this
/// socket — `/snapshot` is the one-shot bundle; this is purely the ongoing
/// feed, subscribed to *before* a follower pulls its snapshot so nothing
/// committed in between is ever missed (the same ordering `/ws`'s
/// `plane.watch` already relies on).
async fn ws_wal_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, q.token) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    ws.on_upgrade(move |socket| ws_wal_task(socket, state))
}

async fn ws_wal_task(mut socket: WebSocket, state: Arc<AppState>) {
    let mut changes = state.wal_changes.subscribe();
    loop {
        match changes.recv().await {
            Ok(batch) => {
                let bytes = match postcard::to_stdvec(&*batch) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to encode a replicated batch");
                        continue;
                    }
                };
                if socket.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
            // A follower that falls too far behind loses the overflow, same
            // best-effort posture as the `changes` feed — but for WAL
            // replication that's not survivable (every op matters), so it
            // must trigger a fresh resync rather than silently continuing.
            Err(broadcast::error::RecvError::Lagged(_)) => {
                tracing::warn!("follower fell behind the WAL feed; closing so it resyncs");
                break;
            }
            Err(broadcast::error::RecvError::Closed) => break,
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
    /// The page to return: where to start, and how many rows. The dashboard
    /// shows a screenful at a time — see [`PAGE`].
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    /// Vector properties and columns as markers rather than floats — the
    /// default, and what makes an answer readable. `lean=false` is how the
    /// dashboard's "1024 dims" button asks for the one row behind it.
    #[serde(default)]
    lean: Option<bool>,
}

/// Rows per page when the dashboard does not say otherwise.
///
/// A query answers with as many rows as it matches; a reader looks at a
/// screenful. The two were the same number until a pattern over a real
/// codebase answered with four thousand functions, each carrying its own
/// source — eleven megabytes to ship, parse and lay out, for a table nobody
/// scrolls to the end of. The count of what matched is still returned whole,
/// so the page says what it is a page of.
const PAGE: usize = 200;

/// `POST /cypher?plane=startup` — the query text in the body, run against the
/// plane. A read returns `{nodes, edges, count}` (the result set + induced
/// edges) for the plot; a write (`CREATE`, …) mutates and returns its
/// change-counts. A read is **paged**: `offset` and `limit` (200 by default)
/// say which rows come back, and `total` says how many there were.
/// **Write-gated**: the language can mutate, so this needs write
/// authorization even for a read query (the single-token model collapses the
/// levels anyway; the browser UI is write-capable). Runs on a blocking task; a
/// parse/compile error comes back as 400 with the message.
///
/// Nodes come back **lean** by default — a vector property is the marker
/// `$vector(N dims, omitted)` rather than N floats. The dashboard renders no
/// embeddings anywhere, and shipping them is not a rounding error: a query
/// matching twelve hundred vectorized nodes answered in 35.9 MB, of which
/// 34 MB were floats no one reads, and the browser spent minutes parsing
/// them. The same query lean is 1.6 MB. `plane.cypher` keeps the choice for
/// callers that want the vectors themselves.
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
    let plane = q.plane.clone();
    let embed = q.embed.clone().unwrap_or_else(|| "openai".to_string());

    let built = tokio::task::spawn_blocking({
        let state = state.clone();
        let plane = plane.clone();
        // The SPA doesn't send params; plane.cypher does (methods::plane_cypher).
        move || {
            methods::cypher_subgraph(
                &state.ctx(),
                &plane,
                &query,
                &embed,
                &Default::default(),
                q.lean.unwrap_or(true),
                methods::Page {
                    offset: q.offset.unwrap_or(0),
                    limit: Some(q.limit.unwrap_or(PAGE)),
                },
            )
        }
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

#[derive(serde::Deserialize)]
struct CompleteQuery {
    #[serde(default)]
    plane: String,
}

/// `POST /cypher/complete?plane=startup` — the query text **up to the caret**
/// in the body; what may follow it comes back as JSON (see
/// [`methods::completion`]).
///
/// **Read-gated**, unlike `/cypher`: this runs nothing at all. It reads the
/// plane's shape — its labels, edge types and properties, with their counts —
/// and says which of them could come next, so a query is written against the
/// graph that exists rather than against one the author remembers.
///
/// Advisory by nature, but not silent: a plane that cannot be resolved is a
/// 400 rather than an empty list, because "no suggestions" and "no such
/// plane" are different things and only one of them is worth fixing.
async fn complete_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<CompleteQuery>,
    body: Bytes,
) -> Response {
    let creds = match resolve_credentials(&state, &headers, None) {
        Ok(c) => c,
        Err(resp) => return *resp,
    };
    if !state.authorizer.allows(Access::Read, &creds) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let prefix = match String::from_utf8(body.to_vec()) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "query body must be UTF-8").into_response(),
    };
    let built = tokio::task::spawn_blocking({
        let state = state.clone();
        let plane = q.plane.clone();
        move || {
            state
                .vocab(&plane)
                .map(|vocab| methods::completion(&prefix, &vocab))
        }
    })
    .await;

    match built {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, e.message).into_response(),
        Err(_) => {
            tracing::error!(plane = %q.plane, "completion task panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, "completion task failed").into_response()
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

/// Why [`run`] returned: a normal shutdown (Ctrl-C / SIGTERM), or — only ever
/// in `--follow` mode — the replication stream from the master was lost.
/// The caller (the CLI's `Serve` handler) matches on this to decide whether
/// to reopen a fresh, empty database and call [`crate::serve`] again to
/// resync from scratch, per arch/01 §9's "every reconnect is a full resync"
/// design.
pub enum ServeOutcome {
    Stopped,
    ResyncNeeded,
}

/// Runs the server until Ctrl-C — or, in `--follow` mode, until the
/// replication stream is lost. Owns the tokio runtime setup's payload; the
/// synchronous `serve` wrapper in `lib.rs` drives it with `block_on`.
pub async fn run(
    db: Database,
    db_path: Option<PathBuf>,
    opts: ServeOptions,
) -> anyhow::Result<ServeOutcome> {
    startup_banner();
    // Read the secret once so the checker (`authorizer`) and the SPA's injected
    // copy (`bootstrap_token`) can never disagree.
    let token = std::env::var("DRSG_TOKEN").ok().filter(|t| !t.is_empty());
    let shared_token = SharedToken::new(token.clone());
    if shared_token.is_configured() {
        tracing::info!(
            "auth ENABLED — every request requires DRSG_TOKEN (Authorization: Bearer <token>; WebSocket via ?token=<token>)"
        );
    } else {
        tracing::warn!(
            "no DRSG_TOKEN set; the API is reachable only from the local browser UI. Set DRSG_TOKEN to allow programmatic (SDK / curl) access."
        );
    }
    // `serve --follow` (arch/01 §9): every write RPC is refused regardless of
    // token — a third, orthogonal layer alongside the Origin guard and the
    // bearer token itself.
    let authorizer: Arc<dyn Authorizer> = if opts.follow.is_some() {
        tracing::info!("read-only replica (serve --follow): all write RPCs are refused");
        Arc::new(ReadOnlyAuthorizer(shared_token))
    } else {
        Arc::new(shared_token)
    };
    // Change feed (ROADMAP §5): publish every committed ChangeSet to a
    // broadcast channel that `/ws` subscribers drain. Registered before the db
    // is shared, and best-effort — `send` failing (no live subscriber) is fine.
    let (changes, _) = broadcast::channel::<Arc<ChangeSet>>(CHANGE_FEED_CAPACITY);
    {
        let tx = changes.clone();
        db.on_change(move |cs| {
            let _ = tx.send(Arc::new(cs));
        });
    }
    // Raw-WAL replication feed (arch/01 §9): every commit's ops, for any
    // `/ws/wal` subscriber. Native-only; a redb/in-memory database (existing
    // tests use `Database::in_memory()`) simply has no feed to publish —
    // fine, unless this process itself is meant to be followed by someone
    // else, which `--follow` never is (a follower doesn't serve its own
    // followers in this design), so that combination can't arise.
    let (wal_changes, _) = broadcast::channel::<Arc<ReplicatedBatch>>(CHANGE_FEED_CAPACITY);
    #[cfg(feature = "native-backend")]
    {
        let tx = wal_changes.clone();
        if let Err(e) = db.on_wal_commit(move |batch| {
            let _ = tx.send(Arc::new(batch));
        }) {
            tracing::debug!(error = %e, "WAL replication feed unavailable for this database");
        }
    }
    if opts.follow.is_some() && !cfg!(feature = "native-backend") {
        anyhow::bail!("serve --follow requires the native-backend feature");
    }
    // History retention (see `ServeOptions::retain_commits`): bound how far
    // back time-travel reaches, so a long-lived server's store stays near the
    // size of what it holds now. Native-only — the other engines keep no
    // versions to bound. Set before the replica path too: a follower applies
    // its master's commits through the same engine and compacts the same way.
    #[cfg(feature = "native-backend")]
    {
        db.set_retention(opts.retain_commits);
        match opts.retain_commits {
            Some(n) => tracing::info!(
                commits = n,
                "history retention: time-travel reaches this many commits back; older versions are reclaimed at compaction"
            ),
            None => tracing::info!(
                "history retention: unbounded — every version ever written stays on disk and in every compaction"
            ),
        }
    }

    // `serve --follow`: bootstrap from the master before this server ever
    // answers a request — an empty database serving reads would just be
    // wrong, not merely stale. Subscribes to `/ws/wal` before pulling the
    // snapshot (see `follow::bootstrap`), so nothing committed on the master
    // during the pull is lost. `mod follow` (and so this block) only exists
    // under `native-backend`; the bail-out above already guarantees
    // `opts.follow` is `None` whenever it's compiled out.
    let resync_needed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let follow_lost = Arc::new(tokio::sync::Notify::new());
    #[cfg(feature = "native-backend")]
    if let Some(follow_opts) = opts.follow.clone() {
        let bootstrapped = follow::bootstrap(&db, &follow_opts)
            .await
            .context("bootstrapping from the master")?;
        tracing::info!(
            seq = bootstrapped.stats.seq,
            nodes = bootstrapped.stats.nodes,
            edges = bootstrapped.stats.edges,
            "resynced from master"
        );
        let db_for_tail = Arc::new(db);
        tokio::spawn(follow::run_live_tail(
            db_for_tail.clone(),
            bootstrapped.batches,
            resync_needed.clone(),
            follow_lost.clone(),
        ));
        let state = Arc::new(AppState {
            db: db_for_tail,
            db_path,
            authorizer,
            origins: AllowedOrigins::from_env(),
            bootstrap_token: token,
            changes,
            wal_changes,
            digest: opts.digest,
            fetch: opts.fetch.clone(),
            query_timeout: opts.query_timeout,
            vocab_cache: Mutex::new(None),
        });
        return run_app(state, opts, &resync_needed, &follow_lost).await;
    }
    let state = Arc::new(AppState {
        db: Arc::new(db),
        db_path,
        authorizer,
        origins: AllowedOrigins::from_env(),
        bootstrap_token: token,
        changes,
        wal_changes,
        digest: opts.digest,
        fetch: opts.fetch.clone(),
        query_timeout: opts.query_timeout,
        vocab_cache: Mutex::new(None),
    });
    run_app(state, opts, &resync_needed, &follow_lost).await
}

/// The rest of `run`, shared by the follow and non-follow paths once
/// `AppState` exists: start `on_start`, build the router, bind, and serve
/// until shutdown or (`--follow` only) `follow_lost`.
async fn run_app(
    state: Arc<AppState>,
    mut opts: ServeOptions,
    resync_needed: &std::sync::atomic::AtomicBool,
    follow_lost: &tokio::sync::Notify,
) -> anyhow::Result<ServeOutcome> {
    // The caller's background task (e.g. the repository watcher behind
    // `drsg serve watch`) gets its own handle to the shared database. A plain
    // thread rather than a tokio task: the work is blocking (git, parsing,
    // write transactions) and must not stall the request executor.
    if let Some(on_start) = opts.on_start.take() {
        let db = state.db.clone();
        std::thread::spawn(move || on_start(db));
    }
    // Held past the serve future: the watcher thread's Arc clone keeps the
    // database's Drop from ever running, so the sidecars must be saved
    // explicitly at shutdown or every restart rebuilds the indexes.
    let db_at_shutdown = state.db.clone();
    let app = router(
        state,
        opts.max_concurrent,
        opts.embed_provider.clone(),
        opts.source_root.clone(),
    );
    // Bind a std listener up front so we can report the actual port (handy when
    // the caller asked for :0) before either serving path takes over. Both paths
    // register it with tokio, which rejects a blocking fd — so make it
    // non-blocking here, once.
    let std_listener = std::net::TcpListener::bind(opts.addr)?;
    std_listener.set_nonblocking(true)?;
    let bound = std_listener.local_addr()?;
    let scheme = if opts.tls.is_some() { "https" } else { "http" };
    tracing::info!(
        %bound,
        max_concurrent = opts.max_concurrent,
        "drsg serve: dashboard + JSON-RPC listening on {scheme}://{bound}"
    );
    let served = match opts.tls {
        Some(tls) => serve_tls(app, std_listener, tls, follow_lost).await,
        None => {
            let listener = tokio::net::TcpListener::from_std(std_listener)?;
            // Bound the drain, as the TLS path already does. `axum::serve`
            // waits for in-flight connections forever, and `/mcp` holds a
            // standalone SSE stream for the life of an agent session with a
            // 15s keep-alive, so it is never idle and its body never ends: an
            // attached editor would leave Ctrl-C hanging indefinitely. Worse
            // since 1.4.2, because the database lock is held for the process's
            // lifetime, so no replacement can start until this one dies.
            let (fired_tx, fired_rx) = tokio::sync::oneshot::channel();
            let serve = std::future::IntoFuture::into_future(
                axum::serve(listener, app).with_graceful_shutdown(async move {
                    shutdown_signal().await;
                    let _ = fired_tx.send(());
                }),
            );
            tokio::pin!(serve);
            tokio::select! {
                res = &mut serve => res?,
                _ = async move {
                    let _ = fired_rx.await;
                    tokio::time::sleep(DRAIN_GRACE).await;
                } => {
                    tracing::warn!(
                        grace = ?DRAIN_GRACE,
                        "connections still open after the drain deadline; exiting anyway"
                    );
                }
                // `--follow` losing its replication stream: resync as soon as
                // possible rather than waiting out a graceful drain — unlike
                // Ctrl-C, this isn't an operator-requested shutdown, so
                // in-flight reads are cut short rather than awaited.
                _ = follow_lost.notified() => {
                    tracing::warn!("replication stream lost; ending this session to resync");
                }
            }
            Ok(())
        }
    };
    db_at_shutdown.save_sidecars();
    tracing::info!("index sidecars saved; next open loads instead of rebuilding");
    served?;
    Ok(
        if resync_needed.load(std::sync::atomic::Ordering::Relaxed) {
            ServeOutcome::ResyncNeeded
        } else {
            ServeOutcome::Stopped
        },
    )
}

/// Serve HTTPS on an already-bound listener, terminating TLS with rustls.
/// Graceful shutdown is wired to the same signal as the plain-HTTP path.
async fn serve_tls(
    app: Router,
    listener: std::net::TcpListener,
    tls: crate::TlsOptions,
    follow_lost: &tokio::sync::Notify,
) -> anyhow::Result<()> {
    use axum_server::tls_rustls::RustlsConfig;

    // Install a process-default rustls crypto provider (ring). Idempotent — the
    // error just means one is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = RustlsConfig::from_pem_file(&tls.cert, &tls.key)
        .await
        .with_context(|| {
            format!(
                "loading TLS certificate {} / key {}",
                tls.cert.display(),
                tls.key.display()
            )
        })?;

    // axum_server drains in-flight connections when the handle is triggered.
    let handle = axum_server::Handle::new();
    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown_signal().await;
            handle.graceful_shutdown(Some(DRAIN_GRACE));
        }
    });
    let serve = axum_server::from_tcp_rustls(listener, config)?
        .handle(handle)
        .serve(app.into_make_service());
    tokio::select! {
        res = serve => res?,
        // `--follow` losing its replication stream: same immediate-not-
        // graceful posture as the plain-HTTP path (see there for why).
        _ = follow_lost.notified() => {
            tracing::warn!("replication stream lost; ending this session to resync");
        }
    }
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
    // Change-feed subscription (ROADMAP §5): a receiver is always live so no
    // events are missed between `plane.watch` and the first select; `watch`
    // holds the active filter — `(plane id, plane name, optional label)`.
    let mut changes = state.changes.subscribe();
    let mut watch: Option<(PlaneId, String, Option<String>)> = None;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(note) = stats_notification(&state).await
                    && socket.send(Message::Text(note.into())).await.is_err()
                {
                    break;
                }
            }
            // A committed change set: forward it if this connection is watching
            // its plane and the filter matches. Lagged = dropped overflow
            // (best-effort); Closed = the sender is gone (shouldn't happen).
            recv = changes.recv() => {
                match recv {
                    Ok(cs) => {
                        if let Some((pid, name, label)) = &watch
                            && cs.plane == *pid
                            && let Some(msg) = methods::change_message(&cs, name, label.as_deref())
                            && socket.send(Message::Text(msg.into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        // `plane.watch` / `plane.unwatch` are per-connection and
                        // stateful, so they can't go through the stateless RPC
                        // dispatch — handle them here; everything else is a
                        // normal request/response.
                        if let Some(reply) = handle_ws_subscription(&state, &mut watch, &text).await {
                            if socket.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
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

/// Handle a `plane.watch` / `plane.unwatch` control message on a WebSocket,
/// mutating this connection's `watch` filter. Returns the JSON-RPC ack to send
/// back, or `None` if the message isn't a subscription control (so the caller
/// falls through to normal dispatch). Read-authorized already at upgrade.
async fn handle_ws_subscription(
    state: &Arc<AppState>,
    watch: &mut Option<(PlaneId, String, Option<String>)>,
    text: &str,
) -> Option<String> {
    let msg: serde_json::Value = serde_json::from_str(text).ok()?;
    let method = msg.get("method").and_then(|m| m.as_str())?;
    let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let params = msg
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let result = match method {
        "plane.watch" => {
            let plane = params.get("plane").and_then(|p| p.as_str());
            let label = params
                .get("label")
                .and_then(|l| l.as_str())
                .map(str::to_string);
            match plane {
                None => Err("plane.watch requires a `plane`"),
                Some(name) => {
                    // Resolve name → id once, so the hot forward path is a
                    // cheap integer compare.
                    let name = name.to_string();
                    let st = state.clone();
                    let want = name.clone();
                    let pid = tokio::task::spawn_blocking(move || {
                        st.db.plane(&want).map(|h| h.id()).ok()
                    })
                    .await
                    .ok()
                    .flatten();
                    match pid {
                        Some(pid) => {
                            *watch = Some((pid, name.clone(), label.clone()));
                            Ok(json!({ "watching": name, "label": label }))
                        }
                        None => Err("no such plane"),
                    }
                }
            }
        }
        "plane.unwatch" => {
            *watch = None;
            Ok(json!({ "watching": serde_json::Value::Null }))
        }
        _ => return None, // not a subscription control — fall through to dispatch
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "result": value, "id": id }).to_string(),
        Err(message) => {
            json!({ "jsonrpc": "2.0", "error": { "code": -32602, "message": message }, "id": id })
                .to_string()
        }
    })
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
