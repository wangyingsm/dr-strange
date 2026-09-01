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
        let opts = dr_strange_web::ServeOptions {
            addr,
            ..Default::default()
        };
        dr_strange_web::serve(db, None, opts).unwrap();
    });
    addr
}

/// A read/write RPC as the browser UI makes it: same-origin, so the no-token
/// local-UI fallback authorizes it (the whole surface is authenticated now).
async fn rpc(client: &reqwest::Client, base: &str, method: &str, params: Value) -> Value {
    client
        .post(format!("{base}/rpc"))
        .header("origin", base)
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
    // Hardening: every response carries the static defensive headers.
    assert_eq!(index.headers()["x-content-type-options"], "nosniff");
    assert_eq!(index.headers()["x-frame-options"], "DENY");
    let html = index.text().await.unwrap();
    assert!(html.contains("<div id=\"app\">") || html.contains("dr-strange"));

    // /health is an unauthenticated liveness probe — no Origin, no token.
    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert!(health.status().is_success());
    assert_eq!(health.json::<Value>().await.unwrap()["status"], "ok");

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
        .header("origin", &base) // authenticated like the browser UI
        .body(big.clone())
        .send()
        .await
        .unwrap();
    assert!(extract.status().is_success(), "large upload was rejected");
    let ndjson = extract.text().await.unwrap();
    let last = ndjson.lines().rfind(|l| !l.trim().is_empty()).unwrap();
    let body: Value = serde_json::from_str(last).unwrap();
    assert_eq!(body["chars"], big.len());

    // /cypher compiles an openCypher-subset query and runs it; same-origin auth
    // like the browser UI. The seeded graph has one Person, "alice".
    let cy = client
        .post(format!("{base}/cypher?plane=startup"))
        .header("origin", &base)
        .body("MATCH (n:Person) RETURN n")
        .send()
        .await
        .unwrap();
    assert!(cy.status().is_success());
    let cyv: Value = cy.json().await.unwrap();
    assert_eq!(cyv["count"], 1);
    assert_eq!(cyv["nodes"][0]["external_key"], "alice");

    // A projecting query answers with a table instead of a subgraph — the
    // columns are the answer, so there is nothing to plot.
    let table = client
        .post(format!("{base}/cypher?plane=startup"))
        .header("origin", &base)
        .body("MATCH (n:Person) RETURN key(n) AS who, count(*) AS n")
        .send()
        .await
        .unwrap();
    assert!(table.status().is_success());
    let tv: Value = table.json().await.unwrap();
    assert_eq!(tv["columns"], serde_json::json!(["who", "n"]));
    assert_eq!(tv["rows"], serde_json::json!([["alice", 1]]));

    // A malformed query is a 400 with the parser's message, not a panic.
    let bad = client
        .post(format!("{base}/cypher?plane=startup"))
        .header("origin", &base)
        .body("MATCH (n)")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // A CREATE write mutates the plane and returns its change-counts.
    let created = client
        .post(format!("{base}/cypher?plane=startup"))
        .header("origin", &base)
        .body(r#"CREATE (w:Widget {key:"w1"})"#)
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    let cv: Value = created.json().await.unwrap();
    assert_eq!(cv["write"], true);
    assert_eq!(cv["nodes_created"], 1);
    // …and it's really there.
    let got = rpc(
        &client,
        &base,
        "node.get",
        json!({ "plane": "startup", "key": "w1" }),
    )
    .await;
    assert_eq!(got["result"]["external_key"], "w1");
}

/// The auth gate, exercised over the real HTTP wiring (the header →
/// [`Credentials`] extraction + Origin guard live in the server layer, not in
/// the pure dispatcher). This server has no `DRSG_TOKEN`, so the *entire*
/// surface — reads included — is reachable only from the same-origin browser
/// UI; a native client (no Origin) is refused everywhere.
#[tokio::test]
async fn auth_gate_over_http() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    let post = |body: Value, origin: Option<&'static str>| {
        let mut req = client.post(format!("{base}/rpc")).json(&body);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        req.send()
    };
    let write = json!({
        "jsonrpc": "2.0", "method": "digest.write",
        "params": { "plane": "startup", "nodes": [], "edges": [] }, "id": 1,
    });
    let read = json!({ "jsonrpc": "2.0", "method": "db.stats", "id": 2 });

    // 1. Native client (no Origin), no token → EVERY method denied (-32001),
    //    reads too. Programmatic access requires DRSG_TOKEN.
    let denied_write: Value = post(write.clone(), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(denied_write["error"]["code"], -32001, "native write denied");
    let denied_read: Value = post(read.clone(), None)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        denied_read["error"]["code"], -32001,
        "native read denied too"
    );

    // 2. Same-origin browser (loopback Origin) → the local-UI fallback allows it.
    let ok_read: Value = post(read.clone(), Some("http://127.0.0.1"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ok_read["result"]["nodes"], 1);
    let ok_write: Value = post(write.clone(), Some("http://127.0.0.1"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        ok_write.get("error").is_none(),
        "unexpected error: {ok_write}"
    );
    assert_eq!(ok_write["result"]["nodes_written"], 0);

    // 3. Cross-origin browser → refused at the Origin guard (403), for reads
    //    and writes alike — a malicious page can neither mutate nor snoop.
    let forbidden_write = post(write, Some("https://evil.example.com")).await.unwrap();
    assert_eq!(forbidden_write.status(), reqwest::StatusCode::FORBIDDEN);
    let forbidden_read = post(read, Some("https://evil.example.com")).await.unwrap();
    assert_eq!(forbidden_read.status(), reqwest::StatusCode::FORBIDDEN);
}

/// `digest.write` must never take a key that is already bound in the plane.
///
/// It writes through the bulk path, which stamps the external-key index
/// unconditionally — so before this was gated, a proposal naming an existing
/// entity overwrote that index entry and left the original node reachable only
/// by id. Every `key(...)` read against it then came back empty while the data
/// sat there untouched — observed in practice, on nodes an application
/// addressed by key, with no error and no log line to notice it by.
///
/// Skipping is the contract, not failing: a distillation proposes every entity
/// as new, so naming something already known is the normal case. The edge is
/// still written and must land on the *original* node.
#[tokio::test]
async fn digest_write_never_shadows_an_existing_key() {
    let addr = spawn_server();
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    wait_ready(&client, &base).await;

    let created = rpc(
        &client,
        &base,
        "node.create",
        json!({ "plane": "startup", "key": "proj", "labels": ["Project"],
                "properties": { "path": "/keep/me" } }),
    )
    .await;
    let original = created["result"]["id"].as_u64().unwrap();

    // A proposal that re-proposes `proj`, names it twice, and hangs a new node
    // off it — exactly the shape digest.run emits with `link: false`.
    let w = rpc(
        &client,
        &base,
        "digest.write",
        json!({
            "plane": "startup",
            "nodes": [
                { "key": "proj",  "label": "Project", "properties": { "path": "/overwritten" } },
                { "key": "proj",  "label": "Project", "properties": {} },
                { "key": "fresh", "label": "Note",    "properties": {} }
            ],
            "edges": [ { "src": "fresh", "dst": "proj", "type": "ABOUT" } ],
        }),
    )
    .await["result"]
        .clone();

    assert_eq!(
        w["nodes_written"], 1,
        "only the genuinely new node is written"
    );
    assert_eq!(w["nodes_skipped"], 2, "the taken key, twice over");
    assert_eq!(
        w["skipped_keys"],
        json!(["proj", "proj"]),
        "skips are reported, not silent"
    );
    assert_eq!(w["edges_written"], 1, "the edge still lands");

    // The key still resolves to the node that owned it, properties intact.
    let got = rpc(
        &client,
        &base,
        "node.get",
        json!({ "plane": "startup", "key": "proj" }),
    )
    .await;
    assert_eq!(got["result"]["id"], original, "key must not have moved");
    assert_eq!(
        got["result"]["properties"]["path"], "/keep/me",
        "not overwritten"
    );

    // …and the edge attached to that original node, not to a second one.
    let n = rpc(
        &client,
        &base,
        "plane.cypher",
        json!({
            "plane": "startup",
            "query": "MATCH (p:Project)<-[:ABOUT]-(x) WHERE key(p) = $k RETURN x",
            "params": { "k": "proj" },
        }),
    )
    .await;
    assert_eq!(
        n["result"]["nodes"].as_array().unwrap().len(),
        1,
        "edge onto a skipped key must resolve to the node already there"
    );
}
