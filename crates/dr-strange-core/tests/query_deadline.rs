//! A query started from a handle with a deadline stops instead of running on.
//!
//! Unbounded is right embedded — the caller is running its own query, and
//! cutting it off part-way is a worse answer than a slow one. It is wrong once
//! a server runs someone else's query: since `/mcp` shipped, one agent's
//! runaway `MATCH` had nothing bounding it at all.

use std::time::{Duration, Instant};

use dr_strange_core::{Database, Error, Properties};

fn seeded(n: usize) -> Database {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    for i in 0..n {
        txn.create_node_with_key(&format!("n{i}"), &["N"], Properties::new())
            .unwrap();
    }
    txn.commit().unwrap();
    db
}

#[test]
fn an_expired_deadline_stops_the_query() {
    let db = seeded(64);
    let past = Instant::now() - Duration::from_secs(1);
    let plane = db.plane("startup").unwrap().with_deadline(past);

    let err = plane
        .query()
        .scan_label("N")
        .ids()
        .expect_err("an expired deadline must stop the query");
    assert!(
        matches!(err, Error::Timeout(_)),
        "a query that ran out of time is a Timeout, not {err:?}"
    );
    // The message has to tell an agent what to do differently, since the
    // query itself was valid — it was only too big.
    let msg = err.to_string();
    assert!(msg.contains("time budget"), "unhelpful: {msg}");
    assert!(
        msg.contains("LIMIT") || msg.contains("query_timeout_secs"),
        "should say how to proceed: {msg}"
    );
}

#[test]
fn a_generous_deadline_does_not_interfere() {
    let db = seeded(64);
    let plane = db
        .plane("startup")
        .unwrap()
        .with_deadline(Instant::now() + Duration::from_secs(60));

    let ids = plane
        .query()
        .scan_label("N")
        .ids()
        .expect("should complete");
    assert_eq!(
        ids.len(),
        64,
        "a deadline that has not passed must not cut rows"
    );
}

#[test]
fn no_deadline_is_the_default() {
    let db = seeded(64);
    let ids = db
        .plane("startup")
        .unwrap()
        .query()
        .scan_label("N")
        .ids()
        .expect("an embedded caller keeps running to completion");
    assert_eq!(ids.len(), 64);
}

/// A `Sort` drains its whole input before yielding anything, so a check only
/// on the finished pipeline would never fire for it. The source is wrapped for
/// exactly this reason.
#[test]
fn a_barrier_step_is_bounded_too() {
    use dr_strange_core::{SortKey, p};

    let db = seeded(64);
    let past = Instant::now() - Duration::from_secs(1);
    let plane = db.plane("startup").unwrap().with_deadline(past);

    let err = plane
        .query()
        .scan_label("N")
        .sort_by(vec![SortKey {
            expr: p("missing"),
            descending: false,
        }])
        .ids()
        .expect_err("a sort must not drain an unbounded input past the deadline");
    assert!(
        matches!(err, Error::Timeout(_)),
        "expected Timeout: {err:?}"
    );
}
