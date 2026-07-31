//! Graph operations over the KV encoding (arch/01 §2–§3), split by concern:
//!
//! - [`meta`] — format/init, id allocation, the label & edge-type
//!   dictionaries, plane lifecycle, and vector-index declarations
//! - [`node`] — node CRUD, external keys, property mutation, node scans
//! - [`edge`] — edge CRUD, adjacency, and 1-hop neighbour expansion
//!
//! Everything takes `&dyn ReadTransaction` / `&mut dyn WriteTransaction` so it
//! is written once for every backend. The API layer (arch/04) wraps these in
//! `Database` / `PlaneHandle` / `WriteTxn` handles, and refers to them as
//! `graph::create_node`, `graph::init`, … via the re-exports below.
//!
//! Convention: every raw integer stored in the KV — keys AND standalone
//! values (counters, dictionary entries, id pointers) — is big-endian.
//! Record bodies are the codec's business (postcard varint).

mod bulk;
mod edge;
mod meta;
mod node;

#[cfg(test)]
mod tests;

pub use bulk::{BulkEdge, BulkEdgeById, BulkNode, BulkStats, bulk_load, bulk_load_edges};

pub(crate) use meta::IdAllocator;
pub use meta::{
    DEFAULT_PLANE_NAME, FORMAT_VERSION, bump_commit_seq, create_plane, declare_keyword_index,
    declare_vector_index, drop_plane, init, intern_edge_type, intern_label, list_keyword_indexes,
    list_planes, list_vector_indexes, lookup_edge_type, lookup_label, plane_id_by_name,
    read_commit_seq, read_plane, rename_plane, resolve_label, set_plane_properties,
};

pub(crate) use node::insert_node;
pub use node::{
    create_node, create_node_with_key, delete_node, get_node, get_node_by_external_key,
    node_id_by_external_key, node_text, node_vector, remove_node_prop, scan_all, scan_label,
    set_node_labels, set_node_prop,
};

pub(crate) use edge::insert_edge;
pub use edge::{
    create_edge, delete_edge, get_edge, neighbors, remove_edge_prop, scan_edges, set_edge_prop,
    set_edge_type,
};
