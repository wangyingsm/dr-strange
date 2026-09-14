//! A `drsg serve` bound off loopback (`--addr 0.0.0.0:…`, the Docker shape).
//! Its `GET /` is answered to the whole network, so the page must not carry
//! the token — even to a peer on this machine — and the browser UI must still
//! be able to work by presenting the token itself (arch/08 §4.1). Its own
//! test binary because `DRSG_TOKEN` is process-wide (see `mcp.rs` for why it
//! is set once) and `http.rs` needs it unset.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use dr_strange_core::{Database, Properties};
use serde_json::{Value, json};

const TOKEN: &str = "test-bind-token";

fn set_token_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: serialised by `Once`, and no server thread — the only reader —
    // exists until this returns (same reasoning as `mcp.rs`).
    ONCE.call_once(|| unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) });
}

/// Bind the wildcard address on a port the kernel picks, then let the server
/// rebind it. Reached through 127.0.0.1, so the *peer* is loopback while the
/// *bind* is not — exactly the case the injection rule must refuse.
fn spawn_lan_server() -> SocketAddr {
    set_token_once();
    let port = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bind: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    txn.create_node_with_key("alice", &["Person"], Properties::new())
        .unwrap();
    txn.commit().unwrap();
    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr: bind,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    format!("127.0.0.1:{port}").parse().unwrap()
}

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
async fn a_lan_listener_never_writes_its_token_into_the_page() {
    let addr = spawn_lan_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    // The page is served (the SPA will ask the user for the token) but
    // carries no credential, not to a loopback peer either.
    let index = client.get(&base).send().await.unwrap();
    assert!(index.status().is_success());
    let html = index.text().await.unwrap();
    assert!(!html.contains(TOKEN), "{html}");
    assert!(!html.contains("drsg-token"), "{html}");
    // Deep links go through the same fallback and must be as clean.
    let html = client
        .get(format!("{base}/explore"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains(TOKEN), "{html}");

    // The same-origin UI is not trusted on its Origin alone here…
    let post = |origin: bool, bearer: bool| {
        let mut req = client
            .post(format!("{base}/rpc"))
            .json(&json!({ "jsonrpc": "2.0", "method": "db.stats", "id": 1 }));
        if origin {
            req = req.header("origin", base.clone());
        }
        if bearer {
            req = req.header("authorization", format!("Bearer {TOKEN}"));
        }
        async move { req.send().await.unwrap().json::<Value>().await.unwrap() }
    };
    assert_eq!(post(true, false).await["error"]["code"], -32001);
    // …but works once it presents the token it asked the user for, and so
    // does a native client.
    assert_eq!(post(true, true).await["result"]["nodes"], 1);
    assert_eq!(post(false, true).await["result"]["nodes"], 1);
}
