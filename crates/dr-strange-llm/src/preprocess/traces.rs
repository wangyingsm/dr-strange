//! Spans in, service dependencies out.
//!
//! A trace export is the first ops input that is *observed* rather than
//! declared (`arch/10-ops.md` §8), and the only one that does not come from a
//! file in the tree: a collector writes it, or a query answers with it. What
//! arrives either way is spans, and a span is a fact about one request — so it
//! is an input here, never a node. The graph records the dependency the spans
//! witness, and says how many spans it read to find it.

pub mod otlp;

use std::collections::{BTreeMap, BTreeSet};

use dr_strange_core::{
    PlaneHandle, PropDesc, PropValue, Properties, Result, time::epoch_ms_to_rfc3339,
};

use crate::digest::{DigestEdge, DigestNode};

/// The stamp a fact folded from traces carries, in place of a plugin's.
const GENERATED_BY: &str = "otel";

/// The label a service takes, declared or observed (`arch/10-ops.md` §3).
const SERVICE: &str = "Service";

/// The edge a span and its parent witness between two services.
const CALLS: &str = "CALLS";

/// What a service no file declares is keyed under, so that a name the
/// topology does not hold cannot collide with one it does.
const OBSERVED: &str = "observed/";

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

/// What the declared topology answers to a service name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// Exactly one node answers to it; its key is the one to draw on.
    One(String),
    /// Several do. Choosing between them would be a guess, so the service
    /// stays observed and records the candidates — the arm `SCRAPES` takes
    /// for the same reason (`arch/10-ops.md` §6).
    Many(Vec<String>),
}

/// What the fold made of a batch of spans.
#[derive(Debug, Default)]
pub struct Folded {
    /// One node per service nothing declares. A declared service already has
    /// its node, and is not rewritten here: its props belong to the reader
    /// that read the file.
    pub nodes: Vec<DigestNode>,
    /// `CALLS`, one per distinct caller-callee pair.
    pub edges: Vec<DigestEdge>,
    /// What was seen, and what could not be linked.
    pub report: Report,
}

/// What the fold saw.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Spans folded.
    pub spans: usize,
    /// Services they came from.
    pub services: usize,
    /// Services joined to the one node that answers to the name.
    pub joined: usize,
    /// Services several declared nodes answer to, left observed.
    pub ambiguous: usize,
    /// Spans whose parent is in none of the documents read. Sampling and
    /// per-batch export both cause it, so the edges are a lower bound by
    /// exactly this many spans.
    pub orphans: usize,
    /// Spans whose parent is in the same service — a call inside one service,
    /// which is not an edge between two.
    pub internal: usize,
}

impl Report {
    /// Whether anything happened worth saying.
    pub fn any(&self) -> bool {
        self.spans > 0
    }
}

/// What one pair of services accumulated across the spans that witnessed it.
#[derive(Debug, Default)]
struct Pair {
    calls: usize,
    errors: usize,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
}

impl Pair {
    /// Count one span against the pair its parent put it in.
    fn saw(&mut self, span: &Span) {
        self.calls += 1;
        self.errors += usize::from(span.error);
        if span.start_ms > 0 {
            self.first_ms = Some(self.first_ms.unwrap_or(span.start_ms).min(span.start_ms));
            self.last_ms = Some(self.last_ms.unwrap_or(span.start_ms).max(span.start_ms));
        }
    }
}

/// What the ops plane declares, by the name a service answers to — the map
/// [`fold`] needs to name a traced service as the topology names it.
///
/// `None` before the plane exists, which is every tree whose manifests have
/// not been read: everything then stays observed, and says so.
///
/// The nodes a previous fold minted are left out. A service matching the
/// `observed/` node its own last import wrote is not one the topology
/// declares, and counting it as joined would have the report claim this tree
/// declares what it does not.
pub fn declared(plane: Option<&PlaneHandle<'_>>) -> Result<BTreeMap<String, Named>> {
    let mut out = BTreeMap::new();
    for (name, keys) in super::scrape::addressable(&[], plane)? {
        let keys: Vec<String> = keys
            .into_iter()
            .filter(|key| !key.starts_with(OBSERVED))
            .collect();
        match keys.as_slice() {
            [] => {}
            [one] => {
                out.insert(name, Named::One(one.clone()));
            }
            _ => {
                out.insert(name, Named::Many(keys));
            }
        }
    }
    Ok(out)
}

/// Fold spans into the service dependencies they witness.
///
/// `declared` is what the ops plane already holds, by the name a service
/// answers to — empty before any manifest has been read, and the reason a
/// traced service can be named as the topology names it rather than twice.
pub fn fold(spans: &[Span], declared: &BTreeMap<String, Named>) -> Folded {
    let by_id: BTreeMap<&str, &Span> = spans.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out = Folded::default();
    out.report.spans = spans.len();

    let mut seen: BTreeMap<&str, Pair> = BTreeMap::new();
    let mut pairs: BTreeMap<(String, String), Pair> = BTreeMap::new();
    for span in spans {
        seen.entry(span.service.as_str()).or_default().saw(span);
        let Some(parent) = span.parent.as_deref() else {
            continue;
        };
        let Some(caller) = by_id.get(parent) else {
            out.report.orphans += 1;
            continue;
        };
        if caller.service == span.service {
            out.report.internal += 1;
            continue;
        }
        let src = key_of(&caller.service, declared);
        let dst = key_of(&span.service, declared);
        if src.0 == dst.0 {
            out.report.internal += 1;
            continue;
        }
        pairs.entry((src.0, dst.0)).or_default().saw(span);
    }

    out.report.services = seen.len();
    let mut minted = BTreeSet::new();
    for (service, totals) in seen {
        let (key, candidates) = key_of(service, declared);
        match &candidates {
            Some(_) => out.report.ambiguous += 1,
            None if !key.starts_with(OBSERVED) => out.report.joined += 1,
            None => {}
        }
        if key.starts_with(OBSERVED) && minted.insert(key.clone()) {
            out.nodes
                .push(observed_service(&key, service, &totals, candidates));
        }
    }
    for ((src, dst), totals) in pairs {
        out.edges.push(DigestEdge {
            src,
            dst,
            ty: CALLS.into(),
            props: call_props(&totals),
        });
    }
    out
}

/// The key a service name takes, and the candidates where the name is not
/// enough to choose one.
fn key_of(service: &str, declared: &BTreeMap<String, Named>) -> (String, Option<Vec<String>>) {
    match declared.get(service) {
        Some(Named::One(key)) => (key.clone(), None),
        Some(Named::Many(keys)) => (format!("{OBSERVED}{service}"), Some(keys.clone())),
        None => (format!("{OBSERVED}{service}"), None),
    }
}

/// A node for a service the traces show and no file declares.
fn observed_service(
    key: &str,
    service: &str,
    totals: &Pair,
    candidates: Option<Vec<String>>,
) -> DigestNode {
    let mut props = stamped();
    props.insert(
        "name".into(),
        PropDesc::new(PropValue::Str(service.to_string())),
    );
    props.insert(
        "origin".into(),
        PropDesc::described(
            "seen in a trace; no file in this tree declares it",
            PropValue::Str("observed".into()),
        ),
    );
    props.insert(
        "spans".into(),
        PropDesc::new(PropValue::Int(totals.calls as i64)),
    );
    seen_props(&mut props, totals);
    if let Some(keys) = candidates {
        props.insert(
            "candidates".into(),
            PropDesc::described(
                "the declared nodes this service name answers to, where more than one \
                 does — the name is kept observed, because choosing between them would \
                 be a guess",
                PropValue::List(keys.into_iter().map(PropValue::Str).collect()),
            ),
        );
    }
    DigestNode {
        key: key.to_string(),
        label: SERVICE.into(),
        extra_labels: Vec::new(),
        props,
    }
}

/// What a `CALLS` edge says about the spans that witnessed it.
fn call_props(totals: &Pair) -> Properties {
    let mut props = stamped();
    props.insert(
        "origin".into(),
        PropDesc::described(
            "witnessed by spans, not declared by a file — the first ops edge that is \
             observed (arch/10-ops.md §8)",
            PropValue::Str("observed".into()),
        ),
    );
    props.insert(
        "calls".into(),
        PropDesc::described(
            "spans whose parent was in the calling service, in what was read — a count \
             of the sample, not of the traffic",
            PropValue::Int(totals.calls as i64),
        ),
    );
    props.insert(
        "errors".into(),
        PropDesc::new(PropValue::Int(totals.errors as i64)),
    );
    seen_props(&mut props, totals);
    props
}

/// The window the spans fall in, where any of them stated a time.
fn seen_props(props: &mut Properties, totals: &Pair) {
    if let (Some(first), Some(last)) = (totals.first_ms, totals.last_ms) {
        props.insert(
            "first_seen".into(),
            PropDesc::new(PropValue::Str(epoch_ms_to_rfc3339(first))),
        );
        props.insert(
            "last_seen".into(),
            PropDesc::new(PropValue::Str(epoch_ms_to_rfc3339(last))),
        );
        props.insert(
            "last_seen_ts".into(),
            PropDesc::new(PropValue::Int(last.div_euclid(1000))),
        );
    }
}

/// The provenance stamp, in the shape `_generated_by` already has.
fn stamped() -> Properties {
    let mut props = Properties::new();
    props.insert(
        "_generated_by".into(),
        PropDesc::new(PropValue::Str(GENERATED_BY.into())),
    );
    props
}

#[cfg(test)]
mod tests;
