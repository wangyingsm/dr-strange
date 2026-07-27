//! Vector similarity: the metric, the `VectorIndex` seam, and an exact
//! brute-force implementation (arch/01 §5).
//!
//! The KV is the single source of truth for vectors (they live in node
//! records); an index is an accelerator built from them. [`BruteForceIndex`]
//! is both the small-plane implementation (arch/01 §5: below a threshold,
//! skip ANN) and the exact oracle that [`super::hnsw`]'s recall is tested
//! against.
//!
//! Everything here is **total** in the face of soft-schema data: a candidate
//! whose vector dimension doesn't match the query is simply skipped, never an
//! error — the same posture as the expression evaluator (arch/03 §2).

use std::collections::BinaryHeap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};

/// Similarity metric. `distance` is "smaller = closer" (what the index ranks
/// by); `similarity` is "larger = more similar" (what the query score channel
/// exposes). The two are always monotonically opposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    /// Cosine similarity in `[-1, 1]`; distance `1 - cos` in `[0, 2]`.
    Cosine,
    /// Dot product; distance is its negation.
    Dot,
    /// Euclidean (L2); distance is the L2 norm, similarity its negation.
    L2,
}

impl Metric {
    /// Distance between two equal-length vectors (smaller = closer).
    /// Mismatched dimensions yield `+∞` so the pair never ranks as close.
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return f32::INFINITY;
        }
        match self {
            Metric::Cosine => 1.0 - cosine(a, b),
            Metric::Dot => -dot(a, b),
            Metric::L2 => l2(a, b),
        }
    }

    /// Similarity (larger = more similar) — the score-channel value.
    pub fn similarity(self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return f32::NEG_INFINITY;
        }
        match self {
            Metric::Cosine => cosine(a, b),
            Metric::Dot => dot(a, b),
            Metric::L2 => -l2(a, b),
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot(a, b) / (na * nb)).clamp(-1.0, 1.0)
    }
}

/// Restricts a search to a candidate id set — how a graph frontier or label
/// predicate is pushed into vector search (arch/03 §4.3, §4.6).
pub trait IdFilter {
    fn contains(&self, id: u64) -> bool;
}

impl IdFilter for std::collections::HashSet<u64> {
    fn contains(&self, id: u64) -> bool {
        std::collections::HashSet::contains(self, &id)
    }
}

/// A vector search accelerator over a set of `(id, vector)` pairs.
pub trait VectorIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()>;
    fn remove(&mut self, id: u64) -> Result<()>;

    /// Up to `k` nearest ids to `query`, ascending by distance. `filter`, if
    /// given, restricts results to matching ids.
    fn search(&self, query: &[f32], k: usize, filter: Option<&dyn IdFilter>) -> Result<Vec<Hit>>;

    fn persist(&self, path: &Path) -> Result<()>;
}

/// One search result: an id and its distance (smaller = closer).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: u64,
    pub distance: f32,
}

/// A max-heap entry for top-k: ordered by distance via `f32::total_cmp` (so
/// NaN is well-ordered rather than panicking). A bounded max-heap keeps the
/// `k` smallest distances by popping the current largest when it overflows.
#[derive(PartialEq)]
struct HeapItem {
    distance: f32,
    id: u64,
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance.total_cmp(&other.distance)
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Collects the `k` smallest-distance items from an `(id, distance)` stream.
/// Shared by brute force and HNSW's layer-0 result gathering.
pub(crate) fn top_k(items: impl Iterator<Item = (u64, f32)>, k: usize) -> Vec<Hit> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::with_capacity(k + 1);
    for (id, distance) in items {
        heap.push(HeapItem { distance, id });
        if heap.len() > k {
            heap.pop(); // drop the current farthest
        }
    }
    let mut hits: Vec<Hit> = heap
        .into_iter()
        .map(|h| Hit {
            id: h.id,
            distance: h.distance,
        })
        .collect();
    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    hits
}

/// Exact nearest-neighbour index: stores every vector, scans them all per
/// query. Exact and dependency-free; the correctness oracle for HNSW and the
/// implementation used for planes too small to be worth an ANN graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BruteForceIndex {
    metric: Metric,
    vectors: Vec<(u64, Vec<f32>)>,
}

impl BruteForceIndex {
    pub fn new(metric: Metric) -> Self {
        Self {
            metric,
            vectors: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        postcard::from_bytes(&bytes).map_err(backend)
    }
}

impl VectorIndex for BruteForceIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()> {
        // Overwrite any existing vector for this id (idempotent upsert).
        if let Some(slot) = self.vectors.iter_mut().find(|(x, _)| *x == id) {
            slot.1 = vector.to_vec();
        } else {
            self.vectors.push((id, vector.to_vec()));
        }
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        self.vectors.retain(|(x, _)| *x != id);
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize, filter: Option<&dyn IdFilter>) -> Result<Vec<Hit>> {
        let items = self
            .vectors
            .iter()
            .filter(|(id, _)| filter.is_none_or(|f| f.contains(*id)))
            .map(|(id, v)| (*id, self.metric.distance(query, v)));
        Ok(top_k(items, k))
    }

    fn persist(&self, path: &Path) -> Result<()> {
        let bytes = postcard::to_stdvec(self).map_err(backend)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn metric_ranks_closest_first() {
        // cosine: [1,0] is closer to [1,0] than to [0,1]
        assert!(Metric::Cosine.distance(&[1.0, 0.0], &[1.0, 0.0]) < 1e-6);
        assert!(
            Metric::Cosine.distance(&[1.0, 0.0], &[1.0, 0.0])
                < Metric::Cosine.distance(&[1.0, 0.0], &[0.0, 1.0])
        );
        // similarity is opposed to distance
        assert!(
            Metric::Cosine.similarity(&[1.0, 0.0], &[1.0, 0.0])
                > Metric::Cosine.similarity(&[1.0, 0.0], &[0.0, 1.0])
        );
        // L2 of identical vectors is 0
        assert_eq!(Metric::L2.distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
        // dot distance is negated dot
        assert_eq!(Metric::Dot.distance(&[1.0, 2.0], &[3.0, 4.0]), -(3.0 + 8.0));
    }

    #[test]
    fn dimension_mismatch_is_far_not_a_panic() {
        assert_eq!(Metric::Cosine.distance(&[1.0], &[1.0, 2.0]), f32::INFINITY);
        assert_eq!(
            Metric::L2.similarity(&[1.0], &[1.0, 2.0]),
            f32::NEG_INFINITY
        );
    }

    fn build(metric: Metric, vecs: &[(u64, &[f32])]) -> BruteForceIndex {
        let mut idx = BruteForceIndex::new(metric);
        for (id, v) in vecs {
            idx.insert(*id, v).unwrap();
        }
        idx
    }

    #[test]
    fn brute_force_topk_orders_by_distance() {
        let idx = build(
            Metric::L2,
            &[
                (1, &[0.0, 0.0]),
                (2, &[1.0, 0.0]),
                (3, &[5.0, 5.0]),
                (4, &[0.5, 0.0]),
            ],
        );
        let hits = idx.search(&[0.0, 0.0], 2, None).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 4]);
        // distances ascending
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[test]
    fn brute_force_filter_restricts_candidates() {
        let idx = build(Metric::L2, &[(1, &[0.0]), (2, &[1.0]), (3, &[2.0])]);
        let allow: HashSet<u64> = [2, 3].into_iter().collect();
        let hits = idx.search(&[0.0], 5, Some(&allow)).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn insert_overwrites_and_remove_deletes() {
        let mut idx = build(Metric::L2, &[(1, &[0.0]), (2, &[9.0])]);
        idx.insert(2, &[0.1]).unwrap(); // move 2 close to origin
        assert_eq!(idx.len(), 2);
        let hits = idx.search(&[0.0], 2, None).unwrap();
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 2);
        idx.remove(1).unwrap();
        let hits = idx.search(&[0.0], 5, None).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn k_zero_and_k_larger_than_set() {
        let idx = build(Metric::L2, &[(1, &[0.0]), (2, &[1.0])]);
        assert!(idx.search(&[0.0], 0, None).unwrap().is_empty());
        assert_eq!(idx.search(&[0.0], 99, None).unwrap().len(), 2);
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.idx");
        let idx = build(Metric::Cosine, &[(1, &[1.0, 0.0]), (2, &[0.0, 1.0])]);
        idx.persist(&path).unwrap();
        let loaded = BruteForceIndex::load(&path).unwrap();
        assert_eq!(
            loaded.search(&[1.0, 0.0], 1, None).unwrap()[0].id,
            idx.search(&[1.0, 0.0], 1, None).unwrap()[0].id
        );
    }
}
