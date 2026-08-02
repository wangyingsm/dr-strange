//! Hybrid retrieval fusion (ROADMAP §2): merge up to three ranked channels —
//! vector similarity, BM25 keyword, and graph proximity — into one ranking via
//! **weighted, min-max-normalized score fusion**.
//!
//! Each channel produces raw `(node, score)` hits. Scores across channels are
//! incomparable (a cosine distance, a BM25 score, a decay weight), so each
//! channel is normalized to `[0, 1]` (best = 1) before a weighted sum. A node
//! absent from a channel contributes 0 for that channel. The result carries the
//! fused score plus each channel's raw contribution, so a caller can see *why*
//! a node ranked where it did.
//!
//! The graph channel is a *proximity* signal: seed from the strongest
//! vector/keyword hits, expand `hops` outward, and score each reached node by
//! `decay^distance` — so a node near several strong hits gets boosted even if
//! its own text/vector match is weak.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::cache::GraphReader;
use crate::error::{Error, Result};
use crate::storage::vector::Metric;
use crate::types::{Dir, NodeId};

/// Per-channel weights for the fused score. Graph proximity defaults to a
/// softer boost than the two primary retrieval channels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HybridWeights {
    pub vector: f32,
    pub keyword: f32,
    pub graph: f32,
}

impl Default for HybridWeights {
    fn default() -> Self {
        Self {
            vector: 1.0,
            keyword: 1.0,
            graph: 0.5,
        }
    }
}

/// One fused result. `score` is the weighted sum of the normalized channel
/// contributions; the `vector`/`keyword`/`graph` fields carry each channel's
/// *raw* score for that node (`None` if the node wasn't in that channel):
/// vector = distance (lower is closer), keyword = BM25, graph = `decay^hops`.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridHit {
    pub node: NodeId,
    pub score: f32,
    pub vector: Option<f32>,
    pub keyword: Option<f32>,
    pub graph: Option<f32>,
}

/// A complete hybrid query: which channels are enabled, how they are weighted,
/// and how deep to look. Serializable, so it is both what [`HybridBuilder`]
/// assembles and what a `Source::Hybrid` plan node carries over the wire.
///
/// [`HybridBuilder`]: crate::HybridBuilder
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HybridSpec {
    /// Scope every channel to this label. Required by the keyword channel (its
    /// index is keyed on the label); optional for the others.
    pub label: Option<String>,
    pub vector: Option<VectorChannel>,
    pub keyword: Option<KeywordChannel>,
    pub graph: Option<GraphChannel>,
    pub weights: HybridWeights,
    /// Per-channel candidate pool fetched before fusion.
    pub candidates: usize,
    /// How many fused hits to return.
    pub k: usize,
}

/// The vector channel: rank by distance from `query` under `metric`. The query
/// is already embedded — the core never calls a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorChannel {
    pub property: String,
    pub query: Vec<f32>,
    pub metric: Metric,
}

/// The BM25 channel: rank by relevance of `property` to the text `query`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeywordChannel {
    pub property: String,
    pub query: String,
}

/// The graph-proximity channel: seed from the strongest primary-channel hits,
/// expand `hops` outward, decaying the boost by `decay` per hop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphChannel {
    pub hops: u32,
    pub decay: f32,
    /// Top hits taken from each primary channel as seeds.
    pub seeds: usize,
}

/// Run a hybrid query over one reader: gather each enabled channel and fuse
/// them. The single implementation behind both `plane.hybrid()` and a
/// `Source::Hybrid` plan node, so the two can never drift.
pub fn run<R: GraphReader + ?Sized>(reader: &R, spec: &HybridSpec) -> Result<Vec<HybridHit>> {
    let label = spec.label.as_deref();

    let vector = match &spec.vector {
        Some(v) => {
            let hits =
                reader.vector_search(label, &v.property, &v.query, v.metric, spec.candidates)?;
            Some(Channel {
                hits: hits
                    .into_iter()
                    .map(|h| (NodeId(h.id), h.distance))
                    .collect(),
                higher_better: false,
            })
        }
        None => None,
    };

    let keyword = match &spec.keyword {
        Some(kw) => {
            let label = label.ok_or_else(|| {
                Error::InvalidArgument("the hybrid keyword channel requires a label".into())
            })?;
            Some(Channel {
                hits: reader.keyword_search(label, &kw.property, &kw.query, spec.candidates)?,
                higher_better: true,
            })
        }
        None => None,
    };

    let graph = match &spec.graph {
        Some(g) => Some(Channel {
            hits: graph_proximity(
                reader,
                &top_seeds(&vector, &keyword, g.seeds),
                g.hops,
                g.decay,
            )?,
            higher_better: true,
        }),
        None => None,
    };

    Ok(fuse(vector, keyword, graph, spec.weights, spec.k))
}

/// One channel's raw ranked hits. `higher_better` flags the score direction:
/// BM25 and proximity are higher-is-better; vector *distance* is lower-is-better.
pub(crate) struct Channel {
    pub hits: Vec<(NodeId, f32)>,
    pub higher_better: bool,
}

impl Channel {
    /// Normalize this channel's scores to `[0, 1]` with best = 1. When every
    /// score is equal (or there is a single hit) they are all equally best → 1.
    fn normalized(&self) -> HashMap<NodeId, f32> {
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for &(_, s) in &self.hits {
            lo = lo.min(s);
            hi = hi.max(s);
        }
        let span = hi - lo;
        self.hits
            .iter()
            .map(|&(n, s)| {
                let norm = if span <= f32::EPSILON {
                    1.0
                } else if self.higher_better {
                    (s - lo) / span
                } else {
                    (hi - s) / span
                };
                (n, norm)
            })
            .collect()
    }

    fn raw(&self) -> HashMap<NodeId, f32> {
        self.hits.iter().copied().collect()
    }
}

/// Fuse the (optional) channels into a single ranking, highest fused score
/// first (ties by ascending node id), truncated to `k`.
pub(crate) fn fuse(
    vector: Option<Channel>,
    keyword: Option<Channel>,
    graph: Option<Channel>,
    weights: HybridWeights,
    k: usize,
) -> Vec<HybridHit> {
    let vn = vector.as_ref().map(Channel::normalized);
    let kn = keyword.as_ref().map(Channel::normalized);
    let gn = graph.as_ref().map(Channel::normalized);
    let vraw = vector.as_ref().map(Channel::raw);
    let kraw = keyword.as_ref().map(Channel::raw);
    let graw = graph.as_ref().map(Channel::raw);

    // Union of every node any channel surfaced.
    let mut nodes: BTreeSet<NodeId> = BTreeSet::new();
    for norm in [&vn, &kn, &gn].into_iter().flatten() {
        nodes.extend(norm.keys().copied());
    }

    let contrib = |norm: &Option<HashMap<NodeId, f32>>, w: f32, node: NodeId| -> f32 {
        norm.as_ref()
            .and_then(|m| m.get(&node))
            .copied()
            .unwrap_or(0.0)
            * w
    };
    let raw = |m: &Option<HashMap<NodeId, f32>>, node: NodeId| -> Option<f32> {
        m.as_ref().and_then(|m| m.get(&node)).copied()
    };

    let mut hits: Vec<HybridHit> = nodes
        .into_iter()
        .map(|node| HybridHit {
            node,
            score: contrib(&vn, weights.vector, node)
                + contrib(&kn, weights.keyword, node)
                + contrib(&gn, weights.graph, node),
            vector: raw(&vraw, node),
            keyword: raw(&kraw, node),
            graph: raw(&graw, node),
        })
        .collect();

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.node.0.cmp(&b.node.0))
    });
    hits.truncate(k);
    hits
}

/// Seed nodes for the graph channel: the top `n` hits from each of the vector
/// and keyword channels (each already sorted best-first), deduplicated.
pub(crate) fn top_seeds(
    vector: &Option<Channel>,
    keyword: &Option<Channel>,
    n: usize,
) -> Vec<NodeId> {
    let mut seeds: BTreeSet<NodeId> = BTreeSet::new();
    for ch in [vector, keyword].into_iter().flatten() {
        for &(node, _) in ch.hits.iter().take(n) {
            seeds.insert(node);
        }
    }
    seeds.into_iter().collect()
}

/// Multi-source BFS proximity: from `seeds` (distance 0), expand up to `hops`
/// over undirected neighbours, scoring each reached node `decay^distance`.
/// A seed scores 1.0; its 1-hop neighbours `decay`; and so on.
pub(crate) fn graph_proximity<R: GraphReader + ?Sized>(
    reader: &R,
    seeds: &[NodeId],
    hops: u32,
    decay: f32,
) -> Result<Vec<(NodeId, f32)>> {
    let mut dist: HashMap<NodeId, u32> = HashMap::new();
    for &s in seeds {
        dist.entry(s).or_insert(0);
    }
    let mut frontier: Vec<NodeId> = dist.keys().copied().collect();
    for d in 1..=hops {
        let mut next = Vec::new();
        for &node in &frontier {
            for nb in reader.neighbors(node, Dir::Both, None)?.iter() {
                if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(nb.node) {
                    e.insert(d);
                    next.push(nb.node);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(dist
        .into_iter()
        .map(|(node, d)| (node, decay.powi(d as i32)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(hits: &[(u64, f32)], higher_better: bool) -> Channel {
        Channel {
            hits: hits.iter().map(|&(n, s)| (NodeId(n), s)).collect(),
            higher_better,
        }
    }

    #[test]
    fn normalization_flips_vector_distance() {
        // Distances: node 1 closest (0.1), node 2 farthest (0.9) → 1 normalizes
        // to 1.0, node 2 to 0.0.
        let c = ch(&[(1, 0.1), (2, 0.9)], false);
        let n = c.normalized();
        assert!((n[&NodeId(1)] - 1.0).abs() < 1e-6);
        assert!((n[&NodeId(2)] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fusion_sums_weighted_normalized_channels() {
        // vector: node 1 best; keyword: node 2 best. Equal weights → both ~1.0,
        // but node 1 also appears weakly in keyword, tipping it ahead.
        let vector = ch(&[(1, 0.0), (2, 1.0)], false); // 1→1.0, 2→0.0
        let keyword = ch(&[(2, 10.0), (1, 5.0)], true); // 2→1.0, 1→0.0
        let fused = fuse(
            Some(vector),
            Some(keyword),
            None,
            HybridWeights::default(),
            10,
        );
        // node 1: 1.0*1 + 0.0*1 = 1.0 ; node 2: 0.0 + 1.0 = 1.0 → tie, id order.
        assert_eq!(fused[0].node, NodeId(1));
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].vector, Some(0.0)); // raw distance carried through
        assert_eq!(fused[0].keyword, Some(5.0));
    }

    #[test]
    fn missing_channel_contributes_zero() {
        let keyword = ch(&[(1, 3.0), (2, 1.0)], true);
        let fused = fuse(None, Some(keyword), None, HybridWeights::default(), 10);
        assert_eq!(fused[0].node, NodeId(1));
        assert_eq!(fused[0].vector, None);
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn top_seeds_unions_both_channels() {
        let vector = ch(&[(1, 0.0), (2, 0.5)], false);
        let keyword = ch(&[(3, 9.0), (1, 2.0)], true);
        let seeds = top_seeds(&Some(vector), &Some(keyword), 1);
        // top-1 of each: vector→1, keyword→3.
        assert_eq!(seeds, vec![NodeId(1), NodeId(3)]);
    }
}
