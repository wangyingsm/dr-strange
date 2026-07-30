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

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::storage::hnsw::HnswIndex;
use crate::storage::vector::{Hit, Metric, VectorIndex};
use crate::types::{NodeId, PlaneId};

type IndexKey = (PlaneId, String, String);

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
}

/// Live set of vector indexes. Not internally locked — the owning
/// [`Database`](crate::Database) wraps it in an `RwLock`.
#[derive(Default)]
pub struct VectorRegistry {
    entries: HashMap<IndexKey, Entry>,
}

impl VectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discards all indexes and rebuilds them from the declarations in `txn`
    /// (called on database open).
    pub fn rebuild_from(&mut self, txn: &dyn ReadTransaction) -> Result<()> {
        self.entries.clear();
        for (plane, label, property, metric) in graph::list_vector_indexes(txn)? {
            self.build_entry(txn, plane, &label, &property, metric)?;
        }
        Ok(())
    }

    /// Builds (or replaces) one index by scanning its label and inserting
    /// every node's vector for `property`.
    pub fn build_entry(
        &mut self,
        txn: &dyn ReadTransaction,
        plane: PlaneId,
        label: &str,
        property: &str,
        metric: Metric,
    ) -> Result<()> {
        let mut index = HnswIndex::new(metric);
        for id in graph::scan_label(txn, plane, label)? {
            if let Some(v) = graph::node_vector(txn, plane, id, property)? {
                index.insert(id.0, &v)?;
            }
        }
        self.entries.insert(
            (plane, label.to_string(), property.to_string()),
            Entry { metric, index },
        );
        Ok(())
    }

    /// Serialize the whole registry to `path`, stamped with `seq` (the commit
    /// sequence the indexes are coherent with). Best-effort caller: a failure
    /// only costs a rebuild-from-KV on the next open.
    pub fn save_sidecar(&self, path: &Path, seq: u64) -> Result<()> {
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
        // Write to a temp path then rename, so a crash mid-write can't leave a
        // torn sidecar that decodes to garbage (rename is atomic on the same fs).
        let tmp = path.with_extension("hnsw.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load a registry from `path`, but only if it is fresh: its stamped
    /// sequence must equal `expected_seq` and its version must match. Returns
    /// `None` (→ caller rebuilds from KV) on absence, staleness, version
    /// mismatch, or any decode error — never an `Err`, since a bad sidecar is
    /// always recoverable by rebuilding.
    pub fn load_sidecar(path: &Path, expected_seq: u64) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let payload = bytes.strip_prefix(SIDECAR_MAGIC)?;
        let sidecar: SidecarOwned = postcard::from_bytes(payload).ok()?;
        if sidecar.version != SIDECAR_VERSION || sidecar.seq != expected_seq {
            return None;
        }
        let mut entries = HashMap::with_capacity(sidecar.entries.len());
        for entry in sidecar.entries {
            let mut index = entry.index;
            // id_to_idx is #[serde(skip)] — rebuild it before the index is used.
            index.reindex();
            entries.insert(
                (entry.plane, entry.label, entry.property),
                Entry {
                    metric: entry.metric,
                    index,
                },
            );
        }
        Some(Self { entries })
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
        if let Some(entry) = self
            .entries
            .get_mut(&(plane, label.to_string(), property.to_string()))
        {
            entry.index.insert(node.0, vector)?;
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
            graph::declare_vector_index(&mut txn, PlaneId::STARTUP, "Doc", "emb", Metric::L2)
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
