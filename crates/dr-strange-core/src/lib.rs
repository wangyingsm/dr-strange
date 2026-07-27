//! dr-strange-core — AI-native embedded graph database.
//!
//! Layer map (see `arch/` for the design docs):
//! - [`storage`] — graph encoding over the `StorageEngine` trait (arch/01)
//! - [`cache`]   — decoded-object cache between storage and computation (arch/02)
//! - [`compute`] — logical plans, executor, hybrid search, catalog (arch/03)
//! - [`api`]     — public surface: `Database`, `PlaneHandle`, query builder (arch/04)
//!
//! Planes (arch/09) are a data-model primitive and appear at every layer.

pub mod api;
pub mod cache;
pub mod compute;
pub mod error;
pub mod index;
pub mod storage;
pub mod types;

pub use api::{Database, PlaneHandle, QueryBuilder, WriteTxn};
pub use compute::{
    CatalogSnapshot, Connection, EdgeTypeStats, Expr, LabelStats, LogicalPlan, PropStats, Row,
    SortKey, Source, Step, ValueType, distance, has_label, hops, lit, p, score, similarity,
};
pub use error::{Error, Result};
pub use storage::vector::Metric;
pub use types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
};
