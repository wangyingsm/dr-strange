//! Public API layer (arch/04): `Database`, `PlaneHandle`, `WriteTxn`.
//! The only surface wrappers may use. M1 scope: M0's vertical slice plus
//! deletes, external keys, property mutation, and batched id allocation.
//!
//! Engine dispatch: graph logic lives in `storage::graph` and is written
//! against `&dyn` transactions; this layer only chooses the backend (a
//! small enum — the one place that knows concrete engine types) and owns
//! transaction lifecycles. TODO(M2): query builder mirroring plan operators.

use std::path::Path;

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, StorageEngine, WriteTransaction};
use crate::storage::graph::{self, IdAllocator};
use crate::storage::memory::{MemoryEngine, MemoryWriteTxn};
use crate::storage::redb_backend::{RedbEngine, RedbWriteTxn};
use crate::types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, Properties,
};

enum Engine {
    // Boxed: the memory engine embeds its tables inline and dwarfs the redb
    // handle (clippy::large_enum_variant).
    Memory(Box<MemoryEngine>),
    Redb(RedbEngine),
}

impl Engine {
    fn with_read<T>(&self, f: impl FnOnce(&dyn ReadTransaction) -> Result<T>) -> Result<T> {
        match self {
            Engine::Memory(e) => f(&e.begin_read()?),
            Engine::Redb(e) => f(&e.begin_read()?),
        }
    }

    /// Runs `f` in a write transaction and commits iff it succeeded.
    fn with_write<T>(&self, f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T>) -> Result<T> {
        match self {
            Engine::Memory(e) => {
                let mut txn = e.begin_write()?;
                let out = f(&mut txn)?;
                txn.commit()?;
                Ok(out)
            }
            Engine::Redb(e) => {
                let mut txn = e.begin_write()?;
                let out = f(&mut txn)?;
                txn.commit()?;
                Ok(out)
            }
        }
    }
}

/// An embedded dr-strange database. Cheap to share behind `&`; all reads run
/// on stable snapshots; writes serialize on the backend's single writer.
pub struct Database {
    engine: Engine,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self.engine {
            Engine::Memory(_) => "memory",
            Engine::Redb(_) => "redb",
        };
        f.debug_struct("Database")
            .field("backend", &backend)
            .finish()
    }
}

impl Database {
    /// Opens (creating if needed) a database file backed by redb.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Engine::Redb(RedbEngine::open(path)?))
    }

    /// A fresh, empty in-memory database (tests, scratch work).
    pub fn in_memory() -> Result<Self> {
        Self::init(Engine::Memory(Box::default()))
    }

    fn init(engine: Engine) -> Result<Self> {
        engine.with_write(|txn| graph::init(txn))?;
        Ok(Self { engine })
    }

    /// Looks up an existing plane by name. The `"startup"` plane always exists.
    pub fn plane(&self, name: &str) -> Result<PlaneHandle<'_>> {
        let id = self
            .engine
            .with_read(|txn| graph::plane_id_by_name(txn, name))?
            .ok_or_else(|| Error::NotFound(format!("plane '{name}'")))?;
        Ok(PlaneHandle { db: self, id })
    }

    /// Creates a new, empty plane (arch/09 §3). Errors with `PlaneExists`
    /// if the name is taken.
    pub fn create_plane(&self, name: &str, props: Properties) -> Result<PlaneHandle<'_>> {
        let id = self
            .engine
            .with_write(|txn| graph::create_plane(txn, name, &props))?;
        Ok(PlaneHandle { db: self, id })
    }

    /// Deletes a plane and everything on it (arch/09 §3). Idempotent for an
    /// already-absent plane id. Errors with `InvalidArgument` for
    /// `PlaneId::STARTUP`, which always exists.
    pub fn drop_plane(&self, id: PlaneId) -> Result<()> {
        self.engine.with_write(|txn| graph::drop_plane(txn, id))
    }
}

/// Scope handle for one plane — all data access goes through one of these
/// (arch/09 §4). Copy-cheap, borrows the database.
#[derive(Clone, Copy)]
pub struct PlaneHandle<'db> {
    db: &'db Database,
    id: PlaneId,
}

impl std::fmt::Debug for PlaneHandle<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneHandle").field("id", &self.id).finish()
    }
}

impl<'db> PlaneHandle<'db> {
    pub fn id(&self) -> PlaneId {
        self.id
    }

    /// Fetches one node with decoded labels and properties; `None` if the id
    /// does not exist in this plane.
    pub fn node(&self, id: NodeId) -> Result<Option<NodeRecord>> {
        self.db
            .engine
            .with_read(|txn| graph::get_node(txn, self.id, id))
    }

    /// Fetches the node bound to a caller-supplied external key (arch/01 §2);
    /// `None` if no node in this plane carries that key.
    pub fn node_by_key(&self, external_key: &str) -> Result<Option<NodeRecord>> {
        self.db
            .engine
            .with_read(|txn| graph::get_node_by_external_key(txn, self.id, external_key))
    }

    /// Fetches one edge with its resolved type name and properties; `None`
    /// if the id does not exist in this plane.
    pub fn edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>> {
        self.db
            .engine
            .with_read(|txn| graph::get_edge(txn, self.id, id))
    }

    /// 1-hop expansion; `ty = None` means any edge type.
    pub fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<&str>) -> Result<Vec<Neighbor>> {
        self.db
            .engine
            .with_read(|txn| graph::neighbors(txn, self.id, id, dir, ty))
    }

    /// Starts a write transaction scoped to this plane. Blocks while another
    /// write transaction is open (single writer, arch/01 §6).
    pub fn write(&self) -> Result<WriteTxn<'db>> {
        let inner = match &self.db.engine {
            Engine::Memory(e) => TxnInner::Memory(Box::new(e.begin_write()?)),
            Engine::Redb(e) => TxnInner::Redb(Box::new(e.begin_write()?)),
        };
        Ok(WriteTxn {
            plane: self.id,
            inner,
            ids: IdAllocator::new(),
        })
    }
}

// Both variants boxed: whichever backend txn is larger, the enum stays two
// pointers wide, and a write transaction allocates once per begin_write.
enum TxnInner<'db> {
    Memory(Box<MemoryWriteTxn<'db>>),
    Redb(Box<RedbWriteTxn>),
}

/// A plane-scoped write transaction. Dropped without [`commit`](Self::commit)
/// ⇒ all changes discarded.
pub struct WriteTxn<'db> {
    plane: PlaneId,
    inner: TxnInner<'db>,
    /// Batched node/edge id allocator (arch/01 §2 TODO) — see
    /// `graph::IdAllocator` for the abort/commit-safety argument.
    ids: IdAllocator,
}

impl WriteTxn<'_> {
    fn txn(&mut self) -> &mut dyn WriteTransaction {
        match &mut self.inner {
            TxnInner::Memory(t) => &mut **t,
            TxnInner::Redb(t) => &mut **t,
        }
    }

    /// Splits `self` into disjoint `(txn, ids)` borrows — needed by callers
    /// that both allocate an id and write with it in the same statement.
    /// A plain `self.ids.next_node_id(self.txn())` doesn't borrow-check:
    /// `self.txn()` takes `&mut self` as a method call, which the compiler
    /// can't see is disjoint from `self.ids` the way a direct field-pattern
    /// destructure can.
    fn txn_and_ids(&mut self) -> (&mut dyn WriteTransaction, &mut IdAllocator) {
        let WriteTxn { inner, ids, .. } = self;
        let txn: &mut dyn WriteTransaction = match inner {
            TxnInner::Memory(t) => &mut **t,
            TxnInner::Redb(t) => &mut **t,
        };
        (txn, ids)
    }

    pub fn create_node(&mut self, labels: &[&str], props: Properties) -> Result<NodeId> {
        let plane = self.plane;
        let (txn, ids) = self.txn_and_ids();
        let id = ids.next_node_id(txn)?;
        graph::insert_node(txn, plane, id, None, labels, &props)?;
        Ok(id)
    }

    /// Creates a node with a caller-supplied stable key, unique within the
    /// plane (arch/01 §2). Errors with `Conflict` if the key is already
    /// bound to a different node in this plane.
    pub fn create_node_with_key(
        &mut self,
        external_key: &str,
        labels: &[&str],
        props: Properties,
    ) -> Result<NodeId> {
        let plane = self.plane;
        let (txn, ids) = self.txn_and_ids();
        let id = ids.next_node_id(txn)?;
        graph::insert_node(txn, plane, id, Some(external_key), labels, &props)?;
        Ok(id)
    }

    /// Creates a directed edge; both endpoints must exist in this plane
    /// (cross-plane edges are rejected — arch/09 §1).
    pub fn create_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        ty: &str,
        props: Properties,
    ) -> Result<EdgeId> {
        let plane = self.plane;
        let (txn, ids) = self.txn_and_ids();
        let id = ids.next_edge_id(txn)?;
        graph::insert_edge(txn, plane, id, src, dst, ty, &props)?;
        Ok(id)
    }

    /// Deletes a node, cascading to every incident edge in both directions
    /// (arch/01 §2). Idempotent: deleting an absent node is `Ok(())`.
    pub fn delete_node(&mut self, id: NodeId) -> Result<()> {
        let plane = self.plane;
        graph::delete_node(self.txn(), plane, id)
    }

    /// Deletes an edge and both of its adjacency entries. Idempotent.
    pub fn delete_edge(&mut self, id: EdgeId) -> Result<()> {
        let plane = self.plane;
        graph::delete_edge(self.txn(), plane, id)
    }

    /// Sets (inserts or overwrites) one property on an existing node.
    /// Errors with `NotFound` if the node does not exist.
    pub fn set_prop(&mut self, id: NodeId, key: &str, prop: PropDesc) -> Result<()> {
        let plane = self.plane;
        graph::set_node_prop(self.txn(), plane, id, key, prop)
    }

    /// Removes one property from an existing node; removing an absent key
    /// is not an error (soft schema — arch/01 §4).
    pub fn remove_prop(&mut self, id: NodeId, key: &str) -> Result<()> {
        let plane = self.plane;
        graph::remove_node_prop(self.txn(), plane, id, key)
    }

    /// Sets (inserts or overwrites) one property on an existing edge.
    /// Errors with `NotFound` if the edge does not exist.
    pub fn set_edge_prop(&mut self, id: EdgeId, key: &str, prop: PropDesc) -> Result<()> {
        let plane = self.plane;
        graph::set_edge_prop(self.txn(), plane, id, key, prop)
    }

    /// Removes one property from an existing edge; removing an absent key
    /// is not an error.
    pub fn remove_edge_prop(&mut self, id: EdgeId, key: &str) -> Result<()> {
        let plane = self.plane;
        graph::remove_edge_prop(self.txn(), plane, id, key)
    }

    pub fn commit(self) -> Result<()> {
        match self.inner {
            TxnInner::Memory(t) => (*t).commit(),
            TxnInner::Redb(t) => (*t).commit(),
        }
    }
}
