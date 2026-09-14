//! `serve --follow` replication (arch/01 §9): a batch captured from one
//! native engine's commits, replayed into a fresh engine via
//! `apply_replicated`, must reproduce the exact same KV content — not just
//! the same graph-level view. Replication ships raw ops precisely so tables
//! the graph API doesn't itself model (indices, counters) stay in sync too,
//! so these tests compare every `TableId`, not just nodes/edges.

#![cfg(feature = "native-backend")]

use std::sync::{Arc, Mutex};

use dr_strange_core::storage::engine::{ReadTransaction, StorageEngine, TableId, WriteTransaction};
use dr_strange_core::storage::native::NativeEngine;
use dr_strange_core::{Database, Properties, ReplicatedBatch};

/// A scratch directory that cleans up after itself.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-repl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every visible `(table, key, value)` triple, for whole-engine comparison.
fn dump(e: &NativeEngine) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
    let txn = e.begin_read().unwrap();
    let mut out = Vec::new();
    for t in TableId::ALL {
        for kv in txn.range(t, &[], None).unwrap() {
            let (k, v) = kv.unwrap();
            out.push((t as u8, k, v));
        }
    }
    out.sort();
    out
}

/// Wire up `src` so every commit's batch lands in a shared `Vec`, in order.
fn capture(src: &NativeEngine) -> Arc<Mutex<Vec<ReplicatedBatch>>> {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    src.set_wal_observer(Some(Arc::new(move |batch: ReplicatedBatch| {
        sink.lock().unwrap().push(batch);
    })));
    batches
}

#[test]
fn apply_replicated_reproduces_exact_kv_content() {
    let dir_a = Dir::new("content-a");
    let dir_b = Dir::new("content-b");
    let a = NativeEngine::open(&dir_a.0).unwrap();
    let b = NativeEngine::open(&dir_b.0).unwrap();
    let batches = capture(&a);

    for i in 0..3u8 {
        let mut w = a.begin_write().unwrap();
        w.put(TableId::Nodes, &[i], &[i, i]).unwrap();
        w.put(TableId::Meta, b"counter", &[i]).unwrap();
        w.commit().unwrap();
    }
    // A delete, so the replicated tombstone path is exercised too.
    {
        let mut w = a.begin_write().unwrap();
        w.delete(TableId::Nodes, &[0]).unwrap();
        w.commit().unwrap();
    }

    for batch in batches.lock().unwrap().drain(..) {
        b.apply_replicated(batch).unwrap();
    }

    assert_eq!(
        dump(&a),
        dump(&b),
        "replica KV content must match the source byte-for-byte"
    );
    assert_eq!(
        a.committed_seq(),
        b.committed_seq(),
        "replica must land the source's exact commit sequence"
    );
}

#[test]
fn apply_replicated_lands_the_sources_own_sequence_even_after_a_gap() {
    // If the replica had allocated its own sequence instead of using the
    // batch's, this would diverge from the source the moment any commit is
    // skipped or reordered — exactly the bug `durable_commit(seq, ..)` (using
    // the batch's `seq` verbatim) exists to avoid.
    let dir_a = Dir::new("gap-a");
    let dir_b = Dir::new("gap-b");
    let a = NativeEngine::open(&dir_a.0).unwrap();
    let b = NativeEngine::open(&dir_b.0).unwrap();
    let batches = capture(&a);

    for i in 0..5u8 {
        let mut w = a.begin_write().unwrap();
        w.put(TableId::Nodes, &[i], &[i]).unwrap();
        w.commit().unwrap();
    }

    // Apply only the last batch — as a full resync's live tail would, after
    // a snapshot already covered everything before it.
    let last = batches.lock().unwrap().pop().unwrap();
    let expected_seq = last.seq;
    b.apply_replicated(last).unwrap();

    assert_eq!(b.committed_seq(), expected_seq);
}

#[test]
fn read_only_rejects_begin_write_but_not_apply_replicated() {
    let dir = Dir::new("read-only");
    let engine = NativeEngine::open_read_only(&dir.0).unwrap();

    assert!(
        engine.begin_write().is_err(),
        "a read-only engine must refuse begin_write"
    );

    let batch = ReplicatedBatch {
        seq: 1,
        ops: vec![dr_strange_core::ReplicatedOp {
            table: TableId::Nodes,
            key: vec![1],
            value: Some(vec![1]),
        }],
    };
    engine
        .apply_replicated(batch)
        .expect("apply_replicated bypasses read_only — it's the replica's own write path");
    assert_eq!(engine.committed_seq(), 1);
}

#[test]
fn apply_replicated_refuses_a_sequence_that_does_not_advance() {
    // A replica's own sequence can run ahead of a batch's (its bootstrap and
    // restore commits allocate sequences locally). Landing such a batch would
    // stamp its versions older than what is already visible — they would lose
    // to the current versions in the memtable and a reader pinned at the
    // current sequence would see them appear mid-snapshot — so it is refused
    // with a typed error the follower turns into a resync, never silently
    // applied or dropped.
    let dir = Dir::new("regress");
    let e = NativeEngine::open(&dir.0).unwrap();
    for i in 1..=3u8 {
        let mut w = e.begin_write().unwrap();
        w.put(TableId::Nodes, &[i], &[i]).unwrap();
        w.commit().unwrap();
    }
    let before = dump(&e);
    let batch = |seq: u64| ReplicatedBatch {
        seq,
        ops: vec![dr_strange_core::ReplicatedOp {
            table: TableId::Nodes,
            key: vec![1],
            value: Some(vec![0xff]),
        }],
    };

    for stale in [1u64, 3] {
        let err = e.apply_replicated(batch(stale)).unwrap_err();
        assert!(matches!(err, dr_strange_core::Error::Conflict(_)), "{err}");
        assert_eq!(e.committed_seq(), 3, "the sequence never moves backwards");
        assert_eq!(dump(&e), before, "a refused batch changes nothing");
    }

    // The next sequence — or any later one — is still accepted.
    e.apply_replicated(batch(4)).unwrap();
    assert_eq!(e.committed_seq(), 4);
    let txn = e.begin_read().unwrap();
    assert_eq!(txn.get(TableId::Nodes, &[1]).unwrap(), Some(vec![0xff]));
}

/// Wire up a `Database` master so every commit's batch lands in a shared
/// `Vec`, in order — the `Database`-level twin of [`capture`].
fn capture_db(src: &Database) -> Arc<Mutex<Vec<ReplicatedBatch>>> {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let sink = batches.clone();
    src.on_wal_commit(move |batch| sink.lock().unwrap().push(batch))
        .unwrap();
    batches
}

fn apply_all(replica: &Database, batches: &Mutex<Vec<ReplicatedBatch>>) {
    for batch in batches.lock().unwrap().drain(..) {
        replica.apply_replicated(batch).unwrap();
    }
}

#[test]
fn reopening_a_replica_writes_nothing_so_its_sequence_stays_the_masters() {
    // A replica's commit sequence is the master's, landed verbatim by
    // `apply_replicated`. If opening the database ran bootstrap commits of
    // its own, every restart of `serve --follow` would push the replica's
    // sequence past the master's, and the next replicated batch would move
    // it backwards (or, once the engine refuses regressions, be rejected).
    // So an open of an already-initialised database must commit nothing.
    let dir_m = Dir::new("reopen-master");
    let dir_r = Dir::new("reopen-replica");
    let master = Database::open(&dir_m.0).unwrap();
    let batches = capture_db(&master);

    // The very first open of the replica is the only one allowed to write:
    // it lays down the meta the master's batches will then overwrite.
    let bootstrapped = {
        let replica = Database::open_read_only(&dir_r.0).unwrap();
        replica.commit_seq().unwrap()
    };
    {
        let replica = Database::open_read_only(&dir_r.0).unwrap();
        assert_eq!(
            replica.commit_seq().unwrap(),
            bootstrapped,
            "a second open must not advance the sequence"
        );
    }

    let plane = master.plane("startup").unwrap();
    let first = {
        let mut w = plane.write().unwrap();
        let id = w.create_node(&["Doc"], Properties::new()).unwrap();
        w.commit().unwrap();
        id
    };
    {
        let replica = Database::open_read_only(&dir_r.0).unwrap();
        apply_all(&replica, &batches);
        assert_eq!(replica.commit_seq().unwrap(), master.commit_seq().unwrap());
    }

    // Reopen twice more, then keep following: the replica's sequence must
    // still be the master's before and after every further batch.
    for _ in 0..2 {
        let replica = Database::open_read_only(&dir_r.0).unwrap();
        assert_eq!(
            replica.commit_seq().unwrap(),
            master.commit_seq().unwrap(),
            "reopening the replica ran it ahead of the master"
        );
    }
    let second = {
        let mut w = plane.write().unwrap();
        let id = w.create_node(&["Doc"], Properties::new()).unwrap();
        w.commit().unwrap();
        id
    };
    let replica = Database::open_read_only(&dir_r.0).unwrap();
    apply_all(&replica, &batches);
    assert_eq!(replica.commit_seq().unwrap(), master.commit_seq().unwrap());
    let seen = replica.plane("startup").unwrap();
    assert!(seen.node(first).unwrap().is_some());
    assert!(seen.node(second).unwrap().is_some());
}

#[test]
fn reopening_a_database_commits_nothing() {
    let dir = Dir::new("reopen-plain");
    let seq = {
        let db = Database::open(&dir.0).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut w = plane.write().unwrap();
        w.create_node(&["Doc"], Properties::new()).unwrap();
        w.commit().unwrap();
        db.commit_seq().unwrap()
    };
    for _ in 0..3 {
        let db = Database::open(&dir.0).unwrap();
        assert_eq!(db.commit_seq().unwrap(), seq, "open is not a write");
    }
}
