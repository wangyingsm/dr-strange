//! Brute-force protection on the bearer check (arch/08 §4.1): a peer that
//! keeps presenting wrong tokens is made to wait, answered 429 before any
//! handler runs, and a correct token clears it. Its own test binary because
//! `DRSG_TOKEN` is process-wide (see `mcp.rs` for why it is set once) and
//! `http.rs` needs it unset — and because the lockout is per peer, and every
//! test in one binary is the same peer.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use dr_strange_core::Database;
use serde_json::{Value, json};

const TOKEN: &str = "test-throttle-token";

fn set_token_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: serialised by `Once`, and no server thread — the only reader —
    // exists until this returns (same reasoning as `mcp.rs`).
    ONCE.call_once(|| unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) });
}

fn spawn_server() -> SocketAddr {
    set_token_once();
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let db = Database::in_memory().unwrap();
    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
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

async fn stats_as(client: &reqwest::Client, base: &str, bearer: &str) -> reqwest::Response {
    client
        .post(format!("{base}/rpc"))
        .header("authorization", format!("Bearer {bearer}"))
        .json(&json!({ "jsonrpc": "2.0", "method": "db.stats", "id": 1 }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn repeated_wrong_tokens_lock_the_peer_out_and_the_right_one_clears_it() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    // The free allowance: each wrong token is an ordinary unauthorized reply.
    for i in 0..dr_strange_web::FREE_FAILURES {
        let resp = stats_as(&client, &base, "nope").await;
        assert_eq!(resp.status(), 200, "attempt {i}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32001, "attempt {i}: {body}");
    }

    // One more and the peer is throttled: 429 with a Retry-After.
    let resp = stats_as(&client, &base, "nope").await;
    assert_eq!(resp.status(), 429);
    let retry: u64 = resp.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(retry, 1);

    // While locked out, even the right token is refused — the peer waits —
    // and so is a request to a different route.
    assert_eq!(stats_as(&client, &base, TOKEN).await.status(), 429);
    let history = client
        .get(format!("{base}/cypher/history"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(history.status(), 429);

    // After the wait, the correct token is accepted and clears the strikes:
    // the free allowance starts over.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let resp = stats_as(&client, &base, TOKEN).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("result").is_some(), "{body}");
    for _ in 0..dr_strange_web::FREE_FAILURES {
        assert_eq!(stats_as(&client, &base, "nope").await.status(), 200);
    }

    // A request with no bearer is not a guess: it is neither counted nor
    // blocked, so the liveness probe and the page itself still answer while
    // this client is locked — behind a proxy or NAT many humans share one
    // address, and one guesser must not take the page away from the rest.
    let resp = stats_as(&client, &base, "nope").await;
    assert_eq!(resp.status(), 429);
    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    // The lockout still stands for anything presenting a bearer.
    assert_eq!(stats_as(&client, &base, TOKEN).await.status(), 429);
}

/// Behind a reverse proxy on this machine every client is the same loopback
/// peer; the proxy's `X-Forwarded-For` tells them apart, so a guesser locks
/// out only itself and the next client's guesses and correct token go
/// through as if the guesser had never been.
#[tokio::test]
async fn a_forwarded_client_is_throttled_apart_from_the_others_behind_the_proxy() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;
    let as_client = |ip: &'static str, bearer: &'static str| {
        client
            .post(format!("{base}/rpc"))
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-forwarded-for", ip)
            .json(&json!({ "jsonrpc": "2.0", "method": "db.stats", "id": 1 }))
            .send()
    };
    for _ in 0..dr_strange_web::FREE_FAILURES {
        assert_eq!(
            as_client("203.0.113.9", "nope").await.unwrap().status(),
            200
        );
    }
    assert_eq!(
        as_client("203.0.113.9", "nope").await.unwrap().status(),
        429
    );
    assert_eq!(as_client("203.0.113.9", TOKEN).await.unwrap().status(), 429);
    // Another client through the same proxy is untouched.
    assert_eq!(
        as_client("198.51.100.7", "nope").await.unwrap().status(),
        200
    );
    assert_eq!(
        as_client("198.51.100.7", TOKEN).await.unwrap().status(),
        200
    );
    // And so is a request the proxy did not forward — the peer itself.
    assert_eq!(stats_as(&client, &base, TOKEN).await.status(), 200);
}
