//! Data-model primitives shared by every layer (arch/01 §2, §4; arch/09 §2).

use std::collections::BTreeMap;

/// Globally unique across all planes; monotonically allocated, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Globally unique across all planes; monotonically allocated, never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeId(pub u64);

/// Plane 0 is the default plane, named "main" (arch/09 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaneId(pub u32);

impl PlaneId {
    pub const MAIN: PlaneId = PlaneId(0);
}

/// A property set: open map, soft schema — may expand or shrink per record.
pub type Properties = BTreeMap<String, PropDesc>;

/// A property value plus an optional natural-language description of what it
/// means, making records self-describing (arch/01 §4).
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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
