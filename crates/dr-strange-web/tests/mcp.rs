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

const TOKEN: &str = "test-mcp-token";

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// Spins up a real server with a seeded node, gated by `DRSG_TOKEN` — the
/// realistic configuration for a programmatic client (ROADMAP §10's "token
/// posture" fork: with no token, `/mcp` refuses even reads, same as `/rpc`).
/// SAFETY: this file is its own test binary (`cargo test` gives every
/// `tests/*.rs` file its own process), so no other test observes this env var.
fn spawn_server() -> SocketAddr {
    unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) };
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
