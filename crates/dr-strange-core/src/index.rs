//! In-memory vector-index registry (arch/01 §5).
//!
//! One HNSW index per declared `(plane, label, property)`. Only the
//! *declaration* is durable (in `meta`); the KV is the single source of truth,
//! so the index structure can always be rebuilt from it by scanning the
//! declared labels ([`VectorRegistry::rebuild_from`]).
//!
//! **Sidecar** (arch/01 §5): rebuilding a large HNSW graph from the KV on every
//! open is expensive (the graph is O(nodes·M) distance computations to
//! reconstruct). So the built graph is also persisted verbatim to a `.hnsw`
//! sidecar next to the database file and *loaded* on open — a pure open-time
//! speedup. The sidecar is only a cache: it is stamped with the commit
//! sequence it was written at, and loaded only when that equals the database's
//! current commit sequence. Any write bumps the sequence (arch/02 §3) —
//! including declaring or dropping an index — so a single equality check
//! covers both data drift and declaration drift; on any mismatch, absence, or
//! decode error we fall back to rebuilding from the KV (always correct).
//!
//! The registry is owned by [`Database`](crate::Database) behind an
//! `RwLock`: query reads take a shared lock; write-transaction commits take
//! an exclusive lock to apply the coherence events buffered during the write
//! (see `api::WriteTxn`).

use ahash::AHashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::error::{Result, backend};
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::storage::hnsw::HnswIndex;
use crate::storage::vector::{Hit, IndexWidth, Metric, VectorIndex};
use crate::types::{NodeId, PlaneId};

type IndexKey = (PlaneId, String, String);

/// One declared vector index: its metric, the width of the vectors it holds,
/// and the width its declaration fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexInfo {
    pub label: String,
    pub property: String,
    pub metric: Metric,
    /// What the index holds, folded over every vector in it.
    pub width: IndexWidth,
    /// What the declaration promised it would hold, if it named a width.
    pub declared: Option<usize>,
}

impl VectorIndexInfo {
    /// The width a vector written into this index has to be, when the store
    /// knows of one.
    ///
    /// The declaration wins: it is what the caller said the index is for, and
    /// it is the only thing that can be right when the first vector written
    /// is the wrong one. Failing that, an index already holding one width
    /// holds its writes to it; an empty index has no width yet and a mixed one
    /// no single width, so neither constrains a write.
    pub fn required_width(&self) -> Option<usize> {
        self.declared.or_else(|| self.width.uniform())
    }

    /// Folds one more vector of `len` into the width this index holds — or
    /// refuses, when the width it is held to says a vector that wide cannot
    /// be one of its own (#123).
    ///
    /// Left unrefused, such a write makes the index hold two widths, and a
    /// two-width index is what switches the query-side width refusal (#112)
    /// off for the pair: the mistake disables the guard built to catch it.
    pub fn accept(&mut self, len: usize) -> Result<()> {
        let (label, property) = (&self.label, &self.property);
        if let Some(required) = self.required_width()
            && required != len
        {
            let held = match self.declared {
                Some(d) => format!("is declared {d}-wide"),
                None => format!("holds {required}-wide vectors"),
            };
            return Err(Error::InvalidArgument(format!(
                "{property} is {len}-wide but the vector index on ({label}, {property}) {held} \
                 — a vector of a different width can never rank, so indexing it would leave the \
                 index holding two widths and every search over it answering less than it \
                 should. Embed with the model the index was built with, or keep {len}-wide \
                 vectors under a property of their own."
            )));
        }
        self.width = self.width.with(len);
        Ok(())
    }
}

/// Magic bytes prefixing a sidecar file; a cheap guard against handing a
/// wrong/foreign file to the decoder. Followed by the postcard payload.
const SIDECAR_MAGIC: &[u8; 4] = b"DRSH";
/// On-disk sidecar layout version. Bump whenever the serialized shape of
/// [`HnswIndex`]/`Node` or the sidecar envelope changes; an older version is
/// treated as stale (→ rebuild), never misdecoded.
const SIDECAR_VERSION: u32 = 1;

struct Entry {
    metric: Metric,
    index: HnswIndex,
    /// Folded width of everything indexed here. Derived, never serialized: the
    /// sidecar recomputes it on load, so adding it costs no format version.
    width: IndexWidth,
    /// The width the declaration fixed, if it named one (#123). Read from the
    /// declaration, not from the vectors — and like `width` it is not in the
    /// sidecar, so a sidecar load adopts it with
    /// [`VectorRegistry::adopt_declared_widths`].
    declared: Option<usize>,
}

/// Live set of vector indexes. Not internally locked — the owning
/// [`Database`](crate::Database) wraps it in an `RwLock`.
#[derive(Default)]
pub struct VectorRegistry {
    entries: AHashMap<IndexKey, Entry>,
    /// Test-only fault seam: when set, the next `upsert` fails instead of
    /// indexing. HNSW insertion has no failure of its own to provoke, and
    /// the commit path's promise — a failed index event after a durable
    /// commit is counted, logged and marks the registry diverged, never
    /// turned into an `Err` — needs one to be tested through `commit()`.
    #[cfg(test)]
    fail_next_upsert: bool,
}

impl VectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm the fault seam: the next `upsert` returns an error (see the
    /// field). Gated like its one caller, `api::index_divergence_tests`,
    /// so the redb-only test build does not see an unused method.
    #[cfg(all(test, feature = "native-backend"))]
    pub(crate) fn fail_next_upsert(&mut self) {
        self.fail_next_upsert = true;
    }

    /// Discards all indexes and rebuilds them from the declarations in `txn`
    /// (called on database open).
    pub fn rebuild_from(&mut self, txn: &dyn ReadTransaction) -> Result<()> {
        self.entries.clear();
        for decl in graph::list_vector_indexes(txn)? {
            self.build_entry(txn, &decl)?;
        }
        Ok(())
    }

    /// Takes each entry's declared width from the declarations in `txn`,
    /// without touching the indexes themselves.
    ///
    /// The sidecar carries the built graphs but not the declarations, so a
    /// registry loaded from one knows every vector it holds and nothing about
    /// the width it was promised to hold. Every sidecar load calls this; a
    /// rebuild does not need it, since it reads the declarations anyway.
    pub fn adopt_declared_widths(&mut self, txn: &dyn ReadTransaction) -> Result<()> {
        for decl in graph::list_vector_indexes(txn)? {
            if let Some(entry) = self
                .entries
                .get_mut(&(decl.plane, decl.label, decl.property))
            {
                entry.declared = decl.dim;
            }
        }
        Ok(())
    }

    /// Builds (or replaces) one declared index by scanning its label and
    /// inserting every node's vector for its property. The vectors are
    /// collected first and handed to [`HnswIndex::build_parallel`], which
    /// parallelizes large builds across threads (small ones stay on the
    /// sequential path).
    pub fn build_entry(
        &mut self,
        txn: &dyn ReadTransaction,
        decl: &graph::VectorDecl,
    ) -> Result<()> {
        let (plane, label, property) = (decl.plane, &decl.label, &decl.property);
        let mut items = Vec::new();
        for id in graph::scan_label(txn, plane, label)? {
            if let Some(v) = graph::node_vector(txn, plane, id, property)? {
                items.push((id.0, v));
            }
        }
        let mut index = HnswIndex::new(decl.metric);
        index.build_parallel(&items)?;
        let width = index.width();
        self.entries.insert(
            (plane, label.to_string(), property.to_string()),
            Entry {
                metric: decl.metric,
                index,
                width,
                declared: decl.dim,
            },
        );
        Ok(())
    }

    /// Serialize the whole registry to `path`, stamped with `seq` (the commit
    /// sequence the indexes are coherent with). Best-effort caller: a failure
    /// only costs a rebuild-from-KV on the next open.
    pub fn save_sidecar(&self, path: &Path, seq: u64) -> Result<()> {
        let bytes = self.to_bytes(seq)?;
        // Write to a temp path then rename, so a crash mid-write can't leave a
        // torn sidecar that decodes to garbage (rename is atomic on the same fs).
        let tmp = path.with_extension("hnsw.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The registry serialized to the sidecar byte form (magic + postcard),
    /// stamped with `seq`. The in-memory equivalent of [`save_sidecar`] — used
    /// to embed the built index in a database snapshot (ROADMAP §6).
    pub fn to_bytes(&self, seq: u64) -> Result<Vec<u8>> {
        // Borrowing view — serialize the live indexes in place, no clone of the
        // (potentially large) graphs.
        let entries: Vec<SidecarEntryRef<'_>> = self
            .entries
            .iter()
            .map(|((plane, label, property), e)| SidecarEntryRef {
                plane: *plane,
                label,
                property,
                metric: e.metric,
                index: &e.index,
            })
            .collect();
        let sidecar = SidecarRef {
            version: SIDECAR_VERSION,
            seq,
            entries,
        };
        let mut bytes = Vec::from(*SIDECAR_MAGIC);
        bytes.extend_from_slice(&postcard::to_stdvec(&sidecar).map_err(backend)?);
        Ok(bytes)
    }

    /// Load a registry from `path`, but only if it is fresh: its stamped
    /// sequence must equal `expected_seq` and its version must match. Returns
    /// `None` (→ caller rebuilds from KV) on absence, staleness, version
    /// mismatch, or any decode error — never an `Err`, since a bad sidecar is
    /// always recoverable by rebuilding.
    pub fn load_sidecar(path: &Path, expected_seq: u64) -> Option<Self> {
        Self::from_bytes(&std::fs::read(path).ok()?, expected_seq)
    }

    /// Parse a registry from the sidecar byte form (the in-memory counterpart of
    /// [`load_sidecar`]), only if fresh: version + `seq` must match. `None` on
    /// any mismatch/decode error — the caller rebuilds from KV.
    pub fn from_bytes(bytes: &[u8], expected_seq: u64) -> Option<Self> {
        let payload = bytes.strip_prefix(SIDECAR_MAGIC)?;
        let sidecar: SidecarOwned = postcard::from_bytes(payload).ok()?;
        if sidecar.version != SIDECAR_VERSION || sidecar.seq != expected_seq {
            return None;
        }
        let mut entries = AHashMap::with_capacity(sidecar.entries.len());
        for entry in sidecar.entries {
            let mut index = entry.index;
            // Decoded bytes are untrusted: a graph with dangling adjacency
            // would panic at first search. Stale-sidecar posture: rebuild.
            if !index.is_wellformed() {
                return None;
            }
            // id_to_idx is #[serde(skip)] — rebuild it before the index is used.
            index.reindex();
            let width = index.width();
            entries.insert(
                (entry.plane, entry.label, entry.property),
                Entry {
                    metric: entry.metric,
                    index,
                    width,
                    // Not in the sidecar — adopted from the declarations by
                    // `adopt_declared_widths`, which every load calls.
                    declared: None,
                },
            );
        }
        // Built through `new` so the test-only fault seam (when compiled in)
        // starts disarmed without a cfg-split initializer here.
        let mut registry = Self::new();
        registry.entries = entries;
        Some(registry)
    }

    /// Declared indexes on `plane`, as `(label, property, metric)` — the
    /// snapshot a write transaction takes to know which mutations to mirror.
    pub fn declared(&self, plane: PlaneId) -> Vec<(String, String, Metric)> {
        self.entries
            .iter()
            .filter(|((p, _, _), _)| *p == plane)
            .map(|((_, l, prop), e)| (l.clone(), prop.clone(), e.metric))
            .collect()
    }

    /// The width of the vectors one declared index holds, or `None` if that
    /// `(plane, label, property)` has no index. Removing a vector never
    /// narrows [`IndexWidth::Mixed`] back to `Uniform`, so this claims less
    /// than it might rather than more.
    pub fn width(&self, plane: PlaneId, label: &str, property: &str) -> Option<IndexWidth> {
        self.entries
            .get(&(plane, label.to_string(), property.to_string()))
            .map(|e| e.width)
    }

    /// [`Self::declared`] plus each index's widths — what a caller needs to
    /// see whether its own vectors are the shape the index holds, and what a
    /// write is held to.
    pub fn declared_widths(&self, plane: PlaneId) -> Vec<VectorIndexInfo> {
        self.entries
            .iter()
            .filter(|((p, _, _), _)| *p == plane)
            .map(|((_, label, property), e)| VectorIndexInfo {
                label: label.clone(),
                property: property.clone(),
                metric: e.metric,
                width: e.width,
                declared: e.declared,
            })
            .collect()
    }

    /// Index-backed similarity search, or `None` if no matching index exists
    /// (the caller then falls back to exact brute force). The declared metric
    /// must match the requested one — an index built for cosine can't answer
    /// an L2 query.
    pub fn search(
        &self,
        plane: PlaneId,
        label: &str,
        property: &str,
        query: &[f32],
        metric: Metric,
        k: usize,
    ) -> Option<Result<Vec<Hit>>> {
        let entry = self
            .entries
            .get(&(plane, label.to_string(), property.to_string()))?;
        if entry.metric != metric {
            return None;
        }
        Some(entry.index.search(query, k, None))
    }

    /// Insert/replace a node's vector in the matching index (no-op if that
    /// `(plane, label, property)` isn't indexed).
    pub fn upsert(
        &mut self,
        plane: PlaneId,
        label: &str,
        property: &str,
        node: NodeId,
        vector: &[f32],
    ) -> Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_upsert) {
            return Err(backend(std::io::Error::other(
                "injected vector index fault",
            )));
        }
        if let Some(entry) = self
            .entries
            .get_mut(&(plane, label.to_string(), property.to_string()))
        {
            entry.index.insert(node.0, vector)?;
            entry.width = entry.width.with(vector.len());
        }
        Ok(())
    }

    /// Remove a node from one specific index.
    pub fn remove_one(
        &mut self,
        plane: PlaneId,
        label: &str,
        property: &str,
        node: NodeId,
    ) -> Result<()> {
        if let Some(entry) = self
            .entries
            .get_mut(&(plane, label.to_string(), property.to_string()))
        {
            entry.index.remove(node.0)?;
        }
        Ok(())
    }

    /// Remove a node from every index (node deletion). Ids are globally
    /// unique, so removing from indexes it was never in is a harmless no-op.
    pub fn remove_node(&mut self, node: NodeId) -> Result<()> {
        for entry in self.entries.values_mut() {
            entry.index.remove(node.0)?;
        }
        Ok(())
    }

    /// Forget every index declared on `plane` (plane drop). The declarations
    /// go with the plane's KV rows; this keeps the live registry — and the
    /// sidecar saved from it — from carrying an index for a plane that no
    /// longer exists.
    pub fn drop_plane(&mut self, plane: PlaneId) {
        self.entries.retain(|(p, _, _), _| *p != plane);
    }
}

// ---- Sidecar wire form -----------------------------------------------------
//
// postcard is not self-describing, so the borrowing (save) and owning (load)
// structs need only agree on field ORDER and wire types — `&str`/`String`
// serialize identically. Keeping them separate lets `save` borrow the live
// indexes (no clone) while `load` produces owned ones.

#[derive(Serialize)]
struct SidecarRef<'a> {
    version: u32,
    seq: u64,
    entries: Vec<SidecarEntryRef<'a>>,
}

#[derive(Serialize)]
struct SidecarEntryRef<'a> {
    plane: PlaneId,
    label: &'a str,
    property: &'a str,
    metric: Metric,
    index: &'a HnswIndex,
}

#[derive(Deserialize)]
struct SidecarOwned {
    version: u32,
    seq: u64,
    entries: Vec<SidecarEntryOwned>,
}

#[derive(Deserialize)]
struct SidecarEntryOwned {
    plane: PlaneId,
    label: String,
    property: String,
    metric: Metric,
    index: HnswIndex,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::engine::{StorageEngine, WriteTransaction};
    use crate::storage::memory::MemoryEngine;
    use crate::types::{PropDesc, PropValue, Properties};

    fn emb(v: Vec<f32>) -> Properties {
        [("emb".to_string(), PropDesc::new(PropValue::Vector(v)))]
            .into_iter()
            .collect()
    }

    /// Builds a startup-plane graph with three "Doc" nodes and a declared
    /// L2 index over them, returning the engine + node ids.
    fn setup() -> (MemoryEngine, VectorRegistry, [NodeId; 3]) {
        let eng = MemoryEngine::new();
        let ids;
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            let a =
                graph::create_node(&mut txn, PlaneId::STARTUP, &["Doc"], &emb(vec![0.0])).unwrap();
            let b =
                graph::create_node(&mut txn, PlaneId::STARTUP, &["Doc"], &emb(vec![5.0])).unwrap();
            let c =
                graph::create_node(&mut txn, PlaneId::STARTUP, &["Doc"], &emb(vec![9.0])).unwrap();
            graph::declare_vector_index(&mut txn, PlaneId::STARTUP, "Doc", "emb", Metric::L2, None)
                .unwrap();
            ids = [a, b, c];
            txn.commit().unwrap();
        }
        let mut reg = VectorRegistry::new();
        {
            let txn = eng.begin_read().unwrap();
            reg.rebuild_from(&txn).unwrap();
        }
        (eng, reg, ids)
    }

    #[test]
    fn rebuild_declared_and_search() {
        let (_eng, reg, [a, _b, _c]) = setup();
        assert_eq!(
            reg.declared(PlaneId::STARTUP),
            vec![("Doc".to_string(), "emb".to_string(), Metric::L2)]
        );
        let hits = reg
            .search(PlaneId::STARTUP, "Doc", "emb", &[0.0], Metric::L2, 1)
            .unwrap()
            .unwrap();
        assert_eq!(hits[0].id, a.0);
    }

    /// The width is observed, not declared: uniform while every indexed
    /// vector agrees, `Mixed` once one does not, and `None` for a pair that
    /// has no index at all. This is what tells a client its 384-wide query
    /// cannot match a 1-wide index, instead of an empty answer.
    #[test]
    fn width_is_uniform_until_a_vector_disagrees() {
        let (_eng, mut reg, _) = setup();
        assert_eq!(
            reg.width(PlaneId::STARTUP, "Doc", "emb"),
            Some(IndexWidth::Uniform(1))
        );
        assert_eq!(reg.width(PlaneId::STARTUP, "Ghost", "emb"), None);
        assert_eq!(
            reg.declared_widths(PlaneId::STARTUP),
            vec![VectorIndexInfo {
                label: "Doc".to_string(),
                property: "emb".to_string(),
                metric: Metric::L2,
                width: IndexWidth::Uniform(1),
                declared: None,
            }]
        );

        reg.upsert(PlaneId::STARTUP, "Doc", "emb", NodeId(99), &[1.0, 2.0])
            .unwrap();
        assert_eq!(
            reg.width(PlaneId::STARTUP, "Doc", "emb"),
            Some(IndexWidth::Mixed)
        );
    }

    /// A declared index with nothing in it has no width to disagree with —
    /// the case in which no refusal is possible and a search just answers
    /// empty.
    #[test]
    fn width_of_an_empty_index_is_empty() {
        let eng = MemoryEngine::new();
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            graph::declare_vector_index(&mut txn, PlaneId::STARTUP, "Doc", "emb", Metric::L2, None)
                .unwrap();
            txn.commit().unwrap();
        }
        let mut reg = VectorRegistry::new();
        reg.rebuild_from(&eng.begin_read().unwrap()).unwrap();
        assert_eq!(
            reg.width(PlaneId::STARTUP, "Doc", "emb"),
            Some(IndexWidth::Empty)
        );
    }

    /// The width is derived, so it survives a sidecar round trip without the
    /// serialized shape — and therefore `SIDECAR_VERSION` — having to change.
    #[test]
    fn width_is_recomputed_from_a_sidecar() {
        let (_eng, reg, _) = setup();
        let bytes = reg.to_bytes(7).unwrap();
        let loaded = VectorRegistry::from_bytes(&bytes, 7).expect("fresh sidecar loads");
        assert_eq!(
            loaded.width(PlaneId::STARTUP, "Doc", "emb"),
            Some(IndexWidth::Uniform(1))
        );
    }

    #[test]
    fn search_returns_none_for_missing_or_wrong_metric() {
        let (_eng, reg, _) = setup();
        // no such index
        assert!(
            reg.search(PlaneId::STARTUP, "Ghost", "emb", &[0.0], Metric::L2, 1)
                .is_none()
        );
        // declared for L2, queried as Cosine
        assert!(
            reg.search(PlaneId::STARTUP, "Doc", "emb", &[0.0], Metric::Cosine, 1)
                .is_none()
        );
    }

    #[test]
    fn sidecar_roundtrips_and_honors_seq_and_magic() {
        let (_eng, reg, [a, _b, _c]) = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.drsg.hnsw");

        // Save stamped with seq 7; a fresh load at 7 restores the same index.
        reg.save_sidecar(&path, 7).unwrap();
        let loaded = VectorRegistry::load_sidecar(&path, 7).expect("fresh sidecar loads");
        let hit = loaded
            .search(PlaneId::STARTUP, "Doc", "emb", &[0.0], Metric::L2, 1)
            .unwrap()
            .unwrap();
        assert_eq!(hit[0].id, a.0);
        assert_eq!(
            loaded.declared(PlaneId::STARTUP),
            vec![("Doc".to_string(), "emb".to_string(), Metric::L2)]
        );

        // A different expected seq (data/declaration drifted) → not loaded.
        assert!(VectorRegistry::load_sidecar(&path, 8).is_none());

        // A missing file → not loaded (→ caller rebuilds).
        assert!(VectorRegistry::load_sidecar(&dir.path().join("absent.hnsw"), 7).is_none());

        // A file without the magic prefix → not loaded, never misdecoded.
        std::fs::write(&path, b"not a sidecar").unwrap();
        assert!(VectorRegistry::load_sidecar(&path, 7).is_none());
    }

    #[test]
    fn upsert_remove_one_and_remove_node() {
        let (_eng, mut reg, [a, b, _c]) = setup();
        let near0 = |r: &VectorRegistry| {
            r.search(PlaneId::STARTUP, "Doc", "emb", &[0.0], Metric::L2, 1)
                .unwrap()
                .unwrap()[0]
                .id
        };
        assert_eq!(near0(&reg), a.0);

        // move b to the origin via upsert; it becomes nearest
        reg.upsert(PlaneId::STARTUP, "Doc", "emb", b, &[0.0])
            .unwrap();
        // both a and b at 0 now; remove a → b is the unique nearest
        reg.remove_one(PlaneId::STARTUP, "Doc", "emb", a).unwrap();
        assert_eq!(near0(&reg), b.0);

        // remove_one on an index that doesn't exist is a harmless no-op
        reg.remove_one(PlaneId::STARTUP, "Ghost", "emb", b).unwrap();
        // upsert into a non-existent index is also a no-op
        reg.upsert(PlaneId::STARTUP, "Ghost", "emb", b, &[0.0])
            .unwrap();

        // remove_node strips b from every index
        reg.remove_node(b).unwrap();
        assert!(
            reg.search(PlaneId::STARTUP, "Doc", "emb", &[0.0], Metric::L2, 5)
                .unwrap()
                .unwrap()
                .iter()
                .all(|h| h.id != b.0)
        );
    }
}
