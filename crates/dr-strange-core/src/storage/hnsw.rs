//! Hand-rolled HNSW index (Malkov & Yashunin, 2016) behind [`VectorIndex`]
//! (arch/01 §5, open-Q 2 resolved: pure-Rust, hand-rolled — no C++ build
//! chain, and we own the on-disk format).
//!
//! Approximate: correctness is defined as *recall vs [`BruteForceIndex`]*,
//! which the tests assert with a tolerance on seeded random data. Determinism
//! (needed for reproducible tests) comes from a seeded xorshift used for
//! level assignment — no `rand` dependency.
//!
//! Deletion is tombstoning: a removed node stays in the graph for
//! connectivity but is never returned. The KV remains the source of truth
//! (arch/01 §5), so a compacting rebuild-from-KV is always available; this
//! index never needs to reclaim tombstones itself.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::vector::{Hit, IdFilter, Metric, VectorIndex, top_k};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HnswParams {
    /// Max neighbors per node per layer above 0.
    pub m: usize,
    /// Max neighbors at layer 0 (denser; conventionally `2*m`).
    pub m0: usize,
    /// Candidate-list width during insertion.
    pub ef_construction: usize,
    /// Candidate-list width during search.
    pub ef_search: usize,
    /// Level-generation normalization `1/ln(m)`.
    pub ml: f64,
    pub seed: u64,
}

impl HnswParams {
    pub fn new(m: usize) -> Self {
        Self {
            m,
            m0: m * 2,
            ef_construction: 200,
            ef_search: 64,
            ml: 1.0 / (m as f64).ln(),
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

impl Default for HnswParams {
    fn default() -> Self {
        Self::new(16)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    id: u64,
    vector: Vec<f32>,
    deleted: bool,
    /// Adjacency per layer: `layers[l]` holds internal indices of neighbors
    /// at layer `l`. `layers.len() - 1` is this node's top layer.
    layers: Vec<Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HnswIndex {
    metric: Metric,
    params: HnswParams,
    nodes: Vec<Node>,
    entry: Option<usize>,
    top_layer: usize,
    rng: u64,
    // Rebuilt on load, so skipped in the on-disk form.
    #[serde(skip)]
    id_to_idx: HashMap<u64, usize>,
}

/// `f32` wrapper with a total order (via `total_cmp`) for use in heaps.
#[derive(Clone, Copy, PartialEq)]
struct Dist(f32);
impl Eq for Dist {}
impl Ord for Dist {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for Dist {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl HnswIndex {
    pub fn new(metric: Metric) -> Self {
        Self::with_params(metric, HnswParams::default())
    }

    pub fn with_params(metric: Metric, params: HnswParams) -> Self {
        Self {
            metric,
            params,
            nodes: Vec::new(),
            entry: None,
            top_layer: 0,
            rng: params.seed,
            id_to_idx: HashMap::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut idx: HnswIndex = postcard::from_bytes(&bytes).map_err(backend)?;
        // Rebuild the live-id index (not serialized).
        for (i, node) in idx.nodes.iter().enumerate() {
            if !node.deleted {
                idx.id_to_idx.insert(node.id, i);
            }
        }
        Ok(idx)
    }

    pub fn len(&self) -> usize {
        self.id_to_idx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_to_idx.is_empty()
    }

    fn dist(&self, q: &[f32], idx: usize) -> f32 {
        self.metric.distance(q, &self.nodes[idx].vector)
    }

    fn next_rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn random_level(&mut self) -> usize {
        // u in (0,1]; level = floor(-ln(u) * ml)
        let bits = self.next_rand() >> 11; // 53 bits
        let u = ((bits as f64) / ((1u64 << 53) as f64)).max(f64::MIN_POSITIVE);
        (-u.ln() * self.params.ml) as usize
    }

    fn max_conn(&self, layer: usize) -> usize {
        if layer == 0 {
            self.params.m0
        } else {
            self.params.m
        }
    }

    /// The ef nearest nodes to `q` reachable from `entry_points` at `layer`,
    /// ascending by distance. Tombstoned nodes are traversed for connectivity
    /// but never included in the returned set.
    fn search_layer(
        &self,
        q: &[f32],
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<(f32, usize)> {
        let mut visited: HashSet<usize> = HashSet::new();
        // Candidate frontier: min-heap (nearest first) via Reverse.
        let mut cand: BinaryHeap<Reverse<(Dist, usize)>> = BinaryHeap::new();
        // Results: max-heap (farthest on top) capped at ef, live nodes only.
        let mut w: BinaryHeap<(Dist, usize)> = BinaryHeap::new();

        for &e in entry_points {
            let d = self.dist(q, e);
            visited.insert(e);
            cand.push(Reverse((Dist(d), e)));
            if !self.nodes[e].deleted {
                push_bounded(&mut w, ef, d, e);
            }
        }

        while let Some(Reverse((Dist(cd), c))) = cand.pop() {
            if w.len() >= ef
                && let Some((Dist(fd), _)) = w.peek()
                && cd > *fd
            {
                break; // nearest candidate worse than our worst keeper
            }
            for &e in &self.nodes[c].layers[layer] {
                if visited.insert(e) {
                    let d = self.dist(q, e);
                    let farthest = w.peek().map(|(Dist(fd), _)| *fd).unwrap_or(f32::INFINITY);
                    if d < farthest || w.len() < ef {
                        cand.push(Reverse((Dist(d), e)));
                        if !self.nodes[e].deleted {
                            push_bounded(&mut w, ef, d, e);
                        }
                    }
                }
            }
        }

        let mut out: Vec<(f32, usize)> = w.into_iter().map(|(Dist(d), i)| (d, i)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    fn connect(&mut self, a: usize, b: usize, layer: usize) {
        self.nodes[a].layers[layer].push(b);
        self.nodes[b].layers[layer].push(a);
        let max = self.max_conn(layer);
        self.prune(a, layer, max);
        self.prune(b, layer, max);
    }

    /// Keep only the `max` nearest neighbors of `node` at `layer` (simple
    /// closest-M selection).
    fn prune(&mut self, node: usize, layer: usize, max: usize) {
        if self.nodes[node].layers[layer].len() <= max {
            return;
        }
        let v = self.nodes[node].vector.clone();
        let mut ns = std::mem::take(&mut self.nodes[node].layers[layer]);
        ns.sort_by(|&x, &y| self.dist(&v, x).total_cmp(&self.dist(&v, y)));
        ns.truncate(max);
        self.nodes[node].layers[layer] = ns;
    }
}

/// Push `(d, e)` into a max-heap capped at `ef`, dropping the farthest on
/// overflow.
fn push_bounded(w: &mut BinaryHeap<(Dist, usize)>, ef: usize, d: f32, e: usize) {
    w.push((Dist(d), e));
    if w.len() > ef {
        w.pop();
    }
}

impl VectorIndex for HnswIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()> {
        // Upsert: tombstone any live node with this id, then add fresh.
        if let Some(&old) = self.id_to_idx.get(&id) {
            self.nodes[old].deleted = true;
            self.id_to_idx.remove(&id);
        }

        let level = self.random_level();
        let idx = self.nodes.len();
        self.nodes.push(Node {
            id,
            vector: vector.to_vec(),
            deleted: false,
            layers: vec![Vec::new(); level + 1],
        });
        self.id_to_idx.insert(id, idx);

        let Some(entry) = self.entry else {
            self.entry = Some(idx);
            self.top_layer = level;
            return Ok(());
        };

        // Descend from the top down to level+1 with a width-1 greedy search.
        let mut ep = entry;
        let mut lc = self.top_layer;
        while lc > level {
            let w = self.search_layer(vector, &[ep], 1, lc);
            if let Some(&(_, nearest)) = w.first() {
                ep = nearest;
            }
            lc -= 1;
        }

        // Insert at every layer from min(level, top) down to 0.
        let start = level.min(self.top_layer);
        let mut entry_points = vec![ep];
        for lc in (0..=start).rev() {
            let w = self.search_layer(vector, &entry_points, self.params.ef_construction, lc);
            let max = self.max_conn(lc);
            for &(_, neighbor) in w.iter().take(max) {
                self.connect(idx, neighbor, lc);
            }
            entry_points = if w.is_empty() {
                vec![ep]
            } else {
                w.iter().map(|&(_, i)| i).collect()
            };
        }

        if level > self.top_layer {
            self.top_layer = level;
            self.entry = Some(idx);
        }
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        if let Some(idx) = self.id_to_idx.remove(&id) {
            self.nodes[idx].deleted = true;
            // If we tombstoned the entry point, pick any live node as the new
            // one (search still works; the graph stays connected enough for
            // recall, and rebuild-from-KV is the real fix — arch/01 §5).
            if self.entry == Some(idx) {
                self.entry = self.id_to_idx.values().next().copied();
            }
        }
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize, filter: Option<&dyn IdFilter>) -> Result<Vec<Hit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let Some(entry) = self.entry else {
            return Ok(Vec::new());
        };

        let mut ep = entry;
        let mut lc = self.top_layer;
        while lc > 0 {
            let w = self.search_layer(query, &[ep], 1, lc);
            if let Some(&(_, nearest)) = w.first() {
                ep = nearest;
            }
            lc -= 1;
        }

        // A restrictive filter needs a wider beam to surface k matches; the
        // KV-backed brute-force path (small frontiers) is the exact fallback.
        let ef = if filter.is_some() {
            self.params.ef_search.max(k * 8)
        } else {
            self.params.ef_search.max(k)
        };
        let w = self.search_layer(query, &[ep], ef, 0);

        let hits = top_k(
            w.into_iter()
                .map(|(d, i)| (self.nodes[i].id, d))
                .filter(|(id, _)| filter.is_none_or(|f| f.contains(*id))),
            k,
        );
        Ok(hits)
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
    use crate::storage::vector::BruteForceIndex;
    use std::collections::HashSet;

    /// Deterministic vector generator (seeded xorshift → f32 in [-1,1]).
    struct Gen(u64);
    impl Gen {
        fn f32(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
        }
        fn vec(&mut self, dim: usize) -> Vec<f32> {
            (0..dim).map(|_| self.f32()).collect()
        }
    }

    fn recall_at_k(hnsw: &[u64], exact: &[u64]) -> f64 {
        let truth: HashSet<u64> = exact.iter().copied().collect();
        let hit = hnsw.iter().filter(|id| truth.contains(id)).count();
        hit as f64 / exact.len().max(1) as f64
    }

    #[test]
    fn recall_vs_brute_force_is_high() {
        let dim = 24;
        let mut vg = Gen(0xABCD_1234_5678_9012);
        let mut hnsw = HnswIndex::new(Metric::Cosine);
        let mut exact = BruteForceIndex::new(Metric::Cosine);
        let mut vectors = Vec::new();
        for id in 0..800u64 {
            let v = vg.vec(dim);
            hnsw.insert(id, &v).unwrap();
            exact.insert(id, &v).unwrap();
            vectors.push(v);
        }
        assert_eq!(hnsw.len(), 800);

        let k = 10;
        let mut total = 0.0;
        let queries = 40;
        for _ in 0..queries {
            let q = vg.vec(dim);
            let h: Vec<u64> = hnsw
                .search(&q, k, None)
                .unwrap()
                .iter()
                .map(|x| x.id)
                .collect();
            let e: Vec<u64> = exact
                .search(&q, k, None)
                .unwrap()
                .iter()
                .map(|x| x.id)
                .collect();
            assert_eq!(h.len(), k);
            total += recall_at_k(&h, &e);
        }
        let recall = total / queries as f64;
        assert!(
            recall >= 0.85,
            "mean recall@{k} was {recall:.3}, expected >= 0.85"
        );
    }

    #[test]
    fn deterministic_across_builds() {
        let dim = 8;
        let build = || {
            let mut g = Gen(42);
            let mut idx = HnswIndex::new(Metric::L2);
            for id in 0..100u64 {
                idx.insert(id, &g.vec(dim)).unwrap();
            }
            let mut q = Gen(7);
            idx.search(&q.vec(dim), 5, None).unwrap()
        };
        let a = build();
        let b = build();
        assert_eq!(
            a.iter().map(|h| h.id).collect::<Vec<_>>(),
            b.iter().map(|h| h.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn removed_ids_never_returned() {
        let dim = 8;
        let mut g = Gen(99);
        let mut idx = HnswIndex::new(Metric::L2);
        for id in 0..200u64 {
            idx.insert(id, &g.vec(dim)).unwrap();
        }
        let removed: HashSet<u64> = (0..100).collect();
        for &id in &removed {
            idx.remove(id).unwrap();
        }
        assert_eq!(idx.len(), 100);
        let mut q = Gen(1);
        for _ in 0..20 {
            let hits = idx.search(&q.vec(dim), 10, None).unwrap();
            assert!(hits.iter().all(|h| !removed.contains(&h.id)));
        }
    }

    #[test]
    fn filtered_search_only_returns_allowed() {
        let dim = 8;
        let mut g = Gen(5);
        let mut idx = HnswIndex::new(Metric::Cosine);
        for id in 0..300u64 {
            idx.insert(id, &g.vec(dim)).unwrap();
        }
        let allow: HashSet<u64> = (0..300).filter(|i| i % 5 == 0).collect();
        let mut q = Gen(3);
        let hits = idx.search(&q.vec(dim), 10, Some(&allow)).unwrap();
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| allow.contains(&h.id)));
    }

    #[test]
    fn empty_and_upsert() {
        let mut idx = HnswIndex::new(Metric::L2);
        assert!(idx.search(&[1.0, 2.0], 5, None).unwrap().is_empty());
        idx.insert(1, &[0.0, 0.0]).unwrap();
        idx.insert(2, &[9.0, 9.0]).unwrap();
        idx.insert(1, &[8.9, 8.9]).unwrap(); // upsert 1 near 2
        assert_eq!(idx.len(), 2);
        let hits = idx.search(&[9.0, 9.0], 2, None).unwrap();
        assert_eq!(hits.len(), 2); // both near the query now
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let dim = 8;
        let mut g = Gen(11);
        let mut idx = HnswIndex::new(Metric::Cosine);
        for id in 0..120u64 {
            idx.insert(id, &g.vec(dim)).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.idx");
        idx.persist(&path).unwrap();
        let loaded = HnswIndex::load(&path).unwrap();
        assert_eq!(loaded.len(), idx.len());

        let mut q = Gen(2);
        for _ in 0..10 {
            let query = q.vec(dim);
            assert_eq!(
                idx.search(&query, 5, None)
                    .unwrap()
                    .iter()
                    .map(|h| h.id)
                    .collect::<Vec<_>>(),
                loaded
                    .search(&query, 5, None)
                    .unwrap()
                    .iter()
                    .map(|h| h.id)
                    .collect::<Vec<_>>(),
            );
        }
    }
}
