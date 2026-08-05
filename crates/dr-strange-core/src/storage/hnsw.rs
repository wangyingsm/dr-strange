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
//!
//! Soft-schema totality (the [`super::vector`] posture): vectors of the wrong
//! dimension can arrive via node properties, and every distance involving one
//! is `+∞` ([`guarded_dist`]) — mismatched pairs rank at the far edge rather
//! than panicking or feeding the unchecked SIMD kernel mismatched slices.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, backend};
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
            self.visited.fill(0);
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

/// The distance every kernel below routes through. Mismatched dimensions are
/// `+∞` — [`super::vector`]'s soft-schema totality posture (such pairs never
/// rank close, never an error) — which also keeps the unchecked SIMD `dot`
/// away from slices of different lengths (out-of-bounds reads in release).
fn guarded_dist(metric: Metric, a: &[f32], na: f32, b: &[f32], nb: f32) -> f32 {
    if a.len() != b.len() {
        return f32::INFINITY;
    }
    metric_dist(metric, dot(a, b), na, nb)
}

/// Distance from a prepared query to node `idx` — one dot.
fn dist_q(nodes: &[Node], metric: Metric, q: Query<'_>, idx: usize) -> f32 {
    let n = &nodes[idx];
    guarded_dist(metric, q.vec, q.norm, &n.vector, n.norm)
}

/// Distance between two stored nodes — one dot (used by pruning).
fn dist_nn(nodes: &[Node], metric: Metric, a: usize, b: usize) -> f32 {
    guarded_dist(
        metric,
        &nodes[a].vector,
        nodes[a].norm,
        &nodes[b].vector,
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
        if !idx.is_wellformed() {
            return Err(Error::Corrupt(
                "hnsw sidecar violates graph invariants".into(),
            ));
        }
        idx.reindex();
        Ok(idx)
    }

    /// Whether the graph upholds the invariants search/insert index by without
    /// bounds checks: every adjacency entry points to an existing node that is
    /// present on that layer, and `entry`/`top_layer` agree with the entry
    /// node. Live indexes hold these by construction; deserialized bytes are
    /// untrusted (a corrupt or foreign sidecar decodes into arbitrary
    /// indices, and searching such a graph panics), so loaders reject a
    /// malformed graph and let the caller rebuild from KV.
    pub(crate) fn is_wellformed(&self) -> bool {
        if let Some(e) = self.entry
            && self
                .nodes
                .get(e)
                .is_none_or(|n| n.layers.len() <= self.top_layer)
        {
            return false;
        }
        self.nodes.iter().all(|n| {
            n.layers.iter().enumerate().all(|(l, list)| {
                list.iter()
                    .all(|&e| self.nodes.get(e).is_some_and(|t| t.layers.len() > l))
            })
        })
    }

    /// Rebuild the live-id lookup (`id_to_idx`) from `nodes`. Needed after any
    /// deserialization, since that map is `#[serde(skip)]`. Idempotent.
    pub(crate) fn reindex(&mut self) {
        self.id_to_idx.clear();
        for (i, node) in self.nodes.iter().enumerate() {
            if !node.deleted {
                self.id_to_idx.insert(node.id, i);
            }
        }
    }

    /// Build the graph from a batch of `(id, vector)` concurrently across
    /// threads. The index MUST be empty. **Non-deterministic** — threads insert
    /// in a racing order, so the resulting graph varies run to run — but recall
    /// is preserved (approximate index; correctness is recall vs brute force).
    /// Small batches or a single core fall back to the sequential, deterministic
    /// [`insert`](Self::insert) path.
    ///
    /// Design: level assignment is precomputed sequentially (so it stays a pure
    /// function of the seed), then the node arena is pre-sized and filled with
    /// immutable data (vectors/norms — read lock-free during search) plus a
    /// per-node `Mutex` guarding only that node's adjacency. A thread never
    /// holds two node locks at once, so there is no lock-ordering deadlock.
    pub fn build_parallel(&mut self, items: &[(u64, Vec<f32>)]) -> Result<()> {
        debug_assert!(self.nodes.is_empty(), "build_parallel needs an empty index");
        let n = items.len();
        let threads = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
            .min(n);
        // Below this, thread setup + locking overhead outweighs the win; stay on
        // the deterministic sequential path (also keeps small-graph tests exact).
        if n < PARALLEL_MIN || threads <= 1 {
            for (id, v) in items {
                self.insert(*id, v)?;
            }
            return Ok(());
        }

        // Levels first, sequentially — a pure function of the seed regardless of
        // how the inserts are then parallelized.
        let meta: Vec<BuildMeta> = items
            .iter()
            .map(|(id, v)| BuildMeta {
                id: *id,
                norm: dot(v, v).sqrt(),
                level: self.random_level(),
                vector: v.clone(),
            })
            .collect();
        let adj: Vec<Mutex<Vec<Vec<usize>>>> = meta
            .iter()
            .map(|m| Mutex::new(vec![Vec::new(); m.level + 1]))
            .collect();
        // Node 0 seeds the graph (no links); the rest insert against it.
        let entry = Mutex::new((0usize, meta[0].level));
        let next = AtomicUsize::new(1);
        let metric = self.metric;
        let ef = self.params.ef_construction;
        let params = self.params;

        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut scratch = Scratch::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        insert_into_build(
                            i,
                            &meta,
                            &adj,
                            &entry,
                            metric,
                            ef,
                            &params,
                            &mut scratch,
                        );
                    }
                });
            }
        });

        // Freeze the arena into the serving representation.
        self.nodes = meta
            .into_iter()
            .zip(adj)
            .map(|(m, a)| Node {
                id: m.id,
                vector: m.vector,
                norm: m.norm,
                deleted: false,
                layers: a.into_inner().unwrap_or_else(|e| e.into_inner()),
            })
            .collect();
        let (e, top) = entry.into_inner().unwrap_or_else(|e| e.into_inner());
        self.entry = Some(e);
        self.top_layer = top;
        self.reindex();
        Ok(())
    }

    /// [`search`](VectorIndex::search) with the layer-0 beam width chosen by
    /// the caller: `search` applies the `ef_search`/`k` clamp and delegates
    /// here. Separate so the `measure_ef_*` sweep tests can measure raw beam
    /// widths the clamp would otherwise floor away.
    fn search_with_ef(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<&dyn IdFilter>,
        ef: usize,
    ) -> Result<Vec<Hit>> {
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

        let w = Self::search_layer(&self.nodes, self.metric, q, &[ep], ef, 0, &mut scratch);

        let hits = top_k(
            w.into_iter()
                .map(|(d, i)| (self.nodes[i].id, d))
                .filter(|(id, _)| filter.is_none_or(|f| f.contains(*id))),
            k,
        );
        Ok(hits)
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

// ---- Parallel bulk build ---------------------------------------------------
//
// A from-scratch build over many vectors, inserting nodes concurrently. The
// node arena is pre-sized; each node's immutable data (vector/norm/level) is
// read lock-free, and only its adjacency is behind a per-node `Mutex`. See
// [`HnswIndex::build_parallel`].

/// Batches at or above this size use the parallel path; smaller ones stay on
/// the sequential deterministic `insert` (setup/locking overhead isn't worth
/// it, and it keeps small-graph tests exact).
const PARALLEL_MIN: usize = 2048;

/// A node's build-time payload — everything except adjacency (which lives in a
/// separate per-node `Mutex`), so the fields here are read without locking.
struct BuildMeta {
    id: u64,
    vector: Vec<f32>,
    norm: f32,
    level: usize,
}

/// Query→node distance during the parallel build (reads immutable `meta`).
fn dist_bm_q(meta: &[BuildMeta], metric: Metric, q: Query<'_>, idx: usize) -> f32 {
    let n = &meta[idx];
    guarded_dist(metric, q.vec, q.norm, &n.vector, n.norm)
}

/// Node→node distance during the parallel build (reads immutable `meta`).
fn dist_bm(meta: &[BuildMeta], metric: Metric, a: usize, b: usize) -> f32 {
    guarded_dist(
        metric,
        &meta[a].vector,
        meta[a].norm,
        &meta[b].vector,
        meta[b].norm,
    )
}

/// The build-time analogue of [`HnswIndex::search_layer`]: the ef nearest nodes
/// to `q` reachable from `entry_points` at `layer`. Neighbor lists are read
/// under each node's lock (cloned out, lock released immediately); vectors are
/// read lock-free. No node is ever tombstoned during a build.
#[allow(clippy::too_many_arguments)]
fn search_build(
    meta: &[BuildMeta],
    adj: &[Mutex<Vec<Vec<usize>>>],
    metric: Metric,
    q: Query<'_>,
    entry_points: &[usize],
    ef: usize,
    layer: usize,
    scratch: &mut Scratch,
) -> Vec<(f32, usize)> {
    scratch.ready(meta.len());
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
            let d = dist_bm_q(meta, metric, q, e);
            cand.push(Reverse((Dist(d), e)));
            push_bounded(w, ef, d, e);
        }
    }

    while let Some(Reverse((Dist(cd), c))) = cand.pop() {
        if w.len() >= ef
            && let Some((Dist(fd), _)) = w.peek()
            && cd > *fd
        {
            break;
        }
        // Snapshot c's neighbors at `layer` under its lock, then release it.
        let neighbors: Vec<usize> = {
            let guard = adj[c].lock().unwrap_or_else(|e| e.into_inner());
            guard.get(layer).cloned().unwrap_or_default()
        };
        for e in neighbors {
            if visited[e] != epoch {
                visited[e] = epoch;
                let d = dist_bm_q(meta, metric, q, e);
                let farthest = w.peek().map(|(Dist(fd), _)| *fd).unwrap_or(f32::INFINITY);
                if d < farthest || w.len() < ef {
                    cand.push(Reverse((Dist(d), e)));
                    push_bounded(w, ef, d, e);
                }
            }
        }
    }

    let mut out: Vec<(f32, usize)> = w.iter().map(|&(Dist(d), i)| (d, i)).collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Keep only the `maxc` nearest neighbors of `node` at `layer`, in the locked
/// adjacency `links`. Distances read immutable `meta`, so no other lock is
/// taken while this node's lock is held.
fn prune_build(
    meta: &[BuildMeta],
    metric: Metric,
    node: usize,
    layer: usize,
    links: &mut [Vec<usize>],
    maxc: usize,
) {
    if links[layer].len() <= maxc {
        return;
    }
    let ns = std::mem::take(&mut links[layer]);
    let mut scored: Vec<(f32, usize)> = ns
        .iter()
        .map(|&x| (dist_bm(meta, metric, node, x), x))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.truncate(maxc);
    links[layer] = scored.into_iter().map(|(_, x)| x).collect();
}

/// Insert node `idx` into the shared build graph. Mirrors the sequential
/// [`HnswIndex::insert`] linking, but reads/writes adjacency through per-node
/// locks and the shared `entry`. Only one node lock is held at a time.
#[allow(clippy::too_many_arguments)]
fn insert_into_build(
    idx: usize,
    meta: &[BuildMeta],
    adj: &[Mutex<Vec<Vec<usize>>>],
    entry: &Mutex<(usize, usize)>,
    metric: Metric,
    ef: usize,
    params: &HnswParams,
    scratch: &mut Scratch,
) {
    let level = meta[idx].level;
    let q = Query {
        vec: &meta[idx].vector,
        norm: meta[idx].norm,
    };
    let max_conn = |layer: usize| if layer == 0 { params.m0 } else { params.m };

    // Snapshot the entry point / top layer for this insertion.
    let (mut ep, top) = *entry.lock().unwrap_or_else(|e| e.into_inner());

    // Greedy width-1 descent from the top down to level+1.
    let mut lc = top;
    while lc > level {
        let w = search_build(meta, adj, metric, q, &[ep], 1, lc, scratch);
        if let Some(&(_, nearest)) = w.first() {
            ep = nearest;
        }
        lc -= 1;
    }

    // Connect at every layer from min(level, top) down to 0.
    let start = level.min(top);
    let mut entry_points = vec![ep];
    for lc in (0..=start).rev() {
        let w = search_build(meta, adj, metric, q, &entry_points, ef, lc, scratch);
        let maxc = max_conn(lc);
        let chosen: Vec<usize> = w.iter().take(maxc).map(|&(_, i)| i).collect();

        // idx → chosen
        {
            let mut mine = adj[idx].lock().unwrap_or_else(|e| e.into_inner());
            for &nb in &chosen {
                mine[lc].push(nb);
            }
        }
        // chosen → idx, pruning each neighbor (its own lock only).
        for &nb in &chosen {
            let mut nl = adj[nb].lock().unwrap_or_else(|e| e.into_inner());
            nl[lc].push(idx);
            prune_build(meta, metric, nb, lc, &mut nl, maxc);
        }
        // prune idx itself
        {
            let mut mine = adj[idx].lock().unwrap_or_else(|e| e.into_inner());
            prune_build(meta, metric, idx, lc, &mut mine, maxc);
        }

        entry_points = if w.is_empty() {
            vec![ep]
        } else {
            w.iter().map(|&(_, i)| i).collect()
        };
    }

    // If this node is taller than the current top, it becomes the entry point.
    if level > top {
        let mut e = entry.lock().unwrap_or_else(|e| e.into_inner());
        if level > e.1 {
            *e = (idx, level);
        }
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
        // A restrictive filter needs a wider beam to surface k matches; the
        // KV-backed brute-force path (small frontiers) is the exact fallback.
        //
        // Unfiltered, the beam still needs headroom over k: at ef = k the deep
        // ranks sit at the beam's edge and their recall drops. The 2× floor is
        // the knee of the recall-vs-latency curve measured by the tests'
        // `measure_ef_multiplier_sweep` (weak-graph fixture, k=100: tail
        // recall 0.74 at ef=k vs 0.92 at ef=2k; past 2× each +50% ef buys
        // ≲0.03 recall).
        let ef = if filter.is_some() {
            self.params.ef_search.max(k * 8)
        } else {
            self.params.ef_search.max(k * 2)
        };
        self.search_with_ef(query, k, filter, ef)
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
    #[ignore = "measurement, not an assertion"]
    fn measure_parallel_vs_sequential_build() {
        use std::time::Instant;
        let dim = 64;
        let count = 50_000u64;
        let mut vg = Gen(0xDEAD_BEEF_CAFE_1234);
        let items: Vec<(u64, Vec<f32>)> = (0..count).map(|id| (id, vg.vec(dim))).collect();

        let mut seq = HnswIndex::new(Metric::Cosine);
        let t = Instant::now();
        for (id, v) in &items {
            seq.insert(*id, v).unwrap();
        }
        let seq_t = t.elapsed();

        let mut par = HnswIndex::new(Metric::Cosine);
        let t = Instant::now();
        par.build_parallel(&items).unwrap();
        let par_t = t.elapsed();

        let cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        println!(
            "build {count}x{dim} on {cores} cores: sequential {seq_t:?} vs parallel {par_t:?} \
             ({:.1}x faster)",
            seq_t.as_secs_f64() / par_t.as_secs_f64()
        );
    }

    #[test]
    fn parallel_build_has_high_recall() {
        // Enough points to cross PARALLEL_MIN and actually exercise threads.
        let dim = 16;
        let count = 3000u64;
        let mut vg = Gen(0x1357_9BDF_2468_ACE0);
        let items: Vec<(u64, Vec<f32>)> = (0..count).map(|id| (id, vg.vec(dim))).collect();

        let mut hnsw = HnswIndex::new(Metric::Cosine);
        hnsw.build_parallel(&items).unwrap();
        assert_eq!(hnsw.len(), count as usize);

        let mut exact = BruteForceIndex::new(Metric::Cosine);
        for (id, v) in &items {
            exact.insert(*id, v).unwrap();
        }

        let k = 10;
        let queries = 40;
        let mut total = 0.0;
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
            "parallel-build mean recall@{k} was {recall:.3}, expected >= 0.85"
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

    /// Soft-schema totality: vectors of the wrong dimension insert and search
    /// without panicking (debug) or out-of-bounds SIMD reads (release), and
    /// rank at `+∞` so they never displace well-formed matches.
    #[test]
    fn mismatched_dimensions_rank_at_infinity_not_ub() {
        let dim = 8;
        let mut g = Gen(77);
        let mut idx = HnswIndex::new(Metric::Cosine);
        for id in 0..50u64 {
            idx.insert(id, &g.vec(dim)).unwrap();
        }
        // Strays from "the wrong model" — wider and narrower both.
        idx.insert(100, &g.vec(dim * 2)).unwrap();
        idx.insert(101, &g.vec(dim / 2)).unwrap();

        for _ in 0..10 {
            let hits = idx.search(&g.vec(dim), 10, None).unwrap();
            assert_eq!(hits.len(), 10);
            assert!(
                hits.iter().all(|h| h.id < 50 && h.distance.is_finite()),
                "mismatched-dimension nodes must never outrank well-formed ones"
            );
        }

        // A wrong-dimension query is equally total: no panic, and everything
        // it returns is ranked at +∞ (callers filter by score).
        let hits = idx.search(&g.vec(dim * 4), 5, None).unwrap();
        assert!(hits.iter().all(|h| h.distance == f32::INFINITY));
    }

    /// The parallel build goes through its own distance kernels (`dist_bm*`);
    /// a mismatched vector in the batch must be as harmless there as in the
    /// sequential path.
    #[test]
    fn parallel_build_tolerates_mismatched_dimensions() {
        let dim = 8;
        let count = 2500u64; // above PARALLEL_MIN → actually threads
        let mut g = Gen(88);
        let mut items: Vec<(u64, Vec<f32>)> = (0..count).map(|id| (id, g.vec(dim))).collect();
        items[1234].1 = g.vec(dim * 2);

        let mut idx = HnswIndex::new(Metric::Cosine);
        idx.build_parallel(&items).unwrap();
        let hits = idx.search(&g.vec(dim), 10, None).unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.iter().all(|h| h.id != 1234 && h.distance.is_finite()));
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

    /// Mean recall of one ground-truth rank slice (`lo..hi`, 0-based) within
    /// the ids `search` returned, averaged over the query set.
    fn slice_recall(
        hnsw: &HnswIndex,
        exact: &BruteForceIndex,
        queries: &[Vec<f32>],
        k: usize,
        lo: usize,
        hi: usize,
    ) -> f64 {
        let mut total = 0.0;
        for q in queries {
            let got: Vec<u64> = hnsw
                .search(q, k, None)
                .unwrap()
                .iter()
                .map(|h| h.id)
                .collect();
            let truth: Vec<u64> = exact.search(q, k, None).unwrap()[lo..hi]
                .iter()
                .map(|h| h.id)
                .collect();
            assert_eq!(got.len(), k, "clamp must still yield k results");
            total += recall_at_k(&got, &truth);
        }
        total / queries.len() as f64
    }

    /// Guards `search`'s `ef = ef_search.max(2k)` clamp — the 2× headroom the
    /// `measure_ef_multiplier_sweep` measurement picked. At `ef = k` the deep
    /// ranks sit at the beam's edge and their recall collapses (tail 0.74,
    /// trailing the head by 0.145 on this fixture); with the 2× floor the tail
    /// holds 0.92, within 0.06 of the head. If the multiplier regresses to 1×,
    /// both assertions fail.
    ///
    /// A weaker-than-default graph (`m=8`, `ef_construction=80`) and a
    /// selective `k/N` (100 of 6000) keep the beam edge visible at a runtime a
    /// unit test can afford — with default build params and small N, recall
    /// saturates at ~1.0 regardless of the clamp.
    #[test]
    fn ef_clamp_headroom_keeps_tail_recall() {
        let dim = 48;
        let count = 6000u64;
        let k = 100; // > ef_search (64) → the 2× clamp runs the beam at ef = 200
        let mut params = HnswParams::new(8);
        params.ef_construction = 80;
        let mut vg = Gen(0x5EED_BEA3_0000_0001);
        let mut hnsw = HnswIndex::with_params(Metric::Cosine, params);
        let mut exact = BruteForceIndex::new(Metric::Cosine);
        for id in 0..count {
            let v = vg.vec(dim);
            hnsw.insert(id, &v).unwrap();
            exact.insert(id, &v).unwrap();
        }
        let queries: Vec<Vec<f32>> = (0..40).map(|_| vg.vec(dim)).collect();

        let head = slice_recall(&hnsw, &exact, &queries, k, 0, 20);
        let tail = slice_recall(&hnsw, &exact, &queries, k, 80, 100);
        eprintln!("clamped ef=2k: head(1-20) {head:.3}, tail(81-100) {tail:.3}");

        assert!(
            tail >= 0.88,
            "tail(81-100) recall {tail:.3} below 0.88 — did the ef clamp lose its 2x headroom?"
        );
        assert!(
            head - tail <= 0.10,
            "tail(81-100) recall {tail:.3} trails head(1-20) {head:.3} by more than 0.10"
        );
    }

    /// Print overall/tail recall@`k` and mean query latency for each raw
    /// layer-0 beam width in `efs` — the measurement loop behind the
    /// `measure_ef_*` sweeps. Calls `search_with_ef` directly so the sweep can
    /// observe beams narrower than the `search` clamp's `2k` floor.
    fn sweep_ef(
        hnsw: &HnswIndex,
        exact: &BruteForceIndex,
        queries: &[Vec<f32>],
        k: usize,
        efs: &[usize],
    ) {
        use std::time::Instant;
        let truths: Vec<Vec<u64>> = queries
            .iter()
            .map(|q| {
                exact
                    .search(q, k, None)
                    .unwrap()
                    .iter()
                    .map(|h| h.id)
                    .collect()
            })
            .collect();
        eprintln!("ef      overall@{k}  tail       mean query");
        for &ef in efs {
            let t = Instant::now();
            let got: Vec<Vec<u64>> = queries
                .iter()
                .map(|q| {
                    hnsw.search_with_ef(q, k, None, ef)
                        .unwrap()
                        .iter()
                        .map(|h| h.id)
                        .collect()
                })
                .collect();
            let per_query = t.elapsed() / queries.len() as u32;
            let mean = |lo: usize| {
                truths
                    .iter()
                    .zip(&got)
                    .map(|(truth, ids)| recall_at_k(ids, &truth[lo..]))
                    .sum::<f64>()
                    / queries.len() as f64
            };
            let (overall, tail) = (mean(0), mean(k * 8 / 10));
            eprintln!("{ef:<7} {overall:<12.3} {tail:<10.3} {per_query:?}");
        }
    }

    /// Sweep the layer-0 beam width to pick the clamp's `ef`-vs-`k` multiplier:
    /// the weak-graph fixture of [`ef_clamp_headroom_keeps_tail_recall`].
    /// Re-run with `--run-ignored all --no-capture` when re-tuning the magic
    /// number.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_ef_multiplier_sweep() {
        let dim = 48;
        let count = 6000u64;
        let k = 100;
        let mut params = HnswParams::new(8);
        params.ef_construction = 80;
        let mut vg = Gen(0x5EED_BEA3_0000_0001);
        let mut hnsw = HnswIndex::with_params(Metric::Cosine, params);
        let mut exact = BruteForceIndex::new(Metric::Cosine);
        for id in 0..count {
            let v = vg.vec(dim);
            hnsw.insert(id, &v).unwrap();
            exact.insert(id, &v).unwrap();
        }
        let queries: Vec<Vec<f32>> = (0..40).map(|_| vg.vec(dim)).collect();

        sweep_ef(
            &hnsw,
            &exact,
            &queries,
            k,
            &[100, 125, 150, 200, 250, 300, 400, 600],
        );
    }

    /// The same sweep at production shape: 1024-dim vectors with the geometry
    /// real embeddings have — points on a low-dimensional manifold (intrinsic
    /// dim 32, mapped through a fixed random basis into 1024 dims, plus small
    /// off-manifold noise), NOT uniform random 1024-dim data (which suffers
    /// distance concentration and misrepresents recall). Default build params
    /// (`m=16`, `ef_construction=200`) and the parallel builder, i.e. what
    /// production runs; queries are drawn from the same manifold.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn measure_ef_sweep_production_dim() {
        let ambient = 1024;
        let intrinsic = 32;
        let noise = 0.05;
        let count = 20_000u64;
        let k = 100;

        let mut vg = Gen(0x5EED_BEA3_0000_0002);
        let basis: Vec<Vec<f32>> = (0..intrinsic).map(|_| vg.vec(ambient)).collect();
        let embed = |g: &mut Gen| {
            let mut v = vec![0.0f32; ambient];
            for b in &basis {
                let z = g.f32();
                for (vi, bi) in v.iter_mut().zip(b) {
                    *vi += z * bi;
                }
            }
            for vi in v.iter_mut() {
                *vi += noise * g.f32();
            }
            v
        };

        let items: Vec<(u64, Vec<f32>)> = (0..count).map(|id| (id, embed(&mut vg))).collect();
        let queries: Vec<Vec<f32>> = (0..40).map(|_| embed(&mut vg)).collect();

        let mut hnsw = HnswIndex::new(Metric::Cosine);
        hnsw.build_parallel(&items).unwrap();
        let mut exact = BruteForceIndex::new(Metric::Cosine);
        for (id, v) in &items {
            exact.insert(*id, v).unwrap();
        }

        sweep_ef(
            &hnsw,
            &exact,
            &queries,
            k,
            &[100, 125, 150, 200, 250, 300, 400, 600],
        );
    }

    /// A sidecar can decode structurally while its graph is nonsense —
    /// dangling adjacency, a neighbor absent from the claimed layer, or a
    /// bogus entry point would all panic at the first search if trusted.
    /// `is_wellformed` gates both load paths (`HnswIndex::load` and the
    /// registry sidecar), so a bad file means rebuild-from-KV, not a crash.
    #[test]
    fn malformed_graphs_are_rejected_on_load() {
        let dim = 8;
        let mut g = Gen(21);
        let mut idx = HnswIndex::new(Metric::L2);
        for id in 0..30u64 {
            idx.insert(id, &g.vec(dim)).unwrap();
        }
        assert!(idx.is_wellformed());

        let mut dangling = idx.clone();
        dangling.nodes[3].layers[0].push(9999);
        assert!(!dangling.is_wellformed());

        let mut wrong_layer = idx.clone();
        wrong_layer.nodes[0].layers.resize(50, Vec::new());
        wrong_layer.nodes[0].layers[49] = vec![1]; // node 1 has no layer 49
        assert!(!wrong_layer.is_wellformed());

        let mut bad_entry = idx.clone();
        bad_entry.entry = Some(1000);
        assert!(!bad_entry.is_wellformed());

        let mut tall_top = idx.clone();
        tall_top.top_layer = 40; // the entry node has no layer 40
        assert!(!tall_top.is_wellformed());

        // The file boundary: a corrupted persisted index must fail load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.idx");
        dangling.persist(&path).unwrap();
        assert!(matches!(HnswIndex::load(&path), Err(Error::Corrupt(_))));
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
