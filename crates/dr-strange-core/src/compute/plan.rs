//! Logical plan algebra (arch/03 §2), v0.
//!
//! A plan is a **source** (where rows come from) followed by a linear chain
//! of **steps** (how each row is transformed). This is the linear-pipeline
//! row model (arch/03 §2): every step operates on the row's current node.
//!
//! The whole plan is serializable — the v1 builder API constructs it, the v2
//! query language will parse into it, and it rides over the wire unchanged
//! (arch/00 §2). The builder mirrors these operators one-to-one; there is no
//! separate builder semantics.
//!
//! Not yet here (M3): the hybrid `VectorTopK`/`FrontierTopK`/`ExpandBeam`
//! operators and `Project`/score-channel machinery.

use serde::{Deserialize, Serialize};

use crate::compute::expr::Expr;
use crate::storage::vector::Metric;
use crate::types::{Dir, NodeId};

/// Where a plan's rows originate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// Every node in the plane.
    ScanAll,
    /// Every node carrying a label (via `label_idx`).
    ScanLabel(String),
    /// Specific node ids (non-existent ids are dropped at execution).
    SeekIds(Vec<NodeId>),
    /// Nodes resolved from external keys (unresolved keys are dropped).
    SeekKeys(Vec<String>),
    /// Global similarity search: the `k` nodes (optionally restricted to
    /// `label`) whose `property` vector is closest to `query` under `metric`.
    /// Seed rows carry their similarity in the score channel (arch/03 §4.1).
    VectorTopK {
        label: Option<String>,
        property: String,
        query: Vec<f32>,
        metric: Metric,
        k: u64,
    },
}

/// One pipeline stage, applied to the stream of rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Step {
    /// Replace each row's current node with each 1-hop neighbor (one row per
    /// neighbor); records the traversed edge in the row's trail.
    Expand { dir: Dir, edge_type: Option<String> },
    /// Variable-length expansion: emit a row for every walk of length in
    /// `min..=max` hops from the current node (walk semantics — see
    /// [`crate::compute::exec`]).
    ExpandVar {
        dir: Dir,
        edge_type: Option<String>,
        min: u32,
        max: u32,
    },
    /// Keep only rows whose current node satisfies the predicate.
    Filter(Expr),
    /// Drop the first `n` rows.
    Skip(u64),
    /// Stop after `n` rows.
    Limit(u64),
    /// Deduplicate by current node id.
    Distinct,
    /// Reorder rows by one or more keys (a pipeline barrier — materializes).
    Sort(Vec<SortKey>),
    /// Graph-constrained vector search (arch/03 §4.3): rank the current
    /// frontier by similarity of `property` to `query` and keep the top `k`,
    /// setting each kept row's score. The "no client-side join" headline.
    /// A barrier — needs the whole frontier.
    FrontierTopK {
        property: String,
        query: Vec<f32>,
        metric: Metric,
        k: u64,
    },
    /// Similarity-guided beam traversal (arch/03 §4.4): at each of `depth`
    /// steps, expand the frontier, score neighbors' `property` against
    /// `query`, keep the best `width`. Emits the kept beam at every level;
    /// each row's score is its similarity. Walk semantics (may revisit).
    ExpandBeam {
        dir: Dir,
        edge_type: Option<String>,
        property: String,
        query: Vec<f32>,
        metric: Metric,
        width: u32,
        depth: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    pub expr: Expr,
    pub descending: bool,
}

/// A complete query: a source and its pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogicalPlan {
    pub source: Source,
    pub steps: Vec<Step>,
}

impl LogicalPlan {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            steps: Vec::new(),
        }
    }

    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::expr::{has_label, p};

    #[test]
    fn plan_serde_roundtrip() {
        use crate::storage::vector::Metric;
        let plan = LogicalPlan {
            source: Source::VectorTopK {
                label: Some("Paper".into()),
                property: "embedding".into(),
                query: vec![0.1, 0.2, 0.3],
                metric: Metric::Cosine,
                k: 25,
            },
            steps: vec![
                Step::Expand {
                    dir: Dir::Out,
                    edge_type: Some("CITES".into()),
                },
                Step::ExpandVar {
                    dir: Dir::Both,
                    edge_type: None,
                    min: 1,
                    max: 2,
                },
                Step::FrontierTopK {
                    property: "embedding".into(),
                    query: vec![0.4, 0.5, 0.6],
                    metric: Metric::Dot,
                    k: 10,
                },
                Step::ExpandBeam {
                    dir: Dir::Out,
                    edge_type: Some("CITES".into()),
                    property: "embedding".into(),
                    query: vec![0.7, 0.8, 0.9],
                    metric: Metric::L2,
                    width: 4,
                    depth: 3,
                },
                Step::Filter(p("year").ge(2020).and(has_label("Paper"))),
                Step::Distinct,
                Step::Sort(vec![SortKey {
                    expr: p("year"),
                    descending: true,
                }]),
                Step::Skip(5),
                Step::Limit(10),
            ],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: LogicalPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn seek_sources_roundtrip() {
        for source in [
            Source::ScanAll,
            Source::SeekIds(vec![NodeId(1), NodeId(2)]),
            Source::SeekKeys(vec!["a".into(), "b".into()]),
        ] {
            let plan = LogicalPlan::new(source);
            let json = serde_json::to_string(&plan).unwrap();
            assert_eq!(plan, serde_json::from_str::<LogicalPlan>(&json).unwrap());
        }
    }
}
