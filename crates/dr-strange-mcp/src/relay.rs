//! Finding the `drsg serve … watch` a repository already runs, and standing
//! in front of it.
//!
//! One process at a time may open a database directly, and inside a
//! repository prepared by `drsg init` two want to: the watch server holds
//! `./graph.drsg` while a host launches `drsg-mcp` in the same directory. The
//! stdio server lost that race and exited, which reaches the host as a bare
//! "connection closed".
//!
//! `drsg init` writes the running server's URL into the repository's
//! `.mcp.json`, so the address is declared. This module reads it, asks whether
//! anything answers there, and if so relays the host's stdio session to it
//! instead of opening the database — the host talks to the process that holds
//! it, whose plane is synced to the repository's commits.
//!
//! A relay, not a second server: messages are forwarded unread in both
//! directions, so the host sees that server's tools, including ones this
//! binary has never heard of.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::service::{RoleClient, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use serde::Deserialize;

/// The file `drsg init` writes, and every MCP host reads.
const MCP_FILE: &str = ".mcp.json";

/// The server name `drsg init` writes, preferred over any other drsg-ish
/// entry.
const WATCH_SERVER: &str = "drsg-watch";

/// How long the liveness probe waits. Short, because a `.mcp.json` naming a
/// server that died is the common case and a host must not sit there looking
/// hung; a loopback server answers this route in microseconds.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A drsg MCP server a repository declares in its `.mcp.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// The entry's name, for the log line naming what was joined.
    pub name: String,
    pub url: String,
    /// The `Authorization` header the entry carries. `drsg init` writes a
    /// bearer token, without which the server refuses reads.
    pub auth: Option<String>,
    /// The file this came from, so a surprising choice can be traced.
    pub source: PathBuf,
}

#[derive(Deserialize)]
struct McpFile {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, ServerEntry>,
}

#[derive(Deserialize)]
struct ServerEntry {
    /// Present on an HTTP entry, absent on a stdio one: how the two are told
    /// apart without trusting the optional `type` field.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// The drsg server declared nearest to `start`, walking up.
///
/// Up, because `drsg init` writes the file at the repository root while a host
/// may launch from anywhere inside. The nearest `.mcp.json` is the answer: one
/// that names no drsg server stops the search rather than deferring to a
/// parent's, which belongs to a different project.
pub fn discover(start: &Path) -> Option<Upstream> {
    for dir in start.ancestors() {
        let path = dir.join(MCP_FILE);
        if !path.is_file() {
            continue;
        }
        // A malformed file falls through to the embedded server.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = serde_json::from_str::<McpFile>(&text) else {
            continue;
        };
        return pick(file, &path);
    }
    None
}

/// The drsg entry among a file's servers: the one `drsg init` writes, else any
/// other whose name says drsg. `BTreeMap` keeps that second choice
/// deterministic.
fn pick(file: McpFile, source: &Path) -> Option<Upstream> {
    let http = |name: &String, entry: &ServerEntry| {
        entry.url.as_ref().map(|url| Upstream {
            name: name.clone(),
            url: url.clone(),
            auth: entry
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone()),
            source: source.to_path_buf(),
        })
    };
    if let Some(entry) = file.servers.get(WATCH_SERVER)
        && let Some(up) = http(&WATCH_SERVER.to_string(), entry)
    {
        return Some(up);
    }
    file.servers
        .iter()
        .filter(|(name, _)| name.to_lowercase().contains("drsg"))
        .find_map(|(name, entry)| http(name, entry))
}

/// `GET /health` on the same origin — the server's unauthenticated liveness
/// route, which does no database work, so a busy server cannot starve the
/// probe and no token is needed.
///
/// `None` when the URL has no origin to take, which is itself a reason not to
/// relay.
pub fn health_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    match authority.is_empty() {
        true => None,
        false => Some(format!("{scheme}://{authority}/health")),
    }
}

/// Whether something is answering at `upstream`.
///
/// Not an MCP `initialize`: that would open a session and abandon it.
pub async fn alive(upstream: &Upstream, timeout: Duration) -> bool {
    let Some(url) = health_url(&upstream.url) else {
        return false;
    };
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return false;
    };
    matches!(client.get(url).send().await, Ok(r) if r.status().is_success())
}

/// The `Authorization` header to send, or `None` when the file's value cannot
/// be one (a newline in it, say) and the session will have to go unauthorized.
///
/// Sent verbatim rather than through the transport's `auth_header`, which
/// prepends `Bearer ` to whatever it is given: a `.mcp.json` carries the
/// finished value (`drsg init` writes `Bearer <token>`), so prepending would
/// send `Bearer Bearer …`. Verbatim also admits a scheme this code does not
/// know.
fn auth_header(value: &str) -> Option<reqwest::header::HeaderValue> {
    match reqwest::header::HeaderValue::from_str(value) {
        Ok(header) => Some(header),
        Err(e) => {
            tracing::warn!(error = %e, "ignoring an unusable Authorization header");
            None
        }
    }
}

/// Serve the host over stdio, forwarding every message to `upstream`.
///
/// Returns when either side closes — the host disconnecting, or the server
/// going away. A host that wants another session starts another process,
/// which discovers the world as it is then.
pub async fn relay(upstream: &Upstream) -> anyhow::Result<()> {
    let (stdin, stdout) = rmcp::transport::stdio();
    relay_over(
        rmcp::transport::async_rw::AsyncRwTransport::new_server(stdin, stdout),
        upstream,
    )
    .await
}

/// [`relay`] over any host transport, so a test can drive the pump over a
/// pipe rather than this process's stdio.
pub async fn relay_over<H>(mut host: H, upstream: &Upstream) -> anyhow::Result<()>
where
    H: Transport<RoleServer>,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    let mut config = StreamableHttpClientTransportConfig::with_uri(upstream.url.clone());
    if let Some(value) = upstream.auth.as_deref().and_then(auth_header) {
        config
            .custom_headers
            .insert(reqwest::header::AUTHORIZATION, value);
    }
    // The transport's default client: it disables idle connection pooling for
    // a latency reason particular to this protocol.
    let mut server = StreamableHttpClientTransport::from_config(config);

    // A message-level pump: neither side's content is inspected, so a tool
    // added to the server reaches a host through a binary built before it.
    loop {
        tokio::select! {
            from_host = host.receive() => match from_host {
                Some(message) => server.send(message).await?,
                None => break,
            },
            from_server = server.receive() => match from_server {
                Some(message) => host.send(message).await?,
                None => break,
            },
        }
    }
    let _ = host.close().await;
    let _ = Transport::<RoleClient>::close(&mut server).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(MCP_FILE);
        std::fs::write(&path, body).unwrap();
        path
    }

    const WATCH: &str = r#"{
      "mcpServers": {
        "drsg-watch": {
          "type": "http",
          "url": "http://127.0.0.1:44453/mcp",
          "headers": { "Authorization": "Bearer secret" }
        }
      }
    }"#;

    #[test]
    fn the_repositorys_own_declaration_is_found_from_any_depth() {
        let root = tempfile::tempdir().unwrap();
        let source = write(root.path(), WATCH);
        let deep = root.path().join("crates/dr-strange-mcp/src");
        std::fs::create_dir_all(&deep).unwrap();

        let found = discover(&deep).expect("walks up to the root");
        assert_eq!(
            found,
            Upstream {
                name: "drsg-watch".into(),
                url: "http://127.0.0.1:44453/mcp".into(),
                auth: Some("Bearer secret".into()),
                source,
            }
        );
    }

    #[test]
    fn a_file_that_names_no_drsg_server_is_the_answer_not_a_step() {
        // The nearest file is this repository's answer; a parent's belongs to
        // a different project.
        let root = tempfile::tempdir().unwrap();
        write(root.path(), WATCH);
        let inner = root.path().join("vendor/other");
        std::fs::create_dir_all(&inner).unwrap();
        write(
            &inner,
            r#"{"mcpServers": {"sentry": {"url": "https://x/mcp"}}}"#,
        );
        assert_eq!(discover(&inner), None);
    }

    #[test]
    fn only_a_server_with_a_url_can_be_relayed_to() {
        let root = tempfile::tempdir().unwrap();
        // A stdio entry names no URL to forward to, and is likely this binary.
        write(
            root.path(),
            r#"{"mcpServers": {"drsg": {"command": "drsg-mcp"}}}"#,
        );
        assert_eq!(discover(root.path()), None);
    }

    #[test]
    fn the_name_drsg_init_writes_wins_over_another_drsg_entry() {
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            r#"{"mcpServers": {
                 "drsg-old": {"url": "http://127.0.0.1:1/mcp"},
                 "drsg-watch": {"url": "http://127.0.0.1:2/mcp"}
               }}"#,
        );
        let found = discover(root.path()).unwrap();
        assert_eq!((found.name.as_str(), found.auth), ("drsg-watch", None));

        // Without it, a hand-written drsg-ish name still counts.
        let root = tempfile::tempdir().unwrap();
        write(
            root.path(),
            r#"{"mcpServers": {"my-drsg": {"url": "http://127.0.0.1:3/mcp"}}}"#,
        );
        assert_eq!(discover(root.path()).unwrap().name, "my-drsg");
    }

    #[test]
    fn a_malformed_file_falls_back_rather_than_failing() {
        let root = tempfile::tempdir().unwrap();
        write(root.path(), "{ not json");
        assert_eq!(discover(root.path()), None);
    }

    #[test]
    fn the_probe_asks_the_same_origin_for_health() {
        assert_eq!(
            health_url("http://127.0.0.1:44453/mcp").as_deref(),
            Some("http://127.0.0.1:44453/health")
        );
        assert_eq!(
            health_url("https://host.example/deep/path?x=1").as_deref(),
            Some("https://host.example/health")
        );
        assert_eq!(
            health_url("http://host").as_deref(),
            Some("http://host/health")
        );
        assert_eq!(health_url("not a url"), None);
        assert_eq!(health_url("http://"), None);
    }

    #[test]
    fn an_authorization_header_goes_through_as_written_or_not_at_all() {
        assert_eq!(
            auth_header("Bearer secret").unwrap().to_str().unwrap(),
            "Bearer secret"
        );
        // Any scheme, since the file carries the finished value.
        assert!(auth_header("Basic dXNlcjpwdw==").is_some());
        // A newline cannot be a header value; the session goes unauthorized
        // and the server says so, rather than this process mangling it.
        assert!(auth_header("Bearer two\nlines").is_none());
    }

    #[tokio::test]
    async fn nothing_answering_is_not_alive() {
        // Port 1 on loopback: refused immediately, as a stale `.mcp.json` is.
        let upstream = Upstream {
            name: "drsg-watch".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            auth: None,
            source: PathBuf::from(MCP_FILE),
        };
        assert!(!alive(&upstream, Duration::from_millis(500)).await);

        // A URL with no origin to probe is not alive either — there is
        // nowhere to ask.
        let unusable = Upstream {
            url: "not a url".into(),
            ..upstream
        };
        assert!(!alive(&unusable, Duration::from_millis(500)).await);
    }
}
