//! `serve --follow` replication (arch/01 §9): a batch captured from one
//! native engine's commits, replayed into a fresh engine via
//! `apply_replicated`, must reproduce the exact same KV content — not just
//! the same graph-level view. Replication ships raw ops precisely so tables
//! the graph API doesn't itself model (indices, counters) stay in sync too,
//! so these tests compare every `TableId`, not just nodes/edges.

#![cfg(feature = "native-backend")]

use std::sync::{Arc, Mutex};

use dr_strange_core::ReplicatedBatch;
use dr_strange_core::storage::engine::{ReadTransaction, StorageEngine, TableId, WriteTransaction};
use dr_strange_core::storage::native::NativeEngine;

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
