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
