//! Backend conformance suite (arch/01 §7): every `StorageEngine` must pass
//! the same semantics tests. Runs generically against the memory and redb
//! backends; a future custom engine plugs in here too.

use super::engine::{StorageEngine, TableId};
use super::memory::MemoryEngine;
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

#[test]
fn redb_backend_edge_cases() {
    let dir = tempfile::tempdir().unwrap();
    let eng = RedbEngine::open(dir.path().join("edge.redb")).unwrap();
    conformance_edge_cases(&eng);
}

#[test]
fn redb_backend_conformance() {
    let dir = tempfile::tempdir().unwrap();
    let eng = RedbEngine::open(dir.path().join("t.redb")).unwrap();
    conformance(&eng);
}

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
