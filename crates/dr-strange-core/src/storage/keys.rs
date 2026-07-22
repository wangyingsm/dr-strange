//! Key encodings for the logical tables (arch/01 §3).
//!
//! All integers are big-endian so byte order equals numeric order and every
//! per-plane / per-node scan is a contiguous prefix range. Fixed widths:
//! plane `u32`, node/edge `u64`, interned label/edge-type ids `u32`.

use crate::error::{Error, Result};
use crate::types::{EdgeId, NodeId, PlaneId};

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

pub fn edge_key(plane: PlaneId, edge: EdgeId) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..].copy_from_slice(&edge.0.to_be_bytes());
    k
}

pub fn node_plane_key(node: NodeId) -> [u8; 8] {
    node.0.to_be_bytes()
}

pub fn label_idx_key(plane: PlaneId, label: u32, node: NodeId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&plane.0.to_be_bytes());
    k[4..8].copy_from_slice(&label.to_be_bytes());
    k[8..].copy_from_slice(&node.0.to_be_bytes());
    k
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
    let u32be = |b: &[u8]| u32::from_be_bytes(b.try_into().expect("checked length"));
    let u64be = |b: &[u8]| u64::from_be_bytes(b.try_into().expect("checked length"));
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
