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

pub mod exec;
pub mod expr;
pub mod plan;

pub use exec::Row;
pub use expr::{Expr, has_label, lit, p};
pub use plan::{LogicalPlan, SortKey, Source, Step};
