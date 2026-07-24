//! Data-model primitives shared by every layer (arch/01 §2, §4; arch/09 §2).

use std::collections::BTreeMap;

/// Globally unique across all planes; monotonically allocated, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Globally unique across all planes; monotonically allocated, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeId(pub u64);

/// Plane 0 is the default plane, named "startup" (arch/09 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaneId(pub u32);

impl PlaneId {
    pub const STARTUP: PlaneId = PlaneId(0);
}

/// A property set: open map, soft schema — may expand or shrink per record.
pub type Properties = BTreeMap<String, PropDesc>;

/// A property value plus an optional natural-language description of what it
/// means, making records self-describing (arch/01 §4).
/// ⚠ On-disk format: postcard encodes by field order, so field order and
/// types here ARE the format. Any change requires bumping
/// `META_FORMAT_VERSION` (arch/01 §4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PropDesc {
    pub description: Option<String>,
    pub value: PropValue,
}

impl PropDesc {
    pub fn new(value: PropValue) -> Self {
        Self {
            description: None,
            value,
        }
    }

    pub fn described(description: impl Into<String>, value: PropValue) -> Self {
        Self {
            description: Some(description.into()),
            value,
        }
    }
}

/// ⚠ On-disk format: postcard encodes the variant index, so variant order
/// here IS the format. Never reorder or remove variants — append only, and
/// bump `META_FORMAT_VERSION` (arch/01 §4).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PropValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    /// First-class embedding vector; indexable per (plane, label, property).
    Vector(Vec<f32>),
    List(Vec<PropValue>),
    /// Nested maps reuse PropDesc so descriptions exist at every level.
    Map(BTreeMap<String, PropDesc>),
}

/// Direction of an adjacency scan / expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Both,
}

/// A fully decoded node, as returned by reads (arch/04 §3).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    pub id: NodeId,
    pub plane: PlaneId,
    /// The caller-supplied stable key this node was created with, if any
    /// (arch/01 §2). Unique within the plane.
    pub external_key: Option<String>,
    pub labels: Vec<String>,
    pub properties: Properties,
}

/// One 1-hop expansion result: the neighbor and the edge that reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbor {
    pub node: NodeId,
    pub edge: EdgeId,
}

/// A fully decoded edge, as returned by reads.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRecord {
    pub id: EdgeId,
    pub plane: PlaneId,
    pub src: NodeId,
    pub dst: NodeId,
    pub ty: String,
    pub properties: Properties,
}
