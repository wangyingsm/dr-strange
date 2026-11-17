//! Spans in, service dependencies out.
//!
//! A trace export is the first ops input that is *observed* rather than
//! declared (`arch/10-ops.md` §8), and the only one that does not come from a
//! file in the tree: a collector writes it, or a query answers with it. What
//! arrives either way is spans, and a span is a fact about one request — so it
//! is an input here, never a node. The graph records the dependency the spans
//! witness, and says how many spans it read to find it.

pub mod otlp;

/// One span, as a trace export states it.
///
/// Only what a dependency edge needs. A span's name, kind and duration are
/// read and dropped: naming the operation would make the edge per-endpoint,
/// and latency is a measurement this does not claim to make from a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The trace it belongs to, hex, lowercased.
    pub trace: String,
    /// Its own id, hex, lowercased.
    pub id: String,
    /// The span that called it, where the export names one.
    pub parent: Option<String>,
    /// The `service.name` of the resource that emitted it.
    pub service: String,
    /// When it started, unix-epoch milliseconds.
    pub start_ms: i64,
    /// Whether its status is OTLP's `ERROR`.
    pub error: bool,
}
