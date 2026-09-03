//! End-to-end proof of the MCP endpoint (ROADMAP §10): a real `drsg serve` on
//! an ephemeral port, driven by real MCP clients over Streamable HTTP — not
//! raw HTTP requests, but `rmcp`'s own client transport, the way an agent
//! host actually talks to it. Proves the thing raw-HTTP smoke tests can't:
//! two independent MCP sessions (simulating two agent hosts, e.g. Claude
//! Code and Codex) reading and writing through `/mcp` see one shared
//! `Database`, not two private ones — the whole point of hosting the tool
//! set on `serve` instead of each host embedding its own.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use dr_strange_core::{Database, Properties};
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "test-mcp-token";

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Sets `DRSG_TOKEN` exactly once, before this binary starts any thread that
/// might read it.
///
/// The tests in this file are separate `#[tokio::test]`s but share one
/// process, and each spawns a server thread that calls `SharedToken::from_env`
/// / `AllowedOrigins::from_env`. A per-test `set_var` would therefore run
/// concurrently with another test's server thread calling `getenv` — under
/// glibc `setenv` may reallocate `environ`, so that is a real data race, and
/// the reason edition 2024 marks the function `unsafe`. It would surface as
/// an occasional segfault or as a server that reads no token and 401s
/// everything.
///
/// `Once` removes the race rather than documenting it: the first caller sets
/// the variable and every later caller blocks until that is finished, so the
/// single write happens-before every server thread this file creates.
///
/// SAFETY: no other thread exists that could be reading the environment —
/// `call_once` serialises the writers, and the only readers are the server
/// threads spawned below, after it returns.
fn set_token_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) });
}

/// Spins up a real server with a seeded node, gated by `DRSG_TOKEN` — the
/// realistic configuration for a programmatic client (ROADMAP §10's "token
/// posture" fork: with no token, `/mcp` refuses even reads, same as `/rpc`).
fn spawn_server() -> SocketAddr {
    set_token_once();
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    txn.create_node_with_key("alice", &["Person"], Properties::new())
        .unwrap();
    txn.commit().unwrap();

    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
}

/// Poll until the server accepts connections.
async fn wait_ready(addr: SocketAddr) {
    for _ in 0..80 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server never started listening");
}

type Client = RunningService<RoleClient, ClientInfo>;

/// A fresh MCP client session against `/mcp` — a new transport and a new
/// `initialize` handshake, simulating a separate agent host process rather
/// than reusing one connection for every call.
async fn connect(addr: SocketAddr, token: &str) -> anyhow::Result<Client> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
        .auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    Ok(ClientInfo::default().serve(transport).await?)
}

async fn call(client: &Client, tool: &str, args: Value) -> Value {
    let args = match args {
        Value::Object(m) => m,
        Value::Null => serde_json::Map::new(),
        other => panic!("tool args must be an object, got {other}"),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true), "{tool} failed: {result:?}");
    let text = &result.content[0].as_text().unwrap().text;
    serde_json::from_str(text).unwrap()
}

/// `drsg-mcp`'s relay: a host speaking stdio reaches the database this
/// running server holds, rather than opening one of its own.
///
/// The whole point of the shortcut, end to end — a `.mcp.json` naming this
/// server, the liveness probe, and the message pump — proved the way it is
/// actually used: an MCP client on one end of a pipe, the relay on the other,
/// and a tool call that can only be answered by *this* server's data (the
/// seeded `alice`, which a freshly opened database would not have).
#[tokio::test]
async fn the_stdio_relay_serves_the_running_servers_database() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // What `drsg init` writes, in a repository the relay then discovers from.
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "drsg-watch": {
                    "type": "http",
                    "url": format!("http://{addr}/mcp"),
                    "headers": { "Authorization": format!("Bearer {TOKEN}") },
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let upstream = dr_strange_mcp::relay::discover(repo.path()).expect("declared by .mcp.json");
    assert!(
        dr_strange_mcp::relay::alive(&upstream, Duration::from_secs(2)).await,
        "the probe must find a server that is plainly answering",
    );

    // A pipe stands in for the host's stdio: the relay serves one end, an MCP
    // client drives the other.
    let (host_side, relay_side) = tokio::io::duplex(64 * 1024);
    let relayed = tokio::spawn(async move {
        let (rx, tx) = tokio::io::split(relay_side);
        dr_strange_mcp::relay::relay_over(
            rmcp::transport::async_rw::AsyncRwTransport::new_server(rx, tx),
            &upstream,
        )
        .await
    });

    let (rx, tx) = tokio::io::split(host_side);
    let client: Client = ClientInfo::default()
        .serve(rmcp::transport::async_rw::AsyncRwTransport::new_client(
            rx, tx,
        ))
        .await
        .expect("the relay carries the initialize handshake");

    // Tools are the upstream's, listed through the relay.
    let tools = client.list_tools(Default::default()).await.unwrap();
    assert!(
        tools.tools.iter().any(|t| t.name == "get_node"),
        "the server's own tool set reaches the host"
    );

    // And the data is the server's: `alice` exists only in the database this
    // process seeded, never in one the relay could have opened itself.
    let node = call(&client, "get_node", json!({ "key": "alice" })).await;
    assert_eq!(node["external_key"], "alice");

    // Closing the host ends the relay, which is how the process exits when a
    // host disconnects.
    client.cancel().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), relayed)
        .await
        .expect("the relay outlived its host");
    ended.unwrap().expect("the relay ended cleanly");
}

/// The agent verbs and the query tool over `/mcp`, in the shapes an agent
/// gets them: `cypher` answers a projecting query with a table, and `fathom`
/// answers with the compact text every agent verb speaks.
#[tokio::test]
async fn tools_answer_in_the_shape_the_question_asked_for() {
    let addr = spawn_server();
    wait_ready(addr).await;
    let client = connect(addr, TOKEN).await.unwrap();

    // A projection: columns and rows, not nodes.
    let table = call(
        &client,
        "cypher",
        json!({ "query": "MATCH (n:Person) RETURN key(n) AS who, count(*) AS n" }),
    )
    .await;
    assert_eq!(table["columns"], json!(["who", "n"]));
    assert_eq!(table["rows"], json!([["alice", 1]]));

    // The same tool without a projection still answers with records.
    let nodes = call(
        &client,
        "cypher",
        json!({ "query": "MATCH (n:Person) RETURN n" }),
    )
    .await;
    assert_eq!(nodes[0]["external_key"], "alice");

    // `fathom` reports the region's makeup — here a lone node, which it says
    // rather than implying it looked further.
    // No depth: the tool's default, what an agent naming only a symbol gets.
    let region = call(&client, "fathom", json!({ "name": "alice" })).await;
    let text = region.as_str().expect("fathom answers with text");
    assert!(
        text.contains("region: 0 hops out and in — 1 nodes, 0 edges"),
        "{text}"
    );
    assert!(text.contains("Person 1"), "{text}");
    assert!(text.contains("nothing further connects"), "{text}");
    assert!(
        text.contains("short of the 2"),
        "depth defaults to 2: {text}"
    );

    client.cancel().await.unwrap();
}

/// A valid token but a non-loopback `Host`: refused by the transport's
/// DNS-rebinding guard, which defaults to `localhost`/`127.0.0.1`/`::1`.
/// Pinned because it is the one way `/mcp` behaves differently from every
/// other route on this server — `/rpc` answers on any hostname — and
/// docs/{en,zh}/src/mcp.md now tells people so.
#[tokio::test]
async fn mcp_endpoint_refuses_a_non_loopback_host() {
    let addr = spawn_server();
    wait_ready(addr).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("host", "memory.example.com")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// No `Authorization` header at all: refused before the tool router ever
/// sees the request, same posture as every other authenticated route on this
/// server. Tested over raw HTTP (like `auth_gate_over_http` in `http.rs`) —
/// this is about the axum middleware, not the MCP client.
#[tokio::test]
async fn mcp_endpoint_refuses_requests_with_no_token() {
    let addr = spawn_server();
    wait_ready(addr).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","method":"initialize","params":{},"id":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// An oversized body is refused on `/mcp`, the same as on `/rpc`.
///
/// Regression: `DefaultBodyLimit` guards the rest of the server, but axum
/// implements it as a request extension that only extractors consult, and
/// `/mcp` is a raw tower service that buffers the body itself — so an
/// authenticated caller could POST unbounded bytes into the process holding
/// the database.
#[tokio::test]
async fn mcp_endpoint_refuses_an_oversized_body() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // Only the request head is written. The limit is enforced from the
    // declared `Content-Length`, so the server answers before a single byte
    // of the body is buffered — which is the property worth pinning, and the
    // reason this is raw TCP rather than `reqwest`: uploading the bytes for
    // real proves the same thing, but the server's early close then reaches
    // the client as a broken pipe instead of the 413 it already wrote.
    let too_big = 64 * 1024 * 1024 + 1;
    let head = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Length: {too_big}\r\n\r\n"
    );
    let status = tokio::time::timeout(Duration::from_secs(30), async {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(head.as_bytes()).await.unwrap();
        let mut buf = [0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    })
    .await
    .expect("no response in 30s — the body was awaited, not refused");
    assert!(
        status.starts_with("HTTP/1.1 413"),
        "expected 413, got: {status}"
    );
}

/// Two independent MCP sessions sharing one `/mcp` endpoint see one
/// `Database`: session A's write is visible to session B, which never opened
/// the file and never talked to session A — only to the one server process.
/// This is ROADMAP §10's whole point, exercised over the real transport an
/// agent host uses.
#[tokio::test]
async fn two_sessions_share_one_database_over_mcp() {
    let addr = spawn_server();
    wait_ready(addr).await;

    // Session A: the seeded node round-trips.
    let a = connect(addr, TOKEN).await.unwrap();
    let alice = call(&a, "get_node", json!({"key": "alice"})).await;
    assert_eq!(alice["external_key"], "alice");

    // Session A writes a new node.
    let created = call(
        &a,
        "write_nodes",
        json!({"nodes": [{"external_key": "bob", "labels": ["Person"]}]}),
    )
    .await;
    assert_eq!(created["created"].as_array().unwrap().len(), 1);

    // Session B: a wholly separate connection (a different simulated agent
    // host) reads it back — the one thing a single-session test can't prove.
    let b = connect(addr, TOKEN).await.unwrap();
    let bob = call(&b, "get_node", json!({"key": "bob"})).await;
    assert_eq!(bob["external_key"], "bob");

    // A third session sees both writes reflected in the plane's count.
    let c = connect(addr, TOKEN).await.unwrap();
    let planes = call(&c, "list_planes", Value::Null).await;
    let startup = planes
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "startup")
        .unwrap();
    assert_eq!(startup["nodes"], 2);

    let _ = a.cancel().await;
    let _ = b.cancel().await;
    let _ = c.cancel().await;
}
