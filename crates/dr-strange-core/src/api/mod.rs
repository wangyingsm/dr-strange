//! Public API layer (arch/04): `Database`, `PlaneHandle`, `WriteTxn`, and the
//! read [`QueryBuilder`]. The only surface wrappers may use. Covers writes
//! (create/delete/mutate, external keys, batched ids) and the M2 query
//! engine (scan/seek → expand/filter/sort/limit → nodes/ids/count/select).
//!
//! Engine dispatch: graph logic lives in `storage::graph` and query execution
//! in `compute::exec`, both written against `&dyn` seams (transactions,
//! `GraphReader`); this layer only chooses the backend (a small enum — the
//! one place that knows concrete engine types) and owns transaction and
//! reader lifecycles.

use std::path::Path;

use crate::cache::{GraphReader, UncachedReader};
use crate::compute::exec;
use crate::compute::expr::{self, Expr};
use crate::compute::plan::{LogicalPlan, SortKey, Source, Step};
use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, StorageEngine, WriteTransaction};
use crate::storage::graph::{self, IdAllocator};
use crate::storage::memory::{MemoryEngine, MemoryWriteTxn};
use crate::storage::redb_backend::{RedbEngine, RedbWriteTxn};
use crate::types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
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

    /// Starts a query in this plane (arch/03, arch/04 §3). Defaults to
    /// scanning every node; call a source method (`scan_label`, `seek_ids`,
    /// …) to narrow it, then chain steps and a terminal:
    ///
    /// ```ignore
    /// let papers = plane.query()
    ///     .scan_label("Paper")
    ///     .expand_out("CITES")
    ///     .filter(p("year").ge(2020))
    ///     .limit(10)
    ///     .nodes()?;
    /// ```
    pub fn query(&self) -> QueryBuilder<'db> {
        QueryBuilder {
            plane: *self,
            plan: LogicalPlan::new(Source::ScanAll),
        }
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

/// A fluent builder for a read query (arch/04 §3). Constructs a
/// [`LogicalPlan`] one operator at a time — the builder mirrors plan
/// operators one-to-one, adding no semantics of its own (arch/03 §2) — then
/// a terminal runs it through the executor over a read snapshot.
///
/// Source methods (`scan_label`, `seek_ids`, …) set where rows come from and
/// are normally called first; step methods append to the pipeline; terminals
/// (`nodes`, `ids`, `count`, `select`) execute.
#[derive(Clone)]
pub struct QueryBuilder<'db> {
    plane: PlaneHandle<'db>,
    plan: LogicalPlan,
}

impl std::fmt::Debug for QueryBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryBuilder")
            .field("plane", &self.plane.id())
            .field("plan", &self.plan)
            .finish()
    }
}

impl<'db> QueryBuilder<'db> {
    // ---- sources (set where rows originate) ------------------------------

    /// Every node in the plane (the default).
    pub fn scan_all(mut self) -> Self {
        self.plan.source = Source::ScanAll;
        self
    }

    /// Every node carrying `label`.
    pub fn scan_label(mut self, label: impl Into<String>) -> Self {
        self.plan.source = Source::ScanLabel(label.into());
        self
    }

    /// Specific node ids (ids not present in the plane are dropped).
    pub fn seek_ids(mut self, ids: impl IntoIterator<Item = NodeId>) -> Self {
        self.plan.source = Source::SeekIds(ids.into_iter().collect());
        self
    }

    /// Nodes resolved from external keys (unresolved keys are dropped).
    pub fn seek_keys<S: Into<String>>(mut self, keys: impl IntoIterator<Item = S>) -> Self {
        self.plan.source = Source::SeekKeys(keys.into_iter().map(Into::into).collect());
        self
    }

    // ---- steps -----------------------------------------------------------

    /// 1-hop expansion in `dir`; `edge_type = None` means any type.
    pub fn expand(mut self, dir: Dir, edge_type: Option<&str>) -> Self {
        self.plan.push(Step::Expand {
            dir,
            edge_type: edge_type.map(str::to_string),
        });
        self
    }

    pub fn expand_out(self, edge_type: &str) -> Self {
        self.expand(Dir::Out, Some(edge_type))
    }

    pub fn expand_in(self, edge_type: &str) -> Self {
        self.expand(Dir::In, Some(edge_type))
    }

    pub fn expand_both(self, edge_type: &str) -> Self {
        self.expand(Dir::Both, Some(edge_type))
    }

    /// Variable-length expansion over `min..=max` hops (walk semantics).
    pub fn expand_var(mut self, dir: Dir, edge_type: Option<&str>, min: u32, max: u32) -> Self {
        self.plan.push(Step::ExpandVar {
            dir,
            edge_type: edge_type.map(str::to_string),
            min,
            max,
        });
        self
    }

    /// Keep rows whose current node satisfies `predicate`.
    pub fn filter(mut self, predicate: Expr) -> Self {
        self.plan.push(Step::Filter(predicate));
        self
    }

    /// Deduplicate by current node id.
    pub fn distinct(mut self) -> Self {
        self.plan.push(Step::Distinct);
        self
    }

    pub fn skip(mut self, n: u64) -> Self {
        self.plan.push(Step::Skip(n));
        self
    }

    pub fn limit(mut self, n: u64) -> Self {
        self.plan.push(Step::Limit(n));
        self
    }

    /// Sort by explicit keys (evaluated on the current node).
    pub fn sort_by(mut self, keys: Vec<SortKey>) -> Self {
        self.plan.push(Step::Sort(keys));
        self
    }

    pub fn sort_asc(self, expr: Expr) -> Self {
        self.sort_by(vec![SortKey {
            expr,
            descending: false,
        }])
    }

    pub fn sort_desc(self, expr: Expr) -> Self {
        self.sort_by(vec![SortKey {
            expr,
            descending: true,
        }])
    }

    // ---- inspection ------------------------------------------------------

    /// The plan built so far (serializable — arch/00 §2).
    pub fn plan(&self) -> &LogicalPlan {
        &self.plan
    }

    // ---- terminals (execute) ---------------------------------------------

    fn with_reader<T>(&self, f: impl FnOnce(&UncachedReader) -> Result<T>) -> Result<T> {
        self.plane.db.engine.with_read(|txn| {
            let reader = UncachedReader::new(txn, self.plane.id());
            f(&reader)
        })
    }

    /// Current-node ids of the matching rows, in pipeline order.
    pub fn ids(&self) -> Result<Vec<NodeId>> {
        self.with_reader(|reader| {
            exec::execute(&self.plan, reader)?
                .map(|r| r.map(|row| row.head))
                .collect()
        })
    }

    /// The full current-node records of the matching rows.
    pub fn nodes(&self) -> Result<Vec<NodeRecord>> {
        self.with_reader(|reader| {
            let mut out = Vec::new();
            for r in exec::execute(&self.plan, reader)? {
                let row = r?;
                if let Some(node) = reader.node(row.head)? {
                    out.push((*node).clone());
                }
            }
            Ok(out)
        })
    }

    /// Number of matching rows.
    pub fn count(&self) -> Result<usize> {
        self.with_reader(|reader| {
            let mut n = 0usize;
            for r in exec::execute(&self.plan, reader)? {
                r?;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Evaluate `exprs` against each matching row's current node — one output
    /// tuple per row (arch/03's projection, terminal form for v0).
    pub fn select(&self, exprs: &[Expr]) -> Result<Vec<Vec<PropValue>>> {
        self.with_reader(|reader| {
            let mut out = Vec::new();
            for r in exec::execute(&self.plan, reader)? {
                let row = r?;
                let node = reader.node(row.head)?;
                out.push(
                    exprs
                        .iter()
                        .map(|e| expr::eval(e, node.as_deref()))
                        .collect(),
                );
            }
            Ok(out)
        })
    }
}
