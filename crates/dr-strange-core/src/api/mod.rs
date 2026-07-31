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

// `Path` is only referenced by the backend-gated `open`; `PathBuf` is used
// unconditionally (sidecar fields), so import them separately to avoid an
// unused-import warning in a no-backend build (e.g. dr-strange-llm's).
#[cfg(any(feature = "redb-backend", feature = "native-backend"))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::RwLock;

use crate::cache::{CachedReader, GraphCache, GraphReader};
use crate::compute::algo::{self, LouvainOptions, PageRankOptions, ShortestPathOptions};
use crate::compute::catalog::{self, CatalogSnapshot};
use crate::compute::exec;
use crate::compute::expr::{self, Expr, score};
use crate::compute::hybrid::{self, Channel, HybridHit, HybridWeights};
use crate::compute::plan::{LogicalPlan, SortKey, Source, Step};
use crate::error::{Error, Result};
use crate::index::VectorRegistry;
use crate::keyword::KeywordRegistry;
use crate::storage::engine::{ReadTransaction, StorageEngine, WriteTransaction};
use crate::storage::graph::{self, BulkEdge, BulkEdgeById, BulkNode, BulkStats, IdAllocator};
use crate::storage::memory::{MemoryEngine, MemoryWriteTxn};
#[cfg(feature = "native-backend")]
use crate::storage::native::{NativeEngine, NativeWriteTxn};
#[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
use crate::storage::redb_backend::{RedbEngine, RedbWriteTxn};
use crate::storage::vector::Metric;
use crate::text::Language;
use crate::types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
};

enum Engine {
    // Boxed: the memory engine embeds its tables inline and dwarfs the redb
    // handle (clippy::large_enum_variant).
    Memory(Box<MemoryEngine>),
    #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
    Redb(RedbEngine),
    #[cfg(feature = "native-backend")]
    Native(Box<NativeEngine>),
}

impl Engine {
    fn with_read<T>(&self, f: impl FnOnce(&dyn ReadTransaction) -> Result<T>) -> Result<T> {
        match self {
            Engine::Memory(e) => f(&e.begin_read()?),
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            Engine::Redb(e) => f(&e.begin_read()?),
            #[cfg(feature = "native-backend")]
            Engine::Native(e) => f(&e.begin_read()?),
        }
    }

    /// Runs `f` in a write transaction and commits iff it succeeded. Every
    /// committed write bumps the commit sequence (arch/02 §3) so the cache's
    /// version stamp advances — coarse invalidation: any write logically
    /// flushes the cache for subsequent snapshots.
    fn with_write<T>(&self, f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T>) -> Result<T> {
        // One body per backend (the concrete txn type differs); a macro keeps
        // them identical. Only one arm runs, so consuming `f` in each is fine.
        macro_rules! run {
            ($e:expr) => {{
                let mut txn = $e.begin_write()?;
                let out = f(&mut txn)?;
                graph::bump_commit_seq(&mut txn)?;
                graph::write_commit_time(&mut txn, now_millis())?;
                txn.commit()?;
                Ok(out)
            }};
        }
        match self {
            Engine::Memory(e) => run!(e),
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            Engine::Redb(e) => run!(e),
            #[cfg(feature = "native-backend")]
            Engine::Native(e) => run!(e),
        }
    }

    /// Runs `f` over a read transaction pinned to a past commit `snapshot`
    /// (time-travel / AS OF). Native-only — only the LSM engine keeps prior
    /// versions — so the whole method is gated to it; a memory database (the
    /// one other variant a native build can hold) has no history and errors.
    #[cfg(feature = "native-backend")]
    fn with_read_at<T>(
        &self,
        snapshot: u64,
        f: impl FnOnce(&dyn ReadTransaction) -> Result<T>,
    ) -> Result<T> {
        match self {
            Engine::Native(e) => f(&e.begin_read_at(snapshot)?),
            Engine::Memory(_) => Err(no_time_travel()),
        }
    }

    /// The latest committed sequence, and the oldest sequence retention keeps
    /// queryable — the inclusive snapshot window `[floor, latest]` time-travel
    /// may address. Native-only.
    #[cfg(feature = "native-backend")]
    fn snapshot_window(&self) -> Result<(u64, u64)> {
        match self {
            Engine::Native(e) => Ok((e.retained_floor(), e.committed_seq())),
            Engine::Memory(_) => Err(no_time_travel()),
        }
    }
}

/// The error a memory database returns for a time-travel request (no history).
#[cfg(feature = "native-backend")]
fn no_time_travel() -> Error {
    Error::InvalidArgument(
        "time-travel (AS OF) needs an on-disk native database; this one is in memory".into(),
    )
}

/// An embedded dr-strange database. Cheap to share behind `&`; all reads run
/// on stable snapshots; writes serialize on the backend's single writer.
/// Cross-query decoded-object cache budget (arch/02 §4). Modest by default —
/// embedded-library ethos; a host can size it up when it wants to.
const CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Current wall-clock time as unix-epoch milliseconds, stamped on each commit
/// for time-addressed time-travel. Saturates to `0` on the (impossible)
/// pre-epoch clock so the commit never fails on a clock quirk.
fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A past point to read the graph at — the address for a time-travel (AS OF)
/// query (ROADMAP §4). Resolved to a storage snapshot by the native backend,
/// the only engine that keeps the history AS OF needs — hence gated to it. Both
/// forms use "at or before" semantics: a value between two commits resolves to
/// the latest commit that is not after it.
#[cfg(feature = "native-backend")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOf {
    /// A commit sequence, as returned by [`Database::commit_seq`].
    Seq(u64),
    /// A wall-clock instant in unix-epoch milliseconds.
    Time(i64),
}

pub struct Database {
    engine: Engine,
    /// In-memory vector indexes (arch/01 §5). On open, loaded from the `.hnsw`
    /// sidecar when fresh, else rebuilt from the KV; read-locked during queries,
    /// write-locked at commit to apply the coherence events a write transaction
    /// buffered.
    indexes: RwLock<VectorRegistry>,
    /// In-memory BM25 keyword indexes (ROADMAP §2). Managed exactly like
    /// `indexes`: sidecar-loaded when fresh else rebuilt, write-locked at commit
    /// to apply buffered text changes.
    keywords: RwLock<KeywordRegistry>,
    /// Cross-query, seq-stamped decoded-record cache (arch/02 §3). Memory-only;
    /// rebuilt cold on open, so it never holds durable state.
    cache: GraphCache,
    /// Where the HNSW sidecar lives (the `.hnsw` file beside the database), or
    /// `None` for an in-memory database. Loaded on open when fresh; saved
    /// best-effort on drop.
    sidecar: Option<PathBuf>,
    /// The `.bm25` keyword sidecar beside the database, or `None` in-memory.
    keyword_sidecar: Option<PathBuf>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backend = match self.engine {
            Engine::Memory(_) => "memory",
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            Engine::Redb(_) => "redb",
            #[cfg(feature = "native-backend")]
            Engine::Native(_) => "native",
        };
        f.debug_struct("Database")
            .field("backend", &backend)
            .finish()
    }
}

/// The HNSW sidecar path for a database file: the file name with `.hnsw`
/// appended (e.g. `graph.drsg` → `graph.drsg.hnsw`), so it sits beside the DB
/// and never collides with it.
#[cfg(any(feature = "redb-backend", feature = "native-backend"))]
fn sidecar_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".hnsw");
    PathBuf::from(name)
}

/// The BM25 keyword sidecar path for a database file (`graph.drsg` →
/// `graph.drsg.bm25`), beside the DB and the `.hnsw` sidecar.
#[cfg(any(feature = "redb-backend", feature = "native-backend"))]
fn keyword_sidecar_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(".bm25");
    PathBuf::from(name)
}

impl Drop for Database {
    /// Persist the vector indexes to the sidecar so the next open can skip the
    /// rebuild-from-KV. Best-effort: the registry is kept coherent with each
    /// committed write, so stamping it with the current commit sequence is
    /// valid; any failure just means the next open rebuilds (still correct).
    fn drop(&mut self) {
        let seq = match self.engine.with_read(|txn| graph::read_commit_seq(txn)) {
            Ok(seq) => seq,
            Err(_) => return, // no meta ⇒ nothing coherent to stamp
        };
        if let Some(path) = self.sidecar.clone()
            && let Err(e) = self.indexes().save_sidecar(&path, seq)
        {
            tracing::warn!(error = %e, path = %path.display(), "failed to write HNSW sidecar");
        }
        if let Some(path) = self.keyword_sidecar.clone()
            && let Err(e) = self.keywords().save_sidecar(&path, seq)
        {
            tracing::warn!(error = %e, path = %path.display(), "failed to write BM25 sidecar");
        }
    }
}

impl Database {
    /// Opens (creating if needed) an on-disk database at `path`, using whichever
    /// storage backend the crate features select (`native-backend` takes
    /// precedence over `redb-backend`). Requires a backend feature; a
    /// no-backend build has the in-memory engine only.
    #[cfg(any(feature = "redb-backend", feature = "native-backend"))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        #[cfg(feature = "native-backend")]
        let engine = Engine::Native(Box::new(NativeEngine::open(path)?));
        #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
        let engine = Engine::Redb(RedbEngine::open(path)?);
        let db = Self::init(
            engine,
            Some(sidecar_path(path)),
            Some(keyword_sidecar_path(path)),
        )?;
        tracing::info!(path = %path.display(), "opened database");
        Ok(db)
    }

    /// A fresh, empty in-memory database (tests, scratch work).
    pub fn in_memory() -> Result<Self> {
        Self::init(Engine::Memory(Box::default()), None, None)
    }

    fn init(
        engine: Engine,
        sidecar: Option<PathBuf>,
        keyword_sidecar: Option<PathBuf>,
    ) -> Result<Self> {
        // The commit sequence of the data as last persisted — read BEFORE
        // `graph::init`, since that runs a write transaction and every write
        // bumps the sequence (arch/02 §3). This is the value the sidecar was
        // stamped with on the previous drop. A brand-new database has no meta
        // yet (read errors) and no sidecar to match anyway → `None`.
        let prior_seq = engine.with_read(|txn| graph::read_commit_seq(txn)).ok();
        engine.with_write(|txn| graph::init(txn))?;
        // Vector indexes (arch/01 §5): load the HNSW sidecar when it is fresh
        // (its stamped commit sequence equals the data's), else rebuild from
        // the KV — the KV is always the source of truth, the sidecar only a
        // cache.
        let registry = match sidecar.as_deref().zip(prior_seq).and_then(|(p, seq)| {
            let reg = VectorRegistry::load_sidecar(p, seq);
            if reg.is_some() {
                tracing::info!(path = %p.display(), "loaded HNSW sidecar");
            }
            reg
        }) {
            Some(reg) => reg,
            None => {
                let mut reg = VectorRegistry::new();
                engine.with_read(|txn| reg.rebuild_from(txn))?;
                reg
            }
        };
        // Keyword indexes (ROADMAP §2): same fresh-sidecar-else-rebuild dance.
        let keywords = match keyword_sidecar
            .as_deref()
            .zip(prior_seq)
            .and_then(|(p, seq)| {
                let reg = KeywordRegistry::load_sidecar(p, seq);
                if reg.is_some() {
                    tracing::info!(path = %p.display(), "loaded BM25 sidecar");
                }
                reg
            }) {
            Some(reg) => reg,
            None => {
                let mut reg = KeywordRegistry::new();
                engine.with_read(|txn| reg.rebuild_from(txn))?;
                reg
            }
        };
        Ok(Self {
            engine,
            indexes: RwLock::new(registry),
            keywords: RwLock::new(keywords),
            cache: GraphCache::new(CACHE_BYTES),
            sidecar,
            keyword_sidecar,
        })
    }

    /// The current commit sequence (arch/02 §3) — the web UI's change token.
    /// Reads it from a fresh snapshot.
    pub fn commit_seq(&self) -> Result<u64> {
        self.engine.with_read(|txn| graph::read_commit_seq(txn))
    }

    /// The retained history window as commit sequences: `(oldest queryable,
    /// latest)` — the inclusive range [`AsOf::Seq`] can address. Native-only
    /// (time-travel needs the LSM engine's versioning).
    #[cfg(feature = "native-backend")]
    pub fn history(&self) -> Result<(u64, u64)> {
        let (floor, latest) = self.engine.snapshot_window()?;
        let oldest = self
            .engine
            .with_read_at(floor, |txn| graph::read_commit_seq(txn))?;
        let newest = self
            .engine
            .with_read_at(latest, |txn| graph::read_commit_seq(txn))?;
        Ok((oldest, newest))
    }

    /// Bound how far back time-travel can reach: keep the last `keep_commits`
    /// commits queryable (`None` ⇒ unbounded, the default). Takes effect at the
    /// next compaction and never resurrects versions an earlier compaction
    /// already reclaimed. A no-op on a memory database (no history to bound).
    #[cfg(feature = "native-backend")]
    pub fn set_retention(&self, keep_commits: Option<u64>) {
        if let Engine::Native(e) = &self.engine {
            e.set_retention(keep_commits);
        }
    }

    /// Resolve a time-travel address ([`AsOf`]) to the storage snapshot to pin.
    /// Both `Seq` and `Time` use "at or before" semantics; a value beyond the
    /// latest commit clamps to it, one older than retained history errors.
    ///
    /// The graph commit sequence and commit time are both monotonic
    /// non-decreasing in the storage snapshot, so the target snapshot is the
    /// binary-search boundary over the retained window — O(log history) small
    /// meta reads, no per-commit index to maintain.
    #[cfg(feature = "native-backend")]
    fn resolve_as_of(&self, at: AsOf) -> Result<u64> {
        let (floor, latest) = self.engine.snapshot_window()?;
        match at {
            AsOf::Seq(target) => {
                self.search_at_or_before(floor, latest, target, "commit sequence", |snap| {
                    self.engine
                        .with_read_at(snap, |txn| graph::read_commit_seq(txn))
                })
            }
            AsOf::Time(target) => self.search_at_or_before(
                floor,
                latest,
                target,
                "timestamp",
                // A pre-time-index snapshot (no stamp) counts as "infinitely
                // old" so it never shadows a later, stamped commit.
                |snap| {
                    self.engine.with_read_at(snap, |txn| {
                        Ok(graph::read_commit_time(txn)?.unwrap_or(i64::MIN))
                    })
                },
            ),
        }
    }

    /// Largest snapshot in `[floor, latest]` whose `probe` value is `<= target`
    /// (the value is monotonic non-decreasing in the snapshot). Errors if even
    /// `floor` is already past `target` — the point predates retained history.
    #[cfg(feature = "native-backend")]
    fn search_at_or_before<V: Ord + Copy>(
        &self,
        floor: u64,
        latest: u64,
        target: V,
        kind: &str,
        probe: impl Fn(u64) -> Result<V>,
    ) -> Result<u64> {
        if probe(floor)? > target {
            return Err(Error::InvalidArgument(format!(
                "requested {kind} is older than the retained history \
                 (raise retention or pick a more recent point)"
            )));
        }
        let (mut lo, mut hi) = (floor, latest);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2); // upper mid ⇒ converge on the max
            if probe(mid)? <= target {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Ok(lo)
    }

    fn indexes(&self) -> std::sync::RwLockReadGuard<'_, VectorRegistry> {
        self.indexes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn indexes_mut(&self) -> std::sync::RwLockWriteGuard<'_, VectorRegistry> {
        self.indexes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn keywords(&self) -> std::sync::RwLockReadGuard<'_, KeywordRegistry> {
        self.keywords
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn keywords_mut(&self) -> std::sync::RwLockWriteGuard<'_, KeywordRegistry> {
        self.keywords
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Looks up an existing plane by name. The `"startup"` plane always exists.
    pub fn plane(&self, name: &str) -> Result<PlaneHandle<'_>> {
        let id = self
            .engine
            .with_read(|txn| graph::plane_id_by_name(txn, name))?
            .ok_or_else(|| Error::NotFound(format!("plane '{name}'")))?;
        Ok(PlaneHandle {
            db: self,
            id,
            #[cfg(feature = "native-backend")]
            as_of: None,
        })
    }

    /// Creates a new, empty plane (arch/09 §3). Errors with `PlaneExists`
    /// if the name is taken.
    pub fn create_plane(&self, name: &str, props: Properties) -> Result<PlaneHandle<'_>> {
        let id = self
            .engine
            .with_write(|txn| graph::create_plane(txn, name, &props))?;
        tracing::info!(name, id = id.0, "created plane");
        Ok(PlaneHandle {
            db: self,
            id,
            #[cfg(feature = "native-backend")]
            as_of: None,
        })
    }

    /// Deletes a plane and everything on it (arch/09 §3). Idempotent for an
    /// already-absent plane id. Errors with `InvalidArgument` for
    /// `PlaneId::STARTUP`, which always exists.
    pub fn drop_plane(&self, id: PlaneId) -> Result<()> {
        self.engine.with_write(|txn| graph::drop_plane(txn, id))?;
        tracing::info!(id = id.0, "dropped plane");
        Ok(())
    }

    /// Every plane as `(id, name)`, ascending by id (arch/04 §1).
    pub fn planes(&self) -> Result<Vec<(PlaneId, String)>> {
        self.engine.with_read(|txn| graph::list_planes(txn))
    }

    /// The soft-schema catalog rolled up across every plane (arch/03 §5).
    pub fn catalog(&self) -> Result<CatalogSnapshot> {
        self.engine.with_read(|txn| {
            let mut rollup = CatalogSnapshot::default();
            for (plane, _name) in graph::list_planes(txn)? {
                rollup.merge(&catalog::compute(txn, plane)?);
            }
            Ok(rollup)
        })
    }
}

/// Scope handle for one plane — all data access goes through one of these
/// (arch/09 §4). Copy-cheap, borrows the database.
#[derive(Clone, Copy)]
pub struct PlaneHandle<'db> {
    db: &'db Database,
    id: PlaneId,
    /// When set, every read on this handle is pinned to this past storage
    /// snapshot (time-travel / AS OF, ROADMAP §4) instead of the latest commit.
    /// Reads only — writes always target the current state. `None` ⇒ live.
    /// Gated to the native backend, the only one that can be time-travelled.
    #[cfg(feature = "native-backend")]
    as_of: Option<u64>,
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

    /// Returns a handle whose reads observe the graph **as of** a past point
    /// (time-travel / AS OF, ROADMAP §4) — every query, traversal, algorithm,
    /// and lookup made through it sees the state at that commit. Read-only:
    /// writes on the returned handle still target the current state.
    ///
    /// Native backend only (hence gated to it); errors if the point is older
    /// than the retained history. A point beyond the latest commit clamps to
    /// the latest (i.e. "now").
    #[cfg(feature = "native-backend")]
    pub fn as_of(mut self, at: AsOf) -> Result<Self> {
        self.as_of = Some(self.db.resolve_as_of(at)?);
        Ok(self)
    }

    /// The snapshot this handle reads at, if it is time-travelling.
    #[cfg(feature = "native-backend")]
    pub fn as_of_snapshot(&self) -> Option<u64> {
        self.as_of
    }

    /// Run `f` over a read txn — pinned to this handle's `as_of` snapshot when
    /// time-travelling, else the latest committed snapshot. The single seam
    /// every read on this handle flows through, so AS OF applies uniformly.
    /// On non-native builds there is no `as_of`, so it is always the latest.
    fn with_read<T>(&self, f: impl FnOnce(&dyn ReadTransaction) -> Result<T>) -> Result<T> {
        #[cfg(feature = "native-backend")]
        if let Some(snapshot) = self.as_of {
            return self.db.engine.with_read_at(snapshot, f);
        }
        self.db.engine.with_read(f)
    }

    /// Like [`with_read`](Self::with_read) but hands `f` a full
    /// [`CachedReader`] (vector registry + the cross-query cache stamped with
    /// this snapshot's commit seq). The query/algorithm/hybrid terminals use it.
    fn with_reader<T>(&self, f: impl FnOnce(&CachedReader) -> Result<T>) -> Result<T> {
        let registry = self.db.indexes();
        let cache = &self.db.cache;
        // A time-travelling read drops the live vector index (built from the
        // latest commit, so it can't answer a past snapshot); its vector
        // searches then brute-force the pinned snapshot — correct, unindexed.
        #[cfg(feature = "native-backend")]
        let historical = self.as_of.is_some();
        #[cfg(not(feature = "native-backend"))]
        let historical = false;
        self.with_read(|txn| {
            // The snapshot's own commit seq stamps the cache — for a historical
            // read that is the past seq, so the exact-seq cache never serves
            // "latest" records to a time-travelling query.
            let seq = graph::read_commit_seq(txn)?;
            let reader = if historical {
                CachedReader::with_cache_no_index(txn, self.id, cache, seq)
            } else {
                CachedReader::with_cache(txn, self.id, &registry, cache, seq)
            };
            f(&reader)
        })
    }

    /// This plane's name.
    pub fn name(&self) -> Result<String> {
        self.read_plane().map(|(name, _)| name)
    }

    /// This plane's own property map (arch/09 §2) — provenance, description,
    /// etc. (Not the graph data; that's `node`/`query`.)
    pub fn properties(&self) -> Result<Properties> {
        self.read_plane().map(|(_, props)| props)
    }

    fn read_plane(&self) -> Result<(String, Properties)> {
        self.with_read(|txn| graph::read_plane(txn, self.id))?
            .ok_or_else(|| Error::NotFound(format!("plane {}", self.id.0)))
    }

    /// Replaces this plane's property map (arch/09 §3).
    pub fn set_properties(&self, props: Properties) -> Result<()> {
        self.db
            .engine
            .with_write(|txn| graph::set_plane_properties(txn, self.id, &props))
    }

    /// Renames this plane (arch/09 §3); the handle's id is unchanged, so it
    /// stays valid. Errors if the name is taken or this is the startup plane.
    pub fn rename(&self, new_name: &str) -> Result<()> {
        self.db
            .engine
            .with_write(|txn| graph::rename_plane(txn, self.id, new_name))
    }

    /// Fetches one node with decoded labels and properties; `None` if the id
    /// does not exist in this plane.
    pub fn node(&self, id: NodeId) -> Result<Option<NodeRecord>> {
        self.with_read(|txn| graph::get_node(txn, self.id, id))
    }

    /// Fetches the node bound to a caller-supplied external key (arch/01 §2);
    /// `None` if no node in this plane carries that key.
    pub fn node_by_key(&self, external_key: &str) -> Result<Option<NodeRecord>> {
        self.with_read(|txn| graph::get_node_by_external_key(txn, self.id, external_key))
    }

    /// Fetches one edge with its resolved type name and properties; `None`
    /// if the id does not exist in this plane.
    pub fn edge(&self, id: EdgeId) -> Result<Option<EdgeRecord>> {
        self.with_read(|txn| graph::get_edge(txn, self.id, id))
    }

    /// 1-hop expansion; `ty = None` means any edge type.
    pub fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<&str>) -> Result<Vec<Neighbor>> {
        self.with_read(|txn| graph::neighbors(txn, self.id, id, dir, ty))
    }

    /// This plane's soft-schema catalog (arch/03 §5) — the descriptive shape
    /// (labels, property types/descriptions, edge-type connectivity, counts)
    /// the MCP layer serves as "schema". Computed by scanning the plane.
    pub fn catalog(&self) -> Result<CatalogSnapshot> {
        self.with_read(|txn| catalog::compute(txn, self.id))
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

    /// A query from an already-built (e.g. deserialized) [`LogicalPlan`] —
    /// the entry point for running a plan received over the wire or from the
    /// CLI's `query` command. Terminals work as usual.
    pub fn query_from_plan(&self, plan: LogicalPlan) -> QueryBuilder<'db> {
        QueryBuilder { plane: *self, plan }
    }

    /// Starts a graph-algorithm run in this plane (ROADMAP §1). Whole-plane by
    /// default; scope to one label with [`AlgoBuilder::label`]. Each terminal
    /// runs over a single read snapshot and returns a transient, read-only
    /// result — no graph mutation:
    ///
    /// ```ignore
    /// let ranks = plane.algo().pagerank(Default::default())?;   // [(id, score)]
    /// let (comp, n) = plane.algo().label("Doc").connected_components()?;
    /// let path = plane.algo().shortest_path(a, b, &Default::default())?;
    /// ```
    pub fn algo(&self) -> AlgoBuilder<'db> {
        AlgoBuilder {
            plane: *self,
            label: None,
        }
    }

    /// Starts a hybrid retrieval query (ROADMAP §2): fuse vector similarity,
    /// BM25 keyword, and graph-proximity channels into one ranking. Add the
    /// channels you want, then `run`:
    ///
    /// ```ignore
    /// let hits = plane.hybrid()
    ///     .label("Doc")
    ///     .vector("emb", query_vec, Metric::Cosine)   // caller pre-embeds the text
    ///     .keyword("body", "graph databases")
    ///     .graph(2, 0.5)                               // proximity boost, 2 hops
    ///     .k(10)
    ///     .run()?;
    /// ```
    pub fn hybrid(&self) -> HybridBuilder<'db> {
        HybridBuilder {
            plane: *self,
            label: None,
            vector: None,
            keyword: None,
            graph: None,
            weights: HybridWeights::default(),
            candidates: 100,
            k: 10,
        }
    }

    /// Declares (and builds) a vector index on `(label, property)` with
    /// `metric` (arch/01 §5). Idempotent; errors if an index already exists
    /// on the same pair with a different metric. Existing matching nodes are
    /// indexed immediately, and later writes keep it coherent.
    pub fn ensure_vector_index(&self, label: &str, property: &str, metric: Metric) -> Result<()> {
        let plane = self.id;
        self.db.engine.with_write(|txn| {
            graph::declare_vector_index(txn, plane, label, property, metric).map(|_| ())
        })?;
        // Build the in-memory index from committed data.
        self.db.engine.with_read(|txn| {
            self.db
                .indexes_mut()
                .build_entry(txn, plane, label, property, metric)
        })
    }

    /// Declares (and builds) a BM25 keyword index on `(label, property)` with
    /// `language` (ROADMAP §2). Idempotent; errors if one already exists on the
    /// same pair with a different language. Existing string values are indexed
    /// immediately, and later writes keep it coherent.
    pub fn ensure_keyword_index(
        &self,
        label: &str,
        property: &str,
        language: Language,
    ) -> Result<()> {
        let plane = self.id;
        self.db.engine.with_write(|txn| {
            graph::declare_keyword_index(txn, plane, label, property, language).map(|_| ())
        })?;
        self.db.engine.with_read(|txn| {
            self.db
                .keywords_mut()
                .build_entry(txn, plane, label, property, language)
        })
    }

    /// BM25 keyword search over a declared index (ROADMAP §2), most-relevant
    /// first. Empty if no keyword index is declared on `(label, property)`.
    pub fn keyword_search(
        &self,
        label: &str,
        property: &str,
        query: &str,
        k: usize,
    ) -> Vec<(NodeId, f32)> {
        self.db
            .keywords()
            .search(self.id, label, property, query, k)
            .unwrap_or_default()
    }

    /// The vector indexes declared on this plane, as `(label, property,
    /// metric)` — what a hybrid/vector search can accelerate against.
    pub fn vector_indexes(&self) -> Vec<(String, String, Metric)> {
        self.db.indexes().declared(self.id)
    }

    /// The keyword indexes declared on this plane, as `(label, property,
    /// language)` — the BM25 channel can only search these.
    pub fn keyword_indexes(&self) -> Vec<(String, String, Language)> {
        self.db.keywords().declared(self.id)
    }

    /// Starts a write transaction scoped to this plane. Blocks while another
    /// write transaction is open (single writer, arch/01 §6).
    pub fn write(&self) -> Result<WriteTxn<'db>> {
        let inner = match &self.db.engine {
            Engine::Memory(e) => TxnInner::Memory(Box::new(e.begin_write()?)),
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            Engine::Redb(e) => TxnInner::Redb(Box::new(e.begin_write()?)),
            #[cfg(feature = "native-backend")]
            Engine::Native(e) => TxnInner::Native(Box::new(e.begin_write()?)),
        };
        // Snapshot this plane's declared indexes so mutations can mirror into
        // them at commit without re-locking per operation.
        let decls = self.db.indexes().declared(self.id);
        let kw_decls = self.db.keywords().declared(self.id);
        Ok(WriteTxn {
            db: self.db,
            plane: self.id,
            inner,
            ids: IdAllocator::new(),
            decls,
            events: Vec::new(),
            kw_decls,
            kw_events: Vec::new(),
        })
    }
}

// Both variants boxed: whichever backend txn is larger, the enum stays two
// pointers wide, and a write transaction allocates once per begin_write.
enum TxnInner<'db> {
    Memory(Box<MemoryWriteTxn<'db>>),
    #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
    Redb(Box<RedbWriteTxn>),
    #[cfg(feature = "native-backend")]
    Native(Box<NativeWriteTxn<'db>>),
}

/// A vector-index coherence event, buffered during a write and applied to the
/// registry at commit (never on abort — mirroring the KV's own semantics).
enum IndexEvent {
    Upsert {
        label: String,
        property: String,
        node: NodeId,
        vector: Vec<f32>,
    },
    Remove {
        label: String,
        property: String,
        node: NodeId,
    },
    RemoveNode(NodeId),
}

/// A keyword-index coherence event (ROADMAP §2) — the BM25 counterpart to
/// [`IndexEvent`], carrying the property *text* instead of a vector.
enum KwEvent {
    Upsert {
        label: String,
        property: String,
        node: NodeId,
        text: String,
    },
    Remove {
        label: String,
        property: String,
        node: NodeId,
    },
    RemoveNode(NodeId),
}

/// A plane-scoped write transaction. Dropped without [`commit`](Self::commit)
/// ⇒ all changes discarded.
pub struct WriteTxn<'db> {
    db: &'db Database,
    plane: PlaneId,
    inner: TxnInner<'db>,
    /// Batched node/edge id allocator (arch/01 §2 TODO) — see
    /// `graph::IdAllocator` for the abort/commit-safety argument.
    ids: IdAllocator,
    /// This plane's declared vector indexes, snapshotted at `write()` (see
    /// `record_node_events`).
    decls: Vec<(String, String, Metric)>,
    /// Buffered vector-index coherence events, applied at commit.
    events: Vec<IndexEvent>,
    /// This plane's declared keyword indexes, snapshotted at `write()`.
    kw_decls: Vec<(String, String, Language)>,
    /// Buffered keyword-index coherence events, applied at commit.
    kw_events: Vec<KwEvent>,
}

impl WriteTxn<'_> {
    fn txn(&mut self) -> &mut dyn WriteTransaction {
        match &mut self.inner {
            TxnInner::Memory(t) => &mut **t,
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            TxnInner::Redb(t) => &mut **t,
            #[cfg(feature = "native-backend")]
            TxnInner::Native(t) => &mut **t,
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
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            TxnInner::Redb(t) => &mut **t,
            #[cfg(feature = "native-backend")]
            TxnInner::Native(t) => &mut **t,
        };
        (txn, ids)
    }

    /// Buffers index events for a node given its labels and property map: an
    /// `Upsert` where a declared index's property is present as a vector, a
    /// `Remove` where it is absent or non-vector. No-op unless some index is
    /// declared for one of the node's labels.
    fn record_node_events(&mut self, node: NodeId, labels: &[&str], props: &Properties) {
        for (label, property, _metric) in &self.decls {
            if !labels.iter().any(|l| l == label) {
                continue;
            }
            match props.get(property).map(|p| &p.value) {
                Some(PropValue::Vector(v)) => self.events.push(IndexEvent::Upsert {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                    vector: v.clone(),
                }),
                _ => self.events.push(IndexEvent::Remove {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                }),
            }
        }
    }

    pub fn create_node(&mut self, labels: &[&str], props: Properties) -> Result<NodeId> {
        let plane = self.plane;
        let (txn, ids) = self.txn_and_ids();
        let id = ids.next_node_id(txn)?;
        graph::insert_node(txn, plane, id, None, labels, &props)?;
        self.record_node_events(id, labels, &props);
        self.record_kw_node_events(id, labels, &props);
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
        self.record_node_events(id, labels, &props);
        self.record_kw_node_events(id, labels, &props);
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

    /// Bulk-loads `nodes` then `edges` into this plane in one pass — the fast
    /// path for initial ingest (arch/01 §2). Much faster than looping
    /// `create_node`/`create_edge`: one contiguous id reservation per kind,
    /// in-memory interning, and sorted batched writes with each table opened
    /// once.
    ///
    /// Edge endpoints are named by external key and must resolve within this
    /// batch or already exist in the plane. External keys are assumed fresh:
    /// in-batch duplicates are rejected, but uniqueness is not checked against
    /// the pre-existing KV — use `create_node_with_key` when you need that.
    pub fn bulk_load(
        &mut self,
        nodes: Vec<BulkNode<'_>>,
        edges: Vec<BulkEdge<'_>>,
    ) -> Result<BulkStats> {
        let plane = self.plane;
        let stats = graph::bulk_load(self.txn(), plane, &nodes, &edges)?;

        // Mirror bulk-loaded vectors into any declared in-memory index. Almost
        // always a no-op: bulk load precedes index declaration (which rebuilds
        // from the KV), so `decls` is empty here.
        if !self.decls.is_empty() {
            let mut new_events = Vec::new();
            for (i, node) in nodes.iter().enumerate() {
                let node_id = NodeId(stats.node_start + i as u64);
                for (label, property, _metric) in &self.decls {
                    if node.labels.contains(&label.as_str())
                        && let Some(PropValue::Vector(v)) =
                            node.props.get(property).map(|p| &p.value)
                    {
                        new_events.push(IndexEvent::Upsert {
                            label: label.clone(),
                            property: property.clone(),
                            node: node_id,
                            vector: v.clone(),
                        });
                    }
                }
            }
            self.events.extend(new_events);
        }
        // Same for bulk-loaded text into any declared keyword index.
        if !self.kw_decls.is_empty() {
            let mut new_events = Vec::new();
            for (i, node) in nodes.iter().enumerate() {
                let node_id = NodeId(stats.node_start + i as u64);
                for (label, property, _lang) in &self.kw_decls {
                    if node.labels.contains(&label.as_str())
                        && let Some(PropValue::Str(s)) = node.props.get(property).map(|p| &p.value)
                    {
                        new_events.push(KwEvent::Upsert {
                            label: label.clone(),
                            property: property.clone(),
                            node: node_id,
                            text: s.clone(),
                        });
                    }
                }
            }
            self.kw_events.extend(new_events);
        }
        Ok(stats)
    }

    /// Bulk-writes edges whose endpoints are already resolved node ids
    /// (arch/01 §2) — the id-based companion to [`bulk_load`](Self::bulk_load),
    /// used by `drsg import` which resolves and validates endpoints itself.
    /// **Trusted**: the caller must guarantee both endpoints exist; a bad id
    /// writes dangling adjacency.
    pub fn bulk_load_edges(&mut self, edges: Vec<BulkEdgeById<'_>>) -> Result<u64> {
        let plane = self.plane;
        graph::bulk_load_edges(self.txn(), plane, &edges)
    }

    /// Deletes a node, cascading to every incident edge in both directions
    /// (arch/01 §2). Idempotent: deleting an absent node is `Ok(())`.
    pub fn delete_node(&mut self, id: NodeId) -> Result<()> {
        let plane = self.plane;
        graph::delete_node(self.txn(), plane, id)?;
        self.events.push(IndexEvent::RemoveNode(id));
        self.kw_events.push(KwEvent::RemoveNode(id));
        Ok(())
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
        let value = prop.value.clone();
        graph::set_node_prop(self.txn(), plane, id, key, prop)?;
        self.record_prop_event(id, key, Some(value.clone()))?;
        self.record_kw_prop_event(id, key, Some(value))
    }

    /// Removes one property from an existing node; removing an absent key
    /// is not an error (soft schema — arch/01 §4).
    pub fn remove_prop(&mut self, id: NodeId, key: &str) -> Result<()> {
        let plane = self.plane;
        graph::remove_node_prop(self.txn(), plane, id, key)?;
        self.record_prop_event(id, key, None)?;
        self.record_kw_prop_event(id, key, None)
    }

    /// Replaces a node's entire label set. Errors `NotFound` if the node is
    /// absent. Adjusts vector-index membership for any declared index whose
    /// label the node gained or lost.
    pub fn set_labels(&mut self, id: NodeId, labels: &[&str]) -> Result<()> {
        let plane = self.plane;
        // Snapshot the old labels + the node's props before the write so index
        // membership can be diffed (a node's spot in a `(label, property)` index
        // depends on whether it still carries that label).
        let node = graph::get_node(self.txn(), plane, id)?
            .ok_or_else(|| Error::NotFound(format!("node {}", id.0)))?;
        graph::set_node_labels(self.txn(), plane, id, labels)?;
        if !self.decls.is_empty() {
            self.record_labels_event(id, &node.labels, labels, &node.properties);
        }
        if !self.kw_decls.is_empty() {
            self.record_kw_labels_event(id, &node.labels, labels, &node.properties);
        }
        Ok(())
    }

    /// Buffers index events for a label-set change: for each declared index the
    /// node newly matches (label gained + carries the vector) an `Upsert`, and
    /// for each it no longer matches (label lost) a `Remove`.
    fn record_labels_event(
        &mut self,
        node: NodeId,
        old: &[String],
        new: &[&str],
        props: &Properties,
    ) {
        let mut events = Vec::new();
        for (label, property, _metric) in &self.decls {
            let had = old.iter().any(|l| l == label);
            let has = new.iter().any(|l| l == label);
            if had == has {
                continue;
            }
            match props.get(property).map(|p| &p.value) {
                Some(PropValue::Vector(v)) if has => events.push(IndexEvent::Upsert {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                    vector: v.clone(),
                }),
                Some(PropValue::Vector(_)) => events.push(IndexEvent::Remove {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                }),
                _ => {} // no vector on this property ⇒ nothing indexed either way
            }
        }
        self.events.extend(events);
    }

    /// Buffers index events for a single-property change on `node`: an
    /// `Upsert` if the new value is a vector on an indexed `(label, key)`, a
    /// `Remove` otherwise. Cheap no-op unless some declared index names `key`.
    fn record_prop_event(
        &mut self,
        node: NodeId,
        key: &str,
        new_value: Option<PropValue>,
    ) -> Result<()> {
        if !self.decls.iter().any(|(_, prop, _)| prop == key) {
            return Ok(());
        }
        let plane = self.plane;
        let labels = match graph::get_node(self.txn(), plane, node)? {
            Some(n) => n.labels,
            None => return Ok(()), // node gone; nothing to mirror
        };
        let mut new_events = Vec::new();
        for (label, property, _metric) in &self.decls {
            if property == key && labels.iter().any(|l| l == label) {
                new_events.push(match &new_value {
                    Some(PropValue::Vector(v)) => IndexEvent::Upsert {
                        label: label.clone(),
                        property: property.clone(),
                        node,
                        vector: v.clone(),
                    },
                    _ => IndexEvent::Remove {
                        label: label.clone(),
                        property: property.clone(),
                        node,
                    },
                });
            }
        }
        self.events.extend(new_events);
        Ok(())
    }

    // ---- keyword-index mirrors (ROADMAP §2) ---------------------------------
    // The BM25 counterparts to the vector `record_*` helpers above: same
    // matching logic, but keyed off `kw_decls` and testing for a string value.

    /// Buffer keyword events for a new/replaced node: `Upsert` where a declared
    /// keyword index's property is present as a string, `Remove` otherwise.
    fn record_kw_node_events(&mut self, node: NodeId, labels: &[&str], props: &Properties) {
        for (label, property, _lang) in &self.kw_decls {
            if !labels.iter().any(|l| l == label) {
                continue;
            }
            match props.get(property).map(|p| &p.value) {
                Some(PropValue::Str(s)) => self.kw_events.push(KwEvent::Upsert {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                    text: s.clone(),
                }),
                _ => self.kw_events.push(KwEvent::Remove {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                }),
            }
        }
    }

    /// Buffer keyword events for a label-set change (gained/lost a declared
    /// index's label).
    fn record_kw_labels_event(
        &mut self,
        node: NodeId,
        old: &[String],
        new: &[&str],
        props: &Properties,
    ) {
        let mut events = Vec::new();
        for (label, property, _lang) in &self.kw_decls {
            let had = old.iter().any(|l| l == label);
            let has = new.iter().any(|l| l == label);
            if had == has {
                continue;
            }
            match props.get(property).map(|p| &p.value) {
                Some(PropValue::Str(s)) if has => events.push(KwEvent::Upsert {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                    text: s.clone(),
                }),
                Some(PropValue::Str(_)) => events.push(KwEvent::Remove {
                    label: label.clone(),
                    property: property.clone(),
                    node,
                }),
                _ => {}
            }
        }
        self.kw_events.extend(events);
    }

    /// Buffer keyword events for a single-property change on `node`.
    fn record_kw_prop_event(
        &mut self,
        node: NodeId,
        key: &str,
        new_value: Option<PropValue>,
    ) -> Result<()> {
        if !self.kw_decls.iter().any(|(_, prop, _)| prop == key) {
            return Ok(());
        }
        let plane = self.plane;
        let labels = match graph::get_node(self.txn(), plane, node)? {
            Some(n) => n.labels,
            None => return Ok(()),
        };
        let mut new_events = Vec::new();
        for (label, property, _lang) in &self.kw_decls {
            if property == key && labels.iter().any(|l| l == label) {
                new_events.push(match &new_value {
                    Some(PropValue::Str(s)) => KwEvent::Upsert {
                        label: label.clone(),
                        property: property.clone(),
                        node,
                        text: s.clone(),
                    },
                    _ => KwEvent::Remove {
                        label: label.clone(),
                        property: property.clone(),
                        node,
                    },
                });
            }
        }
        self.kw_events.extend(new_events);
        Ok(())
    }

    /// Sets (inserts or overwrites) one property on an existing edge.
    /// Errors with `NotFound` if the edge does not exist.
    pub fn set_edge_prop(&mut self, id: EdgeId, key: &str, prop: PropDesc) -> Result<()> {
        let plane = self.plane;
        graph::set_edge_prop(self.txn(), plane, id, key, prop)
    }

    /// Changes an existing edge's type. Errors `NotFound` if the edge is
    /// absent. (Edge indexes key on node labels, so a retype needs no index
    /// bookkeeping.)
    pub fn set_edge_type(&mut self, id: EdgeId, ty: &str) -> Result<()> {
        let plane = self.plane;
        graph::set_edge_type(self.txn(), plane, id, ty)
    }

    /// Removes one property from an existing edge; removing an absent key
    /// is not an error.
    pub fn remove_edge_prop(&mut self, id: EdgeId, key: &str) -> Result<()> {
        let plane = self.plane;
        graph::remove_edge_prop(self.txn(), plane, id, key)
    }

    pub fn commit(mut self) -> Result<()> {
        // Bump the commit sequence inside the txn (arch/02 §3) so it commits
        // atomically with the data — advances the cache's version stamp — and
        // stamp the wall-clock time for time-addressed time-travel (ROADMAP §4).
        graph::bump_commit_seq(self.txn())?;
        graph::write_commit_time(self.txn(), now_millis())?;
        let WriteTxn {
            db,
            plane,
            inner,
            events,
            kw_events,
            ..
        } = self;
        let index_events = events.len();
        // Commit the KV first; only then mirror into the in-memory indexes.
        // If applying events somehow failed, the KV is still the source of
        // truth and rebuild-from-KV on next open restores coherence.
        match inner {
            TxnInner::Memory(t) => (*t).commit()?,
            #[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
            TxnInner::Redb(t) => (*t).commit()?,
            #[cfg(feature = "native-backend")]
            TxnInner::Native(t) => (*t).commit()?,
        }
        if !events.is_empty() {
            let mut registry = db.indexes_mut();
            for event in events {
                apply_index_event(&mut registry, plane, event)?;
            }
        }
        if !kw_events.is_empty() {
            let mut registry = db.keywords_mut();
            for event in kw_events {
                apply_kw_event(&mut registry, plane, event);
            }
        }
        tracing::debug!(plane = plane.0, index_events, "write txn committed");
        Ok(())
    }
}

fn apply_kw_event(registry: &mut KeywordRegistry, plane: PlaneId, event: KwEvent) {
    match event {
        KwEvent::Upsert {
            label,
            property,
            node,
            text,
        } => registry.upsert(plane, &label, &property, node, &text),
        KwEvent::Remove {
            label,
            property,
            node,
        } => registry.remove_one(plane, &label, &property, node),
        KwEvent::RemoveNode(node) => registry.remove_node(node),
    }
}

fn apply_index_event(
    registry: &mut VectorRegistry,
    plane: PlaneId,
    event: IndexEvent,
) -> Result<()> {
    match event {
        IndexEvent::Upsert {
            label,
            property,
            node,
            vector,
        } => registry.upsert(plane, &label, &property, node, &vector),
        IndexEvent::Remove {
            label,
            property,
            node,
        } => registry.remove_one(plane, &label, &property, node),
        IndexEvent::RemoveNode(node) => registry.remove_node(node),
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

    /// Global similarity search: the `k` nodes closest to `query` by their
    /// `property` vector, seeded with a similarity score (arch/03 §4.1).
    /// `label = None` searches the whole plane.
    pub fn vector_top_k(
        mut self,
        label: Option<&str>,
        property: &str,
        query: impl Into<Vec<f32>>,
        metric: Metric,
        k: u64,
    ) -> Self {
        self.plan.source = Source::VectorTopK {
            label: label.map(str::to_string),
            property: property.to_string(),
            query: query.into(),
            metric,
            k,
        };
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

    /// Graph-constrained vector search (arch/03 §4.3): rerank the current
    /// frontier by similarity of `property` to `query`, keeping the top `k`
    /// with scores — one plan, no client-side join.
    pub fn frontier_top_k(
        mut self,
        property: &str,
        query: impl Into<Vec<f32>>,
        metric: Metric,
        k: u64,
    ) -> Self {
        self.plan.push(Step::FrontierTopK {
            property: property.to_string(),
            query: query.into(),
            metric,
            k,
        });
        self
    }

    /// Similarity-guided beam traversal (arch/03 §4.4): walk toward `query`'s
    /// meaning, keeping the best `width` per step for `depth` steps.
    #[allow(clippy::too_many_arguments)]
    pub fn expand_beam(
        mut self,
        dir: Dir,
        edge_type: Option<&str>,
        property: &str,
        query: impl Into<Vec<f32>>,
        metric: Metric,
        width: u32,
        depth: u32,
    ) -> Self {
        self.plan.push(Step::ExpandBeam {
            dir,
            edge_type: edge_type.map(str::to_string),
            property: property.to_string(),
            query: query.into(),
            metric,
            width,
            depth,
        });
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

    /// Sort most-similar-first by the row score channel — the usual final
    /// step after a vector search.
    pub fn sort_by_score(self) -> Self {
        self.sort_desc(score())
    }

    // ---- inspection ------------------------------------------------------

    /// The plan built so far (serializable — arch/00 §2).
    pub fn plan(&self) -> &LogicalPlan {
        &self.plan
    }

    // ---- terminals (execute) ---------------------------------------------

    fn with_reader<T>(&self, f: impl FnOnce(&CachedReader) -> Result<T>) -> Result<T> {
        // Delegates to the plane handle's reader seam, so an AS OF handle runs
        // the whole query against its historical snapshot (arch/01 §5, arch/02).
        self.plane.with_reader(f)
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

    /// The connected **subgraph** the plan matched: every node and edge that
    /// appears on any matching row's path (source → … → current node), not just
    /// the final current nodes. So a traversal like `SeekKeys → Expand` returns
    /// the seed node, the edges walked, and their targets — a graph you can
    /// plot, rather than disconnected endpoints. A non-traversal query (scan +
    /// filter) yields just the matching nodes with no edges. Nodes are ordered
    /// by id, edges by id.
    pub fn subgraph(&self) -> Result<(Vec<NodeRecord>, Vec<EdgeRecord>)> {
        use std::collections::BTreeMap;
        self.with_reader(|reader| {
            let mut nodes: BTreeMap<u64, NodeRecord> = BTreeMap::new();
            let mut edges: BTreeMap<u64, EdgeRecord> = BTreeMap::new();
            let mut add_node = |id: NodeId, reader: &CachedReader| -> Result<()> {
                if let std::collections::btree_map::Entry::Vacant(e) = nodes.entry(id.0)
                    && let Some(n) = reader.node(id)?
                {
                    e.insert((*n).clone());
                }
                Ok(())
            };
            for r in exec::execute(&self.plan, reader)? {
                let row = r?;
                add_node(row.head, reader)?;
                // Each traversed edge carries its endpoints, so walking the
                // trail recovers every intermediate node *and* the source.
                for (edge_id, _) in row.path() {
                    if let Some(edge) = reader.edge(edge_id)? {
                        add_node(edge.src, reader)?;
                        add_node(edge.dst, reader)?;
                        edges.entry(edge_id.0).or_insert_with(|| (*edge).clone());
                    }
                }
            }
            Ok((nodes.into_values().collect(), edges.into_values().collect()))
        })
    }

    /// Like [`nodes`](Self::nodes) but pairs each with its similarity score
    /// (`None` for rows that never passed through a vector operator).
    pub fn scored_nodes(&self) -> Result<Vec<(NodeRecord, Option<f32>)>> {
        self.with_reader(|reader| {
            let mut out = Vec::new();
            for r in exec::execute(&self.plan, reader)? {
                let row = r?;
                if let Some(node) = reader.node(row.head)? {
                    out.push(((*node).clone(), row.score));
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
                let ctx = expr::EvalCtx {
                    node: node.as_deref(),
                    score: row.score,
                    hops: row.hops(),
                };
                out.push(exprs.iter().map(|e| expr::eval(e, &ctx)).collect());
            }
            Ok(out)
        })
    }
}

/// A graph-algorithm run scoped to one plane (ROADMAP §1). Built with
/// [`PlaneHandle::algo`]. Optionally narrowed to a single label; each terminal
/// materializes the graph from one read snapshot and returns a transient,
/// read-only result.
#[derive(Clone)]
pub struct AlgoBuilder<'db> {
    plane: PlaneHandle<'db>,
    label: Option<String>,
}

impl std::fmt::Debug for AlgoBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlgoBuilder")
            .field("plane", &self.plane.id())
            .field("label", &self.label)
            .finish()
    }
}

impl<'db> AlgoBuilder<'db> {
    /// Restrict the run to nodes carrying `label` (and the edges among them);
    /// otherwise the algorithm covers the whole plane.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Run `f` over a `CachedReader` bound to one read snapshot — the same
    /// read path the query executor uses (memoizes decoded records/adjacency,
    /// which the repeated neighbor/edge lookups here benefit from). Honours the
    /// plane handle's AS OF snapshot, so `plane.as_of(..).algo()` runs the
    /// algorithm over historical state.
    fn with_reader<T>(&self, f: impl FnOnce(&CachedReader) -> Result<T>) -> Result<T> {
        self.plane.with_reader(f)
    }

    /// PageRank importance scores, sorted most-important first.
    pub fn pagerank(&self, opts: PageRankOptions) -> Result<Vec<(NodeId, f64)>> {
        self.with_reader(|r| algo::pagerank(r, self.label.as_deref(), opts))
    }

    /// Weakly connected components: `(node, representative)` for every node
    /// (representative = smallest id in the component) plus the component count.
    pub fn connected_components(&self) -> Result<(Vec<(NodeId, NodeId)>, usize)> {
        self.with_reader(|r| algo::connected_components(r, self.label.as_deref()))
    }

    /// Weighted shortest path from `src` to `dst`; `None` if unreachable or an
    /// endpoint is outside the scope.
    pub fn shortest_path(
        &self,
        src: NodeId,
        dst: NodeId,
        opts: &ShortestPathOptions,
    ) -> Result<Option<algo::Path>> {
        self.with_reader(|r| algo::shortest_path(r, self.label.as_deref(), src, dst, opts))
    }

    /// Louvain community detection: `(node, representative)` for every node
    /// (representative = smallest id in the community) plus the community count.
    pub fn louvain(&self, opts: LouvainOptions) -> Result<(Vec<(NodeId, NodeId)>, usize)> {
        self.with_reader(|r| algo::louvain(r, self.label.as_deref(), opts))
    }
}

// ---- hybrid retrieval (ROADMAP §2) ---------------------------------------

struct VectorChannelCfg {
    property: String,
    query: Vec<f32>,
    metric: Metric,
}

struct KeywordChannelCfg {
    property: String,
    query: String,
}

struct GraphChannelCfg {
    hops: u32,
    decay: f32,
    seeds: usize,
}

/// Number of top hits per primary channel used to seed the graph-proximity
/// channel, when `.graph(..)` is enabled without an explicit seed count.
const DEFAULT_GRAPH_SEEDS: usize = 10;

/// Builder for a hybrid retrieval query (ROADMAP §2). Built with
/// [`PlaneHandle::hybrid`]; add channels, then [`run`](Self::run). All channels
/// are optional, but at least one should be set. The vector channel takes a
/// *pre-embedded* query vector (the core never calls an LLM); surfaces embed
/// the query text server-side first.
pub struct HybridBuilder<'db> {
    plane: PlaneHandle<'db>,
    label: Option<String>,
    vector: Option<VectorChannelCfg>,
    keyword: Option<KeywordChannelCfg>,
    graph: Option<GraphChannelCfg>,
    weights: HybridWeights,
    candidates: usize,
    k: usize,
}

impl<'db> HybridBuilder<'db> {
    /// Scope every channel to nodes carrying this label. Required when the
    /// keyword channel is used (its index is keyed on the label).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Add the vector channel over `property`, ranking by distance to the
    /// (already embedded) `query` vector under `metric`.
    pub fn vector(mut self, property: impl Into<String>, query: Vec<f32>, metric: Metric) -> Self {
        self.vector = Some(VectorChannelCfg {
            property: property.into(),
            query,
            metric,
        });
        self
    }

    /// Add the BM25 keyword channel over `property` for the text `query`.
    pub fn keyword(mut self, property: impl Into<String>, query: impl Into<String>) -> Self {
        self.keyword = Some(KeywordChannelCfg {
            property: property.into(),
            query: query.into(),
        });
        self
    }

    /// Add the graph-proximity channel: seed from the strongest vector/keyword
    /// hits, expand `hops` outward, decaying the boost by `decay` per hop.
    pub fn graph(mut self, hops: u32, decay: f32) -> Self {
        self.graph = Some(GraphChannelCfg {
            hops,
            decay,
            seeds: DEFAULT_GRAPH_SEEDS,
        });
        self
    }

    /// Override the per-channel fusion weights (defaults: vector 1, keyword 1,
    /// graph 0.5).
    pub fn weights(mut self, weights: HybridWeights) -> Self {
        self.weights = weights;
        self
    }

    /// Per-channel candidate pool size fetched before fusion (default 100).
    pub fn candidates(mut self, candidates: usize) -> Self {
        self.candidates = candidates.max(1);
        self
    }

    /// Number of fused results to return (default 10).
    pub fn k(mut self, k: usize) -> Self {
        self.k = k;
        self
    }

    /// Run the query: gather each enabled channel over one read snapshot and
    /// fuse them. Errors if the keyword channel is used without a label.
    pub fn run(&self) -> Result<Vec<HybridHit>> {
        let plane_id = self.plane.id;
        let label = self.label.as_deref();
        let registry = self.plane.db.indexes();
        let cache = &self.plane.db.cache;
        // `with_read` honours the plane handle's AS OF snapshot, so a hybrid
        // search can run over historical state too.
        self.plane.with_read(|txn| {
            let seq = graph::read_commit_seq(txn)?;
            let reader = CachedReader::with_cache(txn, plane_id, &registry, cache, seq);

            let vector_ch = match &self.vector {
                Some(v) => {
                    let hits = reader.vector_search(
                        label,
                        &v.property,
                        &v.query,
                        v.metric,
                        self.candidates,
                    )?;
                    Some(Channel {
                        hits: hits
                            .into_iter()
                            .map(|h| (NodeId(h.id), h.distance))
                            .collect(),
                        higher_better: false,
                    })
                }
                None => None,
            };

            let keyword_ch = match &self.keyword {
                Some(kw) => {
                    let label = label.ok_or_else(|| {
                        Error::InvalidArgument(
                            "hybrid keyword channel requires a label (.label(..))".into(),
                        )
                    })?;
                    let hits = self
                        .plane
                        .db
                        .keywords()
                        .search(plane_id, label, &kw.property, &kw.query, self.candidates)
                        .unwrap_or_default();
                    Some(Channel {
                        hits,
                        higher_better: true,
                    })
                }
                None => None,
            };

            let graph_ch = match &self.graph {
                Some(g) => {
                    let seeds = hybrid::top_seeds(&vector_ch, &keyword_ch, g.seeds);
                    let prox = hybrid::graph_proximity(&reader, &seeds, g.hops, g.decay)?;
                    Some(Channel {
                        hits: prox,
                        higher_better: true,
                    })
                }
                None => None,
            };

            Ok(hybrid::fuse(
                vector_ch,
                keyword_ch,
                graph_ch,
                self.weights,
                self.k,
            ))
        })
    }
}

/// Time-travel / AS OF (ROADMAP §4). Native-backend only — the LSM engine keeps
/// prior versions keyed by commit sequence; these tests pin past snapshots and
/// check reads see the historical state.
#[cfg(all(test, feature = "native-backend"))]
mod time_travel_tests {
    use super::*;
    use crate::types::{Dir, PropDesc, PropValue};

    fn status_prop(v: &str) -> Properties {
        let mut p = Properties::new();
        p.insert("status".into(), PropDesc::new(PropValue::Str(v.into())));
        p
    }

    fn status_of(n: &NodeRecord) -> String {
        match n.properties.get("status").map(|d| &d.value) {
            Some(PropValue::Str(s)) => s.clone(),
            other => panic!("expected a status string, got {other:?}"),
        }
    }

    fn open() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("db")).unwrap();
        (dir, db)
    }

    #[test]
    fn as_of_seq_reads_the_historical_property() {
        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();

        let id = {
            let mut w = plane.write().unwrap();
            let id = w
                .create_node_with_key("n1", &["Doc"], status_prop("draft"))
                .unwrap();
            w.commit().unwrap();
            id
        };
        let s1 = db.commit_seq().unwrap();

        {
            let mut w = plane.write().unwrap();
            w.set_prop(id, "status", PropDesc::new(PropValue::Str("final".into())))
                .unwrap();
            w.commit().unwrap();
        }
        let s2 = db.commit_seq().unwrap();
        assert!(s2 > s1, "second commit must advance the sequence");

        // Live read sees the newest value.
        assert_eq!(status_of(&plane.node(id).unwrap().unwrap()), "final");

        // Pinned to s1, the same node reads its old value.
        let past = plane.as_of(AsOf::Seq(s1)).unwrap();
        assert_eq!(status_of(&past.node(id).unwrap().unwrap()), "draft");

        // A future sequence clamps to "now" (at-or-before semantics).
        let future = plane.as_of(AsOf::Seq(s2 + 100)).unwrap();
        assert_eq!(status_of(&future.node(id).unwrap().unwrap()), "final");

        // The original handle is unchanged (as_of returns a new handle).
        assert_eq!(status_of(&plane.node(id).unwrap().unwrap()), "final");
    }

    #[test]
    fn as_of_hides_edges_added_later() {
        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();

        let (a, b) = {
            let mut w = plane.write().unwrap();
            let a = w
                .create_node_with_key("a", &["N"], Properties::new())
                .unwrap();
            let b = w
                .create_node_with_key("b", &["N"], Properties::new())
                .unwrap();
            w.commit().unwrap();
            (a, b)
        };
        let before_edge = db.commit_seq().unwrap();

        {
            let mut w = plane.write().unwrap();
            w.create_edge(a, b, "LINKS", Properties::new()).unwrap();
            w.commit().unwrap();
        }

        // Now: one out-neighbour. As of `before_edge`: none.
        assert_eq!(plane.neighbors(a, Dir::Out, None).unwrap().len(), 1);
        let past = plane.as_of(AsOf::Seq(before_edge)).unwrap();
        assert!(past.neighbors(a, Dir::Out, None).unwrap().is_empty());
    }

    #[test]
    fn history_reports_the_window_and_retention_bounds_it() {
        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();
        let s_first = db.commit_seq().unwrap();

        for i in 0..4 {
            let mut w = plane.write().unwrap();
            w.create_node_with_key(&format!("n{i}"), &["N"], Properties::new())
                .unwrap();
            w.commit().unwrap();
        }
        let (oldest, latest) = db.history().unwrap();
        assert_eq!(latest, db.commit_seq().unwrap());
        assert!(
            oldest <= s_first,
            "unbounded history reaches the first commit"
        );

        // Bounding retention refuses reads older than the window.
        db.set_retention(Some(1));
        assert!(
            plane.as_of(AsOf::Seq(s_first)).is_err(),
            "a commit below the retained floor must be rejected"
        );
        // A recent point still resolves.
        assert!(plane.as_of(AsOf::Seq(latest)).is_ok());
        db.set_retention(None); // restore unbounded
        assert!(plane.as_of(AsOf::Seq(s_first)).is_ok());
    }

    #[test]
    fn as_of_time_resolves_at_or_before() {
        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();
        let id = {
            let mut w = plane.write().unwrap();
            let id = w
                .create_node_with_key("n1", &["Doc"], status_prop("v1"))
                .unwrap();
            w.commit().unwrap();
            id
        };

        // Far future → newest state.
        let future = plane.as_of(AsOf::Time(i64::MAX)).unwrap();
        assert_eq!(status_of(&future.node(id).unwrap().unwrap()), "v1");

        // The epoch predates every real commit → resolves to the empty graph.
        let epoch = plane.as_of(AsOf::Time(0)).unwrap();
        assert!(epoch.node(id).unwrap().is_none());
    }

    #[test]
    fn as_of_vector_search_is_historical() {
        // A declared HNSW index reflects only the latest commit, so a
        // time-travelling vector search must brute-force the snapshot: a node
        // added after the pinned point must not appear in its results.
        use crate::storage::vector::Metric;

        fn embed(v: f32) -> Properties {
            let mut p = Properties::new();
            p.insert(
                "embedding".into(),
                PropDesc::new(PropValue::Vector(vec![v, 0.0])),
            );
            p
        }

        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();
        plane
            .ensure_vector_index("Doc", "embedding", Metric::Cosine)
            .unwrap();

        let a = {
            let mut w = plane.write().unwrap();
            let a = w.create_node_with_key("a", &["Doc"], embed(1.0)).unwrap();
            w.commit().unwrap();
            a
        };
        let s1 = db.commit_seq().unwrap();

        // A second, closer-to-the-query node lands later.
        {
            let mut w = plane.write().unwrap();
            w.create_node_with_key("b", &["Doc"], embed(0.9)).unwrap();
            w.commit().unwrap();
        }

        let query = vec![0.95f32, 0.0];
        // Live: both nodes are candidates.
        let now = plane
            .query()
            .vector_top_k(Some("Doc"), "embedding", query.clone(), Metric::Cosine, 10)
            .ids()
            .unwrap();
        assert_eq!(now.len(), 2);

        // As of s1: only `a` existed, so `b` cannot appear even though the live
        // index knows it.
        let past = plane
            .as_of(AsOf::Seq(s1))
            .unwrap()
            .query()
            .vector_top_k(Some("Doc"), "embedding", query, Metric::Cosine, 10)
            .ids()
            .unwrap();
        assert_eq!(past, vec![a]);
    }

    #[test]
    fn as_of_survives_compaction() {
        // With unbounded retention, an old snapshot stays readable even after
        // enough commits to trigger compaction (the versions aren't reclaimed).
        let (_dir, db) = open();
        let plane = db.create_plane("p", Properties::new()).unwrap();
        let id = {
            let mut w = plane.write().unwrap();
            let id = w
                .create_node_with_key("n1", &["Doc"], status_prop("original"))
                .unwrap();
            w.commit().unwrap();
            id
        };
        let s1 = db.commit_seq().unwrap();

        // Churn the same key many times to build several versions/runs.
        for i in 0..64 {
            let mut w = plane.write().unwrap();
            w.set_prop(
                id,
                "status",
                PropDesc::new(PropValue::Str(format!("rev{i}"))),
            )
            .unwrap();
            w.commit().unwrap();
        }

        let past = plane.as_of(AsOf::Seq(s1)).unwrap();
        assert_eq!(status_of(&past.node(id).unwrap().unwrap()), "original");
    }
}
