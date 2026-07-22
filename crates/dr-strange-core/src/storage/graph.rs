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
use crate::storage::{codec, keys};
use crate::types::{Dir, EdgeId, Neighbor, NodeId, NodeRecord, PlaneId, Properties};

pub const FORMAT_VERSION: u32 = 1;
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

/// Allocates the next id from a meta counter. TODO(M1): batch counter bumps
/// so bulk ingest doesn't pay a meta write per allocation.
fn next_id(txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
    let id = get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, id + 1)?;
    Ok(id)
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

// ---- nodes ----------------------------------------------------------------

pub fn create_node(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    labels: &[&str],
    props: &Properties,
) -> Result<NodeId> {
    let id = NodeId(next_id(txn, keys::META_NEXT_NODE_ID)?);
    let mut label_ids = Vec::with_capacity(labels.len());
    for l in labels {
        label_ids.push(intern_label(txn, l)?);
    }
    let record = codec::encode_node_record(&label_ids, props);
    txn.put(TableId::Nodes, &keys::node_key(plane, id), &record)?;
    txn.put(
        TableId::NodePlane,
        &keys::node_plane_key(id),
        &plane.0.to_be_bytes(),
    )?;
    for lid in label_ids {
        txn.put(TableId::LabelIdx, &keys::label_idx_key(plane, lid, id), b"")?;
    }
    Ok(id)
}

pub fn get_node(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    id: NodeId,
) -> Result<Option<NodeRecord>> {
    let Some(buf) = txn.get(TableId::Nodes, &keys::node_key(plane, id))? else {
        return Ok(None);
    };
    let (label_ids, properties) = codec::decode_node_record(&buf)?;
    let mut labels = Vec::with_capacity(label_ids.len());
    for lid in label_ids {
        labels.push(resolve_label(txn, lid)?);
    }
    Ok(Some(NodeRecord {
        id,
        plane,
        labels,
        properties,
    }))
}

fn node_exists(txn: &dyn ReadTransaction, plane: PlaneId, id: NodeId) -> Result<bool> {
    Ok(txn
        .get(TableId::Nodes, &keys::node_key(plane, id))?
        .is_some())
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
    let id = EdgeId(next_id(txn, keys::META_NEXT_EDGE_ID)?);
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
    Ok(id)
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
