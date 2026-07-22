//! Storage layer (arch/01): graph encoding over a swappable KV backend.
//!
//! - [`engine`] — the `StorageEngine` / transaction traits (the swap seam)
//! - [`memory`] — in-memory backend for tests and property-based testing
//! - [`vector`] — the `VectorIndex` trait (HNSW sidecar lands at M3)
//!
//! Planned for M0/M1 (not yet present): `redb` backend, key encoding
//! (plane-prefixed tables), property codec, graph-level record store.

pub mod codec;
pub mod engine;
pub mod graph;
pub mod keys;
pub mod memory;
pub mod redb_backend;
pub mod vector;

#[cfg(test)]
mod conformance_tests;

pub use engine::{ReadTransaction, StorageEngine, TableId, WriteTransaction};
pub use vector::VectorIndex;
