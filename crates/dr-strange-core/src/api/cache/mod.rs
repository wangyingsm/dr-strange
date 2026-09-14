//! Cache layer (arch/02): the read seam between storage and computation.
//!
//! The executor never touches storage directly — it reads through
//! [`GraphReader`]. Two implementations:
//! - [`UncachedReader`]: a thin pass-through over a storage read transaction —
//!   the always-correct baseline and the oracle for differential tests.
//! - [`CachedReader`] (the query path's default): **per-query** memoization of
//!   decoded `node`/`edge`/`neighbors` results, bound to one snapshot, dropped
//!   at query end. A revisit-heavy traversal decodes each hot record once, not
//!   per visit (measured up to ~3.8× on hot property-rich traversals; a slight
//!   loss on broad low-revisit scans — the benchmark-gated call of arch/02 §5).
//!
//! Behind the per-query reader sits `GraphCache`, the **persistent,
//! cross-query** moka W-TinyLFU cache (arch/02 §3–4) that `CachedReader` uses
//! as its L2. Coherence there is **invalidate-only**: entries are stamped with
//! the [`CommitSeq`] they were read at, and a reader pinned to a snapshot
//! serves an entry only when the stamps match, so any write logically flushes
//! everything older. Nothing is written *into* the cache at commit time — the
//! populating `put_*` calls happen on a read miss, never on the write path.
//!
//! Cacheable reads return `Arc`s, so a cache serves shared clones and the
//! trait signature never changes. Scans return owned `Vec`s — arch/02 §1 lists
//! query/scan results as deliberately *not* cached.

mod store;
pub(crate) use store::GraphCache;

use ahash::AHashMap;
use std::cell::RefCell;
use std::sync::Arc;

use crate::error::Result;
use crate::index::VectorRegistry;
use crate::keyword::KeywordRegistry;
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::storage::vector::{Hit, Metric, top_k};
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

    /// Global similarity search for `VectorTopK`: the `k` nodes (optionally
    /// restricted to `label`) closest to `query` by their `property` vector,
    /// as `(id, distance)`. Uses a declared HNSW index when one matches;
    /// otherwise exact brute force. The default is the brute-force path.
    fn vector_search(
        &self,
        label: Option<&str>,
        property: &str,
        query: &[f32],
        metric: Metric,
        k: usize,
    ) -> Result<Vec<Hit>> {
        brute_force_search(self, label, property, query, metric, k)
    }

    /// BM25 keyword search for `KeywordTopK` and the hybrid keyword channel:
    /// the `k` best-matching nodes on `(label, property)`, as `(id, score)`,
    /// most-relevant first. Unlike vector search there is no brute-force
    /// fallback — BM25 needs the inverted index — so a reader with no keyword
    /// registry (or a pair with no declared index) yields nothing.
    fn keyword_search(
        &self,
        _label: &str,
        _property: &str,
        _query: &str,
        _k: usize,
    ) -> Result<Vec<(NodeId, f32)>> {
        Ok(Vec::new())
    }
}

/// Exact similarity search by scanning candidate records — the fallback when
/// no index is declared, and the oracle the index must match. Skips nodes
/// without the vector property and dimension mismatches (non-finite
/// distance), matching the evaluator's total semantics (arch/01 §5).
fn brute_force_search<R: GraphReader + ?Sized>(
    reader: &R,
    label: Option<&str>,
    property: &str,
    query: &[f32],
    metric: Metric,
    k: usize,
) -> Result<Vec<Hit>> {
    let candidates = match label {
        Some(l) => reader.scan_label(l)?,
        None => reader.scan_all()?,
    };
    let mut items: Vec<(u64, f32)> = Vec::new();
    for id in candidates {
        if let Some(node) = reader.node(id)?
            && let Some(crate::types::PropValue::Vector(v)) =
                node.properties.get(property).map(|p| &p.value)
        {
            let d = metric.distance(query, v);
            if d.is_finite() {
                items.push((id.0, d));
            }
        }
    }
    Ok(top_k(items.into_iter(), k))
}

/// Shared `vector_search` body: use a declared index when it covers this exact
/// `(label, property, metric)`; otherwise exact brute force. A `None` label
/// means "whole plane", which per-label indexes can't answer, so that also
/// brute-forces. Both readers delegate here so their behaviour is identical.
#[allow(clippy::too_many_arguments)] // mirrors the GraphReader::vector_search arity it shares
fn indexed_or_brute<R: GraphReader + ?Sized>(
    reader: &R,
    registry: Option<&VectorRegistry>,
    plane: PlaneId,
    label: Option<&str>,
    property: &str,
    query: &[f32],
    metric: Metric,
    k: usize,
) -> Result<Vec<Hit>> {
    if let (Some(reg), Some(l)) = (registry, label)
        && let Some(result) = reg.search(plane, l, property, query, metric, k)
    {
        return result;
    }
    brute_force_search(reader, label, property, query, metric, k)
}

/// Keep only the hits whose node this reader's snapshot can see — present,
/// and carrying `label` when one was asked for (arch/02 §3).
///
/// The registries live outside the snapshot: a writer publishes its KV
/// commit before it takes the registry lock, and a time-travelling handle
/// pins a past snapshot under a registry built from the latest one. Either
/// way an index can name a node the snapshot does not hold (not yet
/// visible, deleted since, relabelled since). Surfacing such an id would
/// hand the executor a row it cannot decode — a phantom — so the reader
/// drops it here, once, for every terminal that searches an index. The
/// record read is not wasted: the executor decodes the same node next, and
/// finds it in the per-query L1.
fn visible_hits<R: GraphReader + ?Sized, T>(
    reader: &R,
    hits: Vec<T>,
    label: Option<&str>,
    id_of: impl Fn(&T) -> NodeId,
) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let visible = match reader.node(id_of(&hit))? {
            Some(node) => label.is_none_or(|l| node.labels.iter().any(|have| have == l)),
            None => false,
        };
        if visible {
            out.push(hit);
        }
    }
    Ok(out)
}

/// How many live BM25 postings a historical keyword search asks for per
/// result it must return. The live index has no notion of the pinned
/// snapshot, so the reader over-fetches, filters through the snapshot and
/// truncates; a query whose matches mostly postdate the snapshot can still
/// come back short, which the builder documents.
const HISTORICAL_KEYWORD_OVERFETCH: usize = 4;

/// Pass-through `GraphReader` over a storage read transaction (arch/02 §2).
/// Every read hits storage and decodes fresh — the point of comparison the
/// future cache must beat, and the always-correct baseline for differential
/// tests.
pub struct UncachedReader<'a> {
    txn: &'a dyn ReadTransaction,
    plane: PlaneId,
    /// Declared vector indexes (a read-locked view for the query's lifetime).
    /// `None` ⇒ every vector search takes the exact brute-force path.
    registry: Option<&'a VectorRegistry>,
}

impl<'a> UncachedReader<'a> {
    pub fn new(txn: &'a dyn ReadTransaction, plane: PlaneId) -> Self {
        Self {
            txn,
            plane,
            registry: None,
        }
    }

    /// With access to declared indexes, so `vector_search` can use them.
    pub fn with_registry(
        txn: &'a dyn ReadTransaction,
        plane: PlaneId,
        registry: &'a VectorRegistry,
    ) -> Self {
        Self {
            txn,
            plane,
            registry: Some(registry),
        }
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

    fn vector_search(
        &self,
        label: Option<&str>,
        property: &str,
        query: &[f32],
        metric: Metric,
        k: usize,
    ) -> Result<Vec<Hit>> {
        let hits = indexed_or_brute(
            self,
            self.registry,
            self.plane,
            label,
            property,
            query,
            metric,
            k,
        )?;
        visible_hits(self, hits, label, |hit| NodeId(hit.id))
    }
}

/// Per-query memoizing [`GraphReader`] (arch/02): decoded `node`/`edge`/
/// `neighbors` results are cached for the life of one query, so a multi-hop
/// traversal that revisits a node decodes it once, not once per visit. Bound
/// to a single storage snapshot (one read txn), so it needs no MVCC
/// invalidation — the whole reader, and its caches, are dropped at query end.
///
/// Scans (`scan_all`/`scan_label`) and key lookups pass through uncached
/// (arch/02 §1: query/scan results are deliberately not cached). Interior
/// mutability (`RefCell`) is safe: query execution is single-threaded.
/// Adjacency cache key: a node's neighbours in a direction, optionally
/// filtered to one edge type.
type AdjKey = (NodeId, Dir, Option<String>);

pub struct CachedReader<'a> {
    txn: &'a dyn ReadTransaction,
    plane: PlaneId,
    registry: Option<&'a VectorRegistry>,
    /// Declared BM25 indexes (a read-locked view for the query's lifetime).
    /// `None` ⇒ `keyword_search` yields nothing — there is no unindexed path.
    keywords: Option<&'a KeywordRegistry>,
    /// Optional shared cross-query L2 (arch/02 §3). `None` ⇒ pure per-query
    /// (L1 only) — used by tests and the differential oracle.
    l2: Option<(&'a GraphCache, u64)>,
    /// This reader pins a past snapshot (AS OF) while the keyword registry
    /// describes the latest one, so `keyword_search` over-fetches from the
    /// live postings before filtering through the snapshot.
    historical: bool,
    // Per-query L1: fast intra-query hits and negative caching. Bound to this
    // reader's snapshot, dropped at query end.
    nodes: RefCell<AHashMap<NodeId, Option<Arc<NodeRecord>>>>,
    edges: RefCell<AHashMap<EdgeId, Option<Arc<EdgeRecord>>>>,
    adjacency: RefCell<AHashMap<AdjKey, Arc<[Neighbor]>>>,
}

impl<'a> CachedReader<'a> {
    pub fn new(txn: &'a dyn ReadTransaction, plane: PlaneId) -> Self {
        Self::build(txn, plane, None, None)
    }

    pub fn with_registry(
        txn: &'a dyn ReadTransaction,
        plane: PlaneId,
        registry: &'a VectorRegistry,
    ) -> Self {
        Self::build(txn, plane, Some(registry), None)
    }

    /// The full reader used by the query path: per-query L1 plus the shared,
    /// cross-query, seq-stamped L2 (`cache` at snapshot `seq`).
    pub(crate) fn with_cache(
        txn: &'a dyn ReadTransaction,
        plane: PlaneId,
        registry: &'a VectorRegistry,
        cache: &'a GraphCache,
        seq: u64,
    ) -> Self {
        Self::build(txn, plane, Some(registry), Some((cache, seq)))
    }

    /// Like [`with_cache`](Self::with_cache) but for a time-travel read
    /// (ROADMAP §4): no vector index, so every vector search takes the exact
    /// brute-force path over the pinned snapshot — the live HNSW index is
    /// built from the latest commit and can't answer a past one — and
    /// keyword searches (which have no unindexed path) filter the live BM25
    /// postings through the snapshot, over-fetching to compensate.
    pub(crate) fn with_cache_historical(
        txn: &'a dyn ReadTransaction,
        plane: PlaneId,
        cache: &'a GraphCache,
        seq: u64,
    ) -> Self {
        let mut reader = Self::build(txn, plane, None, Some((cache, seq)));
        reader.historical = true;
        reader
    }

    /// Attach the declared keyword indexes, enabling `keyword_search`. Chained
    /// onto a constructor rather than threaded through each one — only the
    /// query path needs it, and the two registries are locked separately.
    pub(crate) fn with_keywords(mut self, keywords: &'a KeywordRegistry) -> Self {
        self.keywords = Some(keywords);
        self
    }

    fn build(
        txn: &'a dyn ReadTransaction,
        plane: PlaneId,
        registry: Option<&'a VectorRegistry>,
        l2: Option<(&'a GraphCache, u64)>,
    ) -> Self {
        Self {
            txn,
            plane,
            registry,
            keywords: None,
            l2,
            historical: false,
            nodes: RefCell::new(AHashMap::new()),
            edges: RefCell::new(AHashMap::new()),
            adjacency: RefCell::new(AHashMap::new()),
        }
    }
}

impl GraphReader for CachedReader<'_> {
    fn plane(&self) -> PlaneId {
        self.plane
    }

    fn node(&self, id: NodeId) -> Result<Option<Arc<NodeRecord>>> {
        if let Some(v) = self.nodes.borrow().get(&id) {
            return Ok(v.clone()); // L1 (per-query), including a cached miss
        }
        if let Some((cache, seq)) = self.l2
            && let Some(node) = cache.node(self.plane, id.0, seq)
        {
            self.nodes.borrow_mut().insert(id, Some(node.clone()));
            return Ok(Some(node)); // L2 (cross-query, seq-valid)
        }
        let v = graph::get_node(self.txn, self.plane, id)?.map(Arc::new);
        if let (Some((cache, seq)), Some(node)) = (self.l2, &v) {
            cache.put_node(self.plane, id.0, seq, node.clone()); // only existing records
        }
        self.nodes.borrow_mut().insert(id, v.clone());
        Ok(v)
    }

    fn edge(&self, id: EdgeId) -> Result<Option<Arc<EdgeRecord>>> {
        if let Some(v) = self.edges.borrow().get(&id) {
            return Ok(v.clone());
        }
        if let Some((cache, seq)) = self.l2
            && let Some(edge) = cache.edge(self.plane, id.0, seq)
        {
            self.edges.borrow_mut().insert(id, Some(edge.clone()));
            return Ok(Some(edge));
        }
        let v = graph::get_edge(self.txn, self.plane, id)?.map(Arc::new);
        if let (Some((cache, seq)), Some(edge)) = (self.l2, &v) {
            cache.put_edge(self.plane, id.0, seq, edge.clone());
        }
        self.edges.borrow_mut().insert(id, v.clone());
        Ok(v)
    }

    fn neighbors(&self, id: NodeId, dir: Dir, ty: Option<&str>) -> Result<Arc<[Neighbor]>> {
        let key = (id, dir, ty.map(str::to_string));
        if let Some(v) = self.adjacency.borrow().get(&key) {
            return Ok(v.clone());
        }
        if let Some((cache, seq)) = self.l2
            && let Some(a) = cache.adj(self.plane, id.0, dir, ty, seq)
        {
            self.adjacency.borrow_mut().insert(key, a.clone());
            return Ok(a);
        }
        let a: Arc<[Neighbor]> = graph::neighbors(self.txn, self.plane, id, dir, ty)?.into();
        if let Some((cache, seq)) = self.l2 {
            cache.put_adj(self.plane, id.0, dir, ty, seq, a.clone());
        }
        self.adjacency.borrow_mut().insert(key, a.clone());
        Ok(a)
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

    fn vector_search(
        &self,
        label: Option<&str>,
        property: &str,
        query: &[f32],
        metric: Metric,
        k: usize,
    ) -> Result<Vec<Hit>> {
        let hits = indexed_or_brute(
            self,
            self.registry,
            self.plane,
            label,
            property,
            query,
            metric,
            k,
        )?;
        visible_hits(self, hits, label, |hit| NodeId(hit.id))
    }

    fn keyword_search(
        &self,
        label: &str,
        property: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<(NodeId, f32)>> {
        // A live read asks for exactly `k`: the only ids the filter can drop
        // are the skew window's, and a short page there is the honest answer.
        let fetch = if self.historical {
            k.saturating_mul(HISTORICAL_KEYWORD_OVERFETCH)
        } else {
            k
        };
        let hits = self
            .keywords
            .and_then(|reg| reg.search(self.plane, label, property, query, fetch))
            .unwrap_or_default();
        let mut hits = visible_hits(self, hits, Some(label), |hit| hit.0)?;
        hits.truncate(k);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::exec;
    use crate::compute::plan::{LogicalPlan, Source, Step};
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

    // ---- CachedReader ----------------------------------------------------

    /// A deterministic dense random graph in a fresh memory engine; returns it
    /// with a seed whose 3-hop neighbourhood revisits most of the graph.
    fn dense_graph(n: u64, fanout: u64) -> (MemoryEngine, NodeId) {
        let eng = MemoryEngine::new();
        let ids: Vec<NodeId>;
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            ids = (0..n)
                .map(|_| {
                    graph::create_node(&mut txn, PlaneId::STARTUP, &["N"], &Properties::new())
                        .unwrap()
                })
                .collect();
            let mut r: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut next = || {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                r
            };
            for &src in &ids {
                for _ in 0..fanout {
                    let dst = ids[(next() % n) as usize];
                    graph::create_edge(
                        &mut txn,
                        PlaneId::STARTUP,
                        src,
                        dst,
                        "E",
                        &Properties::new(),
                    )
                    .unwrap();
                }
            }
            txn.commit().unwrap();
        }
        (eng, ids[0])
    }

    fn traversal_plan(seed: NodeId) -> LogicalPlan {
        LogicalPlan {
            source: Source::SeekIds(vec![seed]),
            steps: vec![
                Step::ExpandVar {
                    dir: Dir::Out,
                    edge_type: None,
                    min: 1,
                    max: 3,
                },
                Step::Distinct,
            ],
            project: None,
        }
    }

    fn heads(reader: &dyn GraphReader, plan: &LogicalPlan) -> Vec<u64> {
        let mut v: Vec<u64> = exec::execute(plan, reader)
            .unwrap()
            .map(|r| r.unwrap().head.0)
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn cached_matches_uncached() {
        let (eng, seed) = dense_graph(300, 6);
        let txn = eng.begin_read().unwrap();
        let uncached = UncachedReader::new(&txn, PlaneId::STARTUP);
        let cached = CachedReader::new(&txn, PlaneId::STARTUP);

        // Direct reader methods agree (arch/02 §6 differential).
        for id in uncached.scan_all().unwrap() {
            assert_eq!(uncached.node(id).unwrap(), cached.node(id).unwrap());
            for dir in [Dir::Out, Dir::In, Dir::Both] {
                assert_eq!(
                    uncached.neighbors(id, dir, None).unwrap(),
                    cached.neighbors(id, dir, None).unwrap()
                );
            }
        }
        assert_eq!(uncached.scan_all().unwrap(), cached.scan_all().unwrap());

        // A revisit-heavy executor traversal yields identical rows, and
        // re-running through the warmed cache stays correct.
        let plan = traversal_plan(seed);
        assert_eq!(heads(&uncached, &plan), heads(&cached, &plan));
        assert_eq!(heads(&uncached, &plan), heads(&cached, &plan));
    }

    #[test]
    fn l2_shares_decoded_arcs_at_same_seq() {
        use crate::index::VectorRegistry;
        let (eng, seed) = dense_graph(50, 3);
        let txn = eng.begin_read().unwrap();
        let cache = GraphCache::new(1 << 20);
        let reg = VectorRegistry::new();

        // Two independent readers at the same seq: the second's node comes
        // from L2, so it's the *same* decoded Arc — proof of a cross-query hit.
        let r1 = CachedReader::with_cache(&txn, PlaneId::STARTUP, &reg, &cache, 7);
        let n1 = r1.node(seed).unwrap().unwrap();
        let r2 = CachedReader::with_cache(&txn, PlaneId::STARTUP, &reg, &cache, 7);
        let n2 = r2.node(seed).unwrap().unwrap();
        assert!(Arc::ptr_eq(&n1, &n2), "same seq ⇒ L2 hit ⇒ shared Arc");
        assert!(cache.weighted_size() > 0);

        // A reader at a newer seq must NOT get the stale entry: seq mismatch is
        // a miss, so it re-decodes a fresh Arc (snapshot isolation).
        let r3 = CachedReader::with_cache(&txn, PlaneId::STARTUP, &reg, &cache, 8);
        let n3 = r3.node(seed).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&n1, &n3), "newer seq ⇒ miss ⇒ fresh decode");
        assert_eq!(*n1, *n3, "same underlying record, just re-decoded");
    }

    #[test]
    fn index_hits_the_snapshot_cannot_see_are_dropped() {
        // The registries are updated after the KV commit publishes (and are
        // never rolled back to a past snapshot), so an index can name a node
        // a reader's snapshot lacks. Model the window directly: index a
        // node, then delete it in storage without telling the registries.
        use crate::index::VectorRegistry;
        use crate::keyword::KeywordRegistry;
        use crate::text::Language;
        use crate::types::{PropDesc, PropValue};

        let eng = MemoryEngine::new();
        let mut props = Properties::new();
        props.insert(
            "emb".into(),
            PropDesc::new(PropValue::Vector(vec![1.0, 0.0])),
        );
        props.insert(
            "body".into(),
            PropDesc::new(PropValue::Str("graph databases".into())),
        );
        let (kept, phantom) = {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            let kept = graph::create_node(&mut txn, PlaneId::STARTUP, &["Doc"], &props).unwrap();
            let phantom = graph::create_node(&mut txn, PlaneId::STARTUP, &["Doc"], &props).unwrap();
            txn.commit().unwrap();
            (kept, phantom)
        };
        let mut vectors = VectorRegistry::new();
        let mut keywords = KeywordRegistry::new();
        {
            let txn = eng.begin_read().unwrap();
            vectors
                .build_entry(&txn, PlaneId::STARTUP, "Doc", "emb", Metric::Cosine)
                .unwrap();
            keywords
                .build_entry(&txn, PlaneId::STARTUP, "Doc", "body", Language::English)
                .unwrap();
        }
        {
            let mut txn = eng.begin_write().unwrap();
            graph::delete_node(&mut txn, PlaneId::STARTUP, phantom).unwrap();
            txn.commit().unwrap();
        }

        let txn = eng.begin_read().unwrap();
        let cached =
            CachedReader::with_registry(&txn, PlaneId::STARTUP, &vectors).with_keywords(&keywords);
        let ids: Vec<u64> = cached
            .vector_search(Some("Doc"), "emb", &[1.0, 0.0], Metric::Cosine, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(
            ids,
            vec![kept.0],
            "the indexed-but-deleted node is not surfaced"
        );
        let ids: Vec<NodeId> = cached
            .keyword_search("Doc", "body", "graph", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![kept]);

        let uncached = UncachedReader::with_registry(&txn, PlaneId::STARTUP, &vectors);
        let ids: Vec<u64> = uncached
            .vector_search(Some("Doc"), "emb", &[1.0, 0.0], Metric::Cosine, 10)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, vec![kept.0], "the oracle agrees");
    }

    /// A chunky property map so postcard decode on read is non-trivial — the
    /// per-visit cost arch/02 says a cache should save.
    #[cfg(feature = "redb-backend")]
    fn fat_props() -> Properties {
        use crate::types::{PropDesc, PropValue};
        let mut p = Properties::new();
        for i in 0..12 {
            p.insert(
                format!("field_{i}"),
                PropDesc {
                    description: Some(format!("description of field {i}")),
                    value: PropValue::Str(format!("value-{i}-lorem-ipsum-dolor-sit-amet")),
                },
            );
        }
        p
    }

    #[cfg(feature = "redb-backend")]
    fn build_dense(txn: &mut dyn WriteTransaction, n: u64, fanout: u64) -> NodeId {
        graph::init(txn).unwrap();
        let ids: Vec<NodeId> = (0..n)
            .map(|_| graph::create_node(txn, PlaneId::STARTUP, &["N"], &fat_props()).unwrap())
            .collect();
        let mut r: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r
        };
        for &src in &ids {
            for _ in 0..fanout {
                let dst = ids[(next() % n) as usize];
                graph::create_edge(txn, PlaneId::STARTUP, src, dst, "E", &Properties::new())
                    .unwrap();
            }
        }
        ids[0]
    }

    /// Sizing measurement (arch/02 §5); not run by default. Run with
    /// `cargo test -p dr-strange-core --lib cache -- --ignored --nocapture`.
    /// Reports both a pure-expansion plan (adjacency reads) and an
    /// expand+filter plan (revisited node-record decodes) on the redb backend,
    /// where reads are B-tree + postcard decode — the case a cache can save.
    #[cfg(feature = "redb-backend")]
    #[test]
    #[ignore]
    fn bench_cached_vs_uncached() {
        use crate::compute::expr::has_label;
        use crate::storage::redb_backend::RedbEngine;
        use std::time::Instant;

        // Two regimes: a small/hot subgraph (deep walks revisit the same nodes
        // heavily — the cache's best case) and a broad one (low revisit — where
        // per-miss overhead can cost more than it saves).
        for (regime, nodes, fanout) in [("hot(150)", 150u64, 10u64), ("broad(3000)", 3000, 10)] {
            let dir = tempfile::tempdir().unwrap();
            let eng = RedbEngine::open(dir.path().join("bench.redb")).unwrap();
            let seed = {
                let mut txn = eng.begin_write().unwrap();
                let s = build_dense(&mut txn, nodes, fanout);
                txn.commit().unwrap();
                s
            };
            let txn = eng.begin_read().unwrap();

            let expand = traversal_plan(seed); // ExpandVar(1,3) + Distinct
            let mut filter = traversal_plan(seed);
            filter.steps.insert(1, Step::Filter(has_label("N"))); // reads node records

            for (name, plan) in [("expand", &expand), ("expand+filter", &filter)] {
                let iters = 100;
                let uncached = UncachedReader::new(&txn, PlaneId::STARTUP);
                let t = Instant::now();
                for _ in 0..iters {
                    let _ = exec::execute(plan, &uncached).unwrap().count();
                }
                let un = t.elapsed().as_secs_f64();

                let t = Instant::now();
                for _ in 0..iters {
                    let cached = CachedReader::new(&txn, PlaneId::STARTUP);
                    let _ = exec::execute(plan, &cached).unwrap().count();
                }
                let ca = t.elapsed().as_secs_f64();

                println!(
                    "{regime:<11} {name:<14} x{iters}: uncached {:>7.1} ms, cached {:>7.1} ms → {:.2}x",
                    un * 1000.0,
                    ca * 1000.0,
                    un / ca
                );
            }
        }
    }
}
