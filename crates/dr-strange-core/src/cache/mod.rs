//! Cache layer (arch/02): decoded records, adjacency segments, and
//! dictionaries between storage and computation. The executor reads only
//! through [`GraphReader`]; `CachedReader` (M2) and an uncached pass-through
//! (M0) are its implementations.
//!
//! TODO(M0): `UncachedReader` over a storage read transaction.
//! TODO(M2): `GraphCache` (moka W-TinyLFU) + `CachedReader` with
//! commit-sequence version stamping (arch/02 §3).

/// Monotonic commit sequence number — the version-stamping and invalidation
/// token for cache entries (arch/02 §3), also the web UI's change token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommitSeq(pub u64);
