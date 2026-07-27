//! In-memory vector-index registry (arch/01 §5).
//!
//! One HNSW index per declared `(plane, label, property)`. Only the
//! *declaration* is durable (in `meta`); the index structure is rebuilt from
//! the KV — the KV is the single source of truth. So a fresh registry is
//! reconstructed on [`Database::open`](crate::Database::open) by scanning the
//! declared labels; there is no on-disk index format to keep in sync.
//!
//! (Deferred: persisting the built HNSW *graph* as a sidecar to skip the
//! rebuild-from-KV on open — a pure open-time speedup, correctness already
//! holds without it. arch/01 §5.)
//!
//! The registry is owned by [`Database`](crate::Database) behind an
//! `RwLock`: query reads take a shared lock; write-transaction commits take
//! an exclusive lock to apply the coherence events buffered during the write
//! (see `api::WriteTxn`).

use std::collections::HashMap;

use crate::error::Result;
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::storage::hnsw::HnswIndex;
use crate::storage::vector::{Hit, Metric, VectorIndex};
use crate::types::{NodeId, PlaneId};

type IndexKey = (PlaneId, String, String);

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
