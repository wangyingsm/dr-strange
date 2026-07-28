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
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::vector::{Hit, IdFilter, Metric, VectorIndex, dot, top_k};

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
    /// L2 norm of `vector`, cached at insert. Lets every metric reduce to a
    /// single dot product against a prepared query (arch/01 §5 build path):
    /// cosine = `1 - dot/(qn·nn)`, L2 = `√(qn² + nn² - 2·dot)`.
    norm: f32,
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
    // Reusable search buffers (see [`Scratch`]); never serialized. Owned by
    // the index so a whole build reuses one allocation instead of allocating
    // a visited-set + heaps per `search_layer` call.
    #[serde(skip)]
    scratch: Scratch,
}

/// Per-search scratch space, reused across the millions of `search_layer`
/// calls a build makes. `visited` is a generation-stamp buffer: a node is
/// "seen this search" iff `visited[i] == epoch`, so resetting is a single
/// `epoch += 1` instead of clearing the set. The two heaps are cleared and
/// refilled rather than reallocated.
#[derive(Debug, Clone, Default)]
struct Scratch {
    visited: Vec<u32>,
    epoch: u32,
    cand: BinaryHeap<Reverse<(Dist, usize)>>,
    w: BinaryHeap<(Dist, usize)>,
}

impl Scratch {
    /// Ready the buffers for a search over `n` nodes: grow `visited`, bump the
    /// generation (clearing on the rare `u32` wrap), and empty the heaps.
    fn ready(&mut self, n: usize) {
        if self.visited.len() < n {
            self.visited.resize(n, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.visited.iter_mut().for_each(|v| *v = 0);
            self.epoch = 1;
        }
        self.cand.clear();
        self.w.clear();
    }
}

/// Distance from a dot product and the two operands' L2 norms — the shared
/// form all three metrics collapse to once norms are cached. Matches
/// [`Metric::distance`] exactly so HNSW recall stays defined against the exact
/// brute-force oracle.
fn metric_dist(metric: Metric, d: f32, na: f32, nb: f32) -> f32 {
    match metric {
        Metric::Dot => -d,
        Metric::Cosine => {
            let denom = na * nb;
            if denom == 0.0 {
                1.0
            } else {
                1.0 - (d / denom).clamp(-1.0, 1.0)
            }
        }
        Metric::L2 => (na * na + nb * nb - 2.0 * d).max(0.0).sqrt(),
    }
}

/// A query vector plus its cached L2 norm — prepared once per search so each
/// distance against it is a single dot product.
#[derive(Clone, Copy)]
struct Query<'a> {
    vec: &'a [f32],
    norm: f32,
}

/// Distance from a prepared query to node `idx` — one dot.
fn dist_q(nodes: &[Node], metric: Metric, q: Query<'_>, idx: usize) -> f32 {
    let n = &nodes[idx];
    metric_dist(metric, dot(q.vec, &n.vector), q.norm, n.norm)
}

/// Distance between two stored nodes — one dot (used by pruning).
fn dist_nn(nodes: &[Node], metric: Metric, a: usize, b: usize) -> f32 {
    metric_dist(
        metric,
        dot(&nodes[a].vector, &nodes[b].vector),
        nodes[a].norm,
        nodes[b].norm,
    )
}

/// `f32` wrapper with a total order (via `total_cmp`) for use in heaps.
#[derive(Debug, Clone, Copy, PartialEq)]
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
            scratch: Scratch::default(),
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

    /// The ef nearest live nodes to a prepared query `(q, q_norm)` reachable
    /// from `entry_points` at `layer`, ascending by distance. Tombstoned nodes
    /// are traversed for connectivity but never returned. Uses (and leaves
    /// dirty) the caller-provided `scratch` buffers — associated, not a method,
    /// so `&nodes` and `&mut scratch` can be borrowed disjointly from `self`.
    fn search_layer(
        nodes: &[Node],
        metric: Metric,
        q: Query<'_>,
        entry_points: &[usize],
        ef: usize,
        layer: usize,
        scratch: &mut Scratch,
    ) -> Vec<(f32, usize)> {
        scratch.ready(nodes.len());
        let Scratch {
            visited,
            epoch,
            cand,
            w,
        } = scratch;
        let epoch = *epoch;

        for &e in entry_points {
            if visited[e] != epoch {
                visited[e] = epoch;
                let d = dist_q(nodes, metric, q, e);
                cand.push(Reverse((Dist(d), e)));
                if !nodes[e].deleted {
                    push_bounded(w, ef, d, e);
                }
            }
        }

        while let Some(Reverse((Dist(cd), c))) = cand.pop() {
            if w.len() >= ef
                && let Some((Dist(fd), _)) = w.peek()
                && cd > *fd
            {
                break; // nearest candidate worse than our worst keeper
            }
            for &e in &nodes[c].layers[layer] {
                if visited[e] != epoch {
                    visited[e] = epoch;
                    let d = dist_q(nodes, metric, q, e);
                    let farthest = w.peek().map(|(Dist(fd), _)| *fd).unwrap_or(f32::INFINITY);
                    if d < farthest || w.len() < ef {
                        cand.push(Reverse((Dist(d), e)));
                        if !nodes[e].deleted {
                            push_bounded(w, ef, d, e);
                        }
                    }
                }
            }
        }

        let mut out: Vec<(f32, usize)> = w.iter().map(|&(Dist(d), i)| (d, i)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// Keep only the `max_conn` nearest neighbors of `node` at `layer`. Scores
    /// each neighbor once (no vector clone, no re-sort recomputation).
    fn prune(&mut self, node: usize, layer: usize) {
        let max = self.max_conn(layer);
        if self.nodes[node].layers[layer].len() <= max {
            return;
        }
        let ns = std::mem::take(&mut self.nodes[node].layers[layer]);
        let mut scored: Vec<(f32, usize)> = ns
            .iter()
            .map(|&x| (dist_nn(&self.nodes, self.metric, node, x), x))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.truncate(max);
        self.nodes[node].layers[layer] = scored.into_iter().map(|(_, x)| x).collect();
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
        let q_norm = dot(vector, vector).sqrt(); // prepared-query norm, cached
        self.nodes.push(Node {
            id,
            vector: vector.to_vec(),
            norm: q_norm,
            deleted: false,
            layers: vec![Vec::new(); level + 1],
        });
        self.id_to_idx.insert(id, idx);

        let Some(entry) = self.entry else {
            self.entry = Some(idx);
            self.top_layer = level;
            return Ok(());
        };

        let metric = self.metric;
        let ef_construction = self.params.ef_construction;
        let q = Query {
            vec: vector,
            norm: q_norm,
        };

        // Descend from the top down to level+1 with a width-1 greedy search.
        let mut ep = entry;
        let mut lc = self.top_layer;
        while lc > level {
            let w = Self::search_layer(&self.nodes, metric, q, &[ep], 1, lc, &mut self.scratch);
            if let Some(&(_, nearest)) = w.first() {
                ep = nearest;
            }
            lc -= 1;
        }

        // Insert at every layer from min(level, top) down to 0. Add all of
        // idx's chosen edges, prune each neighbor once, then prune idx once
        // (vs. re-pruning idx after every single edge).
        let start = level.min(self.top_layer);
        let mut entry_points = vec![ep];
        for lc in (0..=start).rev() {
            let w = Self::search_layer(
                &self.nodes,
                metric,
                q,
                &entry_points,
                ef_construction,
                lc,
                &mut self.scratch,
            );
            let max = self.max_conn(lc);
            let chosen: Vec<usize> = w.iter().take(max).map(|&(_, i)| i).collect();
            for &nb in &chosen {
                self.nodes[idx].layers[lc].push(nb);
                self.nodes[nb].layers[lc].push(idx);
                self.prune(nb, lc);
            }
            self.prune(idx, lc);
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

        let q = Query {
            vec: query,
            norm: dot(query, query).sqrt(),
        };
        let mut scratch = Scratch::default();

        let mut ep = entry;
        let mut lc = self.top_layer;
        while lc > 0 {
            let w = Self::search_layer(&self.nodes, self.metric, q, &[ep], 1, lc, &mut scratch);
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
        let w = Self::search_layer(&self.nodes, self.metric, q, &[ep], ef, 0, &mut scratch);

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
