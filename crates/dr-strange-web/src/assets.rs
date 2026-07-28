//! The bundled single-page app (arch/08 §1 — "serving a bundled SPA", no
//! separate frontend deployment). The Svelte build output in `frontend/dist`
//! is compiled into the binary via `rust-embed`; `build.rs` guarantees the
//! folder exists so `cargo build` works without the JS toolchain.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Serves an embedded asset by path, falling back to `index.html` for any
/// unknown path so client-side routing (deep links, refresh) works.
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref())],
            content.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: hand back index.html for unmatched routes.
    match Assets::get("index.html") {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}
