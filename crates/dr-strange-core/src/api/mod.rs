//! Public API layer (arch/04): `Database`, `PlaneHandle`, `WriteTxn`.
//! The only surface wrappers may use. M0 scope: open/in-memory, plane
//! lifecycle, create node/edge, get node, 1-hop expansion.
//!
//! Engine dispatch: graph logic lives in `storage::graph` and is written
//! against `&dyn` transactions; this layer only chooses the backend (a
//! small enum — the one place that knows concrete engine types) and owns
//! transaction lifecycles. TODO(M2): query builder mirroring plan operators.

use std::path::Path;

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, StorageEngine, WriteTransaction};
use crate::storage::graph;
use crate::storage::memory::{MemoryEngine, MemoryWriteTxn};
use crate::storage::redb_backend::{RedbEngine, RedbWriteTxn};
use crate::types::{Dir, EdgeId, Neighbor, NodeId, NodeRecord, PlaneId, Properties};

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
}

impl WriteTxn<'_> {
    fn txn(&mut self) -> &mut dyn WriteTransaction {
        match &mut self.inner {
            TxnInner::Memory(t) => &mut **t,
            TxnInner::Redb(t) => &mut **t,
        }
    }

    pub fn create_node(&mut self, labels: &[&str], props: Properties) -> Result<NodeId> {
        let plane = self.plane;
        graph::create_node(self.txn(), plane, labels, &props)
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
        graph::create_edge(self.txn(), plane, src, dst, ty, &props)
    }

    pub fn commit(self) -> Result<()> {
        match self.inner {
            TxnInner::Memory(t) => (*t).commit(),
            TxnInner::Redb(t) => (*t).commit(),
        }
    }
}
