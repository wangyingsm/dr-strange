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
pub mod storage;
pub mod types;

pub use api::{Database, PlaneHandle, WriteTxn};
pub use error::{Error, Result};
pub use types::{
    Dir, EdgeId, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties,
};
