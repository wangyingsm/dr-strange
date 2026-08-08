//! Storage layer (arch/01): graph encoding over a swappable KV backend.
//!
//! - [`codec`] — the graph encoding and decoding layer
//! - [`engine`] — the `StorageEngine` / transaction traits (the swap seam)
//! - [`graph`] — the graph data structure and its operations
//! - [`hnsw`] — the HNSW vector index implementation
//! - [`keys`] — the keys in graph storage implementation
//! - [`memory`] — in-memory backend for tests and property-based testing
//! - [`native`] — native backend of the graph storage implementation
//! - [`vector`] — the `VectorIndex` trait
//!

pub mod codec;
pub mod engine;
pub mod graph;
pub mod hnsw;
pub mod keys;
pub mod memory;
#[cfg(feature = "native-backend")]
pub mod native;
pub mod vector;

/// Since v0.2.0, the `redb` backend is obsolete replaced by `native` backend
/// as default.
#[cfg(feature = "redb-backend")]
pub mod redb_backend;

#[cfg(test)]
mod conformance_tests;

pub use engine::{ReadTransaction, StorageEngine, TableId, WriteTransaction};
pub use vector::VectorIndex;
