//! `serve --follow` end to end, in-process (arch/01 §9): a master `drsg
//! serve` and a follower that bootstraps from its `/snapshot`, tails its
//! `/ws/wal`, and mirrors a write made after the bootstrap — without the
//! follower ever asking for a resync. That last part is the sequence
//! contract: the follower's restore lands at the master's sequence, so the
//! master's next batch is above the follower's `committed_seq` and is applied
//! rather than refused (`apply_replicated` refuses a replay, and a refusal
//! ends the follow with `ResyncNeeded`).
//!
//! Its own test binary because `DRSG_TOKEN` is process-wide (see `bind.rs`):
//! the follower fetches the snapshot without a browser Origin, so the master
//! must be able to authenticate it by token.
//!
//! Native-only, like `serve --follow` itself (replication mirrors the native
//! engine's WAL, and `serve` bails without the feature); the redb-only
//! build compiles this binary to nothing rather than to a missing
//! `open_read_only`.
#![cfg(feature = "native-backend")]

use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::time::Duration;

use dr_strange_core::{Database, Properties};
use serde_json::{Value, json};

const TOKEN: &str = "test-follow-token";

fn set_token_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: serialised by `Once`, and no server thread — the only reader —
    // exists until this returns (same reasoning as `mcp.rs`).
    ONCE.call_once(|| unsafe { std::env::set_var("DRSG_TOKEN", TOKEN) });
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn rpc(client: &reqwest::Client, base: &str, method: &str, params: Value) -> Value {
    client
        .post(format!("{base}/rpc"))
        .bearer_auth(TOKEN)
        .json(&json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": 1 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll `db.stats` on `base` until it reports `nodes` (or give up after ~10 s).
async fn wait_for_nodes(client: &reqwest::Client, base: &str, nodes: u64) -> Value {
    for _ in 0..400 {
        if client.get(base).send().await.is_ok() {
            let stats = rpc(client, base, "db.stats", Value::Null).await;
            if stats["result"]["nodes"] == json!(nodes) {
                return stats;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{base} never reported {nodes} nodes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_mirrors_a_write_made_after_its_bootstrap_without_resyncing() {
    set_token_once();
    let master_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    // Young master: one write past a fresh open.
    let master_addr = free_addr();
    let master = Database::open(master_dir.path().join("db")).unwrap();
    {
        let mut txn = master.plane("startup").unwrap().write().unwrap();
        txn.create_node_with_key("alice", &["Person"], Properties::new())
            .unwrap();
        txn.commit().unwrap();
    }
    std::thread::spawn(move || {
        let opts = dr_strange_web::ServeOptions {
            addr: master_addr,
            ..Default::default()
        };
        dr_strange_web::serve(master, None, opts).unwrap();
    });
    let client = reqwest::Client::new();
    let master_base = format!("http://{master_addr}");
    wait_for_nodes(&client, &master_base, 1).await;

    // Fresh follower: read-only open, then serve with --follow. `serve`
    // returns only when the follow is lost; the channel is how the test
    // learns that (a `ResyncNeeded` here would be the refusal loop).
    let follower_addr = free_addr();
    let follower_path = follower_dir.path().join("db");
    let (outcome_tx, outcome_rx) = mpsc::channel();
    {
        let follower = Database::open_read_only(&follower_path).unwrap();
        let follower_path = follower_path.clone();
        std::thread::spawn(move || {
            let opts = dr_strange_web::ServeOptions {
                addr: follower_addr,
                follow: Some(dr_strange_web::FollowOptions {
                    upstream: format!("ws://{master_addr}"),
                    token: Some(TOKEN.into()),
                }),
                ..Default::default()
            };
            let outcome = dr_strange_web::serve(follower, Some(follower_path), opts);
            let _ = outcome_tx
                .send(outcome.map(|o| matches!(o, dr_strange_web::ServeOutcome::ResyncNeeded)));
        });
    }
    let follower_base = format!("http://{follower_addr}");
    // Bootstrapped: the snapshot's node is there.
    wait_for_nodes(&client, &follower_base, 1).await;

    // A write on the master after the bootstrap is the follower's first live
    // batch; it lands.
    let created = rpc(
        &client,
        &master_base,
        "node.create",
        json!({ "plane": "startup", "key": "bob", "labels": ["Person"] }),
    )
    .await;
    assert!(created["error"].is_null(), "{created}");
    let stats = wait_for_nodes(&client, &follower_base, 2).await;
    assert_eq!(stats["result"]["nodes"], 2);

    // And it did not get there by resyncing: the follower's `serve` is still
    // running, and the sequences agree.
    assert!(
        outcome_rx.try_recv().is_err(),
        "the follower asked for a resync — its first live batch was refused"
    );
    let master_stats = rpc(&client, &master_base, "db.stats", Value::Null).await;
    assert_eq!(
        stats["result"]["commit_seq"],
        master_stats["result"]["commit_seq"]
    );

    // The follower refuses a write of its own.
    let refused = rpc(
        &client,
        &follower_base,
        "node.create",
        json!({ "plane": "startup", "key": "carol" }),
    )
    .await;
    assert!(!refused["error"].is_null(), "{refused}");
}
