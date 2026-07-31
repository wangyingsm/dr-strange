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
#[cfg(feature = "json")]
pub mod json;
pub mod keyword;
pub mod storage;
pub mod text;
pub mod types;

pub use api::{AlgoBuilder, Database, HybridBuilder, PlaneHandle, QueryBuilder, WriteTxn};
pub use compute::{
    CatalogSnapshot, Connection, EdgeTypeStats, Expr, HybridHit, HybridWeights, LabelStats,
    LogicalPlan, LouvainOptions, PageRankOptions, Path, PropStats, Row, ShortestPathOptions,
    SortKey, Source, Step, ValueType, distance, has_label, hops, lit, p, score, similarity,
};
pub use error::{Error, Result};
pub use text::{Analyzer, Language};
pub use storage::graph::{BulkEdge, BulkEdgeById, BulkNode, BulkStats};
pub use storage::vector::Metric;
pub use types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
};
