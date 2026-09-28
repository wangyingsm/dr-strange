//! Soft-schema catalog (arch/03 §5): the *descriptive* view of a plane's
//! shape that makes "no DDL" usable — which labels and properties exist, what
//! types and descriptions they carry, and how edge types connect labels.
//!
//! v0 computes it by a full scan on demand (arch/03 §5: "rebuildable by full
//! scan"). Incremental maintenance on writes is a later optimization. The
//! snapshot is a plain serializable struct — exactly what the MCP layer will
//! serve to an LLM as "schema" (descriptive, never prescriptive).

use std::collections::{BTreeMap, BTreeSet};

use ahash::AHashMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::types::{NodeId, PlaneId, PropValue};

/// The kind of a [`PropValue`], for observed-type frequencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ValueType {
    Null,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    Vector,
    List,
    Map,
}

impl ValueType {
    pub fn of(value: &PropValue) -> ValueType {
        match value {
            PropValue::Null => ValueType::Null,
            PropValue::Bool(_) => ValueType::Bool,
            PropValue::Int(_) => ValueType::Int,
            PropValue::Float(_) => ValueType::Float,
            PropValue::Str(_) => ValueType::Str,
            PropValue::Bytes(_) => ValueType::Bytes,
            PropValue::Vector(_) => ValueType::Vector,
            PropValue::List(_) => ValueType::List,
            PropValue::Map(_) => ValueType::Map,
        }
    }
}

/// Observed shape of one property (within one label).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PropStats {
    /// How many of the label's nodes carry this property.
    pub count: u64,
    /// Observed value-type frequencies (a soft-schema property may hold
    /// different types on different nodes).
    pub types: BTreeMap<ValueType, u64>,
    /// Observed `PropDesc` descriptions and their frequencies — the raw
    /// material the MCP layer aggregates into a property's documented meaning
    /// (arch/03 §5). Empty when the property is never described.
    pub descriptions: BTreeMap<String, u64>,
}

/// Observed shape of one label.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LabelStats {
    /// Nodes carrying this label.
    pub count: u64,
    pub properties: BTreeMap<String, PropStats>,
}

/// One `src_label → dst_label` link observed for an edge type, with its
/// frequency.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Connection {
    pub src_label: String,
    pub dst_label: String,
    pub count: u64,
}

/// Observed shape of one edge type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeTypeStats {
    pub count: u64,
    /// Which `(src_label, dst_label)` pairs this edge type actually links,
    /// and how often — the connectivity list (arch/03 §5). A multi-label
    /// endpoint contributes every combination. A `Vec` (not a tuple-keyed
    /// map) so it serializes cleanly to JSON, which MCP serves.
    pub connections: Vec<Connection>,
}

impl EdgeTypeStats {
    /// Where `(src, dst)` lives in the sorted `connections`, by key alone —
    /// `(src, dst)` is unique in the list, so `count` never has to tie-break.
    fn probe(&self, src: &str, dst: &str) -> std::result::Result<usize, usize> {
        self.connections
            .binary_search_by(|c| (c.src_label.as_str(), c.dst_label.as_str()).cmp(&(src, dst)))
    }

    /// Increments the `src → dst` connection count by `by`, inserting it if
    /// new. `connections` stays sorted by construction — the canonical order
    /// that makes two logically identical schemas serialize identically.
    fn add_connection(&mut self, src: &str, dst: &str, by: u64) {
        match self.probe(src, dst) {
            Ok(i) => self.connections[i].count += by,
            Err(i) => self.connections.insert(
                i,
                Connection {
                    src_label: src.to_string(),
                    dst_label: dst.to_string(),
                    count: by,
                },
            ),
        }
    }

    /// The observed count of `src → dst` links (0 if never seen).
    pub fn connection(&self, src: &str, dst: &str) -> u64 {
        self.probe(src, dst)
            .map(|i| self.connections[i].count)
            .unwrap_or(0)
    }
}

/// What `key_prefixes` files a node under when no plugin claims it — a model
/// entity or a document page, which follow no plugin's naming rule.
pub const UNATTRIBUTED: &str = "(no producer)";

/// A plane's (or the whole database's, rolled up) descriptive schema.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub node_count: u64,
    pub edge_count: u64,
    pub labels: BTreeMap<String, LabelStats>,
    pub edge_types: BTreeMap<String, EdgeTypeStats>,
    /// The key prefixes each producer's nodes actually use, keyed by
    /// `_generated_by` (issue #40). A caller has to get the prefix right to name
    /// a key exactly, and it is the part no naming convention reveals: a TS key
    /// is prefixed with a `package.json` name, not the directory. Observed by
    /// scanning rather than declared, so it cannot drift from the keys stored.
    /// Bounded by the packages a producer parsed, not by node count, because a
    /// key with no separator has no prefix and contributes nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub key_prefixes: BTreeMap<String, BTreeSet<String>>,
}

/// The part of an external key before its first separator — what a caller must
/// reproduce for an exact lookup. `None` when the key has no separator, which
/// leaves it nothing to teach.
pub fn key_prefix(key: &str) -> Option<&str> {
    let slash = key.find('/');
    let path = key.find("::");
    let at = match (slash, path) {
        (Some(i), Some(j)) => i.min(j),
        (Some(i), None) => i,
        (None, Some(j)) => j,
        (None, None) => return None,
    };
    Some(&key[..at]).filter(|p| !p.is_empty())
}

impl CatalogSnapshot {
    /// Folds `other` into `self` (the cross-plane roll-up).
    pub fn merge(&mut self, other: &CatalogSnapshot) {
        self.node_count += other.node_count;
        self.edge_count += other.edge_count;
        for (label, stats) in &other.labels {
            let dst = self.labels.entry(label.clone()).or_default();
            dst.count += stats.count;
            for (prop, ps) in &stats.properties {
                let d = dst.properties.entry(prop.clone()).or_default();
                d.count += ps.count;
                merge_counts(&mut d.types, &ps.types);
                merge_counts(&mut d.descriptions, &ps.descriptions);
            }
        }
        for (ty, stats) in &other.edge_types {
            let dst = self.edge_types.entry(ty.clone()).or_default();
            dst.count += stats.count;
            for c in &stats.connections {
                dst.add_connection(&c.src_label, &c.dst_label, c.count);
            }
        }
        for (producer, prefixes) in &other.key_prefixes {
            self.key_prefixes
                .entry(producer.clone())
                .or_default()
                .extend(prefixes.iter().cloned());
        }
    }
}

fn merge_counts<K: Ord + Clone>(dst: &mut BTreeMap<K, u64>, src: &BTreeMap<K, u64>) {
    for (k, v) in src {
        *dst.entry(k.clone()).or_default() += v;
    }
}

/// The always-current summary the dashboard reads: totals and per-name
/// counts, **maintained transactionally** with every mutation and stored as
/// one row per plane — so reading it is a point lookup, not a scan. The full
/// [`CatalogSnapshot`] (property stats, connections) stays scan-computed:
/// it is the deep schema view, not the health panel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaneCounters {
    pub nodes: u64,
    pub edges: u64,
    /// label → nodes carrying it (a two-label node counts in both).
    pub labels: BTreeMap<String, u64>,
    /// edge type → edges of it.
    pub edge_types: BTreeMap<String, u64>,
}

impl PlaneCounters {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        postcard::from_bytes(bytes)
            .map_err(|e| crate::error::Error::Corrupt(format!("counters row: {e}")))
    }

    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("counters serialize cannot fail")
    }

    /// Folds `other` in — the cross-plane roll-up.
    pub fn merge(&mut self, other: &PlaneCounters) {
        self.nodes += other.nodes;
        self.edges += other.edges;
        merge_counts(&mut self.labels, &other.labels);
        merge_counts(&mut self.edge_types, &other.edge_types);
    }
}

/// Counts one plane by scanning it — the backfill for a database written
/// before counters existed, and the fallback for a time-travel snapshot
/// older than its plane's row. Deliberately lighter than [`compute`]: no
/// property stats, no per-edge endpoint lookups.
pub fn count(txn: &dyn ReadTransaction, plane: PlaneId) -> Result<PlaneCounters> {
    let mut out = PlaneCounters::default();
    for id in graph::scan_all(txn, plane)? {
        let Some(node) = graph::get_node(txn, plane, id)? else {
            continue;
        };
        out.nodes += 1;
        for label in &node.labels {
            *out.labels.entry(label.clone()).or_default() += 1;
        }
    }
    for id in graph::scan_edges(txn, plane)? {
        let Some(edge) = graph::get_edge(txn, plane, id)? else {
            continue;
        };
        out.edges += 1;
        *out.edge_types.entry(edge.ty.clone()).or_default() += 1;
    }
    Ok(out)
}

/// Computes the catalog for one plane by scanning it (arch/03 §5).
pub fn compute(txn: &dyn ReadTransaction, plane: PlaneId) -> Result<CatalogSnapshot> {
    let mut cat = CatalogSnapshot::default();

    // Each node's labels, remembered from the node pass as interned ids so
    // the edge pass can resolve both endpoints without decoding the two
    // records again (three decodes per node on a dense graph, otherwise).
    let mut label_ids: AHashMap<String, u32> = AHashMap::new();
    let mut label_names: Vec<String> = Vec::new();
    let mut labels_of: AHashMap<NodeId, Vec<u32>> = AHashMap::new();

    for id in graph::scan_all(txn, plane)? {
        let Some(node) = graph::get_node(txn, plane, id)? else {
            continue;
        };
        cat.node_count += 1;
        let interned: Vec<u32> = node
            .labels
            .iter()
            .map(|l| {
                *label_ids.entry(l.clone()).or_insert_with(|| {
                    label_names.push(l.clone());
                    (label_names.len() - 1) as u32
                })
            })
            .collect();
        labels_of.insert(id, interned);
        if let Some(key) = &node.external_key
            && let Some(prefix) = key_prefix(key)
        {
            let producer = match node.properties.get("_generated_by").map(|d| &d.value) {
                Some(PropValue::Str(by)) => by.clone(),
                _ => UNATTRIBUTED.to_string(),
            };
            cat.key_prefixes
                .entry(producer)
                .or_default()
                .insert(prefix.to_string());
        }
        for label in &node.labels {
            let ls = cat.labels.entry(label.clone()).or_default();
            ls.count += 1;
            for (key, prop) in &node.properties {
                let ps = ls.properties.entry(key.clone()).or_default();
                ps.count += 1;
                *ps.types.entry(ValueType::of(&prop.value)).or_default() += 1;
                if let Some(desc) = &prop.description {
                    *ps.descriptions.entry(desc.clone()).or_default() += 1;
                }
            }
        }
    }

    for id in graph::scan_edges(txn, plane)? {
        let Some(edge) = graph::get_edge(txn, plane, id)? else {
            continue;
        };
        cat.edge_count += 1;
        // Record every (src_label, dst_label) combination this edge links. An
        // endpoint the node pass did not see (a dangling edge) has no labels.
        let src_labels = labels_of.get(&edge.src).map(Vec::as_slice).unwrap_or(&[]);
        let dst_labels = labels_of.get(&edge.dst).map(Vec::as_slice).unwrap_or(&[]);
        let ets = cat.edge_types.entry(edge.ty.clone()).or_default();
        ets.count += 1;
        for &sl in src_labels {
            for &dl in dst_labels {
                ets.add_connection(&label_names[sl as usize], &label_names[dl as usize], 1);
            }
        }
    }

    Ok(cat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_type_of_covers_all_variants() {
        assert_eq!(ValueType::of(&PropValue::Null), ValueType::Null);
        assert_eq!(ValueType::of(&PropValue::Int(1)), ValueType::Int);
        assert_eq!(
            ValueType::of(&PropValue::Vector(vec![1.0])),
            ValueType::Vector
        );
        assert_eq!(
            ValueType::of(&PropValue::List(vec![PropValue::Null])),
            ValueType::List
        );
    }

    #[test]
    fn merge_sums_counts_types_descriptions_and_connections() {
        let one = |ty: &str, sl: &str, dl: &str| {
            let mut c = CatalogSnapshot {
                node_count: 1,
                edge_count: 1,
                ..Default::default()
            };
            let ls = c.labels.entry("L".into()).or_default();
            ls.count = 1;
            let ps = ls.properties.entry("p".into()).or_default();
            ps.count = 1;
            *ps.types.entry(ValueType::Int).or_default() += 1;
            *ps.descriptions.entry("d".into()).or_default() += 1;
            let ets = c.edge_types.entry(ty.into()).or_default();
            ets.count = 1;
            ets.add_connection(sl, dl, 1);
            c
        };

        let mut a = one("T", "A", "B");
        a.merge(&one("T", "A", "B"));
        assert_eq!(a.node_count, 2);
        assert_eq!(a.edge_count, 2);
        assert_eq!(a.labels["L"].count, 2);
        assert_eq!(a.labels["L"].properties["p"].count, 2);
        assert_eq!(a.labels["L"].properties["p"].types[&ValueType::Int], 2);
        assert_eq!(a.labels["L"].properties["p"].descriptions["d"], 2);
        assert_eq!(a.edge_types["T"].count, 2);
        assert_eq!(a.edge_types["T"].connection("A", "B"), 2);

        // distinct keys accumulate independently
        a.merge(&one("U", "C", "D"));
        assert_eq!(a.edge_types.len(), 2);
    }

    /// `add_connection`'s binary-search insertion must keep `connections`
    /// canonically sorted regardless of discovery order — the property the
    /// old sort-after-push maintained — and increments must land on the
    /// existing entry, never mint a duplicate.
    #[test]
    fn connections_stay_sorted_and_deduped_under_any_insert_order() {
        let mut ets = EdgeTypeStats::default();
        // Deliberately anti-sorted discovery order, with same-src ties.
        for (s, d) in [("z", "a"), ("a", "z"), ("m", "m"), ("a", "a"), ("z", "z")] {
            ets.add_connection(s, d, 1);
        }
        let order: Vec<(&str, &str)> = ets
            .connections
            .iter()
            .map(|c| (c.src_label.as_str(), c.dst_label.as_str()))
            .collect();
        assert_eq!(
            order,
            [("a", "a"), ("a", "z"), ("m", "m"), ("z", "a"), ("z", "z")]
        );

        // Re-adding every pair increments in place: same order, same length.
        for (s, d) in [("a", "a"), ("z", "z"), ("m", "m"), ("a", "z"), ("z", "a")] {
            ets.add_connection(s, d, 2);
        }
        assert_eq!(ets.connections.len(), 5);
        assert!(ets.connections.iter().all(|c| c.count == 3));

        // Lookups: every present pair via the same probe, absent pairs are 0 —
        // including probes that would land before, between, and past the ends.
        assert_eq!(ets.connection("a", "z"), 3);
        assert_eq!(ets.connection("a", "0"), 0);
        assert_eq!(ets.connection("m", "z"), 0);
        assert_eq!(ets.connection("zz", "a"), 0);
    }

    /// The catalog's determinism contract: two logically identical schemas
    /// built in different orders serialize identically.
    #[test]
    fn connection_order_is_canonical_across_build_orders() {
        let pairs = [("b", "c"), ("a", "a"), ("c", "b"), ("b", "b")];
        let mut fwd = EdgeTypeStats::default();
        for (s, d) in pairs {
            fwd.add_connection(s, d, 1);
        }
        let mut rev = EdgeTypeStats::default();
        for (s, d) in pairs.iter().rev() {
            rev.add_connection(s, d, 1);
        }
        assert_eq!(fwd.connections, rev.connections);
    }

    /// The prefix is what a caller must reproduce; a key with no separator
    /// teaches nothing, so it contributes nothing rather than filing itself
    /// under its own whole name — which is what makes the set bounded by
    /// packages instead of by nodes (issue #40).
    #[test]
    fn a_key_prefix_is_the_segment_before_the_first_separator() {
        assert_eq!(
            key_prefix("highway_bridge-3.0/src/App.App"),
            Some("highway_bridge-3.0")
        );
        assert_eq!(
            key_prefix("dr_strange_core::compact::resolve"),
            Some("dr_strange_core")
        );
        assert_eq!(key_prefix("@acme/web/src/App"), Some("@acme"));
        assert_eq!(key_prefix("pkg"), None, "no separator, nothing to teach");
        assert_eq!(key_prefix(""), None);
        assert_eq!(key_prefix("/leading"), None, "an empty prefix is no prefix");
        // Whichever separator comes first decides.
        assert_eq!(key_prefix("a/b::c"), Some("a"));
        assert_eq!(key_prefix("a::b/c"), Some("a"));
    }

    #[test]
    fn merging_catalogs_unions_prefixes_per_producer() {
        let of = |producer: &str, prefix: &str| {
            let mut c = CatalogSnapshot::default();
            c.key_prefixes
                .entry(producer.into())
                .or_default()
                .insert(prefix.into());
            c
        };

        let mut a = of("ts@2", "highway_bridge-3.0");
        a.merge(&of("ts@2", "@acme/web"));
        a.merge(&of("rust@3", "dr_strange_core"));
        assert_eq!(
            a.key_prefixes["ts@2"],
            ["@acme/web".to_string(), "highway_bridge-3.0".to_string()]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "the same producer's prefixes union rather than overwrite"
        );
        assert_eq!(a.key_prefixes["rust@3"].len(), 1);

        // Idempotent: merging the same snapshot twice adds no duplicates.
        a.merge(&of("ts@2", "@acme/web"));
        assert_eq!(a.key_prefixes["ts@2"].len(), 2);
    }

    /// The whole backward-compatibility claim: a plane that has no keyed nodes
    /// serialises exactly as it did before the field existed.
    #[test]
    fn an_empty_prefix_map_is_absent_from_the_serialized_form() {
        let empty = CatalogSnapshot::default();
        let json = serde_json::to_string(&empty).unwrap();
        assert!(
            !json.contains("key_prefixes"),
            "an empty map must not appear: {json}"
        );

        let mut some = CatalogSnapshot::default();
        some.key_prefixes
            .entry("ts@2".into())
            .or_default()
            .insert("pkg".into());
        assert!(
            serde_json::to_string(&some)
                .unwrap()
                .contains("key_prefixes")
        );

        // And a payload written before the field existed still reads.
        let old = r#"{"node_count":0,"edge_count":0,"labels":{},"edge_types":{}}"#;
        let back: CatalogSnapshot = serde_json::from_str(old).unwrap();
        assert!(back.key_prefixes.is_empty());
    }
}
