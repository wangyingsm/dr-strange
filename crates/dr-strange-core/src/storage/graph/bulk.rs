//! Bulk-load fast path (arch/01 §2). Loading a graph one `create_node` /
//! `create_edge` at a time pays, per record, several `open_table` round trips
//! (endpoint validation, dictionary lookups) and scattered B-tree inserts.
//! This path instead:
//!
//! - reserves one contiguous id range per entity kind (one meta write each),
//! - interns labels/edge-types through an in-memory cache (dictionary hit once
//!   per *distinct* name, not once per record),
//! - resolves edge endpoints against an in-memory key→id map built from the
//!   nodes in the same batch (no per-edge `get`; cross-plane/dangling edges
//!   are still rejected — the safety the per-record path gives, kept),
//! - buffers each KV table's writes, sorts them by key, and hands each table a
//!   single [`WriteTransaction::put_batch`] (table opened once, near-sequential
//!   B-tree inserts).
//!
//! It is a *loader*, not a general mutation API: endpoint keys must resolve
//! within this batch or already exist in the plane, and external keys are
//! assumed fresh (uniqueness is checked within the batch, not against the KV —
//! that's the per-record path's job).

use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction};
use crate::storage::{codec, keys};
use crate::types::{EdgeId, NodeId, PlaneId, Properties};

use super::meta::{intern_edge_type, intern_label, reserve_id_batch};
use super::node::node_id_by_external_key;

/// One node to bulk-load. `external_key` is required for the node to be
/// referenceable as an edge endpoint within the batch.
pub struct BulkNode<'a> {
    pub external_key: Option<&'a str>,
    pub labels: &'a [&'a str],
    pub props: Properties,
}

/// One edge to bulk-load, endpoints named by external key.
pub struct BulkEdge<'a> {
    pub src_key: &'a str,
    pub dst_key: &'a str,
    pub ty: &'a str,
    pub props: Properties,
}

/// One edge to bulk-load, endpoints already resolved to node ids. Used by the
/// CLI import, which resolves each endpoint itself (by key or by a numeric-id
/// remap) and validates before calling — see [`bulk_load_edges`].
pub struct BulkEdgeById<'a> {
    pub src: NodeId,
    pub dst: NodeId,
    pub ty: &'a str,
    pub props: Properties,
}

/// Outcome of a bulk load. `node_start` is the first assigned node id, so
/// callers (e.g. the API layer's index-event mirroring) can recover each
/// node's id as `node_start + i`.
pub struct BulkStats {
    pub nodes: u64,
    pub edges: u64,
    pub node_start: u64,
}

type Batch = Vec<(Vec<u8>, Vec<u8>)>;

/// Loads `nodes` then `edges` into `plane` (see module docs). Returns counts +
/// the first node id. Nodes are written before edges so intra-batch endpoint
/// resolution works purely in memory.
pub fn bulk_load(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    nodes: &[BulkNode],
    edges: &[BulkEdge],
) -> Result<BulkStats> {
    // ---- nodes -----------------------------------------------------------
    let node_start = reserve_id_batch(txn, keys::META_NEXT_NODE_ID, nodes.len() as u64)?;

    let mut label_ids: HashMap<&str, u32> = HashMap::new();
    let mut key_to_id: HashMap<&str, NodeId> = HashMap::new();

    let mut nodes_b: Batch = Vec::with_capacity(nodes.len());
    let mut node_plane_b: Batch = Vec::with_capacity(nodes.len());
    let mut label_idx_b: Batch = Vec::new();
    let mut ext_keys_b: Batch = Vec::new();

    for (i, node) in nodes.iter().enumerate() {
        let id = NodeId(node_start + i as u64);

        let mut lids = Vec::with_capacity(node.labels.len());
        for &l in node.labels {
            let lid = match label_ids.get(l) {
                Some(&x) => x,
                None => {
                    let x = intern_label(txn, l)?;
                    label_ids.insert(l, x);
                    x
                }
            };
            lids.push(lid);
        }

        if let Some(key) = node.external_key {
            if key_to_id.insert(key, id).is_some() {
                return Err(Error::Conflict(format!(
                    "duplicate external key '{key}' in bulk batch"
                )));
            }
            ext_keys_b.push((
                keys::ext_key_key(plane, key).to_vec(),
                id.0.to_be_bytes().to_vec(),
            ));
        }

        nodes_b.push((
            keys::node_key(plane, id).to_vec(),
            codec::encode_node_record(node.external_key, &lids, &node.props),
        ));
        node_plane_b.push((
            keys::node_plane_key(id).to_vec(),
            plane.0.to_be_bytes().to_vec(),
        ));
        for &lid in &lids {
            label_idx_b.push((keys::label_idx_key(plane, lid, id).to_vec(), Vec::new()));
        }
    }

    // ---- edges -----------------------------------------------------------
    let edge_start = reserve_id_batch(txn, keys::META_NEXT_EDGE_ID, edges.len() as u64)?;

    let mut type_ids: HashMap<&str, u32> = HashMap::new();
    let mut edges_b: Batch = Vec::with_capacity(edges.len());
    let mut adj_fwd_b: Batch = Vec::with_capacity(edges.len());
    let mut adj_rev_b: Batch = Vec::with_capacity(edges.len());

    for (i, e) in edges.iter().enumerate() {
        let id = EdgeId(edge_start + i as u64);
        let src = resolve(&key_to_id, txn, plane, e.src_key)?;
        let dst = resolve(&key_to_id, txn, plane, e.dst_key)?;

        let ty_id = match type_ids.get(e.ty) {
            Some(&x) => x,
            None => {
                let x = intern_edge_type(txn, e.ty)?;
                type_ids.insert(e.ty, x);
                x
            }
        };

        edges_b.push((
            keys::edge_key(plane, id).to_vec(),
            codec::encode_edge_record(src, dst, ty_id, &e.props),
        ));
        adj_fwd_b.push((
            keys::adj_key(plane, src, ty_id, dst, id).to_vec(),
            Vec::new(),
        ));
        adj_rev_b.push((
            keys::adj_key(plane, dst, ty_id, src, id).to_vec(),
            Vec::new(),
        ));
    }

    // ---- sorted batched writes ------------------------------------------
    // nodes/node_plane/edges are already key-sorted (contiguous ids); the
    // index and adjacency tables are keyed by label/endpoint, so sort them for
    // the near-sequential-insert win.
    for b in [
        &mut ext_keys_b,
        &mut label_idx_b,
        &mut adj_fwd_b,
        &mut adj_rev_b,
    ] {
        b.sort_unstable_by(|a, c| a.0.cmp(&c.0));
    }

    txn.put_batch(TableId::Nodes, &nodes_b)?;
    txn.put_batch(TableId::NodePlane, &node_plane_b)?;
    txn.put_batch(TableId::LabelIdx, &label_idx_b)?;
    txn.put_batch(TableId::ExtKeys, &ext_keys_b)?;
    txn.put_batch(TableId::Edges, &edges_b)?;
    txn.put_batch(TableId::AdjFwd, &adj_fwd_b)?;
    txn.put_batch(TableId::AdjRev, &adj_rev_b)?;

    Ok(BulkStats {
        nodes: nodes.len() as u64,
        edges: edges.len() as u64,
        node_start,
    })
}

/// Bulk-writes edges whose endpoints are already resolved node ids — the same
/// sorted, batched, table-opened-once writes as [`bulk_load`]'s edge phase, but
/// **trusted**: the caller guarantees both endpoints exist (no per-edge
/// validation here). The CLI import validates against its in-memory node set
/// before calling, so no dangling adjacency is written.
pub fn bulk_load_edges(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    edges: &[BulkEdgeById],
) -> Result<u64> {
    let edge_start = reserve_id_batch(txn, keys::META_NEXT_EDGE_ID, edges.len() as u64)?;

    let mut type_ids: HashMap<&str, u32> = HashMap::new();
    let mut edges_b: Batch = Vec::with_capacity(edges.len());
    let mut adj_fwd_b: Batch = Vec::with_capacity(edges.len());
    let mut adj_rev_b: Batch = Vec::with_capacity(edges.len());

    for (i, e) in edges.iter().enumerate() {
        let id = EdgeId(edge_start + i as u64);
        let ty_id = match type_ids.get(e.ty) {
            Some(&x) => x,
            None => {
                let x = intern_edge_type(txn, e.ty)?;
                type_ids.insert(e.ty, x);
                x
            }
        };
        edges_b.push((
            keys::edge_key(plane, id).to_vec(),
            codec::encode_edge_record(e.src, e.dst, ty_id, &e.props),
        ));
        adj_fwd_b.push((
            keys::adj_key(plane, e.src, ty_id, e.dst, id).to_vec(),
            Vec::new(),
        ));
        adj_rev_b.push((
            keys::adj_key(plane, e.dst, ty_id, e.src, id).to_vec(),
            Vec::new(),
        ));
    }

    for b in [&mut adj_fwd_b, &mut adj_rev_b] {
        b.sort_unstable_by(|a, c| a.0.cmp(&c.0));
    }
    txn.put_batch(TableId::Edges, &edges_b)?;
    txn.put_batch(TableId::AdjFwd, &adj_fwd_b)?;
    txn.put_batch(TableId::AdjRev, &adj_rev_b)?;

    Ok(edges.len() as u64)
}

/// Resolves an endpoint key: the current batch first (in memory), then the
/// plane's existing external keys. Absent in both ⇒ a rejected edge, so no
/// dangling adjacency is ever written.
fn resolve(
    batch: &HashMap<&str, NodeId>,
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    key: &str,
) -> Result<NodeId> {
    if let Some(&id) = batch.get(key) {
        return Ok(id);
    }
    node_id_by_external_key(txn, plane, key)?
        .ok_or_else(|| Error::NotFound(format!("bulk edge endpoint '{key}' not found")))
}
