//! Node operations (arch/01 §2–§4): create/get/delete, external keys,
//! property mutation, node scans, and vector-property reads. Builds on
//! [`super::meta`] (id allocation, label dictionary) and cascades node
//! deletion into [`super::edge`].

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction, prefix_successor};
use crate::storage::{codec, keys};
use crate::types::{NodeId, NodeRecord, PlaneId, PropDesc, PropValue, Properties};

use super::edge::delete_edge;
use super::meta::{intern_label, lookup_label, next_id, resolve_label};

pub fn create_node(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    labels: &[&str],
    props: &Properties,
) -> Result<NodeId> {
    create_node_impl(txn, plane, None, labels, props)
}

/// Creates a node with a caller-supplied stable key, unique within the
/// plane (arch/01 §2). Errors with `Conflict` if the key is already bound
/// to a different node in this plane.
pub fn create_node_with_key(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    external_key: &str,
    labels: &[&str],
    props: &Properties,
) -> Result<NodeId> {
    create_node_impl(txn, plane, Some(external_key), labels, props)
}

fn create_node_impl(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    external_key: Option<&str>,
    labels: &[&str],
    props: &Properties,
) -> Result<NodeId> {
    let id = NodeId(next_id(txn, keys::META_NEXT_NODE_ID)?);
    insert_node(txn, plane, id, external_key, labels, props)?;
    Ok(id)
}

/// Writes a node under a caller-chosen, already-allocated id. Split out of
/// [`create_node_impl`] so a batched allocator (`meta::IdAllocator`) can supply
/// the id without duplicating the write logic — see `api::WriteTxn`.
/// Callers are responsible for the id being fresh; writing to an id that
/// already holds a node silently overwrites it (same as any `put`).
///
/// The external-key uniqueness check lives *here*, not only in
/// [`create_node_with_key`], so every entry point (including the batched
/// path) gets it — a second node silently sharing a key would desync the
/// key from the reverse index it's supposed to name.
pub(crate) fn insert_node(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: NodeId,
    external_key: Option<&str>,
    labels: &[&str],
    props: &Properties,
) -> Result<()> {
    if let Some(key) = external_key
        && node_id_by_external_key(txn, plane, key)?.is_some()
    {
        return Err(Error::Conflict(format!(
            "external key '{key}' already exists in plane {}",
            plane.0
        )));
    }
    let mut label_ids = Vec::with_capacity(labels.len());
    for l in labels {
        label_ids.push(intern_label(txn, l)?);
    }
    let record = codec::encode_node_record(external_key, &label_ids, props);
    txn.put(TableId::Nodes, &keys::node_key(plane, id), &record)?;
    txn.put(
        TableId::NodePlane,
        &keys::node_plane_key(id),
        &plane.0.to_be_bytes(),
    )?;
    for &lid in &label_ids {
        txn.put(TableId::LabelIdx, &keys::label_idx_key(plane, lid, id), b"")?;
    }
    if let Some(key) = external_key {
        txn.put(
            TableId::ExtKeys,
            &keys::ext_key_key(plane, key),
            &id.0.to_be_bytes(),
        )?;
    }
    Ok(())
}

pub fn node_id_by_external_key(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    external_key: &str,
) -> Result<Option<NodeId>> {
    txn.get(TableId::ExtKeys, &keys::ext_key_key(plane, external_key))?
        .map(|v| {
            v.as_slice()
                .try_into()
                .map(|b| NodeId(u64::from_be_bytes(b)))
                .map_err(|_| Error::Corrupt("bad ext_keys entry".into()))
        })
        .transpose()
}

pub fn get_node_by_external_key(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    external_key: &str,
) -> Result<Option<NodeRecord>> {
    match node_id_by_external_key(txn, plane, external_key)? {
        Some(id) => get_node(txn, plane, id),
        None => Ok(None),
    }
}

pub fn get_node(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    id: NodeId,
) -> Result<Option<NodeRecord>> {
    let Some(buf) = txn.get(TableId::Nodes, &keys::node_key(plane, id))? else {
        return Ok(None);
    };
    let (external_key, label_ids, properties) = codec::decode_node_record(&buf)?;
    let mut labels = Vec::with_capacity(label_ids.len());
    for lid in label_ids {
        labels.push(resolve_label(txn, lid)?);
    }
    Ok(Some(NodeRecord {
        id,
        plane,
        external_key,
        labels,
        properties,
    }))
}

/// Whether a node exists in a plane. `pub(super)` so `edge` can validate
/// endpoints (cross-plane edges are rejected there).
pub(super) fn node_exists(txn: &dyn ReadTransaction, plane: PlaneId, id: NodeId) -> Result<bool> {
    Ok(txn
        .get(TableId::Nodes, &keys::node_key(plane, id))?
        .is_some())
}

/// Reads a node's `property` as a vector, or `None` if absent / not a vector.
pub fn node_vector(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    id: NodeId,
    property: &str,
) -> Result<Option<Vec<f32>>> {
    let Some(node) = get_node(txn, plane, id)? else {
        return Ok(None);
    };
    Ok(match node.properties.get(property).map(|p| &p.value) {
        Some(PropValue::Vector(v)) => Some(v.clone()),
        _ => None,
    })
}

/// Sets (inserts or overwrites) one property on an existing node. Errors
/// with `NotFound` if the node does not exist (unlike delete, a mutation
/// needs somewhere real to land — arch/01 §4: properties may expand or
/// shrink freely, but only on records that exist).
pub fn set_node_prop(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: NodeId,
    key: &str,
    prop: PropDesc,
) -> Result<()> {
    let node_key = keys::node_key(plane, id);
    let Some(buf) = txn.get(TableId::Nodes, &node_key)? else {
        return Err(Error::NotFound(format!("node {}", id.0)));
    };
    let (external_key, label_ids, mut props) = codec::decode_node_record(&buf)?;
    props.insert(key.to_string(), prop);
    let record = codec::encode_node_record(external_key.as_deref(), &label_ids, &props);
    txn.put(TableId::Nodes, &node_key, &record)
}

/// Removes one property from an existing node; removing an absent key is
/// not an error (soft schema — arch/01 §4). Errors with `NotFound` only if
/// the node itself does not exist.
pub fn remove_node_prop(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: NodeId,
    key: &str,
) -> Result<()> {
    let node_key = keys::node_key(plane, id);
    let Some(buf) = txn.get(TableId::Nodes, &node_key)? else {
        return Err(Error::NotFound(format!("node {}", id.0)));
    };
    let (external_key, label_ids, mut props) = codec::decode_node_record(&buf)?;
    props.remove(key);
    let record = codec::encode_node_record(external_key.as_deref(), &label_ids, &props);
    txn.put(TableId::Nodes, &node_key, &record)
}

/// Replaces a node's entire label set (arch/01 §4: labels are soft schema and
/// may be re-declared). Rewrites the label index to match. Errors with
/// `NotFound` if the node does not exist.
pub fn set_node_labels(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: NodeId,
    labels: &[&str],
) -> Result<()> {
    let node_key = keys::node_key(plane, id);
    let Some(buf) = txn.get(TableId::Nodes, &node_key)? else {
        return Err(Error::NotFound(format!("node {}", id.0)));
    };
    let (external_key, old_label_ids, props) = codec::decode_node_record(&buf)?;
    // Intern the new labels first, so a failure leaves the index untouched.
    let mut new_label_ids = Vec::with_capacity(labels.len());
    for l in labels {
        new_label_ids.push(intern_label(txn, l)?);
    }
    for lid in &old_label_ids {
        txn.delete(TableId::LabelIdx, &keys::label_idx_key(plane, *lid, id))?;
    }
    for &lid in &new_label_ids {
        txn.put(TableId::LabelIdx, &keys::label_idx_key(plane, lid, id), b"")?;
    }
    let record = codec::encode_node_record(external_key.as_deref(), &new_label_ids, &props);
    txn.put(TableId::Nodes, &node_key, &record)
}

/// Deletes a node and everything that references it: its label-index
/// entries, its external-key entry, its `node_plane` entry, and — cascading
/// — every incident edge in both directions (with their own adjacency
/// entries). Idempotent: deleting an absent node is `Ok(())`, not an error,
/// matching `delete_prefix`'s cheap, no-questions-asked semantics.
pub fn delete_node(txn: &mut dyn WriteTransaction, plane: PlaneId, id: NodeId) -> Result<()> {
    let Some(buf) = txn.get(TableId::Nodes, &keys::node_key(plane, id))? else {
        return Ok(());
    };
    let (external_key, label_ids, _props) = codec::decode_node_record(&buf)?;

    // Cascade: collect every incident edge id from both adjacency tables
    // before deleting anything (delete_edge itself scans/mutates adjacency).
    let mut incident = std::collections::BTreeSet::new();
    for table in [TableId::AdjFwd, TableId::AdjRev] {
        let prefix = keys::adj_prefix(plane, id).to_vec();
        let end = prefix_successor(&prefix);
        for item in txn.range(table, &prefix, end.as_deref())? {
            let (key, _) = item?;
            incident.insert(keys::parse_adj_key(&key)?.edge);
        }
    }
    for edge in incident {
        delete_edge(txn, plane, edge)?;
    }

    for lid in label_ids {
        txn.delete(TableId::LabelIdx, &keys::label_idx_key(plane, lid, id))?;
    }
    if let Some(key) = &external_key {
        txn.delete(TableId::ExtKeys, &keys::ext_key_key(plane, key))?;
    }
    txn.delete(TableId::NodePlane, &keys::node_plane_key(id))?;
    txn.delete(TableId::Nodes, &keys::node_key(plane, id))?;
    Ok(())
}

/// All node ids in a plane, in ascending id order (a `Nodes`-table prefix
/// scan). The query engine's `ScanAll` source (arch/03 §2).
///
/// Returns an owned `Vec`: v0 materializes the source id list, then the
/// executor pipeline (expand/filter/limit) stays lazy over it. Streaming the
/// source is a later optimization (arch/03 §2 "start scalar").
pub fn scan_all(txn: &dyn ReadTransaction, plane: PlaneId) -> Result<Vec<NodeId>> {
    let prefix = keys::plane_key(plane).to_vec();
    let end = prefix_successor(&prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Nodes, &prefix, end.as_deref())? {
        let (key, _) = item?;
        let (_, node) = keys::parse_node_key(&key)?;
        out.push(node);
    }
    Ok(out)
}

/// All node ids carrying `label` in a plane, ascending (a `label_idx` prefix
/// scan). The `ScanLabel` source. Unknown label ⇒ empty, not an error.
pub fn scan_label(txn: &dyn ReadTransaction, plane: PlaneId, label: &str) -> Result<Vec<NodeId>> {
    let Some(label_id) = lookup_label(txn, label)? else {
        return Ok(Vec::new());
    };
    let prefix = keys::label_idx_prefix(plane, label_id).to_vec();
    let end = prefix_successor(&prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::LabelIdx, &prefix, end.as_deref())? {
        let (key, _) = item?;
        out.push(keys::label_idx_node(&key)?);
    }
    Ok(out)
}
