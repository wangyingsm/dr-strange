//! Computation layer (arch/03): logical plans, the pull-based executor,
//! native hybrid graph+vector search, and the soft-schema catalog.
//!
//! Module plan (lands at M2/M3):
//! - `plan`     — operator algebra (`ScanLabel`, `Expand*`, `VectorTopK`,
//!   `FrontierTopK`, `ExpandBeam`, `Filter`, `Score`, ...), serializable
//! - `expr`     — the serializable `Expr` enum used by Filter/Project/Score
//! - `exec`     — iterator executor over a `GraphReader` snapshot
//! - `catalog`  — per-plane soft schema, `PropDesc` description aggregation
//!
//! Design commitment: hybrid search is an executor capability — one plan,
//! one snapshot — never API-level aggregation (arch/03 §4).
