//! Soft-schema catalog (arch/03 §5): the *descriptive* view of a plane's
//! shape that makes "no DDL" usable — which labels and properties exist, what
//! types and descriptions they carry, and how edge types connect labels.
//!
//! v0 computes it by a full scan on demand (arch/03 §5: "rebuildable by full
//! scan"). Incremental maintenance on writes is a later optimization. The
//! snapshot is a plain serializable struct — exactly what the MCP layer will
//! serve to an LLM as "schema" (descriptive, never prescriptive).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::types::{PlaneId, PropValue};

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

/// Observed shape of one edge type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeTypeStats {
    pub count: u64,
    /// Which `(src_label, dst_label)` pairs this edge type actually links,
    /// and how often — the connectivity map (arch/03 §5). A multi-label
    /// endpoint contributes every combination.
    pub connections: BTreeMap<(String, String), u64>,
}

/// A plane's (or the whole database's, rolled up) descriptive schema.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub node_count: u64,
    pub edge_count: u64,
    pub labels: BTreeMap<String, LabelStats>,
    pub edge_types: BTreeMap<String, EdgeTypeStats>,
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
            merge_counts(&mut dst.connections, &stats.connections);
        }
    }
}

fn merge_counts<K: Ord + Clone>(dst: &mut BTreeMap<K, u64>, src: &BTreeMap<K, u64>) {
    for (k, v) in src {
        *dst.entry(k.clone()).or_default() += v;
    }
}

/// Computes the catalog for one plane by scanning it (arch/03 §5).
pub fn compute(txn: &dyn ReadTransaction, plane: PlaneId) -> Result<CatalogSnapshot> {
    let mut cat = CatalogSnapshot::default();

    for id in graph::scan_all(txn, plane)? {
        let Some(node) = graph::get_node(txn, plane, id)? else {
            continue;
        };
        cat.node_count += 1;
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
        let ets = cat.edge_types.entry(edge.ty.clone()).or_default();
        ets.count += 1;
        // Record every (src_label, dst_label) combination this edge links.
        let src_labels = graph::get_node(txn, plane, edge.src)?
            .map(|n| n.labels)
            .unwrap_or_default();
        let dst_labels = graph::get_node(txn, plane, edge.dst)?
            .map(|n| n.labels)
            .unwrap_or_default();
        for sl in &src_labels {
            for dl in &dst_labels {
                *ets.connections.entry((sl.clone(), dl.clone())).or_default() += 1;
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
            *ets.connections.entry((sl.into(), dl.into())).or_default() += 1;
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
        assert_eq!(
            a.edge_types["T"].connections[&("A".to_string(), "B".to_string())],
            2
        );

        // distinct keys accumulate independently
        a.merge(&one("U", "C", "D"));
        assert_eq!(a.edge_types.len(), 2);
    }
}
