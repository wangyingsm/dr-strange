//! Edge operations and adjacency (arch/01 §3): create/get/delete, property
//! mutation, the edge scan, and 1-hop neighbour expansion. Builds on
//! [`super::meta`] (id allocation, edge-type dictionary) and validates
//! endpoints via [`super::node`].

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction, prefix_successor};
use crate::storage::{codec, keys};
use crate::types::{Dir, EdgeId, EdgeRecord, Neighbor, NodeId, PlaneId, PropDesc, Properties};

use super::meta::{intern_edge_type, lookup_edge_type, next_id};
use super::node::node_exists;

pub fn create_edge(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    src: NodeId,
    dst: NodeId,
    ty: &str,
    props: &Properties,
) -> Result<EdgeId> {
    let id = EdgeId(next_id(txn, keys::META_NEXT_EDGE_ID)?);
    insert_edge(txn, plane, id, src, dst, ty, props)?;
    Ok(id)
}

/// Writes an edge under a caller-chosen, already-allocated id — the
/// `insert_node`/`IdAllocator` split, mirrored for edges (see `create_edge`,
/// `api::WriteTxn`). Endpoint existence is still validated here so the
/// batched path gets the same cross-plane-edge rejection as `create_edge`.
pub(crate) fn insert_edge(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: EdgeId,
    src: NodeId,
    dst: NodeId,
    ty: &str,
    props: &Properties,
) -> Result<()> {
    // Both endpoints must exist in this plane — cross-plane edges are
    // rejected here, at the storage layer (arch/09 §1).
    for (which, node) in [("src", src), ("dst", dst)] {
        if !node_exists(txn, plane, node)? {
            return Err(Error::PlaneMismatch(format!(
                "{which} node {} does not exist in plane {}",
                node.0, plane.0
            )));
        }
    }
    let ty_id = intern_edge_type(txn, ty)?;
    let record = codec::encode_edge_record(src, dst, ty_id, props);
    txn.put(TableId::Edges, &keys::edge_key(plane, id), &record)?;
    txn.put(
        TableId::AdjFwd,
        &keys::adj_key(plane, src, ty_id, dst, id),
        b"",
    )?;
    txn.put(
        TableId::AdjRev,
        &keys::adj_key(plane, dst, ty_id, src, id),
        b"",
    )?;
    Ok(())
}

pub fn get_edge(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    id: EdgeId,
) -> Result<Option<EdgeRecord>> {
    let Some(buf) = txn.get(TableId::Edges, &keys::edge_key(plane, id))? else {
        return Ok(None);
    };
    let (src, dst, ty_id, properties) = codec::decode_edge_record(&buf)?;
    let bytes = txn
        .get(TableId::Meta, &keys::dict_edge_type_rev_key(ty_id))?
        .ok_or_else(|| Error::Corrupt(format!("dangling edge type id {ty_id}")))?;
    let ty = String::from_utf8(bytes).map_err(|_| Error::Corrupt("bad edge type name".into()))?;
    Ok(Some(EdgeRecord {
        id,
        plane,
        src,
        dst,
        ty,
        properties,
    }))
}

/// Deletes an edge and both of its adjacency entries. Idempotent: deleting
/// an absent edge is `Ok(())` (arch/01 §3 — same posture as `delete_node`).
pub fn delete_edge(txn: &mut dyn WriteTransaction, plane: PlaneId, id: EdgeId) -> Result<()> {
    let Some(buf) = txn.get(TableId::Edges, &keys::edge_key(plane, id))? else {
        return Ok(());
    };
    let (src, dst, ty_id, _props) = codec::decode_edge_record(&buf)?;
    txn.delete(TableId::AdjFwd, &keys::adj_key(plane, src, ty_id, dst, id))?;
    txn.delete(TableId::AdjRev, &keys::adj_key(plane, dst, ty_id, src, id))?;
    txn.delete(TableId::Edges, &keys::edge_key(plane, id))?;
    Ok(())
}

/// Sets (inserts or overwrites) one property on an existing edge. Errors
/// with `NotFound` if the edge does not exist.
pub fn set_edge_prop(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: EdgeId,
    key: &str,
    prop: PropDesc,
) -> Result<()> {
    let edge_key = keys::edge_key(plane, id);
    let Some(buf) = txn.get(TableId::Edges, &edge_key)? else {
        return Err(Error::NotFound(format!("edge {}", id.0)));
    };
    let (src, dst, ty_id, mut props) = codec::decode_edge_record(&buf)?;
    props.insert(key.to_string(), prop);
    let record = codec::encode_edge_record(src, dst, ty_id, &props);
    txn.put(TableId::Edges, &edge_key, &record)
}

/// Removes one property from an existing edge; removing an absent key is
/// not an error. Errors with `NotFound` only if the edge itself is absent.
pub fn remove_edge_prop(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: EdgeId,
    key: &str,
) -> Result<()> {
    let edge_key = keys::edge_key(plane, id);
    let Some(buf) = txn.get(TableId::Edges, &edge_key)? else {
        return Err(Error::NotFound(format!("edge {}", id.0)));
    };
    let (src, dst, ty_id, mut props) = codec::decode_edge_record(&buf)?;
    props.remove(key);
    let record = codec::encode_edge_record(src, dst, ty_id, &props);
    txn.put(TableId::Edges, &edge_key, &record)
}

/// Changes an existing edge's type (arch/01 §4). The type is part of the
/// adjacency key, so this moves both adjacency entries and rewrites the record.
/// Errors with `NotFound` if the edge does not exist.
pub fn set_edge_type(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    id: EdgeId,
    ty: &str,
) -> Result<()> {
    let edge_key = keys::edge_key(plane, id);
    let Some(buf) = txn.get(TableId::Edges, &edge_key)? else {
        return Err(Error::NotFound(format!("edge {}", id.0)));
    };
    let (src, dst, old_ty_id, props) = codec::decode_edge_record(&buf)?;
    let new_ty_id = intern_edge_type(txn, ty)?;
    if new_ty_id != old_ty_id {
        txn.delete(
            TableId::AdjFwd,
            &keys::adj_key(plane, src, old_ty_id, dst, id),
        )?;
        txn.delete(
            TableId::AdjRev,
            &keys::adj_key(plane, dst, old_ty_id, src, id),
        )?;
        txn.put(
            TableId::AdjFwd,
            &keys::adj_key(plane, src, new_ty_id, dst, id),
            b"",
        )?;
        txn.put(
            TableId::AdjRev,
            &keys::adj_key(plane, dst, new_ty_id, src, id),
            b"",
        )?;
    }
    let record = codec::encode_edge_record(src, dst, new_ty_id, &props);
    txn.put(TableId::Edges, &edge_key, &record)
}

/// All edge ids in a plane, ascending (an `Edges`-table prefix scan). Used by
/// the catalog to walk edges; edges have the same `plane · id` key layout as
/// nodes.
pub fn scan_edges(txn: &dyn ReadTransaction, plane: PlaneId) -> Result<Vec<EdgeId>> {
    let prefix = keys::plane_key(plane).to_vec();
    let end = prefix_successor(&prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Edges, &prefix, end.as_deref())? {
        let (key, _) = item?;
        let (_, id) = keys::parse_node_key(&key)?; // same 12-byte plane·id layout
        out.push(EdgeId(id.0));
    }
    Ok(out)
}

/// 1-hop expansion via prefix scan on the adjacency tables (arch/01 §3).
pub fn neighbors(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    node: NodeId,
    dir: Dir,
    ty: Option<&str>,
) -> Result<Vec<Neighbor>> {
    // Unknown edge type ⇒ no edges of that type anywhere ⇒ empty result.
    let ty_id = match ty {
        None => None,
        Some(name) => match lookup_edge_type(txn, name)? {
            Some(id) => Some(id),
            None => return Ok(Vec::new()),
        },
    };

    let tables: &[TableId] = match dir {
        Dir::Out => &[TableId::AdjFwd],
        Dir::In => &[TableId::AdjRev],
        Dir::Both => &[TableId::AdjFwd, TableId::AdjRev],
    };

    let prefix: Vec<u8> = match ty_id {
        Some(t) => keys::adj_prefix_typed(plane, node, t).to_vec(),
        None => keys::adj_prefix(plane, node).to_vec(),
    };
    let end = prefix_successor(&prefix);

    let mut out = Vec::new();
    for table in tables {
        for item in txn.range(*table, &prefix, end.as_deref())? {
            let (key, _) = item?;
            let entry = keys::parse_adj_key(&key)?;
            out.push(Neighbor {
                node: entry.to,
                edge: entry.edge,
            });
        }
    }
    Ok(out)
}
