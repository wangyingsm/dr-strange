//! Pull-based iterator executor (arch/03 §3), v0.
//!
//! A plan runs as a chain of iterator adapters over one [`GraphReader`]
//! snapshot. Most steps are lazy — `Limit` short-circuits, so an
//! `expand → filter → limit(10)` pipeline stops after ten matches rather
//! than materializing the whole frontier. Two steps are barriers: `Sort`
//! (obviously) and the source scan (v0 materializes the source id list; the
//! pipeline over it is still lazy — arch/03 §2 "start scalar").
//!
//! Rows follow the linear-pipeline model (arch/03 §2): each row is a current
//! node plus the trail of `(edge, node)` hops taken to reach it. `Filter` and
//! `Sort` address the current node by default — and, through `Expr::At`, any
//! earlier binding, which the trail was already carrying (see
//! [`resolve_bindings`]).

use ahash::{AHashMap, AHashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use crate::cache::GraphReader;
use crate::compute::expr::{self, BindingNeed, Expr};
use crate::compute::plan::{Algo, LogicalPlan, NodeRef, SortKey, Source, Step};
use crate::compute::{algo, hybrid};
use crate::error::Error;
use crate::error::Result;
use crate::storage::vector::{Metric, top_k};
use crate::types::{Dir, EdgeId, EdgeRecord, NodeId, NodeRecord, PropValue};

/// One hop of a path — `(edge traversed, node reached)` plus a link to the
/// previous hop. Rows that branch from a common prefix *share* that prefix by
/// `Rc`, so extending a path is O(1) (an `Rc` bump) instead of cloning an
/// O(hops) `Vec`. It's an immutable cons-list: the whole path is recovered by
/// walking `prev` (see [`Row::path`]). Confined to one query's executor run,
/// which is single-threaded, so `Rc` (not `Arc`) is the right cost.
#[derive(Debug, PartialEq)]
struct TrailNode {
    edge: EdgeId,
    node: NodeId,
    prev: Option<Rc<TrailNode>>,
}

/// One row of the executor's stream: the current node, the path to it, and an
/// optional similarity **score channel** (arch/03 §2, §4.5) set by the hybrid
/// operators and readable in expressions via `score()`.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub head: NodeId,
    /// The node the row started at — pattern slot 0.
    ///
    /// The trail records hop *destinations* only, so the source is the one
    /// binding it cannot reconstruct. Held as a plain id (8 bytes, no
    /// allocation) because every row needs it the moment a query names an
    /// earlier variable.
    pub origin: NodeId,
    /// Path back to the source as a shared cons-list; `None` at a source.
    /// Private: reach it through [`Row::hops`] / [`Row::path`] so the O(1)-step
    /// representation can change without touching callers.
    trail: Option<Rc<TrailNode>>,
    /// Hop count from the source (== path length). Held as a counter so
    /// [`Row::hops`] is O(1) and never walks the list.
    hops: u32,
    pub score: Option<f32>,
}

impl Row {
    fn start(head: NodeId) -> Self {
        Row {
            head,
            origin: head,
            trail: None,
            hops: 0,
            score: None,
        }
    }

    /// A source row that already carries a score (vector-search seeds).
    fn scored(head: NodeId, score: f32) -> Self {
        Row {
            head,
            origin: head,
            trail: None,
            hops: 0,
            score: Some(score),
        }
    }

    /// Extend the path by one hop; the score channel is inherited so a seed's
    /// similarity survives expansion (arch/03 §4.1). O(1): the new cons-cell
    /// points at the shared prefix rather than copying it.
    fn step(&self, edge: EdgeId, node: NodeId) -> Self {
        Row {
            head: node,
            origin: self.origin,
            trail: Some(Rc::new(TrailNode {
                edge,
                node,
                prev: self.trail.clone(),
            })),
            hops: self.hops + 1,
            score: self.score,
        }
    }

    /// The same row with its score channel replaced (rerank operators).
    fn with_score(mut self, score: f32) -> Self {
        self.score = Some(score);
        self
    }

    /// Hops from the source — the path length. O(1).
    pub fn hops(&self) -> usize {
        self.hops as usize
    }

    /// The full path from the source as `(edge, node)` pairs, in traversal
    /// order. O(hops); allocates, so call it only when a query actually needs
    /// the path (path-returning queries — not yet a surface).
    pub fn path(&self) -> Vec<(EdgeId, NodeId)> {
        let mut out = Vec::with_capacity(self.hops as usize);
        let mut cur = self.trail.as_deref();
        while let Some(node) = cur {
            out.push((node.edge, node.node));
            cur = node.prev.as_deref();
        }
        out.reverse();
        out
    }

    fn ctx<'a>(
        &self,
        node: Option<&'a NodeRecord>,
        bindings: expr::Bindings<'a>,
    ) -> expr::EvalCtx<'a> {
        expr::EvalCtx {
            node,
            score: self.score,
            hops: self.hops as usize,
            bindings,
            ..Default::default()
        }
    }
}

/// One row's bindings, owned so an [`expr::Bindings`] can borrow them for the
/// length of an evaluation. Empty — and free — for a plan that names none.
#[derive(Default)]
struct Resolved {
    nodes: Vec<Option<Arc<NodeRecord>>>,
    edges: Vec<Option<(Arc<EdgeRecord>, Dir)>>,
    last: Option<(Arc<EdgeRecord>, Dir)>,
}

impl Resolved {
    fn view(&self) -> expr::Bindings<'_> {
        expr::Bindings {
            nodes: &self.nodes,
            edges: &self.edges,
            edge: self.last.as_ref().map(|(e, d)| (&**e, *d)),
        }
    }
}

/// Load the bindings `need` asks for out of one row's trail.
///
/// Lookups, not traversal: the trail already holds every hop, so slot 0 is the
/// row's origin, slot *i* the destination of hop *i-1*, and hop *i* the edge
/// between them. Nothing is resolved for a plan that names no binding, which
/// is the overwhelmingly common case.
fn resolve_bindings(reader: &dyn GraphReader, row: &Row, need: &BindingNeed) -> Result<Resolved> {
    let mut out = Resolved::default();
    if !need.any() {
        return Ok(out);
    }
    let path = row.path();
    // Slot 0 is where the row started; every later slot is a hop's destination.
    let node_at = |slot: usize| -> NodeId {
        if slot == 0 {
            row.origin
        } else {
            path[slot - 1].1
        }
    };

    if let Some(max) = need.nodes {
        for slot in 0..=max as usize {
            // A slot past this row's length was never bound — a shorter walk
            // through a variable-length hop, say. `None` reads as `Null`.
            out.nodes.push(match slot <= path.len() {
                true => reader.node(node_at(slot))?,
                false => None,
            });
        }
    }
    if let Some(max) = need.edges {
        for hop in 0..=max as usize {
            out.edges.push(match path.get(hop) {
                Some(&(edge, dst)) => load_edge(reader, edge, node_at(hop), dst)?,
                None => None,
            });
        }
    }
    if need.last_edge
        && let Some(hop) = path.len().checked_sub(1)
    {
        let (edge, dst) = path[hop];
        out.last = load_edge(reader, edge, node_at(hop), dst)?;
    }
    Ok(out)
}

/// One hop's edge plus the direction the row actually walked it: `Out` when
/// the edge runs from the hop's source to its destination, `In` when it runs
/// the other way.
///
/// Deriving it here rather than recording it on every `step` keeps the hot
/// expansion path free of state it almost never needs. The cost is one case
/// the endpoints cannot settle — a self-loop satisfies both readings — which
/// reports `Out`, as [`expr::Expr::EdgeDir`] documents.
fn load_edge(
    reader: &dyn GraphReader,
    edge: EdgeId,
    from: NodeId,
    to: NodeId,
) -> Result<Option<(Arc<EdgeRecord>, Dir)>> {
    Ok(reader.edge(edge)?.map(|e| {
        let dir = if e.src == from && e.dst == to {
            Dir::Out
        } else {
            Dir::In
        };
        (e, dir)
    }))
}

type RowIter<'r> = Box<dyn Iterator<Item = Result<Row>> + 'r>;

/// Rows between deadline checks. A clock read per row is real overhead on a
/// pipeline that moves millions, and the granularity buys nothing: at any
/// plausible per-row cost 1024 rows is well under a millisecond.
const DEADLINE_CHECK_EVERY: u32 = 1024;

/// Stops a pipeline once `deadline` passes, yielding [`Error::Timeout`] and
/// then ending.
///
/// Cooperative and row-paced, so it bounds work that *flows* — expansions,
/// filters, the drain a `Sort` performs internally. It cannot interrupt a
/// single call that is already running: a `scan_all` over a huge plane
/// materializes inside the source (see the module note on barriers), and the
/// graph algorithms in [`super::algo`] iterate without producing rows. Those
/// need their own checks; this bounds the query pipeline, which is what an
/// agent's runaway `MATCH` actually is.
struct Deadlined<'r> {
    inner: RowIter<'r>,
    deadline: Instant,
    since_check: u32,
    tripped: bool,
}

impl Iterator for Deadlined<'_> {
    type Item = Result<Row>;

    fn next(&mut self) -> Option<Self::Item> {
        // Once tripped the stream is over: a caller that keeps polling must
        // not get a fresh error every time, nor silently resume.
        if self.tripped {
            return None;
        }
        if self.since_check == 0 || self.since_check >= DEADLINE_CHECK_EVERY {
            self.since_check = 0;
            if Instant::now() >= self.deadline {
                self.tripped = true;
                return Some(Err(Error::Timeout(
                    "query exceeded its time budget; narrow it (add a label, a filter, or LIMIT) \
                     or raise `[server] query_timeout_secs`"
                        .into(),
                )));
            }
        }
        self.since_check += 1;
        self.inner.next()
    }
}

fn deadlined<'r>(iter: RowIter<'r>, deadline: Option<Instant>) -> RowIter<'r> {
    match deadline {
        None => iter,
        Some(deadline) => Box::new(Deadlined {
            inner: iter,
            deadline,
            since_check: 0,
            tripped: false,
        }),
    }
}

/// Builds the row stream for `plan` reading through `reader`. The returned
/// iterator borrows `reader` for its whole lifetime.
pub fn execute<'r>(plan: &LogicalPlan, reader: &'r dyn GraphReader) -> Result<RowIter<'r>> {
    execute_with(plan, reader, None)
}

/// [`execute`], stopping with [`Error::Timeout`] once `deadline` passes.
///
/// The check wraps the source *and* the finished pipeline. Wrapping the source
/// is what makes a barrier step bounded — a `Sort` drains its input before
/// yielding anything, and that drain pulls through the source check.
pub fn execute_with<'r>(
    plan: &LogicalPlan,
    reader: &'r dyn GraphReader,
    deadline: Option<Instant>,
) -> Result<RowIter<'r>> {
    let source: RowIter<'r> = Box::new(source_rows(reader, &plan.source)?.into_iter().map(Ok));
    let mut iter = deadlined(source, deadline);
    // Once per plan, not once per row: which bindings any expression names.
    let need = plan.binding_need();
    for step in &plan.steps {
        iter = apply_step(iter, step, reader, need)?;
    }
    Ok(deadlined(iter, deadline))
}

fn source_rows(reader: &dyn GraphReader, source: &Source) -> Result<Vec<Row>> {
    let ids: Vec<NodeId> = match source {
        Source::ScanAll => reader.scan_all()?,
        Source::ScanLabel(label) => reader.scan_label(label)?,
        Source::SeekIds(ids) => {
            // Keep only ids that actually resolve to a node in this plane, so
            // downstream steps never see a phantom current node.
            let mut out = Vec::new();
            for &id in ids {
                if reader.node(id)?.is_some() {
                    out.push(id);
                }
            }
            out
        }
        Source::SeekKeys(keys) => {
            let mut out = Vec::new();
            for key in keys {
                if let Some(id) = reader.node_id_by_key(key)? {
                    out.push(id);
                }
            }
            out
        }
        Source::VectorTopK {
            label,
            property,
            query,
            metric,
            k,
        } => {
            // Global similarity search — index-accelerated when a matching
            // index is declared, exact brute force otherwise. The reader
            // decides (arch/01 §5); either way the result is `(id, distance)`.
            let hits =
                reader.vector_search(label.as_deref(), property, query, *metric, *k as usize)?;
            return Ok(hits
                .into_iter()
                .map(|hit| {
                    Row::scored(
                        NodeId(hit.id),
                        metric.similarity_from_distance(hit.distance),
                    )
                })
                .collect());
        }
        Source::KeywordTopK {
            label,
            property,
            query,
            k,
        } => {
            // BM25 over the declared keyword index; the relevance score seeds
            // the row's score channel, as a vector seed's similarity does.
            let hits = reader.keyword_search(label, property, query, *k as usize)?;
            return Ok(hits
                .into_iter()
                .map(|(id, score)| Row::scored(id, score))
                .collect());
        }
        Source::Hybrid(spec) => {
            return Ok(hybrid::run(reader, spec)?
                .into_iter()
                .map(|hit| Row::scored(hit.node, hit.score))
                .collect());
        }
        Source::Algo { label, algo } => {
            return algo_rows(reader, label.as_deref(), algo);
        }
    };
    Ok(ids.into_iter().map(Row::start).collect())
}

/// Run a graph algorithm and turn its result into source rows. Each algorithm
/// puts its per-node result in the score channel; the row *order* is the
/// algorithm's natural one (rank order, community order, path order), which a
/// query can still override with `ORDER BY`.
fn algo_rows(reader: &dyn GraphReader, label: Option<&str>, spec: &Algo) -> Result<Vec<Row>> {
    Ok(match spec {
        Algo::PageRank {
            damping,
            max_iters,
            tolerance,
        } => algo::pagerank(
            reader,
            label,
            algo::PageRankOptions {
                damping: *damping,
                max_iters: *max_iters,
                tolerance: *tolerance,
            },
        )?
        .into_iter()
        .map(|(id, rank)| Row::scored(id, rank as f32))
        .collect(),
        Algo::ConnectedComponents => grouped_rows(algo::connected_components(reader, label)?.0),
        Algo::Louvain {
            max_levels,
            min_gain,
        } => grouped_rows(
            algo::louvain(
                reader,
                label,
                algo::LouvainOptions {
                    max_levels: *max_levels,
                    min_gain: *min_gain,
                },
            )?
            .0,
        ),
        Algo::ShortestPath {
            from,
            to,
            dir,
            weight,
        } => {
            let (Some(src), Some(dst)) = (resolve_ref(reader, from)?, resolve_ref(reader, to)?)
            else {
                return Ok(Vec::new()); // an unknown endpoint yields no path
            };
            let opts = algo::ShortestPathOptions {
                dir: *dir,
                weight: weight.clone(),
            };
            match algo::shortest_path(reader, label, src, dst, &opts)? {
                Some(path) => path
                    .nodes
                    .into_iter()
                    .enumerate()
                    .map(|(i, id)| Row::scored(id, i as f32))
                    .collect(),
                None => Vec::new(),
            }
        }
    })
}

/// Turn `(node, community representative)` pairs into rows grouped by
/// community, scoring each node with a dense 0-based community index assigned
/// in order of first appearance.
fn grouped_rows(assignments: Vec<(NodeId, NodeId)>) -> Vec<Row> {
    let mut index: AHashMap<NodeId, usize> = AHashMap::new();
    let mut rows: Vec<(usize, NodeId)> = Vec::with_capacity(assignments.len());
    for (node, community) in assignments {
        let next = index.len();
        let idx = *index.entry(community).or_insert(next);
        rows.push((idx, node));
    }
    rows.sort_unstable();
    rows.into_iter()
        .map(|(idx, node)| Row::scored(node, idx as f32))
        .collect()
}

/// Resolve a plan's node reference to an id in this plane; `None` when the key
/// (or id) doesn't exist here.
fn resolve_ref(reader: &dyn GraphReader, r: &NodeRef) -> Result<Option<NodeId>> {
    match r {
        NodeRef::Id(id) => Ok(reader.node(*id)?.map(|_| *id)),
        NodeRef::Key(key) => reader.node_id_by_key(key),
    }
}

/// Score `candidates` (a graph frontier) by similarity of their `property`
/// vector to `query`, returning the top-`k` as scored rows. Used by
/// `FrontierTopK` — always exact over the (usually small) frontier, matching
/// arch/03 §4.3's "small frontier ⇒ brute force" guidance.
fn vector_top_k_rows(
    reader: &dyn GraphReader,
    candidates: &[NodeId],
    property: &str,
    query: &[f32],
    metric: Metric,
    k: usize,
) -> Result<Vec<Row>> {
    let mut items: Vec<(u64, f32)> = Vec::new();
    for &id in candidates {
        if let Some(v) = node_vector(reader, id, property)? {
            let d = metric.distance(query, &v);
            // Non-finite distance = dimension mismatch (arch/01 §5); skip it,
            // the same way the evaluator drops incomparable values.
            if d.is_finite() {
                items.push((id.0, d));
            }
        }
    }
    // top_k picks smallest distances; convert distance→similarity for the
    // score channel so higher = closer (arch/03 §4.5).
    Ok(top_k(items.into_iter(), k)
        .into_iter()
        .map(|hit| {
            Row::scored(
                NodeId(hit.id),
                metric.similarity_from_distance(hit.distance),
            )
        })
        .collect())
}

fn apply_step<'r>(
    iter: RowIter<'r>,
    step: &Step,
    reader: &'r dyn GraphReader,
    need: BindingNeed,
) -> Result<RowIter<'r>> {
    Ok(match step {
        Step::Expand { dir, edge_type } => {
            let dir = *dir;
            let ty = edge_type.clone();
            Box::new(iter.flat_map(move |rr| expand_one(reader, rr, dir, &ty)))
        }
        Step::ExpandVar {
            dir,
            edge_type,
            min,
            max,
        } => {
            let (dir, ty, min, max) = (*dir, edge_type.clone(), *min, *max);
            Box::new(iter.flat_map(move |rr| match rr {
                Err(e) => vec![Err(e)].into_iter(),
                Ok(row) => expand_var(reader, row, dir, &ty, min, max).into_iter(),
            }))
        }
        Step::Filter(expr) => {
            let pred = expr.clone();
            Box::new(iter.filter_map(move |rr| filter_one(reader, rr, &pred, &need)))
        }
        Step::Skip(n) => Box::new(iter.skip(*n as usize)),
        Step::Limit(n) => Box::new(iter.take(*n as usize)),
        Step::Distinct => {
            let mut seen: AHashSet<NodeId> = AHashSet::new();
            Box::new(iter.filter_map(move |rr| match rr {
                Err(e) => Some(Err(e)),
                Ok(row) => seen.insert(row.head).then_some(Ok(row)),
            }))
        }
        // Barrier: drain, sort, re-emit.
        Step::Sort(keys) => Box::new(sort_rows(iter, keys, reader, &need)?.into_iter().map(Ok)),
        // Barrier: rank the whole frontier by similarity, keep top-k.
        Step::FrontierTopK {
            property,
            query,
            metric,
            k,
        } => {
            let frontier = drain(iter)?;
            let ids: Vec<NodeId> = frontier.iter().map(|r| r.head).collect();
            let ranked = vector_top_k_rows(reader, &ids, property, query, *metric, *k as usize)?;
            // vector_top_k_rows makes fresh scored rows (no trail); re-attach
            // each winner's original trail so path info survives the rerank.
            let mut by_head: ahash::AHashMap<NodeId, Row> =
                frontier.into_iter().map(|r| (r.head, r)).collect();
            let out: Vec<Result<Row>> = ranked
                .into_iter()
                .map(|scored| {
                    let score = scored.score.expect("ranked rows are scored");
                    let base = by_head.remove(&scored.head).unwrap_or(scored);
                    Ok(base.with_score(score))
                })
                .collect();
            Box::new(out.into_iter())
        }
        Step::ExpandBeam {
            dir,
            edge_type,
            property,
            query,
            metric,
            width,
            depth,
        } => {
            let frontier = drain(iter)?;
            let out = expand_beam(
                reader,
                frontier,
                *dir,
                edge_type,
                property,
                query,
                *metric,
                *width as usize,
                *depth,
            )?;
            Box::new(out.into_iter().map(Ok))
        }
    })
}

/// Drains a row stream, propagating the first error (used by barrier steps).
fn drain(iter: RowIter<'_>) -> Result<Vec<Row>> {
    iter.collect()
}

/// Similarity-guided beam search (arch/03 §4.4). At each of `depth` steps,
/// expand every frontier row, score each neighbor's `property` against
/// `query`, keep the globally-best `width` as the next frontier, and emit
/// them. Neighbors lacking the vector property score `-inf` (sink out unless
/// the beam is wider than the candidate set). Walk semantics: a node may be
/// revisited across steps; callers add `Distinct`.
#[allow(clippy::too_many_arguments)]
fn expand_beam(
    reader: &dyn GraphReader,
    start: Vec<Row>,
    dir: Dir,
    edge_type: &Option<String>,
    property: &str,
    query: &[f32],
    metric: Metric,
    width: usize,
    depth: u32,
) -> Result<Vec<Row>> {
    let mut emitted: Vec<Row> = Vec::new();
    let mut frontier = start;
    for _ in 0..depth {
        // Score every neighbor of the current frontier.
        let mut candidates: Vec<(f32, Row)> = Vec::new();
        for row in &frontier {
            for n in reader
                .neighbors(row.head, dir, edge_type.as_deref())?
                .iter()
            {
                let sim = match node_vector(reader, n.node, property)? {
                    Some(v) => metric.similarity(query, &v),
                    None => f32::NEG_INFINITY,
                };
                candidates.push((sim, row.step(n.edge, n.node).with_score(sim)));
            }
        }
        if candidates.is_empty() {
            break;
        }
        // Keep the best `width` by similarity (descending).
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
        candidates.truncate(width);
        frontier = candidates.into_iter().map(|(_, r)| r).collect();
        emitted.extend(frontier.iter().cloned());
    }
    Ok(emitted)
}

fn expand_one(
    reader: &dyn GraphReader,
    rr: Result<Row>,
    dir: Dir,
    ty: &Option<String>,
) -> Box<dyn Iterator<Item = Result<Row>>> {
    let row = match rr {
        Ok(r) => r,
        Err(e) => return Box::new(std::iter::once(Err(e))),
    };
    match reader.neighbors(row.head, dir, ty.as_deref()) {
        Err(e) => Box::new(std::iter::once(Err(e))),
        // Lazy: `step` is O(1) and the neighbour rows are produced on demand,
        // so a following `Limit` can stop without materialising the whole
        // (possibly high) fan-out of a single node. The iterator owns the
        // shared neighbour slice (`Arc`) and the source `row` and indexes into
        // them — nothing borrows a local, so it can outlive this call.
        Ok(ns) => {
            let len = ns.len();
            Box::new((0..len).map(move |i| {
                let n = &ns[i];
                Ok(row.step(n.edge, n.node))
            }))
        }
    }
}

/// Variable-length expansion, **walk semantics** (arch/03 §8 open question):
/// a node/edge may be revisited; the only bound is depth ∈ `min..=max`. Emits
/// one row per distinct walk. Callers wanting uniqueness add `Distinct`.
///
/// DFS with an explicit stack; a `neighbors` error ends that branch as an
/// `Err` row rather than aborting the whole expansion.
fn expand_var(
    reader: &dyn GraphReader,
    start: Row,
    dir: Dir,
    ty: &Option<String>,
    min: u32,
    max: u32,
) -> Vec<Result<Row>> {
    let mut out = Vec::new();
    let mut stack = vec![(start, 0u32)];
    while let Some((row, depth)) = stack.pop() {
        if depth >= min && depth <= max {
            out.push(Ok(row.clone()));
        }
        if depth < max {
            match reader.neighbors(row.head, dir, ty.as_deref()) {
                Err(e) => out.push(Err(e)),
                Ok(ns) => {
                    for n in ns.iter() {
                        stack.push((row.step(n.edge, n.node), depth + 1));
                    }
                }
            }
        }
    }
    out
}

fn filter_one(
    reader: &dyn GraphReader,
    rr: Result<Row>,
    pred: &Expr,
    need: &BindingNeed,
) -> Option<Result<Row>> {
    let row = match rr {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    match matches_pred(reader, &row, pred, need) {
        Err(e) => Some(Err(e)),
        Ok(true) => Some(Ok(row)),
        Ok(false) => None,
    }
}

/// Evaluate `exprs` against one row — the projection primitive, shared by the
/// `select` terminal and (from B1) the plan's projection tail, so a column
/// means the same thing however the query asked for it.
pub fn row_values(
    reader: &dyn GraphReader,
    row: &Row,
    exprs: &[Expr],
    need: &BindingNeed,
) -> Result<Vec<PropValue>> {
    let node = reader.node(row.head)?;
    let bound = resolve_bindings(reader, row, need)?;
    let ctx = row.ctx(node.as_deref(), bound.view());
    Ok(exprs.iter().map(|e| expr::eval(e, &ctx)).collect())
}

/// Whether one row satisfies `pred`: read the current node, resolve whatever
/// bindings the plan names, evaluate. Split out from [`filter_one`] so the two
/// reads can fail with `?` while the caller still decides what a failing row
/// means for the stream.
fn matches_pred(
    reader: &dyn GraphReader,
    row: &Row,
    pred: &Expr,
    need: &BindingNeed,
) -> Result<bool> {
    let node = reader.node(row.head)?;
    let bound = resolve_bindings(reader, row, need)?;
    let ctx = row.ctx(node.as_deref(), bound.view());
    Ok(expr::is_true(&expr::eval(pred, &ctx)))
}

fn sort_rows(
    iter: RowIter<'_>,
    keys: &[SortKey],
    reader: &dyn GraphReader,
    need: &BindingNeed,
) -> Result<Vec<Row>> {
    // Decorate each row with its precomputed key values (one node lookup +
    // eval per row), sort, undecorate — so comparisons don't re-evaluate.
    let mut decorated: Vec<(Vec<PropValue>, Row)> = Vec::new();
    for rr in iter {
        let row = rr?;
        let node = reader.node(row.head)?;
        let bound = resolve_bindings(reader, &row, need)?;
        let ctx = row.ctx(node.as_deref(), bound.view());
        let vals = keys.iter().map(|k| expr::eval(&k.expr, &ctx)).collect();
        decorated.push((vals, row));
    }
    decorated.sort_by(|a, b| cmp_keys(&a.0, &b.0, keys));
    Ok(decorated.into_iter().map(|(_, row)| row).collect())
}

/// Reads the current node's `property` as a vector, or `None` if absent /
/// not a vector — the exact (record-backed) path for hybrid operators.
fn node_vector(reader: &dyn GraphReader, head: NodeId, property: &str) -> Result<Option<Vec<f32>>> {
    let Some(node) = reader.node(head)? else {
        return Ok(None);
    };
    Ok(match node.properties.get(property).map(|p| &p.value) {
        Some(PropValue::Vector(v)) => Some(v.clone()),
        _ => None,
    })
}

fn cmp_keys(a: &[PropValue], b: &[PropValue], keys: &[SortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (i, key) in keys.iter().enumerate() {
        // The canonical total order: NaN and mismatched types get a real,
        // consistent position. (`partial_cmp(..).unwrap_or(Equal)` was NOT
        // total — NaN "equal" to floats that order among themselves — and
        // std's sort panics when it detects such a comparator.)
        let ord = expr::total_cmp(&a[i], &b[i]);
        let ord = if key.descending { ord.reverse() } else { ord };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::UncachedReader;
    use crate::compute::expr::{at_edge, at_node, edge_dir, edge_type, ep, has_label, p};
    use crate::compute::plan::SortKey;
    use crate::storage::engine::{StorageEngine, WriteTransaction};
    use crate::storage::graph;
    use crate::storage::memory::MemoryEngine;
    use crate::types::{PlaneId, PropDesc, Properties};

    fn props(entries: &[(&str, PropValue)]) -> Properties {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
            .collect()
    }

    /// Runs `plan` against a freshly-built graph and returns the head ids.
    fn run(build: impl FnOnce(&mut dyn WriteTransaction), plan: &LogicalPlan) -> Vec<NodeId> {
        let eng = MemoryEngine::new();
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            build(&mut txn);
            txn.commit().unwrap();
        }
        let txn = eng.begin_read().unwrap();
        let reader = UncachedReader::new(&txn, PlaneId::STARTUP);
        execute(plan, &reader)
            .unwrap()
            .map(|r| r.unwrap().head)
            .collect()
    }

    #[test]
    fn scan_all_and_scan_label() {
        let plan = LogicalPlan::new(Source::ScanLabel("Paper".into()));
        let ids = run(
            |txn| {
                graph::create_node(txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
                graph::create_node(txn, PlaneId::STARTUP, &["Person"], &Properties::new()).unwrap();
                graph::create_node(txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
            },
            &plan,
        );
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn expand_then_filter() {
        // a -CITES-> {b(2020), c(1999)}; scan a, expand, keep year >= 2020
        let mut plan = LogicalPlan::new(Source::ScanLabel("Root".into()));
        plan.push(Step::Expand {
            dir: Dir::Out,
            edge_type: Some("CITES".into()),
        });
        plan.push(Step::Filter(p("year").ge(2020)));

        let ids = run(
            |txn| {
                let a = graph::create_node(txn, PlaneId::STARTUP, &["Root"], &Properties::new())
                    .unwrap();
                let b = graph::create_node(
                    txn,
                    PlaneId::STARTUP,
                    &["Paper"],
                    &props(&[("year", PropValue::Int(2020))]),
                )
                .unwrap();
                let c = graph::create_node(
                    txn,
                    PlaneId::STARTUP,
                    &["Paper"],
                    &props(&[("year", PropValue::Int(1999))]),
                )
                .unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a, b, "CITES", &Properties::new())
                    .unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a, c, "CITES", &Properties::new())
                    .unwrap();
            },
            &plan,
        );
        assert_eq!(ids.len(), 1); // only b
    }

    #[test]
    fn limit_short_circuits() {
        let mut plan = LogicalPlan::new(Source::ScanAll);
        plan.push(Step::Limit(3));
        let ids = run(
            |txn| {
                for _ in 0..10 {
                    graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
                }
            },
            &plan,
        );
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn skip_then_limit() {
        let mut plan = LogicalPlan::new(Source::ScanAll);
        plan.push(Step::Skip(2));
        plan.push(Step::Limit(3));
        let ids = run(
            |txn| {
                for _ in 0..10 {
                    graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
                }
            },
            &plan,
        );
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn distinct_dedups_by_head() {
        // b is cited by both a1 and a2; expanding from both yields b twice,
        // Distinct collapses it.
        let mut plan = LogicalPlan::new(Source::ScanLabel("Root".into()));
        plan.push(Step::Expand {
            dir: Dir::Out,
            edge_type: Some("CITES".into()),
        });
        plan.push(Step::Distinct);
        let ids = run(
            |txn| {
                let a1 = graph::create_node(txn, PlaneId::STARTUP, &["Root"], &Properties::new())
                    .unwrap();
                let a2 = graph::create_node(txn, PlaneId::STARTUP, &["Root"], &Properties::new())
                    .unwrap();
                let b = graph::create_node(txn, PlaneId::STARTUP, &["Paper"], &Properties::new())
                    .unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a1, b, "CITES", &Properties::new())
                    .unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a2, b, "CITES", &Properties::new())
                    .unwrap();
            },
            &plan,
        );
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn sort_ascending_and_descending() {
        let build = |txn: &mut dyn WriteTransaction| {
            for y in [2010, 1999, 2020] {
                graph::create_node(
                    txn,
                    PlaneId::STARTUP,
                    &["Paper"],
                    &props(&[("year", PropValue::Int(y))]),
                )
                .unwrap();
            }
        };

        let mut asc = LogicalPlan::new(Source::ScanLabel("Paper".into()));
        asc.push(Step::Sort(vec![SortKey {
            expr: p("year"),
            descending: false,
        }]));
        // resolve years by re-reading through a second run is awkward; instead
        // assert the head order corresponds to sorted years via a fresh graph.
        let eng = MemoryEngine::new();
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            build(&mut txn);
            txn.commit().unwrap();
        }
        let txn = eng.begin_read().unwrap();
        let reader = UncachedReader::new(&txn, PlaneId::STARTUP);

        let years = |plan: &LogicalPlan| -> Vec<i64> {
            execute(plan, &reader)
                .unwrap()
                .map(|r| {
                    let head = r.unwrap().head;
                    match &reader.node(head).unwrap().unwrap().properties["year"].value {
                        PropValue::Int(y) => *y,
                        _ => panic!(),
                    }
                })
                .collect()
        };
        assert_eq!(years(&asc), vec![1999, 2010, 2020]);

        let mut desc = LogicalPlan::new(Source::ScanLabel("Paper".into()));
        desc.push(Step::Sort(vec![SortKey {
            expr: p("year"),
            descending: true,
        }]));
        assert_eq!(years(&desc), vec![2020, 2010, 1999]);
    }

    /// Soft-schema sort keys: NaN and mixed types must sort deterministically,
    /// not panic. (`partial_cmp(..).unwrap_or(Equal)` was a non-total
    /// comparator — NaN "equal" to floats that order among themselves — which
    /// std's sort detects and panics on.)
    #[test]
    fn sort_with_nan_and_mixed_types_is_total_not_a_panic() {
        let values = [
            PropValue::Float(f64::NAN),
            PropValue::Float(2.0),
            PropValue::Str("s".into()),
            PropValue::Float(1.0),
            PropValue::Null,
            PropValue::Float(f64::NAN),
            PropValue::Int(3),
        ];
        let mut plan = LogicalPlan::new(Source::ScanLabel("X".into()));
        plan.push(Step::Sort(vec![SortKey {
            expr: p("v"),
            descending: false,
        }]));
        let ids = run(
            |txn| {
                for v in &values {
                    graph::create_node(txn, PlaneId::STARTUP, &["X"], &props(&[("v", v.clone())]))
                        .unwrap();
                }
            },
            &plan,
        );
        // All rows survive, in the canonical order: Null, then numbers
        // ascending with NaN past everything, then strings.
        assert_eq!(ids.len(), values.len());
        let rank_of = |id: NodeId| values[(id.0 - 1) as usize].clone();
        let sorted: Vec<PropValue> = ids.into_iter().map(rank_of).collect();
        assert!(matches!(sorted[0], PropValue::Null));
        assert!(matches!(sorted[1], PropValue::Float(f) if f == 1.0));
        assert!(matches!(sorted[2], PropValue::Float(f) if f == 2.0));
        assert!(matches!(sorted[3], PropValue::Int(3)));
        assert!(matches!(sorted[4], PropValue::Float(f) if f.is_nan()));
        assert!(matches!(sorted[5], PropValue::Float(f) if f.is_nan()));
        assert!(matches!(sorted[6], PropValue::Str(_)));
    }

    #[test]
    fn expand_var_walk_within_depth_bounds() {
        // chain a -> b -> c -> d ; from a, 1..=2 hops reaches b and c.
        let mut plan = LogicalPlan::new(Source::SeekIds(vec![]));
        plan.push(Step::ExpandVar {
            dir: Dir::Out,
            edge_type: None,
            min: 1,
            max: 2,
        });

        let eng = MemoryEngine::new();
        let a;
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            a = graph::create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            let b =
                graph::create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            let c =
                graph::create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            let d =
                graph::create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            graph::create_edge(&mut txn, PlaneId::STARTUP, a, b, "N", &Properties::new()).unwrap();
            graph::create_edge(&mut txn, PlaneId::STARTUP, b, c, "N", &Properties::new()).unwrap();
            graph::create_edge(&mut txn, PlaneId::STARTUP, c, d, "N", &Properties::new()).unwrap();
            txn.commit().unwrap();
        }
        let plan = {
            let mut plan = LogicalPlan::new(Source::SeekIds(vec![a]));
            plan.push(Step::ExpandVar {
                dir: Dir::Out,
                edge_type: None,
                min: 1,
                max: 2,
            });
            plan
        };
        let txn = eng.begin_read().unwrap();
        let reader = UncachedReader::new(&txn, PlaneId::STARTUP);
        let mut heads: Vec<u64> = execute(&plan, &reader)
            .unwrap()
            .map(|r| r.unwrap().head.0)
            .collect();
        heads.sort_unstable();
        // b and c (1 and 2 hops); d is 3 hops away, excluded
        assert_eq!(heads.len(), 2);
    }

    #[test]
    fn seek_ids_drops_nonexistent() {
        let plan = LogicalPlan::new(Source::SeekIds(vec![NodeId(1), NodeId(9999)]));
        let ids = run(
            |txn| {
                graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            },
            &plan,
        );
        assert_eq!(ids, vec![NodeId(1)]);
    }

    #[test]
    fn multi_hop_chain_expand() {
        let mut plan = LogicalPlan::new(Source::ScanLabel("Start".into()));
        plan.push(Step::Expand {
            dir: Dir::Out,
            edge_type: None,
        });
        plan.push(Step::Expand {
            dir: Dir::Out,
            edge_type: None,
        });
        let ids = run(
            |txn| {
                let a = graph::create_node(txn, PlaneId::STARTUP, &["Start"], &Properties::new())
                    .unwrap();
                let b = graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
                let c = graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a, b, "N", &Properties::new()).unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, b, c, "N", &Properties::new()).unwrap();
            },
            &plan,
        );
        assert_eq!(ids, vec![NodeId(3)]); // two hops from a lands on c
    }

    #[test]
    fn filter_by_label() {
        let mut plan = LogicalPlan::new(Source::ScanAll);
        plan.push(Step::Filter(has_label("Keep")));
        let ids = run(
            |txn| {
                graph::create_node(txn, PlaneId::STARTUP, &["Keep"], &Properties::new()).unwrap();
                graph::create_node(txn, PlaneId::STARTUP, &["Drop"], &Properties::new()).unwrap();
                graph::create_node(txn, PlaneId::STARTUP, &["Keep"], &Properties::new()).unwrap();
            },
            &plan,
        );
        assert_eq!(ids.len(), 2);
    }

    // ---- bindings ---------------------------------------------------------

    /// `a -CITES-> {b, c}`, where `a` and `b` share a file and `c` doesn't.
    /// Two nodes' properties in one predicate is the shape the linear pipeline
    /// used to reject outright.
    fn cross_variable_graph(txn: &mut dyn WriteTransaction) {
        let file = |f: &str| props(&[("file", PropValue::Str(f.into()))]);
        let a = graph::create_node(txn, PlaneId::STARTUP, &["Root"], &file("exec.rs")).unwrap();
        let b = graph::create_node(txn, PlaneId::STARTUP, &[], &file("exec.rs")).unwrap();
        let c = graph::create_node(txn, PlaneId::STARTUP, &[], &file("plan.rs")).unwrap();
        graph::create_edge(txn, PlaneId::STARTUP, a, b, "CITES", &Properties::new()).unwrap();
        graph::create_edge(txn, PlaneId::STARTUP, a, c, "CITES", &Properties::new()).unwrap();
    }

    #[test]
    fn a_filter_can_compare_two_pattern_variables() {
        let mut plan = LogicalPlan::new(Source::ScanLabel("Root".into()));
        plan.push(Step::Expand {
            dir: Dir::Out,
            edge_type: Some("CITES".into()),
        });
        // slot 0 is the source `a`; the current node is `b`/`c`.
        plan.push(Step::Filter(at_node(0, p("file")).eq(p("file"))));
        assert_eq!(run(cross_variable_graph, &plan), vec![NodeId(2)]);
    }

    /// The source node is the one binding the trail cannot reconstruct — it
    /// records hop destinations only — so `Row::origin` earns its 8 bytes.
    #[test]
    fn slot_zero_is_the_source_even_after_several_hops() {
        let mut plan = LogicalPlan::new(Source::ScanLabel("Root".into()));
        for _ in 0..2 {
            plan.push(Step::Expand {
                dir: Dir::Out,
                edge_type: None,
            });
        }
        plan.push(Step::Filter(at_node(0, p("name")).eq("a")));
        let ids = run(
            |txn| {
                let name = |n: &str| props(&[("name", PropValue::Str(n.into()))]);
                let a = graph::create_node(txn, PlaneId::STARTUP, &["Root"], &name("a")).unwrap();
                let b = graph::create_node(txn, PlaneId::STARTUP, &[], &name("b")).unwrap();
                let c = graph::create_node(txn, PlaneId::STARTUP, &[], &name("c")).unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, a, b, "N", &Properties::new()).unwrap();
                graph::create_edge(txn, PlaneId::STARTUP, b, c, "N", &Properties::new()).unwrap();
            },
            &plan,
        );
        assert_eq!(ids, vec![NodeId(3)]);
    }

    /// A `Both` expansion walks each edge in whichever direction reaches the
    /// neighbour, and the resolver recovers which one from the endpoints.
    #[test]
    fn edge_terms_see_the_direction_the_row_actually_walked() {
        let build = |txn: &mut dyn WriteTransaction| {
            let mid =
                graph::create_node(txn, PlaneId::STARTUP, &["Mid"], &Properties::new()).unwrap();
            let up = graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            let down = graph::create_node(txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
            // mid -CALLS-> down, and up -CALLS-> mid
            graph::create_edge(
                txn,
                PlaneId::STARTUP,
                mid,
                down,
                "CALLS",
                &props(&[("weight", PropValue::Int(7))]),
            )
            .unwrap();
            graph::create_edge(txn, PlaneId::STARTUP, up, mid, "CALLS", &Properties::new())
                .unwrap();
        };

        let both = |pred: Expr| {
            let mut plan = LogicalPlan::new(Source::ScanLabel("Mid".into()));
            plan.push(Step::Expand {
                dir: Dir::Both,
                edge_type: None,
            });
            plan.push(Step::Filter(pred));
            plan
        };

        // Out of `mid` is the edge it is the src of — the one reaching `down`.
        assert_eq!(run(build, &both(edge_dir().eq("OUT"))), vec![NodeId(3)]);
        assert_eq!(run(build, &both(edge_dir().eq("IN"))), vec![NodeId(2)]);
        // Type and edge properties come off the same resolved record.
        assert_eq!(run(build, &both(edge_type().eq("CALLS"))).len(), 2);
        assert_eq!(run(build, &both(ep("weight").eq(7))), vec![NodeId(3)]);
        // A hop-qualified term addresses the same single hop here.
        assert_eq!(
            run(build, &both(at_edge(0, edge_dir()).eq("OUT"))),
            vec![NodeId(3)]
        );
    }

    /// Resolution is driven by what the plan names, so a query that reaches
    /// past nothing must ask for nothing.
    #[test]
    fn a_plan_without_bindings_resolves_none() {
        let mut plan = LogicalPlan::new(Source::ScanAll);
        plan.push(Step::Filter(p("x").eq(1)));
        assert!(!plan.binding_need().any());

        plan.push(Step::Sort(vec![SortKey {
            expr: at_node(1, p("x")),
            descending: false,
        }]));
        assert_eq!(plan.binding_need().nodes, Some(1));
    }
}
