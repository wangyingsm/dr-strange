//! Finding the `drsg serve … watch` a repository already runs, and standing
//! in front of it.
//!
//! A database may be opened directly by one process at a time, so the two
//! ways to reach a graph are in tension exactly where they are both most
//! likely: inside a repository prepared by `drsg init`, whose watch server
//! holds `./graph.drsg` while a host launches `drsg-mcp` in the same
//! directory. The stdio server used to lose that race and exit, which
//! reaches the host as a bare "connection closed".
//!
//! It doesn't have to be a race. `drsg init` writes the running server's URL
//! into the repository's `.mcp.json`, so the address is *declared*: this
//! module reads it, asks whether anything is answering there, and — when
//! something is — relays the host's stdio session to it instead of opening
//! the database at all. The host gets the tool set of the process that holds
//! the database, with its plane synced to the repository's commits; nothing
//! opens anything twice.
//!
//! A relay, not a second server: messages are forwarded as they are, in both
//! directions, so whatever tools that server has are the tools the host sees,
//! including ones this binary's own version has never heard of.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::service::{RoleClient, RoleServer};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use serde::Deserialize;

/// The file `drsg init` writes, and every MCP host reads.
const MCP_FILE: &str = ".mcp.json";

/// The server name `drsg init` writes. Preferred over any other drsg-ish
/// entry, since it is the one this project puts there itself.
const WATCH_SERVER: &str = "drsg-watch";

/// How long the liveness probe waits. Short: a `.mcp.json` naming a server
/// that died is the *common* case (the machine rebooted, the watch process
/// was killed), and the cost of finding out must not be a host that sits
/// there looking hung. A loopback server answers this route in microseconds.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// A drsg MCP server a repository declares in its `.mcp.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// The entry's name — for the log line that says what was joined.
    pub name: String,
    pub url: String,
    /// The `Authorization` header the entry carries, if any. `drsg init`
    /// writes a bearer token, and the server refuses reads without it.
    pub auth: Option<String>,
    /// The file this came from, so a surprising choice can be traced to the
    /// thing that declared it.
    pub source: PathBuf,
}

#[derive(Deserialize)]
struct McpFile {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, ServerEntry>,
}

#[derive(Deserialize)]
struct ServerEntry {
    /// Present on an HTTP entry, absent on a stdio one — which is how the two
    /// are told apart without trusting the optional `type` field.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// The drsg server declared nearest to `start`, walking up.
///
/// Up, because `drsg init` writes the file at the repository root while a
/// host may launch its server from anywhere inside — the same walk git does
/// to find its own directory. The first `.mcp.json` that names a drsg server
/// wins; a file that names none stops the search, because it is the
/// repository's answer and a parent's is not.
pub fn discover(start: &Path) -> Option<Upstream> {
    for dir in start.ancestors() {
        let path = dir.join(MCP_FILE);
        if !path.is_file() {
            continue;
        }
        // A malformed file is not worth an error: this is a best-effort
        // shortcut, and the embedded server behind it still works.
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

/// The drsg entry among a file's servers: the one `drsg init` writes, else
/// any other whose name says drsg (a hand-written entry, an older name).
/// `BTreeMap` keeps that second choice deterministic rather than
/// hash-ordered.
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

/// `GET /health` on the same origin — the server's cheap, unauthenticated
/// liveness route, which does no database work, so a probe cannot be starved
/// by a busy server and needs no token to succeed.
///
/// `None` when the URL is not one an origin can be taken from, which is
/// itself a reason not to relay.
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
/// Deliberately not an MCP `initialize`: that would open a session on the
/// server and abandon it, and it would make the probe cost grow with what
/// the server has to say about itself.
pub async fn alive(upstream: &Upstream, timeout: Duration) -> bool {
    let Some(url) = health_url(&upstream.url) else {
        return false;
    };
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return false;
    };
    matches!(client.get(url).send().await, Ok(r) if r.status().is_success())
}

/// Serve the host over stdio, forwarding every message to `upstream`.
///
/// Returns when either side closes: the host disconnecting (it is done with
/// us) or the server going away (it was restarted or killed). Both mean this
/// process is finished — a host that wants another session starts another
/// process, which discovers the world as it is *then*, and embeds the
/// database if the server really is gone.
pub async fn relay(upstream: &Upstream) -> anyhow::Result<()> {
    let (stdin, stdout) = rmcp::transport::stdio();
    relay_over(
        rmcp::transport::async_rw::AsyncRwTransport::new_server(stdin, stdout),
        upstream,
    )
    .await
}

/// [`relay`] over any host transport, so the pump can be driven by a pipe in
/// a test rather than by this process's own stdio.
pub async fn relay_over<H>(mut host: H, upstream: &Upstream) -> anyhow::Result<()>
where
    H: Transport<RoleServer>,
    H::Error: std::error::Error + Send + Sync + 'static,
{
    let mut config = StreamableHttpClientTransportConfig::with_uri(upstream.url.clone());
    // The header goes through verbatim rather than through the transport's
    // `auth_header`, which prepends `Bearer ` to whatever it is given: a
    // `.mcp.json` carries the finished header value (`drsg init` writes
    // `Bearer <token>`), and prepending would send `Bearer Bearer …`. Passing
    // it as written also lets a scheme this code has never heard of work.
    if let Some(auth) = &upstream.auth {
        match reqwest::header::HeaderValue::from_str(auth) {
            Ok(value) => {
                config
                    .custom_headers
                    .insert(reqwest::header::AUTHORIZATION, value);
            }
            // A header value with a newline in it is not one to send; the
            // server will refuse the session and say so, which is clearer
            // than a request this process mangled on the way out.
            Err(e) => tracing::warn!(error = %e, "ignoring an unusable Authorization header"),
        }
    }
    // The transport's own default client, rather than one built here: it
    // disables idle connection pooling for a latency reason particular to
    // this protocol, which a client of ours would silently undo.
    let mut server = StreamableHttpClientTransport::from_config(config);

    // A message-level pump, which is what makes this a relay rather than a
    // reimplementation: neither side's content is inspected, so a tool added
    // to the server tomorrow reaches the host through a binary built today.
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
        // The nearest file is the repository speaking. If it configures other
        // servers and not this one, a parent directory's file is not a better
        // answer — it is a different project's.
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
        // A stdio drsg entry names no URL — there is nothing to forward to,
        // and it is very likely this binary itself.
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

        // Without it, a drsg-ish name still counts — someone wrote it by hand.
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

    #[tokio::test]
    async fn nothing_answering_is_not_alive() {
        // Port 1 on loopback: refused immediately, which is the path a stale
        // `.mcp.json` takes.
        let upstream = Upstream {
            name: "drsg-watch".into(),
            url: "http://127.0.0.1:1/mcp".into(),
            auth: None,
            source: PathBuf::from(MCP_FILE),
        };
        assert!(!alive(&upstream, Duration::from_millis(500)).await);
    }
}
