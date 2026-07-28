//! Graph operations over the KV encoding (arch/01 §2–§3): meta/init,
//! dictionaries, ID allocation, planes, nodes, edges, adjacency.
//!
//! Everything here takes `&dyn ReadTransaction` / `&mut dyn WriteTransaction`
//! so it is written once for every backend. The API layer (arch/04) wraps
//! these in `Database` / `PlaneHandle` / `WriteTxn` handles.
//!
//! Convention: every raw integer stored in the KV — keys AND standalone
//! values (counters, dictionary entries, id pointers) — is big-endian.
//! Record bodies are the codec's business (postcard varint).

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction, prefix_successor};
use crate::storage::vector::Metric;
use crate::storage::{codec, keys};
use crate::types::{
    Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId, PropDesc, Properties,
};

/// v2 (M1): node records gained an inline `external_key` field
/// (arch/01 §2 — `codec::NodeRecordRaw`).
pub const FORMAT_VERSION: u32 = 2;
pub const DEFAULT_PLANE_NAME: &str = "startup";

// ---- meta / init ----------------------------------------------------------

/// First-open initialization; verifies magic/version on an existing database.
pub fn init(txn: &mut dyn WriteTransaction) -> Result<()> {
    match txn.get(TableId::Meta, keys::META_MAGIC)? {
        Some(magic) if magic == keys::MAGIC => {
            let version = get_u32(txn, keys::META_FORMAT_VERSION)?
                .ok_or_else(|| Error::Corrupt("missing format version".into()))?;
            if version != FORMAT_VERSION {
                return Err(Error::Corrupt(format!(
                    "format version {version} not supported (expected {FORMAT_VERSION})"
                )));
            }
            Ok(())
        }
        Some(_) => Err(Error::Corrupt(
            "not a dr-strange database (bad magic)".into(),
        )),
        None => {
            txn.put(TableId::Meta, keys::META_MAGIC, keys::MAGIC)?;
            put_u32(txn, keys::META_FORMAT_VERSION, FORMAT_VERSION)?;
            // Counters start at 1; 0 is never a valid allocated id, and
            // PlaneId(0) is pre-assigned to the startup plane below.
            put_u64(txn, keys::META_NEXT_NODE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_EDGE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_PLANE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_LABEL_ID, 1)?;
            put_u64(txn, keys::META_NEXT_EDGE_TYPE_ID, 1)?;
            write_plane(
                txn,
                PlaneId::STARTUP,
                DEFAULT_PLANE_NAME,
                &Properties::new(),
            )
        }
    }
}

fn decode_u32(bytes: &[u8], what: &str) -> Result<u32> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| Error::Corrupt(format!("bad u32 in {what}")))
}

fn get_u32(txn: &dyn ReadTransaction, key: &[u8]) -> Result<Option<u32>> {
    txn.get(TableId::Meta, key)?
        .map(|v| decode_u32(&v, "meta"))
        .transpose()
}

fn put_u32(txn: &mut dyn WriteTransaction, key: &[u8], v: u32) -> Result<()> {
    txn.put(TableId::Meta, key, &v.to_be_bytes())
}

fn get_u64(txn: &dyn ReadTransaction, key: &[u8]) -> Result<Option<u64>> {
    txn.get(TableId::Meta, key)?
        .map(|v| {
            v.as_slice()
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| Error::Corrupt("bad u64 in meta".into()))
        })
        .transpose()
}

fn put_u64(txn: &mut dyn WriteTransaction, key: &[u8], v: u64) -> Result<()> {
    txn.put(TableId::Meta, key, &v.to_be_bytes())
}

/// Allocates the next id from a meta counter, one meta write per call. Used
/// for planes/labels/edge-types, which are created rarely — no need for
/// [`IdAllocator`]'s batching there.
fn next_id(txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
    let id = get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, id + 1)?;
    Ok(id)
}

/// Reserves `count` contiguous ids from `counter` in one meta write, and
/// returns the batch's starting id — `[start, start + count)` all now
/// belong to the caller. The building block under [`IdAllocator`].
fn reserve_id_batch(txn: &mut dyn WriteTransaction, counter: &[u8], count: u64) -> Result<u64> {
    let start =
        get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, start + count)?;
    Ok(start)
}

/// Number of ids reserved per meta write by [`IdAllocator`]. Deliberately
/// small: it bounds how many ids a transaction can waste by reserving a
/// batch and then committing without using all of it (§ below).
pub(crate) const ID_BATCH_SIZE: u64 = 64;

#[derive(Default)]
struct IdBatch {
    next: u64,
    remaining: u64,
}

impl IdBatch {
    fn take(&mut self, txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
        if self.remaining == 0 {
            self.next = reserve_id_batch(txn, counter, ID_BATCH_SIZE)?;
            self.remaining = ID_BATCH_SIZE;
        }
        let id = self.next;
        self.next += 1;
        self.remaining -= 1;
        Ok(id)
    }
}

/// Batched node/edge id allocator (arch/01 §2 TODO): amortizes the
/// meta-counter write across up to [`ID_BATCH_SIZE`] allocations instead of
/// paying one write per node/edge, which matters for bulk ingest. Owned by
/// `api::WriteTxn` — one instance per write transaction, never persisted.
///
/// Correctness under abort/commit:
/// - **Abort**: reserving a batch bumps the counter via `txn.put`, which is
///   itself part of the write transaction. If the transaction aborts, that
///   put rolls back with everything else, so an aborted transaction's
///   reserved-but-unused ids are simply available again — no waste, no gap.
/// - **Commit**: if a transaction reserves a batch of `ID_BATCH_SIZE` and
///   commits having used only some of it, the counter has already advanced
///   past the rest — that tail is lost forever (ids are never reused once
///   the counter passes them). This is the standard cache-sequence
///   tradeoff (cf. `SERIAL` with `CACHE` in Postgres); ids stay unique and
///   monotonic, just no longer perfectly dense.
#[derive(Default)]
pub(crate) struct IdAllocator {
    node: IdBatch,
    edge: IdBatch,
}

impl IdAllocator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn next_node_id(&mut self, txn: &mut dyn WriteTransaction) -> Result<NodeId> {
        Ok(NodeId(self.node.take(txn, keys::META_NEXT_NODE_ID)?))
    }

    pub(crate) fn next_edge_id(&mut self, txn: &mut dyn WriteTransaction) -> Result<EdgeId> {
        Ok(EdgeId(self.edge.take(txn, keys::META_NEXT_EDGE_ID)?))
    }
}

// ---- dictionaries ---------------------------------------------------------

fn intern(
    txn: &mut dyn WriteTransaction,
    fwd_key: Vec<u8>,
    rev_key: impl FnOnce(u32) -> Vec<u8>,
    counter: &'static [u8],
    name: &str,
) -> Result<u32> {
    if let Some(v) = txn.get(TableId::Meta, &fwd_key)? {
        return decode_u32(&v, "dictionary");
    }
    let id = u32::try_from(next_id(txn, counter)?)
        .map_err(|_| Error::InvalidArgument("dictionary exhausted (u32)".into()))?;
    txn.put(TableId::Meta, &fwd_key, &id.to_be_bytes())?;
    txn.put(TableId::Meta, &rev_key(id), name.as_bytes())?;
    Ok(id)
}

pub fn intern_label(txn: &mut dyn WriteTransaction, name: &str) -> Result<u32> {
    intern(
        txn,
        keys::dict_label_key(name),
        keys::dict_label_rev_key,
        keys::META_NEXT_LABEL_ID,
        name,
    )
}

pub fn intern_edge_type(txn: &mut dyn WriteTransaction, name: &str) -> Result<u32> {
    intern(
        txn,
        keys::dict_edge_type_key(name),
        keys::dict_edge_type_rev_key,
        keys::META_NEXT_EDGE_TYPE_ID,
        name,
    )
}

pub fn lookup_edge_type(txn: &dyn ReadTransaction, name: &str) -> Result<Option<u32>> {
    txn.get(TableId::Meta, &keys::dict_edge_type_key(name))?
        .map(|v| decode_u32(&v, "dictionary"))
        .transpose()
}

/// Read-only label id lookup (mirror of [`lookup_edge_type`]); `None` if the
/// label name was never interned — used by `scan_label`, which then yields no
/// nodes rather than erroring.
pub fn lookup_label(txn: &dyn ReadTransaction, name: &str) -> Result<Option<u32>> {
    txn.get(TableId::Meta, &keys::dict_label_key(name))?
        .map(|v| decode_u32(&v, "dictionary"))
        .transpose()
}

pub fn resolve_label(txn: &dyn ReadTransaction, id: u32) -> Result<String> {
    let bytes = txn
        .get(TableId::Meta, &keys::dict_label_rev_key(id))?
        .ok_or_else(|| Error::Corrupt(format!("dangling label id {id}")))?;
    String::from_utf8(bytes).map_err(|_| Error::Corrupt("bad label name".into()))
}

// ---- planes ---------------------------------------------------------------

fn write_plane(
    txn: &mut dyn WriteTransaction,
    id: PlaneId,
    name: &str,
    props: &Properties,
) -> Result<()> {
    // plane record: u32-BE name length · name bytes · props (codec)
    let name_len = u32::try_from(name.len())
        .map_err(|_| Error::InvalidArgument("plane name too long".into()))?;
    let mut record = name_len.to_be_bytes().to_vec();
    record.extend_from_slice(name.as_bytes());
    record.extend_from_slice(&codec::encode_props(props));
    txn.put(TableId::Planes, &keys::plane_key(id), &record)?;
    txn.put(
        TableId::PlaneNames,
        &keys::plane_name_key(name),
        &id.0.to_be_bytes(),
    )
}

pub fn plane_id_by_name(txn: &dyn ReadTransaction, name: &str) -> Result<Option<PlaneId>> {
    txn.get(TableId::PlaneNames, &keys::plane_name_key(name))?
        .map(|v| decode_u32(&v, "plane_names").map(PlaneId))
        .transpose()
}

pub fn create_plane(
    txn: &mut dyn WriteTransaction,
    name: &str,
    props: &Properties,
) -> Result<PlaneId> {
    if plane_id_by_name(txn, name)?.is_some() {
        return Err(Error::PlaneExists(name.to_string()));
    }
    let id = u32::try_from(next_id(txn, keys::META_NEXT_PLANE_ID)?)
        .map_err(|_| Error::InvalidArgument("plane ids exhausted (u32)".into()))?;
    let id = PlaneId(id);
    write_plane(txn, id, name, props)?;
    Ok(id)
}

/// Reads a plane's `(name, properties)`; `None` if the plane doesn't exist.
pub fn read_plane(txn: &dyn ReadTransaction, id: PlaneId) -> Result<Option<(String, Properties)>> {
    let Some(buf) = txn.get(TableId::Planes, &keys::plane_key(id))? else {
        return Ok(None);
    };
    let len_bytes: [u8; 4] = buf
        .get(..4)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?
        .try_into()
        .expect("checked length");
    let name_len = u32::from_be_bytes(len_bytes) as usize;
    let name_end = 4 + name_len;
    let name_bytes = buf
        .get(4..name_end)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?;
    let name = String::from_utf8(name_bytes.to_vec())
        .map_err(|_| Error::Corrupt("bad plane name".into()))?;
    let props_bytes = buf
        .get(name_end..)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?;
    let properties = codec::decode_props(props_bytes)?;
    Ok(Some((name, properties)))
}

/// Replaces a plane's property map (arch/09 §3), keeping its name. Errors
/// `NotFound` if the plane doesn't exist.
pub fn set_plane_properties(
    txn: &mut dyn WriteTransaction,
    id: PlaneId,
    props: &Properties,
) -> Result<()> {
    let (name, _) =
        read_plane(txn, id)?.ok_or_else(|| Error::NotFound(format!("plane {}", id.0)))?;
    write_plane(txn, id, &name, props)
}

/// Renames a plane (arch/09 §3), keeping its id and properties. Errors
/// `PlaneExists` if the new name is taken, `NotFound` if the plane is
/// absent, and `InvalidArgument` for the startup plane (whose name is an
/// invariant). No-op if `new_name` equals the current name.
pub fn rename_plane(txn: &mut dyn WriteTransaction, id: PlaneId, new_name: &str) -> Result<()> {
    if id == PlaneId::STARTUP {
        return Err(Error::InvalidArgument(
            "the startup plane cannot be renamed".into(),
        ));
    }
    let (old_name, props) =
        read_plane(txn, id)?.ok_or_else(|| Error::NotFound(format!("plane {}", id.0)))?;
    if old_name == new_name {
        return Ok(());
    }
    if plane_id_by_name(txn, new_name)?.is_some() {
        return Err(Error::PlaneExists(new_name.to_string()));
    }
    txn.delete(TableId::PlaneNames, &keys::plane_name_key(&old_name))?;
    write_plane(txn, id, new_name, &props)
}

/// Deletes a plane and everything on it: every plane-scoped table is
/// prefix-range-deleted (arch/09 §1, §3). Idempotent for an absent plane.
/// The `"startup"` plane always exists and cannot be dropped.
pub fn drop_plane(txn: &mut dyn WriteTransaction, id: PlaneId) -> Result<()> {
    if id == PlaneId::STARTUP {
        return Err(Error::InvalidArgument(
            "the startup plane always exists and cannot be dropped".into(),
        ));
    }
    let Some((name, _)) = read_plane(txn, id)? else {
        return Ok(());
    };

    // `node_plane` is keyed by bare node id (no plane prefix — arch/01 §8
    // open question 7), so its entries can't be prefix-deleted. Collect the
    // plane's node ids from the (still-intact) Nodes table first.
    let prefix = keys::plane_key(id).to_vec();
    let end = prefix_successor(&prefix);
    let mut node_ids = Vec::new();
    for item in txn.range(TableId::Nodes, &prefix, end.as_deref())? {
        let (key, _) = item?;
        let (_, node) = keys::parse_node_key(&key)?;
        node_ids.push(node);
    }
    for node in node_ids {
        txn.delete(TableId::NodePlane, &keys::node_plane_key(node))?;
    }

    for table in [
        TableId::Nodes,
        TableId::Edges,
        TableId::AdjFwd,
        TableId::AdjRev,
        TableId::LabelIdx,
        TableId::ExtKeys,
        TableId::PropIdx,
    ] {
        txn.delete_prefix(table, &prefix)?;
    }

    txn.delete(TableId::Planes, &keys::plane_key(id))?;
    txn.delete(TableId::PlaneNames, &keys::plane_name_key(&name))?;
    Ok(())
}

// ---- vector index declarations (arch/01 §5) -------------------------------
// Only the *declaration* (which (plane,label,property) is indexed, and its
// metric) is durable, in `meta`. The index structure itself is rebuilt from
// the KV — the KV is the source of truth (see `crate::index`).

/// Records that `(plane, label, property)` is vector-indexed with `metric`.
/// Returns whether this was a new declaration. Errors if it already exists
/// with a different metric.
pub fn declare_vector_index(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    label: &str,
    property: &str,
    metric: Metric,
) -> Result<bool> {
    let key = keys::vindex_decl_key(plane, label, property);
    if let Some(existing) = txn.get(TableId::Meta, &key)? {
        let current = existing
            .first()
            .and_then(|&t| Metric::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad vindex metric tag".into()))?;
        if current != metric {
            return Err(Error::InvalidArgument(format!(
                "vector index on {label}.{property} already exists with a different metric"
            )));
        }
        return Ok(false);
    }
    txn.put(TableId::Meta, &key, &[metric.tag()])?;
    Ok(true)
}

/// All declared vector indexes, `(plane, label, property, metric)`.
pub fn list_vector_indexes(
    txn: &dyn ReadTransaction,
) -> Result<Vec<(PlaneId, String, String, Metric)>> {
    let prefix = keys::VINDEX_PREFIX;
    let end = prefix_successor(prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Meta, prefix, end.as_deref())? {
        let (key, value) = item?;
        let (plane, label, property) = keys::parse_vindex_decl_key(&key)?;
        let metric = value
            .first()
            .and_then(|&t| Metric::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad vindex metric tag".into()))?;
        out.push((plane, label, property, metric));
    }
    Ok(out)
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
        Some(crate::types::PropValue::Vector(v)) => Some(v.clone()),
        _ => None,
    })
}

// ---- nodes ----------------------------------------------------------------

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
/// [`create_node_impl`] so a batched allocator ([`IdAllocator`]) can supply
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

fn node_exists(txn: &dyn ReadTransaction, plane: PlaneId, id: NodeId) -> Result<bool> {
    Ok(txn
        .get(TableId::Nodes, &keys::node_key(plane, id))?
        .is_some())
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

// ---- edges & adjacency ----------------------------------------------------

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

/// All planes as `(id, name)`, ascending by id (a `plane_names` scan). Used
/// for the cross-plane catalog roll-up.
pub fn list_planes(txn: &dyn ReadTransaction) -> Result<Vec<(PlaneId, String)>> {
    let mut out = Vec::new();
    for item in txn.range(TableId::PlaneNames, b"", None)? {
        let (key, value) = item?;
        let name = String::from_utf8(key).map_err(|_| Error::Corrupt("bad plane name".into()))?;
        let id = value
            .as_slice()
            .try_into()
            .map(|b| PlaneId(u32::from_be_bytes(b)))
            .map_err(|_| Error::Corrupt("bad plane id".into()))?;
        out.push((id, name));
    }
    out.sort_by_key(|(p, _)| p.0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::engine::StorageEngine;
    use crate::storage::memory::MemoryEngine;
    use crate::types::PropValue;

    /// Runs `f` in an initialized write transaction and commits.
    fn with_db<T>(f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T>) -> T {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let out = f(&mut txn).unwrap();
        txn.commit().unwrap();
        out
    }

    #[test]
    fn init_is_idempotent() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        init(&mut txn).unwrap(); // second init on same data: verify, not clobber
        // the startup plane exists exactly once
        assert_eq!(
            plane_id_by_name(&txn, DEFAULT_PLANE_NAME).unwrap(),
            Some(PlaneId::STARTUP)
        );
    }

    #[test]
    fn init_rejects_bad_magic() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        txn.put(TableId::Meta, keys::META_MAGIC, b"NOPE").unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    /// A corrupted database must surface `Corrupt` errors, never panic.
    #[test]
    fn corrupted_meta_errors_cleanly() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();

        // garbage node-id counter (wrong width)
        txn.put(TableId::Meta, keys::META_NEXT_NODE_ID, b"xx")
            .unwrap();
        assert!(matches!(
            create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()),
            Err(Error::Corrupt(_))
        ));
        put_u64(&mut txn, keys::META_NEXT_NODE_ID, 1).unwrap();

        // missing counter
        txn.delete(TableId::Meta, keys::META_NEXT_EDGE_TYPE_ID)
            .unwrap();
        assert!(matches!(
            intern_edge_type(&mut txn, "T"),
            Err(Error::Corrupt(_))
        ));

        // garbage dictionary entry (wrong width)
        txn.put(TableId::Meta, &keys::dict_label_key("Bad"), b"toolong")
            .unwrap();
        assert!(matches!(
            intern_label(&mut txn, "Bad"),
            Err(Error::Corrupt(_))
        ));

        // reverse dictionary entry with invalid utf-8
        let id = intern_label(&mut txn, "Ok").unwrap();
        txn.put(TableId::Meta, &keys::dict_label_rev_key(id), &[0xFF, 0xFE])
            .unwrap();
        assert!(matches!(resolve_label(&txn, id), Err(Error::Corrupt(_))));

        // garbage plane-name entry (wrong width)
        txn.put(TableId::PlaneNames, &keys::plane_name_key("bad"), b"12345")
            .unwrap();
        assert!(matches!(
            plane_id_by_name(&txn, "bad"),
            Err(Error::Corrupt(_))
        ));

        // garbage node record body
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.put(
            TableId::Nodes,
            &keys::node_key(PlaneId::STARTUP, n),
            &[0xFF; 3],
        )
        .unwrap();
        assert!(matches!(
            get_node(&txn, PlaneId::STARTUP, n),
            Err(Error::Corrupt(_))
        ));

        // node referencing a label id with no dictionary entry
        let m = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.put(
            TableId::Nodes,
            &keys::node_key(PlaneId::STARTUP, m),
            &codec::encode_node_record(None, &[4040], &Properties::new()),
        )
        .unwrap();
        assert!(matches!(
            get_node(&txn, PlaneId::STARTUP, m),
            Err(Error::Corrupt(_))
        ));

        // malformed adjacency key (wrong length)
        txn.put(TableId::AdjFwd, b"short", b"").unwrap();
        let mut prefix_hit = keys::adj_prefix(PlaneId::STARTUP, NodeId(0)).to_vec();
        prefix_hit.clear(); // scan whole table via empty prefix
        let _ = prefix_hit;
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        // craft a bad entry under a's own prefix so neighbors() parses it
        let mut bad_key = keys::adj_prefix(PlaneId::STARTUP, a).to_vec();
        bad_key.push(0xAB);
        txn.put(TableId::AdjFwd, &bad_key, b"").unwrap();
        assert!(matches!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn init_rejects_future_format_version() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.put(
            TableId::Meta,
            keys::META_FORMAT_VERSION,
            &(FORMAT_VERSION + 1).to_be_bytes(),
        )
        .unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    #[test]
    fn init_rejects_missing_version() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.delete(TableId::Meta, keys::META_FORMAT_VERSION)
            .unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    #[test]
    fn interning_is_stable_and_distinct() {
        with_db(|txn| {
            let a1 = intern_label(txn, "Person")?;
            let a2 = intern_label(txn, "Person")?;
            let b = intern_label(txn, "Paper")?;
            assert_eq!(a1, a2, "same name → same id");
            assert_ne!(a1, b, "different names → different ids");
            assert_eq!(resolve_label(txn, a1)?, "Person");
            assert_eq!(resolve_label(txn, b)?, "Paper");

            // labels and edge types are separate dictionaries
            let e = intern_edge_type(txn, "Person")?;
            assert_eq!(lookup_edge_type(txn, "Person")?, Some(e));
            assert_eq!(lookup_edge_type(txn, "KNOWS")?, None);
            Ok(())
        });
    }

    #[test]
    fn resolving_a_dangling_label_id_is_corrupt() {
        with_db(|txn| {
            assert!(matches!(resolve_label(txn, 999), Err(Error::Corrupt(_))));
            Ok(())
        });
    }

    #[test]
    fn ids_are_sequential_within_and_across_transactions() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n1 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let n2 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        assert_eq!(n2.0, n1.0 + 1);
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let n3 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        assert_eq!(n3.0, n2.0 + 1);
        txn.commit().unwrap();
    }

    #[test]
    fn aborted_transaction_ids_may_be_reused() {
        // Counter bumps roll back with the transaction: an id handed out by
        // an aborted txn was never committed, so reuse is safe. This test
        // documents that semantic.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let ghost = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        drop(txn); // abort

        let mut txn = eng.begin_write().unwrap();
        let real = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.commit().unwrap();
        assert_eq!(ghost, real);
    }

    #[test]
    fn node_with_no_labels_and_no_props() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert!(rec.labels.is_empty());
        assert!(rec.properties.is_empty());
    }

    #[test]
    fn duplicate_labels_are_preserved_as_given() {
        // Soft schema: storage does not deduplicate; documents behavior.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &["A", "A"], &Properties::new()).unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["A".to_string(), "A".to_string()]);
    }

    #[test]
    fn unicode_names_survive() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let plane = create_plane(&mut txn, "研究-λ", &Properties::new()).unwrap();
        let n = create_node(&mut txn, plane, &["实体", "Ünïcodé"], &Properties::new()).unwrap();
        assert_eq!(plane_id_by_name(&txn, "研究-λ").unwrap(), Some(plane));
        let rec = get_node(&txn, plane, n).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["实体".to_string(), "Ünïcodé".to_string()]);
    }

    #[test]
    fn parallel_edges_coexist() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e1 = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "CITES",
            &Properties::new(),
        )
        .unwrap();
        let e2 = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "CITES",
            &Properties::new(),
        )
        .unwrap();
        assert_ne!(e1, e2);
        let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("CITES")).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|n| n.node == b));
    }

    #[test]
    fn typed_neighbors_filter_by_edge_type() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let c = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "KNOWS",
            &Properties::new(),
        )
        .unwrap();
        create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            c,
            "CITES",
            &Properties::new(),
        )
        .unwrap();

        let knows = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("KNOWS")).unwrap();
        assert_eq!(knows.len(), 1);
        assert_eq!(knows[0].node, b);
        let all = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn self_loop_appears_in_both_directions() {
        // A self-loop writes one adj_fwd and one adj_rev entry, so Dir::Both
        // reports it twice (once per direction). Documents behavior.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, PlaneId::STARTUP, a, a, "SELF", &Properties::new()).unwrap();
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::In, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Both, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn neighbors_of_unknown_node_is_empty() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let out = neighbors(&txn, PlaneId::STARTUP, NodeId(999), Dir::Both, None).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn edge_with_missing_endpoint_reports_which_side() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();

        let err = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            NodeId(999),
            "X",
            &Properties::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::PlaneMismatch(m) if m.contains("dst")),
            "got: {err}"
        );

        let err = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            NodeId(999),
            a,
            "X",
            &Properties::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::PlaneMismatch(m) if m.contains("src")),
            "got: {err}"
        );
    }

    #[test]
    fn adjacency_is_isolated_per_node_and_per_plane() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();

        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

        let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
        let y = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, p2, x, y, "T", &Properties::new()).unwrap();

        // b has no out-edges; a's expansion does not leak plane 2's edges
        assert!(
            neighbors(&txn, PlaneId::STARTUP, b, Dir::Out, None)
                .unwrap()
                .is_empty()
        );
        let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].node, b);
        // and node ids are globally unique across planes
        assert_ne!(a, x);
        assert_ne!(b, y);
    }

    #[test]
    fn plane_ids_are_distinct_and_names_unique() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p1 = create_plane(&mut txn, "p1", &Properties::new()).unwrap();
        let p2 = create_plane(&mut txn, "p2", &Properties::new()).unwrap();
        assert_ne!(p1, p2);
        assert_ne!(p1, PlaneId::STARTUP);
        assert!(matches!(
            create_plane(&mut txn, "p1", &Properties::new()),
            Err(Error::PlaneExists(_))
        ));
        assert_eq!(plane_id_by_name(&txn, "p1").unwrap(), Some(p1));
        assert_eq!(plane_id_by_name(&txn, "absent").unwrap(), None);
    }

    #[test]
    fn get_node_in_wrong_plane_is_none() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();
        assert!(get_node(&txn, p2, n).unwrap().is_none());
        assert!(get_node(&txn, PlaneId::STARTUP, n).unwrap().is_some());
    }

    // ---- scan sources --------------------------------------------------

    #[test]
    fn scan_all_returns_plane_nodes_in_id_order() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &["A"], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &["B"], &Properties::new()).unwrap();
        let _x = create_node(&mut txn, p2, &["A"], &Properties::new()).unwrap();

        assert_eq!(scan_all(&txn, PlaneId::STARTUP).unwrap(), vec![a, b]);
        // other plane's nodes don't leak in
        assert_eq!(scan_all(&txn, p2).unwrap().len(), 1);
    }

    #[test]
    fn scan_label_filters_by_label_and_plane() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let a1 = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
        let _p = create_node(&mut txn, PlaneId::STARTUP, &["Person"], &Properties::new()).unwrap();
        let a2 = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
        create_node(&mut txn, p2, &["Paper"], &Properties::new()).unwrap();

        assert_eq!(
            scan_label(&txn, PlaneId::STARTUP, "Paper").unwrap(),
            vec![a1, a2]
        );
        // a label the plane doesn't use here
        assert!(
            scan_label(&txn, PlaneId::STARTUP, "Org")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn scan_label_unknown_label_is_empty_not_error() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        assert!(
            scan_label(&txn, PlaneId::STARTUP, "NeverInterned")
                .unwrap()
                .is_empty()
        );
        assert_eq!(lookup_label(&txn, "NeverInterned").unwrap(), None);
    }

    #[test]
    fn scan_label_reflects_deletes() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
        delete_node(&mut txn, PlaneId::STARTUP, a).unwrap();
        assert_eq!(
            scan_label(&txn, PlaneId::STARTUP, "Paper").unwrap(),
            vec![b]
        );
    }

    // ---- external keys -----------------------------------------------

    #[test]
    fn external_key_roundtrips_and_is_stored_on_the_node() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node_with_key(
            &mut txn,
            PlaneId::STARTUP,
            "arxiv:2406.01234",
            &["Paper"],
            &Properties::new(),
        )
        .unwrap();

        assert_eq!(
            node_id_by_external_key(&txn, PlaneId::STARTUP, "arxiv:2406.01234").unwrap(),
            Some(n)
        );
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert_eq!(rec.external_key.as_deref(), Some("arxiv:2406.01234"));
        let by_key = get_node_by_external_key(&txn, PlaneId::STARTUP, "arxiv:2406.01234")
            .unwrap()
            .unwrap();
        assert_eq!(by_key.id, n);

        // plain create_node leaves it unset
        let plain = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        assert_eq!(
            get_node(&txn, PlaneId::STARTUP, plain)
                .unwrap()
                .unwrap()
                .external_key,
            None
        );
    }

    #[test]
    fn duplicate_external_key_in_same_plane_is_conflict() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
        assert!(matches!(
            create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn same_external_key_allowed_in_different_planes() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let a =
            create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
        let b = create_node_with_key(&mut txn, p2, "k", &[], &Properties::new()).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            node_id_by_external_key(&txn, PlaneId::STARTUP, "k").unwrap(),
            Some(a)
        );
        assert_eq!(node_id_by_external_key(&txn, p2, "k").unwrap(), Some(b));
    }

    #[test]
    fn unknown_external_key_is_none_not_error() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        assert_eq!(
            node_id_by_external_key(&txn, PlaneId::STARTUP, "nope").unwrap(),
            None
        );
        assert!(
            get_node_by_external_key(&txn, PlaneId::STARTUP, "nope")
                .unwrap()
                .is_none()
        );
    }

    // ---- deletes -------------------------------------------------------

    #[test]
    fn delete_edge_removes_record_and_both_adjacency_entries() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

        delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap();

        assert!(get_edge(&txn, PlaneId::STARTUP, e).unwrap().is_none());
        assert!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            neighbors(&txn, PlaneId::STARTUP, b, Dir::In, None)
                .unwrap()
                .is_empty()
        );
        // both nodes are untouched
        assert!(get_node(&txn, PlaneId::STARTUP, a).unwrap().is_some());
        assert!(get_node(&txn, PlaneId::STARTUP, b).unwrap().is_some());
    }

    #[test]
    fn delete_edge_is_idempotent() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        delete_edge(&mut txn, PlaneId::STARTUP, EdgeId(999)).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
        delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap();
        delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap(); // second delete: still Ok
    }

    #[test]
    fn get_edge_resolves_type_name_and_endpoints() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "CITES",
            &Properties::new(),
        )
        .unwrap();
        let rec = get_edge(&txn, PlaneId::STARTUP, e).unwrap().unwrap();
        assert_eq!(rec.src, a);
        assert_eq!(rec.dst, b);
        assert_eq!(rec.ty, "CITES");
        assert!(
            get_edge(&txn, PlaneId::STARTUP, EdgeId(999))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_node_cascades_to_incident_edges_both_directions() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        // x --T--> center --T--> y, plus a self-loop on center
        let center = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();
        let x = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let y = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e_in = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            x,
            center,
            "T",
            &Properties::new(),
        )
        .unwrap();
        let e_out = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            center,
            y,
            "T",
            &Properties::new(),
        )
        .unwrap();
        let e_self = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            center,
            center,
            "T",
            &Properties::new(),
        )
        .unwrap();

        delete_node(&mut txn, PlaneId::STARTUP, center).unwrap();

        assert!(get_node(&txn, PlaneId::STARTUP, center).unwrap().is_none());
        for e in [e_in, e_out, e_self] {
            assert!(
                get_edge(&txn, PlaneId::STARTUP, e).unwrap().is_none(),
                "edge {e:?} should have been cascade-deleted"
            );
        }
        // x and y survive, and now have no dangling adjacency to `center`
        assert!(get_node(&txn, PlaneId::STARTUP, x).unwrap().is_some());
        assert!(get_node(&txn, PlaneId::STARTUP, y).unwrap().is_some());
        assert!(
            neighbors(&txn, PlaneId::STARTUP, x, Dir::Out, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            neighbors(&txn, PlaneId::STARTUP, y, Dir::In, None)
                .unwrap()
                .is_empty()
        );

        // label_idx entry is gone: scanning label "L" finds no nodes
        let lid = intern_label(&mut txn, "L").unwrap();
        let prefix = keys::label_idx_key(PlaneId::STARTUP, lid, NodeId(0));
        let scan_prefix = &prefix[..8]; // plane · label, dropping the node-id suffix
        let end = prefix_successor(scan_prefix);
        assert_eq!(
            txn.range(TableId::LabelIdx, scan_prefix, end.as_deref())
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn delete_node_removes_external_key_and_node_plane_entries() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n =
            create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
        delete_node(&mut txn, PlaneId::STARTUP, n).unwrap();
        assert_eq!(
            node_id_by_external_key(&txn, PlaneId::STARTUP, "k").unwrap(),
            None
        );
        // the key is free again
        let n2 =
            create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
        assert_ne!(n, n2);
    }

    #[test]
    fn delete_node_is_idempotent() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        delete_node(&mut txn, PlaneId::STARTUP, NodeId(999)).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        delete_node(&mut txn, PlaneId::STARTUP, n).unwrap();
        delete_node(&mut txn, PlaneId::STARTUP, n).unwrap(); // second delete: still Ok
    }

    #[test]
    fn deleting_node_does_not_affect_other_planes_or_nodes() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
        let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();

        delete_node(&mut txn, PlaneId::STARTUP, a).unwrap();

        assert!(get_node(&txn, PlaneId::STARTUP, b).unwrap().is_some());
        assert!(get_node(&txn, p2, x).unwrap().is_some());
    }

    // ---- drop_plane ------------------------------------------------------

    #[test]
    fn drop_plane_wipes_everything_and_frees_the_name() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
        let a = create_node_with_key(&mut txn, p, "k", &["L"], &Properties::new()).unwrap();
        let b = create_node(&mut txn, p, &[], &Properties::new()).unwrap();
        let e = create_edge(&mut txn, p, a, b, "T", &Properties::new()).unwrap();

        drop_plane(&mut txn, p).unwrap();

        assert_eq!(plane_id_by_name(&txn, "scratch").unwrap(), None);
        assert!(get_node(&txn, p, a).unwrap().is_none());
        assert!(get_node(&txn, p, b).unwrap().is_none());
        assert!(get_edge(&txn, p, e).unwrap().is_none());
        assert_eq!(node_id_by_external_key(&txn, p, "k").unwrap(), None);

        // the name is free again, and reuse gets a fresh plane id
        let p2 = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
        assert_ne!(p, p2);
    }

    #[test]
    fn drop_plane_does_not_touch_other_planes() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p1 = create_plane(&mut txn, "p1", &Properties::new()).unwrap();
        let p2 = create_plane(&mut txn, "p2", &Properties::new()).unwrap();
        let a = create_node(&mut txn, p1, &[], &Properties::new()).unwrap();
        let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
        let startup_node =
            create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();

        drop_plane(&mut txn, p1).unwrap();

        assert!(get_node(&txn, p1, a).unwrap().is_none());
        assert!(get_node(&txn, p2, x).unwrap().is_some());
        assert!(
            get_node(&txn, PlaneId::STARTUP, startup_node)
                .unwrap()
                .is_some()
        );
        assert_eq!(plane_id_by_name(&txn, "p2").unwrap(), Some(p2));
    }

    #[test]
    fn drop_plane_rejects_startup() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        assert!(matches!(
            drop_plane(&mut txn, PlaneId::STARTUP),
            Err(Error::InvalidArgument(_))
        ));
        // still there
        assert_eq!(
            plane_id_by_name(&txn, DEFAULT_PLANE_NAME).unwrap(),
            Some(PlaneId::STARTUP)
        );
    }

    #[test]
    fn plane_properties_and_rename() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();

        let mut props = Properties::new();
        props.insert(
            "source".into(),
            PropDesc::described("where this came from", PropValue::Str("arxiv".into())),
        );
        let p = create_plane(&mut txn, "paper-1", &props).unwrap();

        // read back name + props
        let (name, read_props) = read_plane(&txn, p).unwrap().unwrap();
        assert_eq!(name, "paper-1");
        assert_eq!(read_props, props);

        // replace properties
        let mut props2 = Properties::new();
        props2.insert(
            "status".into(),
            PropDesc::new(PropValue::Str("merged".into())),
        );
        set_plane_properties(&mut txn, p, &props2).unwrap();
        let (_, after) = read_plane(&txn, p).unwrap().unwrap();
        assert_eq!(after, props2);

        // rename: name lookup moves, id + props stay
        rename_plane(&mut txn, p, "paper-1-final").unwrap();
        assert_eq!(plane_id_by_name(&txn, "paper-1").unwrap(), None);
        assert_eq!(plane_id_by_name(&txn, "paper-1-final").unwrap(), Some(p));
        let (renamed, still) = read_plane(&txn, p).unwrap().unwrap();
        assert_eq!(renamed, "paper-1-final");
        assert_eq!(still, props2);

        // rename to same name is a no-op
        rename_plane(&mut txn, p, "paper-1-final").unwrap();

        // errors: taken name, absent plane, startup, and props on absent plane
        create_plane(&mut txn, "taken", &Properties::new()).unwrap();
        assert!(matches!(
            rename_plane(&mut txn, p, "taken"),
            Err(Error::PlaneExists(_))
        ));
        assert!(matches!(
            rename_plane(&mut txn, PlaneId(999), "x"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            rename_plane(&mut txn, PlaneId::STARTUP, "x"),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            set_plane_properties(&mut txn, PlaneId(999), &Properties::new()),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn drop_plane_is_idempotent_for_absent_plane() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        drop_plane(&mut txn, PlaneId(9999)).unwrap();
    }

    #[test]
    fn drop_plane_leaves_dictionaries_and_startup_intact() {
        // Labels/edge-type dictionaries are global; dropping a plane must
        // not corrupt them even though it heavily uses the same label ids.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
        let s = create_node(&mut txn, PlaneId::STARTUP, &["Shared"], &Properties::new()).unwrap();
        create_node(&mut txn, p, &["Shared"], &Properties::new()).unwrap();

        drop_plane(&mut txn, p).unwrap();

        let rec = get_node(&txn, PlaneId::STARTUP, s).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["Shared".to_string()]);
    }

    // ---- property mutation ------------------------------------------------

    #[test]
    fn set_node_prop_inserts_and_overwrites() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();

        set_node_prop(
            &mut txn,
            PlaneId::STARTUP,
            n,
            "name",
            PropDesc::new(PropValue::Str("Alice".into())),
        )
        .unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert_eq!(
            rec.properties.get("name").map(|p| &p.value),
            Some(&PropValue::Str("Alice".into()))
        );

        // overwrite
        set_node_prop(
            &mut txn,
            PlaneId::STARTUP,
            n,
            "name",
            PropDesc::described("updated", PropValue::Str("Bob".into())),
        )
        .unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        let p = rec.properties.get("name").unwrap();
        assert_eq!(p.value, PropValue::Str("Bob".into()));
        assert_eq!(p.description.as_deref(), Some("updated"));

        // labels and external key untouched by a prop write
        assert_eq!(rec.labels, vec!["L".to_string()]);
        assert_eq!(rec.external_key, None);
    }

    #[test]
    fn remove_node_prop_shrinks_and_is_idempotent_on_missing_key() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        set_node_prop(
            &mut txn,
            PlaneId::STARTUP,
            n,
            "draft",
            PropDesc::new(PropValue::Bool(true)),
        )
        .unwrap();

        remove_node_prop(&mut txn, PlaneId::STARTUP, n, "draft").unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert!(rec.properties.is_empty());

        // removing again, or removing a key that never existed: not an error
        remove_node_prop(&mut txn, PlaneId::STARTUP, n, "draft").unwrap();
        remove_node_prop(&mut txn, PlaneId::STARTUP, n, "never_existed").unwrap();
    }

    #[test]
    fn node_prop_mutation_on_missing_node_is_not_found() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        assert!(matches!(
            set_node_prop(
                &mut txn,
                PlaneId::STARTUP,
                NodeId(999),
                "k",
                PropDesc::new(PropValue::Null)
            ),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            remove_node_prop(&mut txn, PlaneId::STARTUP, NodeId(999), "k"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn set_edge_prop_inserts_and_overwrites_without_disturbing_adjacency() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

        set_edge_prop(
            &mut txn,
            PlaneId::STARTUP,
            e,
            "weight",
            PropDesc::new(PropValue::Float(0.5)),
        )
        .unwrap();
        let rec = get_edge(&txn, PlaneId::STARTUP, e).unwrap().unwrap();
        assert_eq!(
            rec.properties.get("weight").map(|p| &p.value),
            Some(&PropValue::Float(0.5))
        );
        assert_eq!(rec.src, a);
        assert_eq!(rec.dst, b);
        assert_eq!(rec.ty, "T");

        // adjacency is untouched
        let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("T")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].edge, e);
    }

    #[test]
    fn remove_edge_prop_shrinks_and_is_idempotent_on_missing_key() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
        set_edge_prop(
            &mut txn,
            PlaneId::STARTUP,
            e,
            "w",
            PropDesc::new(PropValue::Int(1)),
        )
        .unwrap();

        remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "w").unwrap();
        assert!(
            get_edge(&txn, PlaneId::STARTUP, e)
                .unwrap()
                .unwrap()
                .properties
                .is_empty()
        );
        remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "w").unwrap();
        remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "never_existed").unwrap();
    }

    #[test]
    fn edge_prop_mutation_on_missing_edge_is_not_found() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        assert!(matches!(
            set_edge_prop(
                &mut txn,
                PlaneId::STARTUP,
                EdgeId(999),
                "k",
                PropDesc::new(PropValue::Null)
            ),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            remove_edge_prop(&mut txn, PlaneId::STARTUP, EdgeId(999), "k"),
            Err(Error::NotFound(_))
        ));
    }

    // ---- IdAllocator -------------------------------------------------------

    #[test]
    fn id_allocator_hands_out_sequential_distinct_ids() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let mut ids = IdAllocator::new();

        let mut nodes = Vec::new();
        for _ in 0..(ID_BATCH_SIZE * 2 + 5) {
            nodes.push(ids.next_node_id(&mut txn).unwrap());
        }
        for w in nodes.windows(2) {
            assert_eq!(
                w[1].0,
                w[0].0 + 1,
                "ids must stay sequential across refills"
            );
        }
        let unique: std::collections::BTreeSet<_> = nodes.iter().collect();
        assert_eq!(unique.len(), nodes.len());

        // node and edge counters are independent
        let e1 = ids.next_edge_id(&mut txn).unwrap();
        let e2 = ids.next_edge_id(&mut txn).unwrap();
        assert_eq!(e2.0, e1.0 + 1);
    }

    #[test]
    fn id_allocator_refill_only_touches_meta_once_per_batch() {
        // Indirect but precise: the meta counter should equal
        // start + ID_BATCH_SIZE after a single allocation (one refill),
        // not start + 1 (which unbatched next_id would produce).
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let before = get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap();

        let mut ids = IdAllocator::new();
        let first = ids.next_node_id(&mut txn).unwrap();
        assert_eq!(first.0, before);

        let after_one_alloc = get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap();
        assert_eq!(after_one_alloc, before + ID_BATCH_SIZE);

        // draining the rest of the batch must not move the counter again
        for _ in 1..ID_BATCH_SIZE {
            ids.next_node_id(&mut txn).unwrap();
        }
        assert_eq!(
            get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap(),
            before + ID_BATCH_SIZE
        );

        // the (ID_BATCH_SIZE+1)-th allocation triggers a second refill
        ids.next_node_id(&mut txn).unwrap();
        assert_eq!(
            get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap(),
            before + ID_BATCH_SIZE * 2
        );
    }

    #[test]
    fn id_allocator_reservation_rolls_back_with_an_aborted_transaction() {
        // The counter bump from reserving a batch is part of the write
        // transaction; if the whole transaction aborts, the reservation
        // never happened as far as any later transaction can tell — so no
        // ids are wasted by an abort, only by a commit that under-uses its
        // batch (documented in `id_allocator_commit_can_waste_a_partial_batch`).
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let mut ids = IdAllocator::new();
        let ghost = ids.next_node_id(&mut txn).unwrap();
        drop(txn); // abort — the batch reservation is discarded too

        let mut txn = eng.begin_write().unwrap();
        let mut ids = IdAllocator::new();
        let real = ids.next_node_id(&mut txn).unwrap();
        txn.commit().unwrap();
        assert_eq!(ghost, real);
    }

    #[test]
    fn id_allocator_commit_can_waste_a_partial_batch() {
        // Accepted tradeoff, documented on `IdAllocator`: a committed
        // transaction that only partially drains its reserved batch loses
        // the unused tail — ids stay unique/monotonic, just not dense.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let mut ids = IdAllocator::new();
        let first = ids.next_node_id(&mut txn).unwrap(); // reserves a full batch
        txn.commit().unwrap(); // only 1 of ID_BATCH_SIZE used

        let mut txn = eng.begin_write().unwrap();
        let mut ids = IdAllocator::new();
        let next = ids.next_node_id(&mut txn).unwrap();
        assert_eq!(
            next.0,
            first.0 + ID_BATCH_SIZE,
            "the rest of the first batch should be permanently skipped"
        );
    }

    #[test]
    fn id_allocator_ids_are_still_usable_to_create_real_nodes() {
        // End-to-end: insert_node with an allocator-supplied id behaves
        // exactly like create_node with an unbatched one.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let mut ids = IdAllocator::new();

        let id = ids.next_node_id(&mut txn).unwrap();
        insert_node(
            &mut txn,
            PlaneId::STARTUP,
            id,
            None,
            &["L"],
            &Properties::new(),
        )
        .unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, id).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["L".to_string()]);

        let eid = ids.next_edge_id(&mut txn).unwrap();
        let other = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        insert_edge(
            &mut txn,
            PlaneId::STARTUP,
            eid,
            id,
            other,
            "T",
            &Properties::new(),
        )
        .unwrap();
        let out = neighbors(&txn, PlaneId::STARTUP, id, Dir::Out, Some("T")).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].edge, eid);
    }

    #[test]
    fn insert_node_rejects_duplicate_external_key_even_with_preallocated_id() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let mut ids = IdAllocator::new();
        let id1 = ids.next_node_id(&mut txn).unwrap();
        insert_node(
            &mut txn,
            PlaneId::STARTUP,
            id1,
            Some("k"),
            &[],
            &Properties::new(),
        )
        .unwrap();

        let id2 = ids.next_node_id(&mut txn).unwrap();
        assert!(matches!(
            insert_node(
                &mut txn,
                PlaneId::STARTUP,
                id2,
                Some("k"),
                &[],
                &Properties::new()
            ),
            Err(Error::Conflict(_))
        ));
    }
}
