//! Key encodings for the logical tables (arch/01 §3).
//!
//! All integers are big-endian so byte order equals numeric order and every
//! per-plane / per-node scan is a contiguous prefix range. Fixed widths:
//! plane `u32`, node/edge `u64`, interned label/edge-type ids `u32`.

use crate::error::{Error, Result};
use crate::types::{EdgeId, NodeId, PlaneId};

/// Big-endian decode of an exactly-4-byte slice. Every parser below
/// length-checks the whole key before slicing, so the width is already
/// proven — hence `expect`, not an error path.
fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().expect("checked length"))
}

/// Big-endian decode of an exactly-8-byte slice (see [`u32be`]).
fn u64be(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().expect("checked length"))
}

// ---- meta table -----------------------------------------------------------

/// Magic value stored under `META_MAGIC`; identifies a dr-strange database.
pub const MAGIC: &[u8; 4] = b"DRSG";

pub const META_MAGIC: &[u8] = b"magic";
pub const META_FORMAT_VERSION: &[u8] = b"format_version";
pub const META_NEXT_NODE_ID: &[u8] = b"next_node_id";
pub const META_NEXT_EDGE_ID: &[u8] = b"next_edge_id";
pub const META_NEXT_PLANE_ID: &[u8] = b"next_plane_id";
pub const META_NEXT_LABEL_ID: &[u8] = b"next_label_id";
pub const META_NEXT_EDGE_TYPE_ID: &[u8] = b"next_edge_type_id";
/// Monotonic commit sequence (arch/02 §3): bumped inside every write txn, so a
/// reader reads it from its own snapshot — the cache's version stamp.
pub const META_COMMIT_SEQ: &[u8] = b"commit_seq";

/// Per-plane summary counters (arch/03 §5): postcard-encoded
/// [`PlaneCounters`](crate::compute::catalog::PlaneCounters), maintained
/// inside every write transaction so the dashboard reads a row instead of
/// scanning the plane.
pub fn counters_key(plane: PlaneId) -> Vec<u8> {
    let mut k = Vec::with_capacity(9 + 4);
    k.extend_from_slice(b"counters/");
    k.extend_from_slice(&plane.0.to_be_bytes());
    k
}

/// Wall-clock time (unix-epoch milliseconds, i64 BE) of the latest commit,
/// stamped inside every write txn. The time index for time-addressed
/// time-travel (ROADMAP §4): `AS OF <timestamp>` resolves against it.
pub const META_COMMIT_TIME: &[u8] = b"commit_time";

/// Vector-index declarations live in `meta`, keyed
/// `vidx:` · `plane_id` · `label` · `\0` · `property`, value = metric tag.
/// The `\0` separates the two variable-length names unambiguously (labels
/// and property keys can't contain a NUL in practice).
pub const VINDEX_PREFIX: &[u8] = b"vidx:";

pub fn vindex_decl_key(plane: PlaneId, label: &str, property: &str) -> Vec<u8> {
    let mut k = VINDEX_PREFIX.to_vec();
    k.extend_from_slice(&plane.0.to_be_bytes());
    k.extend_from_slice(label.as_bytes());
    k.push(0);
    k.extend_from_slice(property.as_bytes());
    k
}

/// Parses a `vidx:` declaration key back into `(plane, label, property)`.
pub fn parse_vindex_decl_key(key: &[u8]) -> Result<(PlaneId, String, String)> {
    parse_index_decl_key(VINDEX_PREFIX, key)
}

/// Keyword-index declarations, laid out exactly like [`VINDEX_PREFIX`] but
/// under `kidx:`, value = the [`Language`](crate::text::Language) tag byte.
pub const KINDEX_PREFIX: &[u8] = b"kidx:";

pub fn kindex_decl_key(plane: PlaneId, label: &str, property: &str) -> Vec<u8> {
    let mut k = KINDEX_PREFIX.to_vec();
    k.extend_from_slice(&plane.0.to_be_bytes());
    k.extend_from_slice(label.as_bytes());
    k.push(0);
    k.extend_from_slice(property.as_bytes());
    k
}

/// Parses a `kidx:` declaration key back into `(plane, label, property)`.
pub fn parse_kindex_decl_key(key: &[u8]) -> Result<(PlaneId, String, String)> {
    parse_index_decl_key(KINDEX_PREFIX, key)
}

/// Shared parser for the `prefix · plane · label · \0 · property` layout used
/// by both the vector- and keyword-index declaration keys.
fn parse_index_decl_key(prefix: &[u8], key: &[u8]) -> Result<(PlaneId, String, String)> {
    let rest = key
        .strip_prefix(prefix)
        .ok_or_else(|| Error::Corrupt("index key missing prefix".into()))?;
    if rest.len() < 4 {
        return Err(Error::Corrupt("index key too short".into()));
    }
    let plane = PlaneId(u32be(&rest[..4]));
    let tail = &rest[4..];
    let sep = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Corrupt("index key missing separator".into()))?;
    let label = String::from_utf8(tail[..sep].to_vec())
        .map_err(|_| Error::Corrupt("bad index label".into()))?;
    let property = String::from_utf8(tail[sep + 1..].to_vec())
        .map_err(|_| Error::Corrupt("bad index property".into()))?;
    Ok((plane, label, property))
}

/// Dictionary entries live in `meta` too: forward `name → u32` and reverse
/// `u32 → name`, per kind (label / edge type).
pub fn dict_label_key(name: &str) -> Vec<u8> {
    [b"dl:", name.as_bytes()].concat()
}

pub fn dict_label_rev_key(id: u32) -> Vec<u8> {
    [&b"dlr:"[..], &id.to_be_bytes()].concat()
}

pub fn dict_edge_type_key(name: &str) -> Vec<u8> {
    [b"dt:", name.as_bytes()].concat()
}

pub fn dict_edge_type_rev_key(id: u32) -> Vec<u8> {
    [&b"dtr:"[..], &id.to_be_bytes()].concat()
}

// ---- record tables --------------------------------------------------------

pub fn plane_key(plane: PlaneId) -> [u8; 4] {
    plane.0.to_be_bytes()
}

pub fn plane_name_key(name: &str) -> Vec<u8> {
    name.as_bytes().to_vec()
}

pub fn node_key(plane: PlaneId, node: NodeId) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..].copy_from_slice(&node.0.to_be_bytes());
    k
}

pub fn parse_node_key(key: &[u8]) -> Result<(PlaneId, NodeId)> {
    if key.len() != 12 {
        return Err(Error::Corrupt(format!(
            "node key has length {}, expected 12",
            key.len()
        )));
    }
    Ok((PlaneId(u32be(&key[..4])), NodeId(u64be(&key[4..]))))
}

pub fn edge_key(plane: PlaneId, edge: EdgeId) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..].copy_from_slice(&edge.0.to_be_bytes());
    k
}

pub fn node_plane_key(node: NodeId) -> [u8; 8] {
    node.0.to_be_bytes()
}

pub fn ext_key_key(plane: PlaneId, external_key: &str) -> Vec<u8> {
    let mut k = plane.0.to_be_bytes().to_vec();
    k.extend_from_slice(external_key.as_bytes());
    k
}

pub fn label_idx_key(plane: PlaneId, label: u32, node: NodeId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..8].copy_from_slice(&label.to_be_bytes());
    k[8..].copy_from_slice(&node.0.to_be_bytes());
    k
}

/// Prefix for "all nodes with `label` in `plane`" — a `label_idx` scan range.
pub fn label_idx_prefix(plane: PlaneId, label: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..].copy_from_slice(&label.to_be_bytes());
    k
}

/// Extracts the trailing node id from a `label_idx` key.
pub fn label_idx_node(key: &[u8]) -> Result<NodeId> {
    if key.len() != 16 {
        return Err(Error::Corrupt(format!(
            "label_idx key has length {}, expected 16",
            key.len()
        )));
    }
    Ok(NodeId(u64be(&key[8..])))
}

// ---- adjacency ------------------------------------------------------------
//
// adj_fwd: plane · src · type · dst · edge      (value: empty)
// adj_rev: plane · dst · type · src · edge      (value: empty)
//
// Layout means "all neighbors of X" and "all neighbors of X via type T" are
// both prefix scans; `edge` in the key allows parallel edges.

pub const ADJ_KEY_LEN: usize = 4 + 8 + 4 + 8 + 8;

pub fn adj_key(
    plane: PlaneId,
    from: NodeId,
    ty: u32,
    to: NodeId,
    edge: EdgeId,
) -> [u8; ADJ_KEY_LEN] {
    let mut k = [0u8; ADJ_KEY_LEN];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..12].copy_from_slice(&from.0.to_be_bytes());
    k[12..16].copy_from_slice(&ty.to_be_bytes());
    k[16..24].copy_from_slice(&to.0.to_be_bytes());
    k[24..].copy_from_slice(&edge.0.to_be_bytes());
    k
}

/// Prefix for "all adjacency entries of `from` in `plane`".
pub fn adj_prefix(plane: PlaneId, from: NodeId) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..].copy_from_slice(&from.0.to_be_bytes());
    k
}

/// Prefix for "adjacency entries of `from` via edge type `ty`".
pub fn adj_prefix_typed(plane: PlaneId, from: NodeId, ty: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..12].copy_from_slice(&from.0.to_be_bytes());
    k[12..].copy_from_slice(&ty.to_be_bytes());
    k
}

/// Parsed adjacency entry: (plane, from, type, to, edge).
pub struct AdjEntry {
    pub plane: PlaneId,
    pub from: NodeId,
    pub ty: u32,
    pub to: NodeId,
    pub edge: EdgeId,
}

pub fn parse_adj_key(key: &[u8]) -> Result<AdjEntry> {
    if key.len() != ADJ_KEY_LEN {
        return Err(Error::Corrupt(format!(
            "adjacency key has length {}, expected {ADJ_KEY_LEN}",
            key.len()
        )));
    }
    Ok(AdjEntry {
        plane: PlaneId(u32be(&key[..4])),
        from: NodeId(u64be(&key[4..12])),
        ty: u32be(&key[12..16]),
        to: NodeId(u64be(&key[16..24])),
        edge: EdgeId(u64be(&key[24..])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adj_key_roundtrip_and_prefix_containment() {
        let k = adj_key(PlaneId(7), NodeId(42), 3, NodeId(9), EdgeId(100));
        let e = parse_adj_key(&k).unwrap();
        assert_eq!(e.plane, PlaneId(7));
        assert_eq!(e.from, NodeId(42));
        assert_eq!(e.ty, 3);
        assert_eq!(e.to, NodeId(9));
        assert_eq!(e.edge, EdgeId(100));

        assert!(k.starts_with(&adj_prefix(PlaneId(7), NodeId(42))));
        assert!(k.starts_with(&adj_prefix_typed(PlaneId(7), NodeId(42), 3)));
        assert!(!k.starts_with(&adj_prefix(PlaneId(7), NodeId(43))));
    }

    #[test]
    fn big_endian_keys_sort_numerically() {
        // node 255 < node 256 must hold in byte order too
        let a = node_key(PlaneId(1), NodeId(255));
        let b = node_key(PlaneId(1), NodeId(256));
        assert!(a < b);
        // and plane is the leading dimension
        let c = node_key(PlaneId(2), NodeId(0));
        assert!(b < c);
    }
}
