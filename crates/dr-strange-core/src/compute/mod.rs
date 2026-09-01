//! Computation layer (arch/03): logical plans, the pull-based executor,
//! native hybrid graph+vector search, and the soft-schema catalog.
//!
//! Modules:
//! - [`expr`] — serializable `Expr` used by Filter/Sort (M2)
//! - [`plan`] — logical operator algebra, serializable (M2)
//! - [`exec`] — pull-based iterator executor over a `GraphReader` (M2)
//!
//! Later (M3): `VectorTopK`/`FrontierTopK`/`ExpandBeam` hybrid operators and
//! a `catalog` module (per-plane soft schema, `PropDesc` aggregation).
//!
//! Design commitment: hybrid search is an executor capability — one plan,
//! one snapshot — never API-level aggregation (arch/03 §4).

pub mod algo;
pub mod catalog;
pub mod exec;
pub mod expr;
pub mod hybrid;
pub mod plan;

pub use algo::{LouvainOptions, PageRankOptions, Path, ShortestPathOptions};
pub use catalog::{CatalogSnapshot, Connection, EdgeTypeStats, LabelStats, PropStats, ValueType};
pub use exec::Row;
pub use expr::{
    Binding, BindingNeed, Expr, at_edge, at_node, distance, edge_dir, edge_type, ep, external_key,
    has_label, hops, lit, node_id, p, score, similarity,
};
pub use hybrid::{
    GraphChannel, HybridHit, HybridSpec, HybridWeights, KeywordChannel, VectorChannel,
};
pub use plan::{Algo, LogicalPlan, NodeRef, SortKey, Source, Step};
