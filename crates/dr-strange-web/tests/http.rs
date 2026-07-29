//! End-to-end smoke test: spin up a real `drsg serve` on an ephemeral port and
//! drive it over HTTP — the embedded SPA on `/` and JSON-RPC on `/rpc`. The
//! protocol rules themselves are unit-tested in `rpc.rs`; this proves the axum
//! wiring, the blocking-task bridge, and the embedded assets all hang
//! together.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use dr_strange_core::{Database, Properties};
use serde_json::{Value, json};

/// Grab a free port, then let the server rebind it. A brief race, fine for a
/// test; the retry loop below tolerates a not-yet-listening server anyway.
fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn spawn_server() -> SocketAddr {
    let addr = free_addr();
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    txn.create_node_with_key("alice", &["Person"], Properties::new())
        .unwrap();
    txn.commit().unwrap();

    std::thread::spawn(move || {
        // Runs its own runtime and blocks until the process exits.
        dr_strange_web::serve(db, None, addr).unwrap();
    });
    addr
}

async fn rpc(client: &reqwest::Client, base: &str, method: &str, params: Value) -> Value {
    client
        .post(format!("{base}/rpc"))
        .json(&json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll `base` until the server is listening (or give up after ~2 s).
async fn wait_ready(client: &reqwest::Client, base: &str) {
    for _ in 0..80 {
        if client.get(base).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server never started listening");
}

#[tokio::test]
async fn serves_dashboard_and_rpc() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    // The embedded SPA is served on `/`.
    let index = client.get(&base).send().await.unwrap();
    assert!(index.status().is_success());
    let html = index.text().await.unwrap();
    assert!(html.contains("<div id=\"app\">") || html.contains("dr-strange"));

    // JSON-RPC: db.stats reflects the seeded node.
    let stats = rpc(&client, &base, "db.stats", Value::Null).await;
    assert_eq!(stats["result"]["nodes"], 1);

    // plane.list surfaces the startup plane.
    let planes = rpc(&client, &base, "plane.list", Value::Null).await;
    assert!(
        planes["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "startup")
    );

    // Unknown method → JSON-RPC method-not-found.
    let miss = rpc(&client, &base, "does.not.exist", Value::Null).await;
    assert_eq!(miss["error"]["code"], -32601);

    // SPA fallback: an unknown non-API path returns index.html, not 404.
    let deep = client
        .get(format!("{base}/planes/xyz"))
        .send()
        .await
        .unwrap();
    assert!(deep.status().is_success());

    // /digest/extract accepts an upload well past axum's 2 MiB default limit —
    // real PDFs are larger, and the old default 413'd them (with a non-JSON
    // body) before the handler ran. 5 MiB of plain text extracts as UTF-8.
    // The response is newline-delimited JSON (progress lines then the result);
    // a .txt has no progress, so the final line carries {chars,text}.
    let big = "a".repeat(5 * 1024 * 1024);
    let extract = client
        .post(format!("{base}/digest/extract?name=big.txt"))
        .body(big.clone())
        .send()
        .await
        .unwrap();
    assert!(extract.status().is_success(), "large upload was rejected");
    let ndjson = extract.text().await.unwrap();
    let last = ndjson.lines().rfind(|l| !l.trim().is_empty()).unwrap();
    let body: Value = serde_json::from_str(last).unwrap();
    assert_eq!(body["chars"], big.len());
}

/// The write-auth gate, exercised over the real HTTP wiring (the header →
/// [`Credentials`] extraction + Origin guard live in the server layer, not in
/// the pure dispatcher). This server has no `DRSG_TOKEN`, so the only path to a
/// write is the same-origin browser UI.
#[tokio::test]
async fn write_auth_gate_over_http() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    // An empty write: reaches the handler iff the auth gate allows it.
    let write = json!({
        "jsonrpc": "2.0",
        "method": "digest.write",
        "params": { "plane": "startup", "nodes": [], "edges": [] },
        "id": 1,
    });

    // 1. Native client (no Origin), no token configured → write denied (-32001).
    let denied: Value = client
        .post(format!("{base}/rpc"))
        .json(&write)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], -32001, "native write should be denied");

    // 2. Same-origin browser (loopback Origin) → the local-UI write is allowed.
    let allowed: Value = client
        .post(format!("{base}/rpc"))
        .header("origin", format!("http://{addr}"))
        .json(&write)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(allowed.get("error").is_none(), "unexpected error: {allowed}");
    assert_eq!(allowed["result"]["nodes_written"], 0);

    // 3. Cross-origin browser → refused at the Origin guard (403), never
    //    dispatched. This is the CSRF/DNS-rebinding defense.
    let forbidden = client
        .post(format!("{base}/rpc"))
        .header("origin", "https://evil.example.com")
        .json(&write)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);

    // A read is likewise refused cross-origin — a malicious page can't even
    // snoop the graph.
    let read_cross = client
        .post(format!("{base}/rpc"))
        .header("origin", "https://evil.example.com")
        .json(&json!({"jsonrpc":"2.0","method":"db.stats","id":2}))
        .send()
        .await
        .unwrap();
    assert_eq!(read_cross.status(), reqwest::StatusCode::FORBIDDEN);
}
