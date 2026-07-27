//! Cache layer (arch/02): the read seam between storage and computation.
//!
//! The executor never touches storage directly — it reads through
//! [`GraphReader`]. Two implementations are planned:
//! - [`UncachedReader`] (this milestone, M2): a thin pass-through over a
//!   storage read transaction, scoped to one plane.
//! - `CachedReader` (later): moka W-TinyLFU over decoded records + adjacency
//!   segments, with commit-sequence version stamping (arch/02 §3). Gated on
//!   traversal benchmarks (arch/02 open-Q 1); the trait is shaped now so it
//!   lands without touching executor code.
//!
//! Cacheable reads (`node`/`edge`/`neighbors`) already return `Arc`s, so the
//! future cache serves shared clones and the trait signature never changes.
//! Scans return owned `Vec`s — arch/02 §1 lists query/scan results as
//! deliberately *not* cached.

use std::sync::Arc;

use crate::error::Result;
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::types::{Dir, EdgeId, EdgeRecord, Neighbor, NodeId, NodeRecord, PlaneId};

/// Monotonic commit sequence number — the version-stamping and invalidation
/// token for cache entries (arch/02 §3), also the web UI's change token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommitSeq(pub u64);

/// A plane-scoped, read-only view of the graph for the executor (arch/02 §2).
///
/// Bound to a single plane because a query runs in one plane context
/// (arch/03 §1); all ids are interpreted within [`plane`](Self::plane).
pub trait GraphReader {
    fn plane(&self) -> PlaneId;

    fn node(&self, id: NodeId) -> Result<Option<Arc<NodeRecord>>>;
    fn edge(&self, id: EdgeId) -> Result<Option<Arc<EdgeRecord>>>;

    /// 1-hop neighbors of `id` (arch/01 §3); `ty = None` means any edge type.
    fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<&str>) -> Result<Arc<[Neighbor]>>;

    /// All node ids in the plane (`ScanAll` source).
    fn scan_all(&self) -> Result<Vec<NodeId>>;
    /// Node ids carrying `label` (`ScanLabel` source); unknown label ⇒ empty.
    fn scan_label(&self, label: &str) -> Result<Vec<NodeId>>;
    /// Resolve a caller-supplied external key to a node id (`SeekKeys`).
    fn node_id_by_key(&self, key: &str) -> Result<Option<NodeId>>;
}

/// Pass-through `GraphReader` over a storage read transaction (arch/02 §2).
/// Every read hits storage and decodes fresh — the point of comparison the
/// future cache must beat, and the always-correct baseline for differential
/// tests.
pub struct UncachedReader<'a> {
    txn: &'a dyn ReadTransaction,
    plane: PlaneId,
}

impl<'a> UncachedReader<'a> {
    pub fn new(txn: &'a dyn ReadTransaction, plane: PlaneId) -> Self {
        Self { txn, plane }
    }
}

impl GraphReader for UncachedReader<'_> {
    fn plane(&self) -> PlaneId {
        self.plane
    }

    fn node(&self, id: NodeId) -> Result<Option<Arc<NodeRecord>>> {
        Ok(graph::get_node(self.txn, self.plane, id)?.map(Arc::new))
    }

    fn edge(&self, id: EdgeId) -> Result<Option<Arc<EdgeRecord>>> {
        Ok(graph::get_edge(self.txn, self.plane, id)?.map(Arc::new))
    }

    fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<&str>) -> Result<Arc<[Neighbor]>> {
        Ok(graph::neighbors(self.txn, self.plane, id, dir, ty)?.into())
    }

    fn scan_all(&self) -> Result<Vec<NodeId>> {
        graph::scan_all(self.txn, self.plane)
    }

    fn scan_label(&self, label: &str) -> Result<Vec<NodeId>> {
        graph::scan_label(self.txn, self.plane, label)
    }

    fn node_id_by_key(&self, key: &str) -> Result<Option<NodeId>> {
        graph::node_id_by_external_key(self.txn, self.plane, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::engine::{StorageEngine, WriteTransaction};
    use crate::storage::memory::MemoryEngine;
    use crate::types::Properties;

    #[test]
    fn uncached_reader_covers_every_graphreader_method() {
        let eng = MemoryEngine::new();
        let (a, b, e);
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            a = graph::create_node_with_key(
                &mut txn,
                PlaneId::STARTUP,
                "a",
                &["Person"],
                &Properties::new(),
            )
            .unwrap();
            b = graph::create_node(&mut txn, PlaneId::STARTUP, &["Person"], &Properties::new())
                .unwrap();
            e = graph::create_edge(
                &mut txn,
                PlaneId::STARTUP,
                a,
                b,
                "KNOWS",
                &Properties::new(),
            )
            .unwrap();
            txn.commit().unwrap();
        }
        let txn = eng.begin_read().unwrap();
        let reader = UncachedReader::new(&txn, PlaneId::STARTUP);

        assert_eq!(reader.plane(), PlaneId::STARTUP);

        // node / edge return shared Arcs
        let node = reader.node(a).unwrap().unwrap();
        assert_eq!(node.labels, vec!["Person".to_string()]);
        assert!(reader.node(NodeId(9999)).unwrap().is_none());
        let edge = reader.edge(e).unwrap().unwrap();
        assert_eq!((edge.src, edge.dst, edge.ty.as_str()), (a, b, "KNOWS"));
        assert!(reader.edge(EdgeId(9999)).unwrap().is_none());

        // neighbors as an Arc slice
        let ns = reader.neighbors(a, Dir::Out, Some("KNOWS")).unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].node, b);

        // scans + key resolution
        assert_eq!(reader.scan_all().unwrap().len(), 2);
        assert_eq!(reader.scan_label("Person").unwrap().len(), 2);
        assert_eq!(reader.node_id_by_key("a").unwrap(), Some(a));
        assert_eq!(reader.node_id_by_key("missing").unwrap(), None);
    }

    #[test]
    fn commit_seq_orders() {
        assert!(CommitSeq(1) < CommitSeq(2));
        assert_eq!(CommitSeq(3), CommitSeq(3));
    }
}
