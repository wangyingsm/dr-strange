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

// aHash, not std's SipHash: `key_to_id` sees one insert per node and two
// lookups per edge, all short string keys — the hot path of a bulk load. The
// keyed random seed keeps crafted-collision (hash-DoS) resistance, which Fx
// would give up; import files are user-supplied data.
use ahash::AHashMap;

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

/// Sort a batch by key — the endpoint-/label-keyed tables aren't naturally
/// ordered, and a sorted batch gives the backend near-sequential inserts.
fn sort_batch(b: &mut Batch) {
    b.sort_unstable_by(|a, c| a.0.cmp(&c.0));
}

/// Batch-local dictionary cache: intern each distinct name once per batch
/// (`intern` is [`intern_label`] or [`intern_edge_type`]).
fn intern_cached<'a>(
    cache: &mut AHashMap<&'a str, u32>,
    txn: &mut dyn WriteTransaction,
    name: &'a str,
    intern: fn(&mut dyn WriteTransaction, &str) -> Result<u32>,
) -> Result<u32> {
    match cache.get(name) {
        Some(&id) => Ok(id),
        None => {
            let id = intern(txn, name)?;
            cache.insert(name, id);
            Ok(id)
        }
    }
}

/// The node phase's output: the four node-table batches plus the key→id map
/// the edge phase resolves endpoints against.
struct StagedNodes<'a> {
    node_start: u64,
    key_to_id: AHashMap<&'a str, NodeId>,
    nodes: Batch,
    node_plane: Batch,
    label_idx: Batch,
    ext_keys: Batch,
}

/// Encode every node's record, plane entry, label-index entries, and external
/// key, assigning contiguous ids from a single reservation.
fn stage_nodes<'a>(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    nodes: &'a [BulkNode],
) -> Result<StagedNodes<'a>> {
    let node_start = reserve_id_batch(txn, keys::META_NEXT_NODE_ID, nodes.len() as u64)?;
    let mut label_ids: AHashMap<&str, u32> = AHashMap::new();
    let mut staged = StagedNodes {
        node_start,
        key_to_id: AHashMap::with_capacity(nodes.len()),
        nodes: Vec::with_capacity(nodes.len()),
        node_plane: Vec::with_capacity(nodes.len()),
        label_idx: Vec::new(),
        ext_keys: Vec::new(),
    };

    for (i, node) in nodes.iter().enumerate() {
        let id = NodeId(node_start + i as u64);

        let mut lids = Vec::with_capacity(node.labels.len());
        for &l in node.labels {
            lids.push(intern_cached(&mut label_ids, txn, l, intern_label)?);
        }

        if let Some(key) = node.external_key {
            if staged.key_to_id.insert(key, id).is_some() {
                return Err(Error::Conflict(format!(
                    "duplicate external key '{key}' in bulk batch"
                )));
            }
            staged.ext_keys.push((
                keys::ext_key_key(plane, key).to_vec(),
                id.0.to_be_bytes().to_vec(),
            ));
        }

        staged.nodes.push((
            keys::node_key(plane, id).to_vec(),
            codec::encode_node_record(node.external_key, &lids, &node.props),
        ));
        staged.node_plane.push((
            keys::node_plane_key(id).to_vec(),
            plane.0.to_be_bytes().to_vec(),
        ));
        for &lid in &lids {
            staged
                .label_idx
                .push((keys::label_idx_key(plane, lid, id).to_vec(), Vec::new()));
        }
    }
    Ok(staged)
}

impl StagedNodes<'_> {
    /// Write the four node tables, each as one sorted `put_batch`. The record
    /// and plane tables are already key-sorted (contiguous ids); the label
    /// index and external keys are not, so they get sorted here.
    fn write(mut self, txn: &mut dyn WriteTransaction) -> Result<()> {
        sort_batch(&mut self.label_idx);
        sort_batch(&mut self.ext_keys);
        txn.put_batch(TableId::Nodes, &self.nodes)?;
        txn.put_batch(TableId::NodePlane, &self.node_plane)?;
        txn.put_batch(TableId::LabelIdx, &self.label_idx)?;
        txn.put_batch(TableId::ExtKeys, &self.ext_keys)
    }
}

/// The three per-edge KV batches (record + both adjacency directions) —
/// the staging half both [`bulk_load`] and [`bulk_load_edges`] share.
struct EdgeBatches {
    edges: Batch,
    adj_fwd: Batch,
    adj_rev: Batch,
}

impl EdgeBatches {
    fn with_capacity(n: usize) -> Self {
        Self {
            edges: Vec::with_capacity(n),
            adj_fwd: Vec::with_capacity(n),
            adj_rev: Vec::with_capacity(n),
        }
    }

    /// Stage one edge: its record plus both adjacency entries.
    fn push(
        &mut self,
        plane: PlaneId,
        id: EdgeId,
        src: NodeId,
        dst: NodeId,
        ty_id: u32,
        props: &Properties,
    ) {
        self.edges.push((
            keys::edge_key(plane, id).to_vec(),
            codec::encode_edge_record(src, dst, ty_id, props),
        ));
        self.adj_fwd.push((
            keys::adj_key(plane, src, ty_id, dst, id).to_vec(),
            Vec::new(),
        ));
        self.adj_rev.push((
            keys::adj_key(plane, dst, ty_id, src, id).to_vec(),
            Vec::new(),
        ));
    }

    /// Write the three edge tables, each as one sorted `put_batch`. Records
    /// are already key-sorted (contiguous ids); adjacency is endpoint-keyed,
    /// so both directions get sorted here.
    fn write(mut self, txn: &mut dyn WriteTransaction) -> Result<()> {
        sort_batch(&mut self.adj_fwd);
        sort_batch(&mut self.adj_rev);
        txn.put_batch(TableId::Edges, &self.edges)?;
        txn.put_batch(TableId::AdjFwd, &self.adj_fwd)?;
        txn.put_batch(TableId::AdjRev, &self.adj_rev)
    }
}

/// Loads `nodes` then `edges` into `plane` (see module docs). Returns counts +
/// the first node id. Nodes are staged before edges so intra-batch endpoint
/// resolution works purely in memory.
pub fn bulk_load(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    nodes: &[BulkNode],
    edges: &[BulkEdge],
) -> Result<BulkStats> {
    let staged = stage_nodes(txn, plane, nodes)?;

    let edge_start = reserve_id_batch(txn, keys::META_NEXT_EDGE_ID, edges.len() as u64)?;
    let mut type_ids: AHashMap<&str, u32> = AHashMap::new();
    let mut batches = EdgeBatches::with_capacity(edges.len());
    for (i, e) in edges.iter().enumerate() {
        let id = EdgeId(edge_start + i as u64);
        let src = resolve(&staged.key_to_id, txn, plane, e.src_key)?;
        let dst = resolve(&staged.key_to_id, txn, plane, e.dst_key)?;
        let ty_id = intern_cached(&mut type_ids, txn, e.ty, intern_edge_type)?;
        batches.push(plane, id, src, dst, ty_id, &e.props);
    }

    let node_start = staged.node_start;
    staged.write(txn)?;
    batches.write(txn)?;

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
    let mut type_ids: AHashMap<&str, u32> = AHashMap::new();
    let mut batches = EdgeBatches::with_capacity(edges.len());
    for (i, e) in edges.iter().enumerate() {
        let id = EdgeId(edge_start + i as u64);
        let ty_id = intern_cached(&mut type_ids, txn, e.ty, intern_edge_type)?;
        batches.push(plane, id, e.src, e.dst, ty_id, &e.props);
    }
    batches.write(txn)?;
    Ok(edges.len() as u64)
}

/// Resolves an endpoint key: the current batch first (in memory), then the
/// plane's existing external keys. Absent in both ⇒ a rejected edge, so no
/// dangling adjacency is ever written.
fn resolve(
    batch: &AHashMap<&str, NodeId>,
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
