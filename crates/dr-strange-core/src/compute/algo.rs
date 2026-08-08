//! Graph algorithms (ROADMAP §1): classic whole-graph operations exposed as
//! first-class, read-only operations over a single snapshot.
//!
//! Unlike the pull-based executor ([`exec`](super::exec)), which streams rows,
//! these are **whole-graph** computations (PageRank, community detection) that
//! don't fit a row model, so they live in their own module and run directly
//! over a [`GraphReader`]. Each one:
//! - reads one consistent snapshot (the caller holds the read txn),
//! - operates over the whole plane, or a single label subset,
//! - returns a **transient** result (node→score / node→community / a path) —
//!   no graph mutation, no schema impact.
//!
//! The graph is materialized once into a compact [`Frame`] (contiguous
//! indices with out-adjacency), which every algorithm shares. This is
//! deliberate: the v1 scope is whole-plane analytics, where one in-memory pass
//! is the simplest correct thing. Subgraph/seeded scoping and property
//! materialization are follow-ups (see ROADMAP §1 forks).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use ahash::AHashMap;

use crate::cache::GraphReader;
use crate::error::Result;
use crate::types::{Dir, EdgeId, NodeId, PropValue};

/// A materialized, index-compact view of the plane's graph for whole-graph
/// algorithms. Nodes are renumbered to `0..n` (their original [`NodeId`]s kept
/// in `nodes`); `out[i]` is the directed out-adjacency of node `i` as
/// `(target_index, edge_id)`, restricted to the scanned universe.
struct Frame {
    /// Original ids, indexed by internal index.
    nodes: Vec<NodeId>,
    /// Directed out-adjacency: `out[i]` = edges leaving node `i`.
    out: Vec<Vec<(usize, EdgeId)>>,
}

impl Frame {
    /// Materialize the scanned universe (whole plane, or one `label`) and its
    /// internal-to-internal out-adjacency. Edges to nodes outside the universe
    /// (only possible under a label subset) are dropped.
    fn build<R: GraphReader + ?Sized>(reader: &R, label: Option<&str>) -> Result<Self> {
        let nodes = match label {
            Some(l) => reader.scan_label(l)?,
            None => reader.scan_all()?,
        };
        let index: AHashMap<NodeId, usize> =
            nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let mut out: Vec<Vec<(usize, EdgeId)>> = vec![Vec::new(); nodes.len()];
        for (i, &nid) in nodes.iter().enumerate() {
            for nb in reader.neighbors(nid, Dir::Out, None)?.iter() {
                if let Some(&j) = index.get(&nb.node) {
                    out[i].push((j, nb.edge));
                }
            }
        }
        Ok(Self { nodes, out })
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Transposed adjacency (in-edges): `t[j]` = edges arriving at node `j`.
    fn transpose(&self) -> Vec<Vec<(usize, EdgeId)>> {
        let mut t: Vec<Vec<(usize, EdgeId)>> = vec![Vec::new(); self.len()];
        for (i, adj) in self.out.iter().enumerate() {
            for &(j, e) in adj {
                t[j].push((i, e));
            }
        }
        t
    }
}

// ---- PageRank ------------------------------------------------------------

/// PageRank tuning knobs. Defaults follow the classic paper (damping 0.85) and
/// converge on most graphs well inside 20 iterations.
#[derive(Debug, Clone, Copy)]
pub struct PageRankOptions {
    /// Probability of following an edge vs teleporting (classic: 0.85).
    pub damping: f64,
    /// Hard cap on iterations.
    pub max_iters: u32,
    /// Stop early once the summed absolute rank change drops below this.
    pub tolerance: f64,
}

impl Default for PageRankOptions {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iters: 20,
            tolerance: 1e-6,
        }
    }
}

/// Compute PageRank over the scanned universe. Dangling nodes (no out-edges)
/// redistribute their mass uniformly, so the scores sum to 1. Deterministic.
/// Returns `(node, score)` sorted by score descending (ties by ascending id).
pub fn pagerank<R: GraphReader + ?Sized>(
    reader: &R,
    label: Option<&str>,
    opts: PageRankOptions,
) -> Result<Vec<(NodeId, f64)>> {
    let frame = Frame::build(reader, label)?;
    let n = frame.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let nf = n as f64;
    let mut rank = vec![1.0 / nf; n];
    let mut next = vec![0.0f64; n];
    let teleport = (1.0 - opts.damping) / nf;

    for _ in 0..opts.max_iters {
        // Dangling mass is spread uniformly so total rank is conserved.
        let dangling: f64 = (0..n)
            .filter_map(|i| frame.out[i].is_empty().then_some(rank[i]))
            .sum();
        let base = teleport + opts.damping * dangling / nf;
        for v in next.iter_mut() {
            *v = base;
        }
        for (i, adj) in frame.out.iter().enumerate() {
            if adj.is_empty() {
                continue;
            }
            let share = opts.damping * rank[i] / adj.len() as f64;
            for &(j, _) in adj {
                next[j] += share;
            }
        }
        let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        std::mem::swap(&mut rank, &mut next);
        if delta < opts.tolerance {
            break;
        }
    }

    let mut scored: Vec<(NodeId, f64)> = frame
        .nodes
        .iter()
        .zip(&rank)
        .map(|(&id, &s)| (id, s))
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.0.cmp(&b.0.0))
    });
    Ok(scored)
}

// ---- Connected components ------------------------------------------------

/// Weakly connected components (edges treated as undirected). Returns
/// `(node, component)` where `component` is the smallest [`NodeId`] in the
/// node's component (a stable, deterministic representative), plus the total
/// component count. Rows are ordered by ascending node id.
pub fn connected_components<R: GraphReader + ?Sized>(
    reader: &R,
    label: Option<&str>,
) -> Result<(Vec<(NodeId, NodeId)>, usize)> {
    let frame = Frame::build(reader, label)?;
    let n = frame.len();
    let mut uf = UnionFind::new(n);
    for i in 0..n {
        for &(j, _) in &frame.out[i] {
            uf.union(i, j);
        }
    }
    // Representative = smallest original id in the set.
    let mut rep_id: Vec<Option<NodeId>> = vec![None; n];
    for i in 0..n {
        let r = uf.find(i);
        let cur = frame.nodes[i];
        rep_id[r] = Some(match rep_id[r] {
            Some(existing) if existing.0 <= cur.0 => existing,
            _ => cur,
        });
    }
    let mut roots: Vec<usize> = (0..n).filter(|&i| uf.find(i) == i).collect();
    roots.sort_unstable();
    let count = roots.len();
    let mut out: Vec<(NodeId, NodeId)> = (0..n)
        .map(|i| (frame.nodes[i], rep_id[uf.find(i)].expect("root has a rep")))
        .collect();
    out.sort_unstable_by_key(|&(node, _)| node.0);
    Ok((out, count))
}

/// Union-find with path compression + union by size.
struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}

// ---- Shortest path -------------------------------------------------------

/// Shortest-path tuning: which direction to follow, and an optional numeric
/// edge property to use as weight (missing/non-numeric ⇒ weight 1.0).
#[derive(Debug, Clone)]
pub struct ShortestPathOptions {
    /// Edge direction to traverse (`Out` = follow edges forward).
    pub dir: Dir,
    /// Edge property giving a non-negative weight; `None` ⇒ unit weights (BFS).
    pub weight: Option<String>,
}

impl Default for ShortestPathOptions {
    fn default() -> Self {
        Self {
            dir: Dir::Out,
            weight: None,
        }
    }
}

/// A found path: the node chain from source to target, the edges traversed
/// (one fewer than nodes), and the total cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
    pub cost: f64,
}

/// Weighted shortest path from `src` to `dst` (Dijkstra). Returns `None` if
/// either endpoint is outside the scanned universe or `dst` is unreachable.
/// Weights must be non-negative; a negative/non-finite weight is clamped to 0.
pub fn shortest_path<R: GraphReader + ?Sized>(
    reader: &R,
    label: Option<&str>,
    src: NodeId,
    dst: NodeId,
    opts: &ShortestPathOptions,
) -> Result<Option<Path>> {
    let frame = Frame::build(reader, label)?;
    let index: AHashMap<NodeId, usize> = frame
        .nodes
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i))
        .collect();
    let (Some(&s), Some(&t)) = (index.get(&src), index.get(&dst)) else {
        return Ok(None);
    };

    // Successor adjacency for the requested direction.
    let succ: Vec<Vec<(usize, EdgeId)>> = match opts.dir {
        Dir::Out => frame.out.clone(),
        Dir::In => frame.transpose(),
        Dir::Both => {
            let mut merged = frame.out.clone();
            for (j, adj) in frame.transpose().into_iter().enumerate() {
                merged[j].extend(adj);
            }
            merged
        }
    };

    let n = frame.len();
    let mut dist = vec![f64::INFINITY; n];
    let mut prev: Vec<Option<(usize, EdgeId)>> = vec![None; n];
    dist[s] = 0.0;
    let mut heap = BinaryHeap::new();
    heap.push(DijkstraState { cost: 0.0, node: s });

    while let Some(DijkstraState { cost, node }) = heap.pop() {
        if node == t {
            break;
        }
        if cost > dist[node] {
            continue;
        }
        for &(j, e) in &succ[node] {
            let w = self::edge_weight(reader, e, opts.weight.as_deref())?;
            let nd = cost + w;
            if nd < dist[j] {
                dist[j] = nd;
                prev[j] = Some((node, e));
                heap.push(DijkstraState { cost: nd, node: j });
            }
        }
    }

    if !dist[t].is_finite() {
        return Ok(None);
    }
    // Reconstruct src→dst by walking predecessors backward.
    let mut nodes = vec![frame.nodes[t]];
    let mut edges = Vec::new();
    let mut cur = t;
    while let Some((p, e)) = prev[cur] {
        edges.push(e);
        nodes.push(frame.nodes[p]);
        cur = p;
    }
    nodes.reverse();
    edges.reverse();
    Ok(Some(Path {
        nodes,
        edges,
        cost: dist[t],
    }))
}

/// Look up a directed edge's weight from `prop`; unit weight when no property
/// is requested, the edge is gone, or the value isn't a finite non-negative
/// number.
fn edge_weight<R: GraphReader + ?Sized>(
    reader: &R,
    edge: EdgeId,
    prop: Option<&str>,
) -> Result<f64> {
    let Some(prop) = prop else {
        return Ok(1.0);
    };
    let w = match reader.edge(edge)? {
        Some(rec) => match rec.properties.get(prop).map(|p| &p.value) {
            Some(PropValue::Int(i)) => *i as f64,
            Some(PropValue::Float(f)) => *f,
            _ => 1.0,
        },
        None => 1.0,
    };
    Ok(if w.is_finite() && w >= 0.0 { w } else { 0.0 })
}

/// Dijkstra frontier entry — a min-heap by cost (via reversed `Ord`).
struct DijkstraState {
    cost: f64,
    node: usize,
}
impl PartialEq for DijkstraState {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost && self.node == other.node
    }
}
impl Eq for DijkstraState {}
impl Ord for DijkstraState {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed so `BinaryHeap` (a max-heap) yields the smallest cost first;
        // node index tie-breaks for determinism.
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(Ordering::Equal)
            .then(other.node.cmp(&self.node))
    }
}
impl PartialOrd for DijkstraState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---- Louvain community detection -----------------------------------------

/// Louvain tuning. The default runs the full multi-level algorithm to a fixed
/// point (bounded by `max_levels`); `min_gain` gates local moves.
#[derive(Debug, Clone, Copy)]
pub struct LouvainOptions {
    /// Cap on aggregation levels.
    pub max_levels: u32,
    /// Minimum modularity gain to accept a node move.
    pub min_gain: f64,
}

impl Default for LouvainOptions {
    fn default() -> Self {
        Self {
            max_levels: 10,
            min_gain: 1e-9,
        }
    }
}

/// Louvain community detection over an undirected, unit-weighted view of the
/// graph (edge direction ignored, multiplicity kept). Returns `(node,
/// community)` where `community` is the smallest [`NodeId`] in the community
/// (a stable representative), plus the community count. Deterministic: nodes
/// are visited in index order, no randomization.
pub fn louvain<R: GraphReader + ?Sized>(
    reader: &R,
    label: Option<&str>,
    opts: LouvainOptions,
) -> Result<(Vec<(NodeId, NodeId)>, usize)> {
    let frame = Frame::build(reader, label)?;
    let n = frame.len();
    if n == 0 {
        return Ok((Vec::new(), 0));
    }

    // Build an undirected weighted graph: each directed edge contributes weight
    // 1 to the undirected pair (self-loops kept separately). `adj[i]` maps a
    // neighbor to the summed weight; `self_w[i]` is the node's self-loop weight.
    let mut adj: Vec<AHashMap<usize, f64>> = vec![AHashMap::new(); n];
    let mut self_w = vec![0.0f64; n];
    for i in 0..n {
        for &(j, _) in &frame.out[i] {
            if i == j {
                self_w[i] += 1.0;
            } else {
                *adj[i].entry(j).or_insert(0.0) += 1.0;
                *adj[j].entry(i).or_insert(0.0) += 1.0;
            }
        }
    }
    // Multi-level: each level condenses communities into super-nodes. `level`
    // is the current (condensed) graph; `community[orig]` tracks each original
    // node's index in that graph's node space. `one_level` returns a *dense*
    // `0..k` membership, so its values are directly the next level's node
    // indices — no second renumbering needed.
    let mut level = WGraph { adj, self_w };
    let mut community: Vec<usize> = (0..n).collect();

    for _ in 0..opts.max_levels {
        let membership = level.one_level(opts.min_gain);
        let moved = membership.iter().enumerate().any(|(i, &c)| c != i);
        // Propagate this level's membership down to original nodes.
        for c in community.iter_mut() {
            *c = membership[*c];
        }
        if !moved {
            break;
        }
        level = level.condense(&membership);
    }

    // Translate community indices to representative NodeIds (smallest id).
    let mut rep: AHashMap<usize, NodeId> = AHashMap::new();
    for (&id, &c) in frame.nodes.iter().zip(&community) {
        rep.entry(c)
            .and_modify(|e| {
                if id.0 < e.0 {
                    *e = id;
                }
            })
            .or_insert(id);
    }
    let count = rep.len();
    let mut out: Vec<(NodeId, NodeId)> = frame
        .nodes
        .iter()
        .zip(&community)
        .map(|(&id, &c)| (id, rep[&c]))
        .collect();
    out.sort_unstable_by_key(|&(node, _)| node.0);
    Ok((out, count))
}

/// Undirected weighted graph used by Louvain, with self-loop weights split out.
struct WGraph {
    adj: Vec<AHashMap<usize, f64>>,
    self_w: Vec<f64>,
}

impl WGraph {
    fn len(&self) -> usize {
        self.adj.len()
    }

    /// Total edge weight `m` (`2m` in modularity): sum of all pairwise weights
    /// (each undirected pair counted once) plus self-loop weights.
    fn total_weight(&self) -> f64 {
        let pairwise: f64 = self
            .adj
            .iter()
            .map(|a| a.values().sum::<f64>())
            .sum::<f64>()
            / 2.0;
        pairwise + self.self_w.iter().sum::<f64>()
    }

    /// Weighted degree of node `i` (self-loops count double, per convention).
    fn degree(&self, i: usize) -> f64 {
        self.adj[i].values().sum::<f64>() + 2.0 * self.self_w[i]
    }

    /// One level of local moving: greedily move nodes to the neighboring
    /// community that maximizes modularity gain until no move helps. Returns
    /// each node's community index (values are node indices used as labels).
    fn one_level(&self, min_gain: f64) -> Vec<usize> {
        let n = self.len();
        let m = self.total_weight();
        let two_m = 2.0 * m;
        let mut comm: Vec<usize> = (0..n).collect();
        let deg: Vec<f64> = (0..n).map(|i| self.degree(i)).collect();
        // Σ_tot: total degree of nodes in each community.
        let mut tot: Vec<f64> = deg.clone();

        if two_m == 0.0 {
            return comm; // no edges: every node its own community
        }

        let mut improved = true;
        while improved {
            improved = false;
            for i in 0..n {
                let ci = comm[i];
                // Weight from i into each neighboring community.
                let mut w_to: AHashMap<usize, f64> = AHashMap::new();
                for (&j, &w) in &self.adj[i] {
                    *w_to.entry(comm[j]).or_insert(0.0) += w;
                }
                // Remove i from its community.
                tot[ci] -= deg[i];
                let w_to_ci = w_to.get(&ci).copied().unwrap_or(0.0);

                // Pick the best community (gain relative to isolated). Baseline
                // is staying isolated in `ci`; we bias toward the current
                // community on ties for stability.
                let mut best_comm = ci;
                let mut best_gain = w_to_ci - tot[ci] * deg[i] / two_m;
                let mut candidates: Vec<(usize, f64)> = w_to.into_iter().collect();
                candidates.sort_unstable_by_key(|&(c, _)| c);
                for (c, w) in candidates {
                    let gain = w - tot[c] * deg[i] / two_m;
                    if gain > best_gain + min_gain {
                        best_gain = gain;
                        best_comm = c;
                    }
                }
                tot[best_comm] += deg[i];
                if best_comm != ci {
                    comm[i] = best_comm;
                    improved = true;
                }
            }
        }
        renumber(&comm)
    }

    /// Condense communities into super-nodes: build the aggregated weighted
    /// graph, where `membership` is a dense `0..k` labeling of this graph's
    /// nodes.
    fn condense(&self, membership: &[usize]) -> WGraph {
        let k = membership.iter().copied().max().map_or(0, |m| m + 1);
        let mut adj: Vec<AHashMap<usize, f64>> = vec![AHashMap::new(); k];
        let mut self_w = vec![0.0f64; k];
        for i in 0..self.len() {
            let ci = membership[i];
            self_w[ci] += self.self_w[i];
            for (&j, &w) in &self.adj[i] {
                let cj = membership[j];
                if ci == cj {
                    // Intra-community edge becomes a self-loop; halve because
                    // each undirected pair is seen from both endpoints.
                    self_w[ci] += w / 2.0;
                } else {
                    *adj[ci].entry(cj).or_insert(0.0) += w;
                }
            }
        }
        WGraph { adj, self_w }
    }
}

/// Compress arbitrary label values into a dense `0..k` numbering, preserving
/// first-seen order so the mapping is deterministic.
fn renumber(labels: &[usize]) -> Vec<usize> {
    let mut map: AHashMap<usize, usize> = AHashMap::new();
    let mut out = Vec::with_capacity(labels.len());
    for &l in labels {
        let next = map.len();
        let id = *map.entry(l).or_insert(next);
        out.push(id);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::types::{PropDesc, PropValue, Properties};
    use crate::{Database, Dir, NodeId};

    use super::*;

    fn db() -> Database {
        Database::in_memory().unwrap()
    }

    fn weight_prop(w: i64) -> Properties {
        [("w".to_string(), PropDesc::new(PropValue::Int(w)))]
            .into_iter()
            .collect()
    }

    #[test]
    fn pagerank_conserves_mass_and_ranks_hub_top() {
        let db = db();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        // A hub `h` that three others point at (and nothing leaves h).
        let h = txn.create_node(&["N"], Properties::new()).unwrap();
        let a = txn.create_node(&["N"], Properties::new()).unwrap();
        let b = txn.create_node(&["N"], Properties::new()).unwrap();
        let c = txn.create_node(&["N"], Properties::new()).unwrap();
        for &s in &[a, b, c] {
            txn.create_edge(s, h, "E", Properties::new()).unwrap();
        }
        txn.commit().unwrap();

        let ranks = plane.algo().pagerank(PageRankOptions::default()).unwrap();
        assert_eq!(ranks.len(), 4);
        // Total rank is conserved (dangling mass redistributed).
        let sum: f64 = ranks.iter().map(|&(_, s)| s).sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum={sum}");
        // The hub is the most important node.
        assert_eq!(ranks[0].0, h);
    }

    #[test]
    fn components_finds_disjoint_groups() {
        let db = db();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let a = txn.create_node(&["N"], Properties::new()).unwrap();
        let b = txn.create_node(&["N"], Properties::new()).unwrap();
        let c = txn.create_node(&["N"], Properties::new()).unwrap();
        let d = txn.create_node(&["N"], Properties::new()).unwrap();
        let _e = txn.create_node(&["N"], Properties::new()).unwrap(); // isolated
        txn.create_edge(a, b, "E", Properties::new()).unwrap();
        txn.create_edge(c, d, "E", Properties::new()).unwrap();
        txn.commit().unwrap();

        let (comp, count) = plane.algo().connected_components().unwrap();
        assert_eq!(count, 3, "two pairs + one isolated");
        let of = |id: NodeId| comp.iter().find(|&&(n, _)| n == id).unwrap().1;
        assert_eq!(of(a), of(b));
        assert_eq!(of(c), of(d));
        assert_ne!(of(a), of(c));
        // Representative is the smallest id in the component.
        assert_eq!(of(a), a);
    }

    #[test]
    fn shortest_path_respects_weights_and_direction() {
        let db = db();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let n: Vec<NodeId> = (0..4)
            .map(|_| txn.create_node(&["N"], Properties::new()).unwrap())
            .collect();
        // Chain 0->1->2->3 (unit) plus a direct 0->3 with weight 10.
        txn.create_edge(n[0], n[1], "E", Properties::new()).unwrap();
        txn.create_edge(n[1], n[2], "E", Properties::new()).unwrap();
        txn.create_edge(n[2], n[3], "E", Properties::new()).unwrap();
        txn.create_edge(n[0], n[3], "E", weight_prop(10)).unwrap();
        txn.commit().unwrap();

        // Unweighted (BFS): the 1-hop direct edge wins.
        let unit = plane
            .algo()
            .shortest_path(n[0], n[3], &ShortestPathOptions::default())
            .unwrap()
            .expect("reachable");
        assert_eq!(unit.nodes, vec![n[0], n[3]]);
        assert_eq!(unit.cost, 1.0);

        // Weighted by "w" (missing ⇒ 1): the chain (cost 3) beats direct (10).
        let opts = ShortestPathOptions {
            dir: Dir::Out,
            weight: Some("w".to_string()),
        };
        let weighted = plane
            .algo()
            .shortest_path(n[0], n[3], &opts)
            .unwrap()
            .expect("reachable");
        assert_eq!(weighted.nodes, vec![n[0], n[1], n[2], n[3]]);
        assert_eq!(weighted.cost, 3.0);

        // Following edges backward from 3 reaches 0 only via Both/In.
        let out_only = plane
            .algo()
            .shortest_path(n[3], n[0], &ShortestPathOptions::default())
            .unwrap();
        assert!(out_only.is_none(), "no forward path 3->0");
        let both = ShortestPathOptions {
            dir: Dir::Both,
            weight: None,
        };
        assert!(
            plane
                .algo()
                .shortest_path(n[3], n[0], &both)
                .unwrap()
                .is_some(),
            "Both direction reaches 0 from 3"
        );
    }

    #[test]
    fn louvain_separates_two_cliques() {
        let db = db();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let g: Vec<NodeId> = (0..6)
            .map(|_| txn.create_node(&["N"], Properties::new()).unwrap())
            .collect();
        // Triangle {0,1,2} and triangle {3,4,5}, bridged by a single 2-3 edge.
        for &(a, b) in &[(0, 1), (1, 2), (0, 2), (3, 4), (4, 5), (3, 5), (2, 3)] {
            txn.create_edge(g[a], g[b], "E", Properties::new()).unwrap();
        }
        txn.commit().unwrap();

        let (comm, count) = plane.algo().louvain(LouvainOptions::default()).unwrap();
        assert_eq!(count, 2, "two cliques ⇒ two communities");
        let of = |id: NodeId| comm.iter().find(|&&(n, _)| n == id).unwrap().1;
        assert_eq!(of(g[0]), of(g[1]));
        assert_eq!(of(g[1]), of(g[2]));
        assert_eq!(of(g[3]), of(g[4]));
        assert_eq!(of(g[4]), of(g[5]));
        assert_ne!(of(g[0]), of(g[3]));
    }

    #[test]
    fn label_scope_restricts_the_universe() {
        let db = db();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let doc1 = txn.create_node(&["Doc"], Properties::new()).unwrap();
        let doc2 = txn.create_node(&["Doc"], Properties::new()).unwrap();
        let _other = txn.create_node(&["Other"], Properties::new()).unwrap();
        txn.create_edge(doc1, doc2, "E", Properties::new()).unwrap();
        txn.commit().unwrap();

        // Whole plane: 3 nodes, 2 components ({doc1,doc2}, {other}).
        let (_, whole) = plane.algo().connected_components().unwrap();
        assert_eq!(whole, 2);
        // Label-scoped to Doc: only the two connected docs, 1 component.
        let (rows, scoped) = plane.algo().label("Doc").connected_components().unwrap();
        assert_eq!(scoped, 1);
        assert_eq!(rows.len(), 2);
    }
}
