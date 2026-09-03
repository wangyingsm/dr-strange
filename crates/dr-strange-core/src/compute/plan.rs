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
//! A plan may end in a **projection** ([`Projection`]) — arch/03 §2's
//! `Project`, modelled as a tail rather than a step because it turns node
//! rows into value rows and so nothing can follow it.

use serde::{Deserialize, Serialize};

use crate::compute::expr::{BindingNeed, Expr};
use crate::compute::hybrid::HybridSpec;
use crate::storage::vector::Metric;
use crate::types::{Dir, NodeId};

/// Where a plan's rows originate.
///
/// `#[non_exhaustive]`: the engine grows new ways to seed a query (keyword,
/// hybrid, algorithms — ROADMAP §7), so a downstream match must carry a
/// wildcard arm and each addition stays a minor release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// BM25 keyword search (ROADMAP §2): the `k` nodes whose `property` best
    /// matches the text `query`, most-relevant first, each seeded with its BM25
    /// score. `label` is required — the inverted index is keyed on the pair.
    /// Empty when no keyword index is declared on `(label, property)`.
    KeywordTopK {
        label: String,
        property: String,
        query: String,
        k: u64,
    },
    /// Fused vector + keyword + graph-proximity retrieval (ROADMAP §2). Seed
    /// rows carry the fused score; the same engine as `plane.hybrid()`.
    Hybrid(Box<HybridSpec>),
    /// A graph algorithm as a source (ROADMAP §1): its nodes become the rows,
    /// with the per-node result in the score channel. `label` scopes the
    /// algorithm's universe to nodes carrying it (whole plane when `None`).
    Algo { label: Option<String>, algo: Algo },
}

/// Which algorithm a [`Source::Algo`] runs, with its tuning. The score each
/// one puts on a row differs — see [`crate::compute::exec`] for the mapping.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Algo {
    /// Rows ordered by rank, highest first; score = the rank.
    PageRank {
        damping: f64,
        max_iters: u32,
        tolerance: f64,
    },
    /// Weakly connected components. Rows grouped by component; score = a dense
    /// 0-based component index (the representative id itself doesn't survive
    /// an `f32`, and a compact index is what a caller can group on).
    ConnectedComponents,
    /// Louvain communities, scored like [`Algo::ConnectedComponents`].
    Louvain { max_levels: u32, min_gain: f64 },
    /// The shortest path between two nodes. Rows are the path's nodes in
    /// order; score = the node's 0-based position along it. Empty when either
    /// endpoint is unknown or the target is unreachable.
    ShortestPath {
        from: NodeRef,
        to: NodeRef,
        dir: Dir,
        /// Edge property holding a non-negative weight; `None` ⇒ unit weights.
        weight: Option<String>,
    },
}

/// How a plan names a specific node: by internal id, or by the external key
/// it was created with (which a caller — or an LLM — actually knows).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeRef {
    Id(NodeId),
    Key(String),
}

/// One pipeline stage, applied to the stream of rows.
///
/// `#[non_exhaustive]` for the same reason as [`Source`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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

/// One projected column: a name and what it computes.
///
/// The name is the table header: the query's alias, or the item's source text
/// (`n.name`, `count(*)`) when it wrote none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjItem {
    pub name: String,
    pub expr: ProjExpr,
}

impl ProjItem {
    /// A column reading one value per row — `RETURN n.year`.
    pub fn value(name: impl Into<String>, expr: Expr) -> Self {
        Self {
            name: name.into(),
            expr: ProjExpr::Value(expr),
        }
    }

    /// A column folding a group of rows into one value — `RETURN count(*)`.
    pub fn agg(name: impl Into<String>, agg: Agg) -> Self {
        Self {
            name: name.into(),
            expr: ProjExpr::Agg(agg),
        }
    }
}

/// What a projected column computes: a value per row, or a fold over a group
/// of rows.
///
/// Two cases rather than an [`Expr`] variant: an aggregate is a fold over
/// rows, not a value a row has, and only a projection can evaluate one.
/// Keeping it out of `Expr` makes `WHERE count(*) > 3` unwritable rather than
/// silently `Null`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProjExpr {
    /// One value per row, and a grouping key once any other column
    /// aggregates — Cypher's implicit `GROUP BY`.
    Value(Expr),
    Agg(Agg),
}

/// An aggregate over the rows of one group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Agg {
    pub func: AggFunc,
    /// The expression folded, evaluated per row. `None` is `count(*)`, which
    /// counts rows and so needs none.
    pub arg: Option<Expr>,
    /// Fold each distinct value once — `count(DISTINCT n.file)`, by the total
    /// order [`Projection::distinct`] uses.
    pub distinct: bool,
}

impl Agg {
    /// `count(*)` — rows in the group, including rows whose every value is
    /// null.
    pub fn count() -> Self {
        Self {
            func: AggFunc::Count,
            arg: None,
            distinct: false,
        }
    }

    /// `func(expr)` — folded over the group's non-null values of `expr`.
    pub fn of(func: AggFunc, expr: Expr) -> Self {
        Self {
            func,
            arg: Some(expr),
            distinct: false,
        }
    }

    /// Fold each distinct value once. No effect on `count(*)`, which reads
    /// no value.
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }
}

/// Which fold an [`Agg`] performs.
///
/// All are total: a null, or a value the fold cannot use (a string under
/// `sum`), is skipped rather than failing the query. They differ in what they
/// return for a group that gave them nothing — see [`crate::compute::exec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggFunc {
    /// How many: rows for `count(*)`, non-null values otherwise. `0` over
    /// nothing.
    Count,
    /// Numeric total; `0` over nothing. Stays `Int` while every value is one.
    Sum,
    /// Numeric mean as a `Float`; `Null` over nothing, since the mean of no
    /// values is unanswerable rather than zero.
    Avg,
    /// Least value under the total order; `Null` over nothing.
    Min,
    /// Greatest value under the total order; `Null` over nothing.
    Max,
    /// Every non-null value as a `List`, in row order.
    Collect,
}

/// Ordering over projected tuples, by **column index** rather than
/// expression: after a projection a column is all there is to address.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TupleSortKey {
    pub column: usize,
    pub descending: bool,
}

/// The terminal that turns node rows into value rows (arch/03 §2's `Project`).
///
/// A **tail**, not a [`Step`]: nothing downstream of a projection can expand
/// or filter a node, so as a field "the projection is last" is
/// unrepresentable-otherwise rather than a rule the executor enforces.
///
/// `order_by`/`skip`/`limit` are here for the same reason — over tuples they
/// mean something different from the steps of the same name over node rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    /// Output columns, in order.
    pub items: Vec<ProjItem>,
    /// Deduplicate whole projected tuples (`RETURN DISTINCT n.file`), which is
    /// a different question from [`Step::Distinct`]'s "by node id".
    pub distinct: bool,
    pub order_by: Vec<TupleSortKey>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// A complete query: a source, its pipeline, and an optional projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogicalPlan {
    pub source: Source,
    pub steps: Vec<Step>,
    /// `None` — the default, and what every plan written before this existed
    /// deserializes to — means the query returns node records, exactly as it
    /// always has. `Some` means it returns a table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Projection>,
}

impl LogicalPlan {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            steps: Vec::new(),
            project: None,
        }
    }

    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
    }

    /// Which of a row's bindings this plan's expressions name (`Expr::At` and
    /// the edge terms).
    ///
    /// Computed once per execution so the executor resolves only what is
    /// asked for: a plan that never reaches past its current node reports
    /// nothing to resolve, and rows cost exactly what they cost today.
    pub fn binding_need(&self) -> BindingNeed {
        let mut need = BindingNeed::default();
        for step in &self.steps {
            match step {
                Step::Filter(e) => need.add(e),
                Step::Sort(keys) => keys.iter().for_each(|k| need.add(&k.expr)),
                Step::Expand { .. }
                | Step::ExpandVar { .. }
                | Step::Skip(_)
                | Step::Limit(_)
                | Step::Distinct
                | Step::FrontierTopK { .. }
                | Step::ExpandBeam { .. } => {}
            }
        }
        for item in self.project.iter().flat_map(|p| &p.items) {
            match &item.expr {
                ProjExpr::Value(e) => need.add(e),
                ProjExpr::Agg(agg) => agg.arg.iter().for_each(|e| need.add(e)),
            }
        }
        need
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
            project: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: LogicalPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn projection_tail_roundtrips_and_stays_off_the_wire_when_absent() {
        let mut plan = LogicalPlan::new(Source::ScanLabel("Paper".into()));
        // A plan that doesn't project doesn't mention a projection, so what
        // rides over the wire is byte-for-byte the plan it always was.
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("project"), "{json}");
        assert_eq!(plan, serde_json::from_str::<LogicalPlan>(&json).unwrap());

        plan.project = Some(Projection {
            items: vec![
                ProjItem::value("year", p("year")),
                ProjItem::agg("papers", Agg::count()),
                ProjItem::agg("files", Agg::of(AggFunc::Collect, p("file")).distinct()),
            ],
            distinct: true,
            order_by: vec![TupleSortKey {
                column: 0,
                descending: true,
            }],
            skip: Some(2),
            limit: Some(5),
        });
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(plan, serde_json::from_str::<LogicalPlan>(&json).unwrap());
    }

    #[test]
    fn retrieval_and_algo_sources_roundtrip() {
        use crate::compute::hybrid::{
            GraphChannel, HybridSpec, HybridWeights, KeywordChannel, VectorChannel,
        };
        use crate::storage::vector::Metric;

        for source in [
            Source::KeywordTopK {
                label: "Doc".into(),
                property: "body".into(),
                query: "graph databases".into(),
                k: 10,
            },
            Source::Hybrid(Box::new(HybridSpec {
                label: Some("Doc".into()),
                vector: Some(VectorChannel {
                    property: "embedding".into(),
                    query: vec![0.1, 0.2],
                    metric: Metric::Cosine,
                }),
                keyword: Some(KeywordChannel {
                    property: "body".into(),
                    query: "graph".into(),
                }),
                graph: Some(GraphChannel {
                    hops: 2,
                    decay: 0.5,
                    seeds: 10,
                }),
                weights: HybridWeights::default(),
                candidates: 100,
                k: 10,
            })),
            Source::Algo {
                label: Some("Paper".into()),
                algo: Algo::PageRank {
                    damping: 0.85,
                    max_iters: 20,
                    tolerance: 1e-6,
                },
            },
            Source::Algo {
                label: None,
                algo: Algo::ConnectedComponents,
            },
            Source::Algo {
                label: None,
                algo: Algo::Louvain {
                    max_levels: 10,
                    min_gain: 1e-9,
                },
            },
            Source::Algo {
                label: None,
                algo: Algo::ShortestPath {
                    from: NodeRef::Key("ada".into()),
                    to: NodeRef::Id(NodeId(7)),
                    dir: Dir::Out,
                    weight: Some("cost".into()),
                },
            },
        ] {
            let plan = LogicalPlan::new(source);
            let json = serde_json::to_string(&plan).unwrap();
            assert_eq!(plan, serde_json::from_str::<LogicalPlan>(&json).unwrap());
        }
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
