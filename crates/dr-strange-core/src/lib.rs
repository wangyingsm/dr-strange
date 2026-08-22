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
pub use api::cache;
#[cfg(feature = "json")]
pub mod compact;
pub mod compute;
pub mod error;
pub mod index;
#[cfg(feature = "json")]
pub mod json;
pub mod keyword;
pub mod storage;
pub mod text;
pub mod types;

/// Time-travel address (ROADMAP §4) — only the native LSM backend keeps the
/// prior versions AS OF needs, so the type ships only with `native-backend`.
#[cfg(feature = "native-backend")]
pub use api::AsOf;
pub use api::{
    AlgoBuilder, Change, ChangeKind, ChangeOp, ChangeSet, Database, HybridBuilder, PlaneHandle,
    QueryBuilder, SnapshotStats, WriteTxn,
};
pub use compute::{
    Algo, CatalogSnapshot, Connection, EdgeTypeStats, Expr, GraphChannel, HybridHit, HybridSpec,
    HybridWeights, KeywordChannel, LabelStats, LogicalPlan, LouvainOptions, NodeRef,
    PageRankOptions, Path, PropStats, Row, ShortestPathOptions, SortKey, Source, Step, ValueType,
    VectorChannel, distance, external_key, has_label, hops, lit, p, score, similarity,
};
pub use error::{Error, Result};
pub use storage::graph::{BulkEdge, BulkEdgeById, BulkNode, BulkStats};
pub use storage::vector::Metric;
/// Replication wire types (`serve --follow`, arch/01 §9) — ship regardless of
/// backend feature; only [`Database::apply_replicated`]/[`Database::on_wal_commit`]
/// (native-only) actually produce or consume them.
pub use storage::{ReplicatedBatch, ReplicatedOp};
pub use text::{Analyzer, Language};
pub use types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
};
