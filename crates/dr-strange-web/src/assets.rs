//! The bundled single-page app (arch/08 §1 — "serving a bundled SPA", no
//! separate frontend deployment). The Svelte build output in `frontend/dist`
//! is compiled into the binary via `rust-embed`; `build.rs` guarantees the
//! folder exists so `cargo build` works without the JS toolchain.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::server::AppState;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Serves an embedded asset by path, falling back to `index.html` for any
/// unknown path so client-side routing (deep links, refresh) works. The HTML
/// entry point gets the auth bootstrap token spliced in (see [`inject_token`]).
pub async fn static_handler(State(state): State<Arc<AppState>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(content) = Assets::get(path) {
        if path.ends_with(".html") {
            return html_response(&content.data, state.bootstrap_token.as_deref());
        }
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [(header::CONTENT_TYPE, mime.as_ref())],
            content.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: hand back index.html for unmatched routes.
    match Assets::get("index.html") {
        Some(content) => html_response(&content.data, state.bootstrap_token.as_deref()),
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

/// Build the HTML response, splicing in the bootstrap token when one is set.
fn html_response(bytes: &[u8], token: Option<&str>) -> Response {
    let html = String::from_utf8_lossy(bytes);
    let body = match token {
        Some(t) => inject_token(&html, t),
        None => html.into_owned(),
    };
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Splice `<script>window.__DRSG_TOKEN__=…</script>` before `</head>` so the
/// SPA can read the shared token synchronously at load and send it as a bearer
/// credential (arch/08 security model).
///
/// Handing the token to our own same-origin page is safe: the only thing it
/// unlocks is `/rpc` + `/ws`, both behind the Origin guard, so a page from any
/// other origin — including a DNS-rebinding attacker that reads this HTML — is
/// refused when it tries to *use* the token (its `Origin` isn't loopback). The
/// value is JSON-encoded (a valid JS string literal) with `<` escaped so a
/// token can't break out of the script element.
fn inject_token(html: &str, token: &str) -> String {
    let encoded = serde_json::to_string(token)
        .unwrap_or_else(|_| "null".into())
        .replace('<', "\\u003c");
    let script = format!("<script>window.__DRSG_TOKEN__={encoded};</script>");
    match html.find("</head>") {
        Some(i) => {
            let mut out = String::with_capacity(html.len() + script.len());
            out.push_str(&html[..i]);
            out.push_str(&script);
            out.push_str(&html[i..]);
            out
        }
        // No <head> (a degenerate/placeholder doc) — prepend so it still runs.
        None => format!("{script}{html}"),
    }
}

#[cfg(test)]
mod tests {
    use super::inject_token;

    #[test]
    fn token_is_spliced_before_head_close() {
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = inject_token(html, "s3cret");
        assert!(out.contains(r#"window.__DRSG_TOKEN__="s3cret";"#));
        // Inserted inside <head>, before its close.
        assert!(out.find("__DRSG_TOKEN__").unwrap() < out.find("</head>").unwrap());
    }

    #[test]
    fn token_cannot_break_out_of_the_script() {
        // A hostile token can't close the <script> or the JS string.
        let out = inject_token("<head></head>", "a\"</script><b>");
        assert!(!out.contains("</script><b>"));
        assert!(out.contains("\\u003c/script>"));
    }

    #[test]
    fn no_head_prepends_script() {
        let out = inject_token("<body>hi</body>", "t");
        assert!(out.starts_with("<script>window.__DRSG_TOKEN__=\"t\";</script>"));
    }
}
