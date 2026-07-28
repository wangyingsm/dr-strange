//! Durability / crash-recovery, part 1: a fault-injecting `StorageEngine`
//! wrapper (arch/01 §7). It fails the Nth mutating op or the commit of a
//! write transaction, letting us assert that (a) graph operations propagate a
//! backend error rather than silently half-applying, and (b) a failed commit
//! is atomic — the previously committed state is untouched, nothing partial
//! lands. Deterministic and CI-safe (no OS-level kill).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dr_strange_core::storage::engine::{
    KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction,
};
use dr_strange_core::storage::graph;
use dr_strange_core::storage::memory::{MemoryEngine, MemoryReadTxn, MemoryWriteTxn};
use dr_strange_core::{Error, PlaneId, PropDesc, PropValue, Properties, Result};

// ---- the fault-injecting engine ------------------------------------------

/// Wraps a `MemoryEngine`; can be armed to fail a specific write op or the
/// commit. Arming is per-`begin_write` (each transaction snapshots the current
/// arming), so a test can init/commit clean, then arm and exercise a failure.
struct FaultyEngine {
    inner: MemoryEngine,
    /// 1-based index of the mutating op to fail (0 = never).
    fail_op: AtomicUsize,
    fail_commit: AtomicBool,
}

impl FaultyEngine {
    fn new() -> Self {
        Self {
            inner: MemoryEngine::new(),
            fail_op: AtomicUsize::new(0),
            fail_commit: AtomicBool::new(false),
        }
    }

    fn arm_op(&self, nth: usize) {
        self.fail_op.store(nth, Ordering::SeqCst);
    }
    fn arm_commit(&self) {
        self.fail_commit.store(true, Ordering::SeqCst);
    }
    fn disarm(&self) {
        self.fail_op.store(0, Ordering::SeqCst);
        self.fail_commit.store(false, Ordering::SeqCst);
    }
}

fn injected() -> Error {
    Error::Io(std::io::Error::other("injected fault"))
}

impl StorageEngine for FaultyEngine {
    type ReadTxn<'a> = MemoryReadTxn;
    type WriteTxn<'a> = FaultyWriteTxn<'a>;

    fn begin_read(&self) -> Result<MemoryReadTxn> {
        self.inner.begin_read()
    }

    fn begin_write(&self) -> Result<FaultyWriteTxn<'_>> {
        Ok(FaultyWriteTxn {
            inner: self.inner.begin_write()?,
            fail_op: self.fail_op.load(Ordering::SeqCst),
            fail_commit: self.fail_commit.load(Ordering::SeqCst),
            ops: 0,
        })
    }
}

struct FaultyWriteTxn<'a> {
    inner: MemoryWriteTxn<'a>,
    fail_op: usize,
    fail_commit: bool,
    ops: usize,
}

impl FaultyWriteTxn<'_> {
    /// Counts a mutating op and returns an error if this is the armed one.
    fn tick(&mut self) -> Result<()> {
        self.ops += 1;
        if self.fail_op != 0 && self.ops == self.fail_op {
            return Err(injected());
        }
        Ok(())
    }
}

impl ReadTransaction for FaultyWriteTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.inner.get(table, key)
    }
    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        self.inner.range(table, start, end)
    }
}

impl WriteTransaction for FaultyWriteTxn<'_> {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()> {
        self.tick()?;
        self.inner.put(table, key, value)
    }
    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()> {
        self.tick()?;
        self.inner.delete(table, key)
    }
    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()> {
        self.tick()?;
        self.inner.delete_prefix(table, prefix)
    }
    fn commit(self) -> Result<()> {
        if self.fail_commit {
            return Err(injected()); // drop the staged writes, apply nothing
        }
        self.inner.commit()
    }
}

// ---- helpers -------------------------------------------------------------

fn props(entries: &[(&str, PropValue)]) -> Properties {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
        .collect()
}

/// Node ids present in the startup plane (a committed read).
fn startup_nodes(eng: &FaultyEngine) -> Vec<u64> {
    let txn = eng.begin_read().unwrap();
    graph::scan_all(&txn, PlaneId::STARTUP)
        .unwrap()
        .iter()
        .map(|n| n.0)
        .collect()
}

fn init(eng: &FaultyEngine) {
    let mut w = eng.begin_write().unwrap();
    graph::init(&mut w).unwrap();
    w.commit().unwrap();
}

// ---- tests ---------------------------------------------------------------

#[test]
fn wrapper_is_transparent_when_disarmed() {
    // Sanity: with no fault armed, graph ops behave exactly as on a plain
    // MemoryEngine — so the failure tests below are meaningful.
    let eng = FaultyEngine::new();
    init(&eng);
    let mut w = eng.begin_write().unwrap();
    let a = graph::create_node(
        &mut w,
        PlaneId::STARTUP,
        &["N"],
        &props(&[("v", PropValue::Int(1))]),
    )
    .unwrap();
    let b = graph::create_node(&mut w, PlaneId::STARTUP, &["N"], &Properties::new()).unwrap();
    graph::create_edge(&mut w, PlaneId::STARTUP, a, b, "E", &Properties::new()).unwrap();
    w.commit().unwrap();

    assert_eq!(startup_nodes(&eng), vec![a.0, b.0]);
    let txn = eng.begin_read().unwrap();
    assert_eq!(
        graph::neighbors(&txn, PlaneId::STARTUP, a, dr_strange_core::Dir::Out, None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_failed_put_propagates_as_an_error() {
    let eng = FaultyEngine::new();
    init(&eng);
    // Fail the very first mutating op of the next transaction.
    eng.arm_op(1);
    let mut w = eng.begin_write().unwrap();
    let result = graph::create_node(&mut w, PlaneId::STARTUP, &["N"], &Properties::new());
    assert!(matches!(result, Err(Error::Io(_))), "got: {result:?}");
    drop(w); // never committed
    eng.disarm();
    // Nothing persisted (the txn was never committed).
    assert!(startup_nodes(&eng).is_empty());
}

#[test]
fn a_failure_midway_through_a_multi_put_op_persists_nothing() {
    // create_node issues several puts (id counter, dictionary, node record,
    // node_plane, label index). Failing partway must not commit a torn node.
    let eng = FaultyEngine::new();
    init(&eng);
    eng.arm_op(3); // fail the 3rd put inside create_node
    let mut w = eng.begin_write().unwrap();
    assert!(graph::create_node(&mut w, PlaneId::STARTUP, &["Paper"], &Properties::new()).is_err());
    drop(w);
    eng.disarm();
    assert!(startup_nodes(&eng).is_empty());
    // and the label dictionary / scan is clean afterwards
    let txn = eng.begin_read().unwrap();
    assert!(
        graph::scan_label(&txn, PlaneId::STARTUP, "Paper")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_failed_commit_is_atomic() {
    let eng = FaultyEngine::new();
    init(&eng);
    // Commit one node successfully — the baseline that must survive.
    let baseline = {
        let mut w = eng.begin_write().unwrap();
        let id =
            graph::create_node(&mut w, PlaneId::STARTUP, &["Keep"], &Properties::new()).unwrap();
        w.commit().unwrap();
        id
    };
    assert_eq!(startup_nodes(&eng), vec![baseline.0]);

    // Now a transaction that builds fine but fails at commit.
    eng.arm_commit();
    let mut w = eng.begin_write().unwrap();
    graph::create_node(&mut w, PlaneId::STARTUP, &["Doomed"], &Properties::new()).unwrap();
    graph::create_node(&mut w, PlaneId::STARTUP, &["Doomed"], &Properties::new()).unwrap();
    assert!(matches!(w.commit(), Err(Error::Io(_))));
    eng.disarm();

    // Atomic: exactly the baseline remains; the doomed nodes never landed.
    assert_eq!(startup_nodes(&eng), vec![baseline.0]);
}

#[test]
fn a_failed_delete_during_drop_plane_leaves_it_intact() {
    // drop_plane issues many deletes; a mid-drop failure must leave the plane
    // fully readable (nothing committed).
    let eng = FaultyEngine::new();
    init(&eng);
    let (plane, node) = {
        let mut w = eng.begin_write().unwrap();
        let plane = graph::create_plane(&mut w, "scratch", &Properties::new()).unwrap();
        let node = graph::create_node(&mut w, plane, &["N"], &Properties::new()).unwrap();
        w.commit().unwrap();
        (plane, node)
    };

    eng.arm_op(1); // fail the first delete inside drop_plane
    let mut w = eng.begin_write().unwrap();
    assert!(graph::drop_plane(&mut w, plane).is_err());
    drop(w);
    eng.disarm();

    // The plane and its node are untouched.
    let txn = eng.begin_read().unwrap();
    assert_eq!(
        graph::plane_id_by_name(&txn, "scratch").unwrap(),
        Some(plane)
    );
    assert!(graph::get_node(&txn, plane, node).unwrap().is_some());
}
