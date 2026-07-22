//! Vector index seam (arch/01 §5). One index per (plane, label, property);
//! sidecar-persisted, rebuildable from the KV, which stays the single source
//! of truth. HNSW implementation chosen by benchmark at M3.

use std::path::Path;

use crate::error::Result;

/// Restricts an ANN search to a candidate set — how graph predicates are
/// pushed into vector search (arch/03 §4.3, §4.6).
pub trait IdFilter {
    fn contains(&self, id: u64) -> bool;
}

pub trait VectorIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()>;
    fn remove(&mut self, id: u64) -> Result<()>;

    /// Top-k by the index's metric; `filter` enables filtered ANN.
    fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&dyn IdFilter>,
    ) -> Result<Vec<(u64, f32)>>;

    fn persist(&self, path: &Path) -> Result<()>;
}
