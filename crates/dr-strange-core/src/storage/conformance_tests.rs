//! Backend conformance suite (arch/01 §7): every `StorageEngine` must pass
//! the same semantics tests. Runs generically against the memory and redb
//! backends; a future custom engine plugs in here too.

use super::engine::{StorageEngine, TableId};
use super::memory::MemoryEngine;
#[cfg(feature = "native-backend")]
use super::native::NativeEngine;
#[cfg(feature = "redb-backend")]
use super::redb_backend::RedbEngine;
use crate::storage::{ReadTransaction, WriteTransaction};

fn conformance<E: StorageEngine>(eng: &E) {
    // put/get roundtrip after commit; tables are separate namespaces
    let mut w = eng.begin_write().unwrap();
    w.put(TableId::Nodes, b"k1", b"v1").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get(TableId::Nodes, b"k1").unwrap(), Some(b"v1".to_vec()));
    assert_eq!(r.get(TableId::Nodes, b"nope").unwrap(), None);
    assert_eq!(r.get(TableId::Edges, b"k1").unwrap(), None);
    drop(r);

    // write txn reads its own uncommitted writes
    let mut w = eng.begin_write().unwrap();
    w.put(TableId::Nodes, b"k2", b"v2").unwrap();
    assert_eq!(w.get(TableId::Nodes, b"k2").unwrap(), Some(b"v2".to_vec()));
    drop(w); // dropped without commit

    // dropped txn left no trace
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get(TableId::Nodes, b"k2").unwrap(), None);
    drop(r);

    // readers hold their snapshot across a later commit
    let before = eng.begin_read().unwrap();
    let mut w = eng.begin_write().unwrap();
    w.put(TableId::Nodes, b"k1", b"updated").unwrap();
    w.commit().unwrap();
    assert_eq!(
        before.get(TableId::Nodes, b"k1").unwrap(),
        Some(b"v1".to_vec())
    );
    drop(before);
    let after = eng.begin_read().unwrap();
    assert_eq!(
        after.get(TableId::Nodes, b"k1").unwrap(),
        Some(b"updated".to_vec())
    );
    drop(after);

    // ordered range, end-exclusive and unbounded
    let mut w = eng.begin_write().unwrap();
    for k in [&b"a"[..], b"b", b"c", b"d"] {
        w.put(TableId::AdjFwd, k, b"").unwrap();
    }
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    let keys: Vec<Vec<u8>> = r
        .range(TableId::AdjFwd, b"b", Some(b"d"))
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(keys, vec![b"b".to_vec(), b"c".to_vec()]);
    let keys: Vec<Vec<u8>> = r
        .range(TableId::AdjFwd, b"b", None)
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(keys, vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    drop(r);

    // delete + delete_prefix
    let mut w = eng.begin_write().unwrap();
    for k in [&b"aa1"[..], b"aa2", b"ab1", b"zz"] {
        w.put(TableId::LabelIdx, k, b"").unwrap();
    }
    w.delete(TableId::LabelIdx, b"zz").unwrap();
    w.delete_prefix(TableId::LabelIdx, b"aa").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    let keys: Vec<Vec<u8>> = r
        .range(TableId::LabelIdx, b"", None)
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(keys, vec![b"ab1".to_vec()]);
}

/// KV edge cases every backend must agree on.
fn conformance_edge_cases<E: StorageEngine>(eng: &E) {
    // overwrite replaces the value
    let mut w = eng.begin_write().unwrap();
    w.put(TableId::Meta, b"k", b"one").unwrap();
    w.put(TableId::Meta, b"k", b"two").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get(TableId::Meta, b"k").unwrap(), Some(b"two".to_vec()));
    drop(r);

    // deleting a missing key is a no-op, not an error
    let mut w = eng.begin_write().unwrap();
    w.delete(TableId::Meta, b"never-existed").unwrap();

    // empty values are valid and distinct from absent keys
    w.put(TableId::Meta, b"empty", b"").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.get(TableId::Meta, b"empty").unwrap(), Some(Vec::new()));
    assert_eq!(r.get(TableId::Meta, b"absent").unwrap(), None);

    // range over an empty table yields nothing
    assert_eq!(
        r.range(TableId::PropIdx, b"", None).unwrap().count(),
        0,
        "empty table should have no entries"
    );
    drop(r);

    // WRITE transactions must support range over staged data too — this is
    // what graph ops rely on mid-transaction (e.g. future delete flows).
    let mut w = eng.begin_write().unwrap();
    for k in [&b"wa"[..], b"wb", b"wc"] {
        w.put(TableId::ExtKeys, k, b"v").unwrap();
    }
    let staged: Vec<Vec<u8>> = w
        .range(TableId::ExtKeys, b"wa", Some(b"wc"))
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(staged, vec![b"wa".to_vec(), b"wb".to_vec()]);
    let staged_all: Vec<Vec<u8>> = w
        .range(TableId::ExtKeys, b"", None)
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(staged_all.len(), 3);
    // delete_prefix inside the same transaction sees staged writes
    w.delete_prefix(TableId::ExtKeys, b"w").unwrap();
    assert_eq!(w.range(TableId::ExtKeys, b"", None).unwrap().count(), 0);
    drop(w); // abort — none of it lands

    let r = eng.begin_read().unwrap();
    assert_eq!(r.get(TableId::ExtKeys, b"wa").unwrap(), None);
    drop(r);

    // empty prefix deletes the whole table (and only that table)
    let mut w = eng.begin_write().unwrap();
    w.put(TableId::PropIdx, b"x", b"").unwrap();
    w.put(TableId::PropIdx, b"y", b"").unwrap();
    w.put(TableId::LabelIdx, b"keep", b"").unwrap();
    w.delete_prefix(TableId::PropIdx, b"").unwrap();
    w.commit().unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(r.range(TableId::PropIdx, b"", None).unwrap().count(), 0);
    assert_eq!(r.get(TableId::LabelIdx, b"keep").unwrap(), Some(Vec::new()));
}

#[test]
fn memory_backend_conformance() {
    conformance(&MemoryEngine::new());
}

#[test]
fn memory_backend_edge_cases() {
    conformance_edge_cases(&MemoryEngine::new());
}

#[cfg(feature = "redb-backend")]
#[test]
fn redb_backend_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let eng = RedbEngine::open(dir.path().join("edge.redb")).unwrap();
    conformance_edge_cases(&eng);
}

#[cfg(feature = "redb-backend")]
#[test]
fn redb_backend_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let eng = RedbEngine::open(dir.path().join("t.redb")).unwrap();
    conformance(&eng);
}

#[cfg(feature = "redb-backend")]
#[test]
fn redb_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    {
        let eng = RedbEngine::open(&path).unwrap();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"persist", b"me").unwrap();
        w.commit().unwrap();
    }
    let eng = RedbEngine::open(&path).unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(
        r.get(TableId::Nodes, b"persist").unwrap(),
        Some(b"me".to_vec())
    );
}

#[cfg(feature = "native-backend")]
#[test]
fn native_backend_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let eng = NativeEngine::open(dir.path().join("edge.drsg")).unwrap();
    conformance_edge_cases(&eng);
}

#[cfg(feature = "native-backend")]
#[test]
fn native_backend_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let eng = NativeEngine::open(dir.path().join("t.drsg")).unwrap();
    conformance(&eng);
}

#[cfg(feature = "native-backend")]
#[test]
fn native_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.drsg");
    {
        let eng = NativeEngine::open(&path).unwrap();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"persist", b"me").unwrap();
        w.commit().unwrap();
    }
    let eng = NativeEngine::open(&path).unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(
        r.get(TableId::Nodes, b"persist").unwrap(),
        Some(b"me".to_vec())
    );
}

// The whole conformance surface, but with a 1-byte flush threshold so *every*
// commit is flushed to its own SST — exercising the merged memtable+SST read
// path (MVCC across runs, tombstones-in-SSTs, range merge) end to end.
#[cfg(feature = "native-backend")]
#[test]
fn native_conformance_while_flushing() {
    let dir = tempfile::tempdir().unwrap();
    let eng = NativeEngine::open_with_threshold(dir.path().join("f.drsg"), 1).unwrap();
    conformance(&eng);
}

#[cfg(feature = "native-backend")]
#[test]
fn native_edge_cases_while_flushing() {
    let dir = tempfile::tempdir().unwrap();
    let eng = NativeEngine::open_with_threshold(dir.path().join("fe.drsg"), 1).unwrap();
    conformance_edge_cases(&eng);
}

// Many flushes trigger compaction: the run count stays bounded (well below the
// number of flushes), reads see the latest values, deletes are reclaimed — and
// a reader pinned before the churn STILL sees its old value, so the merge did
// not drop a version a live snapshot needs.
#[cfg(feature = "native-backend")]
#[test]
fn native_compacts_and_preserves_mvcc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.drsg");
    let eng = NativeEngine::open_with_threshold(&path, 1).unwrap();

    let commit = |k: &[u8], v: &[u8]| {
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, k, v).unwrap();
        w.commit().unwrap();
    };

    commit(b"k", b"v0");
    // Pin a reader here — before "gone" exists and before the churn.
    let pinned = eng.begin_read().unwrap();
    commit(b"gone", b"x");

    for i in 1..=8 {
        commit(b"k", format!("v{i}").as_bytes());
    }
    {
        let mut w = eng.begin_write().unwrap();
        w.delete(TableId::Nodes, b"gone").unwrap();
        w.commit().unwrap();
    }

    // ~11 flushes, but compaction bounded the run count.
    let ssts = std::fs::read_dir(&path)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("sst-"))
        .count();
    assert!(
        (1..=5).contains(&ssts),
        "compaction should bound runs; found {ssts}"
    );

    // Newest reader: latest value, deletion reclaimed.
    let now = eng.begin_read().unwrap();
    assert_eq!(now.get(TableId::Nodes, b"k").unwrap(), Some(b"v8".to_vec()));
    assert_eq!(now.get(TableId::Nodes, b"gone").unwrap(), None);

    // Pinned reader: still its original snapshot, kept alive through compaction.
    assert_eq!(
        pinned.get(TableId::Nodes, b"k").unwrap(),
        Some(b"v0".to_vec())
    );
    assert_eq!(pinned.get(TableId::Nodes, b"gone").unwrap(), None); // didn't exist yet
}

// Force several flushes, confirm SST files appear, then reopen and read back —
// data now comes from the SSTs (the WAL was rotated), including an update that
// shadows an older run and a delete that tombstones across a flush.
#[cfg(feature = "native-backend")]
#[test]
fn native_reads_and_reopens_across_ssts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.drsg");
    {
        let eng = NativeEngine::open_with_threshold(&path, 1).unwrap();
        for (k, v) in [(&b"a"[..], &b"1"[..]), (b"b", b"2"), (b"c", b"3")] {
            let mut w = eng.begin_write().unwrap();
            w.put(TableId::Nodes, k, v).unwrap();
            w.commit().unwrap();
        }
        // Update `a` (shadows the older SST) and delete `b` (tombstone).
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"a", b"updated").unwrap();
        w.delete(TableId::Nodes, b"b").unwrap();
        w.commit().unwrap();

        // Several sst-* files exist; the WAL is empty after the last flush.
        let ssts = std::fs::read_dir(&path)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("sst-"))
            .count();
        assert!(ssts >= 3, "expected multiple SSTs, found {ssts}");
    }

    // Reopen: everything is served from the SSTs.
    let eng = NativeEngine::open_with_threshold(&path, 1).unwrap();
    let r = eng.begin_read().unwrap();
    assert_eq!(
        r.get(TableId::Nodes, b"a").unwrap(),
        Some(b"updated".to_vec())
    );
    assert_eq!(r.get(TableId::Nodes, b"b").unwrap(), None); // deleted
    assert_eq!(r.get(TableId::Nodes, b"c").unwrap(), Some(b"3".to_vec()));
    let keys: Vec<Vec<u8>> = r
        .range(TableId::Nodes, b"", None)
        .unwrap()
        .map(|kv| kv.unwrap().0)
        .collect();
    assert_eq!(keys, vec![b"a".to_vec(), b"c".to_vec()]);
}
