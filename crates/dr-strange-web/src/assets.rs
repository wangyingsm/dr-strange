//! The bundled single-page app (arch/08 §1 — "serving a bundled SPA", no
//! separate frontend deployment). The Svelte build output in `frontend/dist`
//! is compiled into the binary via `rust-embed`; `build.rs` guarantees the
//! folder exists so `cargo build` works without the JS toolchain.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::server::AppState;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Whether a path is asking for a built asset rather than a client-side route.
///
/// It matters because the SPA fallback must not answer these. A build gives
/// every asset a content hash, so after a rebuild the old names are simply
/// gone; falling back would hand the browser `index.html` — HTML, labelled
/// `text/html` — where it asked for JavaScript, and the page would fail with
/// nothing in the log to say why. A 404 is the honest answer.
fn is_asset_request(path: &str) -> bool {
    path.starts_with("assets/")
        || path
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains('.') && !last.ends_with(".html"))
}

/// What a path may be cached as.
///
/// `index.html` is the only unhashed file and the only thing pointing at the
/// hashed ones, so it must be revalidated on every load: served without a
/// directive it falls to the browser's heuristic freshness, and a rebuilt
/// server keeps handing back the previous bundle's name. The assets it points
/// at carry their content in their filename, so they can be kept forever.
fn cache_control_for(path: &str) -> &'static str {
    if is_asset_request(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Whether the page served to this peer may carry the bootstrap token.
///
/// `GET /` is unauthenticated — it has to be, it is how the browser obtains
/// the page that will authenticate — so whatever is in the page is public to
/// whoever can fetch it. That is acceptable only when "whoever" is a process
/// on this machine: the listener is loopback-bound *and* the connection came
/// from a loopback address. Both, not either — a loopback bind reached
/// through a forwarded port still shows a loopback peer, and a LAN bind
/// reached from the same host shows a loopback peer too, while the same page
/// is also being served to the network. An unknown peer counts as remote.
pub(crate) fn may_inject_token(bind_is_loopback: bool, peer: Option<IpAddr>) -> bool {
    bind_is_loopback && peer.is_some_and(|ip| ip.is_loopback())
}

/// Serves an embedded asset by path, falling back to `index.html` for any
/// unknown *route* so client-side routing (deep links, refresh) works. The HTML
/// entry point gets the auth bootstrap token spliced in (see [`inject_token`])
/// when [`may_inject_token`] allows; otherwise the SPA asks the user for it.
pub async fn static_handler(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    // Read from the extensions rather than the `ConnectInfo` extractor: the
    // extractor fails the whole request (a 500) when the service was built
    // without connect info, whereas "unknown peer" has a safe answer here.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let token = if may_inject_token(state.bind_is_loopback, peer) {
        state.bootstrap_token.as_deref()
    } else {
        None
    };

    if let Some(content) = Assets::get(path) {
        if path.ends_with(".html") {
            return html_response(&content.data, token);
        }
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_control_for(path)),
            ],
            content.data.into_owned(),
        )
            .into_response();
    }

    // A missing asset is a 404, not a route.
    if is_asset_request(path) {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

    // SPA fallback: hand back index.html for unmatched routes.
    match Assets::get("index.html") {
        Some(content) => html_response(&content.data, token),
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
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // Never cached: it is the only pointer to the hashed bundles, and a
            // stale copy asks for assets that no longer exist.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// The element the token rides in. The SPA reads
/// `document.querySelector('meta[name="drsg-token"]').content`.
pub const TOKEN_META_NAME: &str = "drsg-token";

/// Splice `<meta name="drsg-token" content="…">` before `</head>` so the SPA
/// can read the shared token synchronously at load and send it as a bearer
/// credential (arch/08 security model). A `<meta>` rather than an inline
/// `<script>`, so the page runs under a `script-src 'self'` policy with no
/// `'unsafe-inline'` — the one thing that keeps an injected script from
/// running should any renderer ever slip.
///
/// Handing the token to our own same-origin page is safe: the only thing it
/// unlocks is `/rpc` + `/ws`, both behind the Origin guard, so a page from any
/// other origin — including a DNS-rebinding attacker that reads this HTML — is
/// refused when it tries to *use* the token (its `Origin` isn't loopback). The
/// value is attribute-escaped so a token cannot close the element.
fn inject_token(html: &str, token: &str) -> String {
    let encoded = attr_escape(token);
    let meta = format!("<meta name=\"{TOKEN_META_NAME}\" content=\"{encoded}\">");
    match html.find("</head>") {
        Some(i) => {
            let mut out = String::with_capacity(html.len() + meta.len());
            out.push_str(&html[..i]);
            out.push_str(&meta);
            out.push_str(&html[i..]);
            out
        }
        // No <head> (a degenerate/placeholder doc) — prepend so it is still
        // in the document.
        None => format!("{meta}{html}"),
    }
}

/// Escape a string for a double-quoted HTML attribute value.
fn attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{cache_control_for, inject_token, is_asset_request, may_inject_token};

    #[test]
    fn a_built_asset_is_told_apart_from_a_client_side_route() {
        for asset in [
            "assets/index-BwXo5wVD.css",
            "assets/index-DEyNKplC.js",
            "magic-circle.svg",
            "favicon.ico",
        ] {
            assert!(is_asset_request(asset), "{asset} is an asset");
        }
        for route in ["", "explore", "plane/startup", "index.html", "deep/link"] {
            assert!(!is_asset_request(route), "{route} is a route");
        }
    }

    #[test]
    fn the_entry_point_is_never_cached_and_its_assets_are_cached_forever() {
        // index.html is the only unhashed file and the only thing naming the
        // hashed ones; a stale copy points at bundles that no longer exist.
        assert_eq!(cache_control_for("index.html"), "no-cache");
        assert!(cache_control_for("assets/index-DEyNKplC.js").contains("immutable"));
    }

    #[test]
    fn token_is_spliced_before_head_close() {
        let html = "<html><head><title>x</title></head><body></body></html>";
        let out = inject_token(html, "s3cret");
        assert!(out.contains(r#"<meta name="drsg-token" content="s3cret">"#));
        // Inserted inside <head>, before its close.
        assert!(out.find("drsg-token").unwrap() < out.find("</head>").unwrap());
        // Never as script: the page runs under `script-src 'self'`.
        assert!(!out.contains("<script"));
    }

    #[test]
    fn token_cannot_break_out_of_the_element() {
        // A hostile token can't close the attribute or open an element.
        let out = inject_token("<head></head>", "a\"><script>x</script><b>");
        assert!(!out.contains("<script>"));
        assert!(out.contains("content=\"a&quot;&gt;&lt;script&gt;x&lt;/script&gt;&lt;b&gt;\">"));
    }

    #[test]
    fn no_head_prepends_the_element() {
        let out = inject_token("<body>hi</body>", "t");
        assert!(out.starts_with("<meta name=\"drsg-token\" content=\"t\">"));
    }

    #[test]
    fn the_token_is_handed_only_to_a_loopback_peer_of_a_loopback_listener() {
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let local6: IpAddr = "::1".parse().unwrap();
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(may_inject_token(true, Some(local)));
        assert!(may_inject_token(true, Some(local6)));
        // A LAN bind never injects, even to a peer on this machine — the
        // same page is being served to the network.
        assert!(!may_inject_token(false, Some(local)));
        // A loopback bind reached from elsewhere (a forwarded port shows
        // a loopback peer, so this is the tunnelled/proxied shape) doesn't.
        assert!(!may_inject_token(true, Some(lan)));
        // No peer address at all is treated as remote.
        assert!(!may_inject_token(true, None));
    }
}
