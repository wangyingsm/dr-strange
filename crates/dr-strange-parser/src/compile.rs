//! Compile a [`Query`] AST into a core [`LogicalPlan`].
//!
//! The one real subtlety is that core's executor is a linear pipeline over a
//! single "current node" (arch/03 §2), while a Cypher pattern binds several
//! variables. We reconcile them by **filter pushdown**: the pattern's nodes are
//! visited in path order (node 0 is the source, node *i* is current right after
//! the *i*-th `Expand`), so a predicate that mentions exactly one variable is
//! placed at that variable's slot — where it *is* the current node. A predicate
//! spanning two variables can't be evaluated in this model, so it's rejected
//! with a clear message rather than silently mis-scoped.

use std::collections::{BTreeSet, HashMap};

use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};
use dr_strange_core::types::Dir;
use dr_strange_core::{
    Algo, Expr, GraphChannel, HybridSpec, HybridWeights, KeywordChannel, LogicalPlan,
    LouvainOptions, NodeId, NodeRef, PageRankOptions, PropValue, SortKey, Source, Step,
    VectorChannel,
};

use crate::Embedder;
use crate::ast::*;

/// Resolve a query-vector argument: a literal passes through; text is embedded
/// via the supplied `embedder`, failing clearly when none is configured.
fn resolve(arg: &VecArg, embedder: Option<&dyn Embedder>) -> Result<Vec<f32>, String> {
    match arg {
        VecArg::Vector(v) => Ok(v.clone()),
        VecArg::Text(t) => {
            let e = embedder.ok_or_else(|| {
                "SEARCH by text needs an embedding provider (set the API key), \
                 or pass a literal vector like NEAR [..]"
                    .to_string()
            })?;
            e.embed(t)
                .map_err(|e| format!("embedding the query text failed: {e}"))
        }
    }
}

/// One hop along the query's path: a relationship (`Expand`/`ExpandVar`) or a
/// similarity beam (`ExpandBeam`). Each advances the current node.
enum Hop<'a> {
    Rel(&'a RelPat),
    Beam(&'a BeamClause),
}

pub fn compile(
    q: Query,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<LogicalPlan, String> {
    // Every source consumes its first node's label (into ScanLabel /
    // VectorTopK / KeywordTopK / …), so it never becomes a HasLabel filter.
    let first_node = &q.source.first;
    let source = compile_source(&q.source, embedder, params)?;

    // The hops after the source, in path order: the source's relationship
    // tail (if any), then any BEAM clauses.
    let mut hops: Vec<(Hop, &NodePat)> = Vec::new();
    hops.extend(q.source.rest.iter().map(|(r, n)| (Hop::Rel(r), n)));
    hops.extend(q.beams.iter().map(|b| (Hop::Beam(b), &b.node)));

    // Nodes in path order; node i becomes the current row after i hops.
    let mut nodes: Vec<&NodePat> = vec![first_node];
    nodes.extend(hops.iter().map(|(_, n)| *n));
    let last = nodes.len() - 1;

    // Variable → slot index. Reusing a variable would mean a graph constraint
    // (same node reached two ways), which the linear model can't express.
    let mut var_slot: HashMap<&str, usize> = HashMap::new();
    for (idx, n) in nodes.iter().enumerate() {
        if let Some(v) = &n.var
            && var_slot.insert(v.as_str(), idx).is_some()
        {
            return Err(format!(
                "variable `{v}` is bound twice; reusing a pattern variable isn't supported yet"
            ));
        }
    }

    // WHERE conjuncts, each pushed down to its single variable's slot. Kept as
    // parser expressions for now so the key-seek rewrite below can still see
    // `key(n) = "…"` before the qualifier is dropped.
    let mut slot_filters: Vec<Vec<PExpr>> = vec![Vec::new(); nodes.len()];
    if let Some(w) = q.where_clause {
        for conj in split_and(w) {
            let vars = referenced_vars(&conj);
            let slot = match vars.len() {
                0 => 0, // constant predicate — evaluate at the source
                1 => {
                    let v = vars.iter().next().unwrap();
                    *var_slot
                        .get(v.as_str())
                        .ok_or_else(|| format!("WHERE refers to unknown variable `{v}`"))?
                }
                _ => {
                    return Err(
                        "a WHERE condition may reference only one pattern variable; \
                         cross-variable predicates aren't supported yet"
                            .to_string(),
                    );
                }
            };
            slot_filters[slot].push(conj);
        }
    }

    // Key-seek: a `key(n) = "…"` / `key(n) IN […]` on a *scanned* source
    // becomes a `SeekKeys` seek — an index lookup instead of a scan-and-filter.
    // The scan's label survives as a HasLabel filter, since the seek is by key
    // alone. Only for MATCH: on a retrieval seed the predicate stays a filter.
    let mut source = source;
    if matches!(source, Source::ScanAll | Source::ScanLabel(_)) {
        let var = first_node.var.as_deref();
        if let Some(pos) = slot_filters[0]
            .iter()
            .position(|e| key_seek_keys(e, var, params).is_some())
        {
            let keys = key_seek_keys(&slot_filters[0][pos], var, params).expect("just matched")?;
            slot_filters[0].remove(pos);
            if let Source::ScanLabel(label) = &source {
                slot_filters[0].insert(
                    0,
                    PExpr::HasLabel {
                        var: var.unwrap_or_default().to_string(),
                        label: label.clone(),
                    },
                );
            }
            source = Source::SeekKeys(keys);
        }
    }

    let mut steps: Vec<Step> = Vec::new();
    for e in slot_filters[0].drain(..) {
        steps.push(Step::Filter(compile_expr(&e, embedder, params)?));
    }

    for idx in 1..nodes.len() {
        match &hops[idx - 1].0 {
            Hop::Rel(rel) => match rel.var_len {
                None => steps.push(Step::Expand {
                    dir: rel.dir,
                    edge_type: rel.ty.clone(),
                }),
                Some(VarLen { min, max }) => {
                    let max = max.ok_or_else(|| {
                        "unbounded variable-length relationships aren't supported; \
                         give an upper bound, e.g. *1..3"
                            .to_string()
                    })?;
                    if min > max {
                        return Err(format!(
                            "variable-length range is empty: min ({min}) exceeds max ({max})"
                        ));
                    }
                    steps.push(Step::ExpandVar {
                        dir: rel.dir,
                        edge_type: rel.ty.clone(),
                        min,
                        max,
                    });
                }
            },
            Hop::Beam(b) => steps.push(Step::ExpandBeam {
                dir: b.dir,
                edge_type: b.edge_type.clone(),
                property: b.property.clone(),
                query: resolve(&b.query, embedder)?,
                metric: b.metric,
                width: b.width,
                depth: b.depth,
            }),
        }
        // A non-source node's label becomes a HasLabel filter on the frontier.
        if let Some(l) = &nodes[idx].label {
            steps.push(Step::Filter(Expr::HasLabel(l.clone())));
        }
        for e in slot_filters[idx].drain(..) {
            steps.push(Step::Filter(compile_expr(&e, embedder, params)?));
        }
    }

    // RETURN: the pipeline ends on the last pattern node, so only that variable
    // (or `*`) can be returned without a projection/multi-binding model.
    if let ReturnItem::Var(v) = &q.ret.item {
        let slot = *var_slot
            .get(v.as_str())
            .ok_or_else(|| format!("RETURN refers to unknown variable `{v}`"))?;
        if slot != last {
            return Err(format!(
                "RETURN must name the pattern's last variable; returning an earlier \
                 variable (`{v}`) isn't supported yet"
            ));
        }
    }

    if q.ret.distinct {
        steps.push(Step::Distinct);
    }

    if !q.order_by.is_empty() {
        let mut keys = Vec::with_capacity(q.order_by.len());
        for ok in &q.order_by {
            for v in referenced_vars(&ok.expr) {
                let slot = *var_slot
                    .get(v.as_str())
                    .ok_or_else(|| format!("ORDER BY refers to unknown variable `{v}`"))?;
                if slot != last {
                    return Err(
                        "ORDER BY may reference only the returned (last) variable".to_string()
                    );
                }
            }
            keys.push(SortKey {
                expr: compile_expr(&ok.expr, embedder, params)?,
                descending: ok.descending,
            });
        }
        steps.push(Step::Sort(keys));
    }

    if let Some(s) = q.skip {
        steps.push(Step::Skip(s));
    }
    if let Some(l) = q.limit {
        steps.push(Step::Limit(l));
    }

    Ok(LogicalPlan { source, steps })
}

/// Compile a query's source clause into a plan [`Source`]. The first node's
/// label is consumed here (as the scan's label, or the retrieval scope), so it
/// never becomes a redundant filter.
fn compile_source(
    src: &QuerySource,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<Source, String> {
    let label = src.first.label.clone();
    Ok(match &src.kind {
        SourceKind::Match => match label {
            Some(l) => Source::ScanLabel(l),
            None => Source::ScanAll,
        },
        SourceKind::Search {
            property,
            query,
            metric,
            k,
        } => Source::VectorTopK {
            label,
            property: property.clone(),
            query: resolve(query, embedder)?,
            metric: *metric,
            k: *k,
        },
        SourceKind::Keyword { property, query, k } => Source::KeywordTopK {
            // BM25 indexes are declared per `(label, property)`, so unlike a
            // vector search there is nothing to search without a label.
            label: label.ok_or_else(|| {
                "a keyword SEARCH needs a label — the BM25 index is declared on \
                 (label, property), e.g. SEARCH (d:Doc) ON body MATCHING \"…\""
                    .to_string()
            })?,
            property: property.clone(),
            query: query.clone(),
            k: *k,
        },
        SourceKind::Hybrid(h) => Source::Hybrid(Box::new(compile_hybrid(h, label, embedder)?)),
        SourceKind::Call(c) => Source::Algo {
            label,
            algo: compile_algo(c, params)?,
        },
    })
}

/// Compile a `HYBRID` clause into the fused-retrieval spec. At least one
/// channel must be present, else the query would rank nothing.
fn compile_hybrid(
    h: &HybridClause,
    label: Option<String>,
    embedder: Option<&dyn Embedder>,
) -> Result<HybridSpec, String> {
    if h.vector.is_none() && h.keyword.is_none() {
        return Err(
            "HYBRID needs at least one of a VECTOR or KEYWORD channel; GRAPH only \
             boosts what those find"
                .to_string(),
        );
    }
    if h.keyword.is_some() && label.is_none() {
        return Err(
            "the HYBRID KEYWORD channel needs a label — the BM25 index is declared \
             on (label, property), e.g. HYBRID (d:Doc) KEYWORD ON body MATCHING \"…\""
                .to_string(),
        );
    }
    let mut weights = HybridWeights::default();
    let vector = match &h.vector {
        Some(v) => {
            if let Some(w) = v.weight {
                weights.vector = w;
            }
            Some(VectorChannel {
                property: v.property.clone(),
                query: resolve(&v.query, embedder)?,
                metric: v.metric,
            })
        }
        None => None,
    };
    let keyword = h.keyword.as_ref().map(|k| {
        if let Some(w) = k.weight {
            weights.keyword = w;
        }
        KeywordChannel {
            property: k.property.clone(),
            query: k.query.clone(),
        }
    });
    let graph = h.graph.as_ref().map(|g| {
        if let Some(w) = g.weight {
            weights.graph = w;
        }
        GraphChannel {
            hops: g.hops,
            decay: g.decay,
            seeds: g.seeds.unwrap_or(DEFAULT_GRAPH_SEEDS) as usize,
        }
    });
    Ok(HybridSpec {
        label,
        vector,
        keyword,
        graph,
        weights,
        candidates: h.candidates.unwrap_or(DEFAULT_CANDIDATES) as usize,
        k: h.k.unwrap_or(DEFAULT_HYBRID_K) as usize,
    })
}

/// Seeds per primary channel for `GRAPH` when `SEEDS` is omitted; matches the
/// builder API's default.
const DEFAULT_GRAPH_SEEDS: u64 = 10;
/// Per-channel candidate pool when `CANDIDATES` is omitted.
const DEFAULT_CANDIDATES: u64 = 100;
/// Fused hits returned when `TOPK` is omitted.
const DEFAULT_HYBRID_K: u64 = 10;

/// Compile a `CALL name(args)` into an algorithm. Every unknown name and
/// argument is a clear error — never a silently ignored knob.
fn compile_algo(c: &CallClause, params: &crate::Params) -> Result<Algo, String> {
    let name = c.name.to_ascii_lowercase();
    let mut args = AlgoArgs::new(c, params)?;
    let algo = match name.as_str() {
        "pagerank" => {
            let d = PageRankOptions::default();
            Algo::PageRank {
                damping: args.float("damping")?.unwrap_or(d.damping),
                // `iterations` reads better in a query than the API's field name.
                max_iters: args
                    .int("iterations")?
                    .or(args.int("max_iters")?)
                    .map(|n| n as u32)
                    .unwrap_or(d.max_iters),
                tolerance: args.float("tolerance")?.unwrap_or(d.tolerance),
            }
        }
        "components" | "connected_components" => Algo::ConnectedComponents,
        "louvain" => {
            let d = LouvainOptions::default();
            Algo::Louvain {
                max_levels: args
                    .int("max_levels")?
                    .map(|n| n as u32)
                    .unwrap_or(d.max_levels),
                min_gain: args.float("min_gain")?.unwrap_or(d.min_gain),
            }
        }
        "shortest_path" => Algo::ShortestPath {
            from: args.node_ref("from")?,
            to: args.node_ref("to")?,
            dir: match args.string("dir")?.as_deref() {
                None | Some("out") => Dir::Out,
                Some("in") => Dir::In,
                Some("both") => Dir::Both,
                Some(other) => {
                    return Err(format!(
                        "shortest_path dir must be out, in or both, not `{other}`"
                    ));
                }
            },
            weight: args.string("weight")?,
        },
        other => {
            return Err(format!(
                "unknown algorithm `{other}`; expected pagerank, components, \
                 shortest_path or louvain"
            ));
        }
    };
    args.finish(&name)?;
    Ok(algo)
}

/// The argument list of one `CALL`, consumed by name so anything left over at
/// the end is reported as unknown.
struct AlgoArgs {
    values: Vec<(String, PropValue)>,
}

impl AlgoArgs {
    fn new(c: &CallClause, params: &crate::Params) -> Result<Self, String> {
        let mut values = Vec::with_capacity(c.args.len());
        for (name, val) in &c.args {
            let v = match val {
                Val::Lit(v) => v.clone(),
                Val::Param(p) => crate::resolve_param(params, p)?,
            };
            values.push((name.to_ascii_lowercase(), v));
        }
        Ok(Self { values })
    }

    fn take(&mut self, name: &str) -> Option<PropValue> {
        let pos = self.values.iter().position(|(n, _)| n == name)?;
        Some(self.values.remove(pos).1)
    }

    fn float(&mut self, name: &str) -> Result<Option<f64>, String> {
        match self.take(name) {
            None => Ok(None),
            Some(PropValue::Float(f)) => Ok(Some(f)),
            Some(PropValue::Int(n)) => Ok(Some(n as f64)),
            Some(other) => Err(format!("`{name}` must be a number, got {other:?}")),
        }
    }

    fn int(&mut self, name: &str) -> Result<Option<i64>, String> {
        match self.take(name) {
            None => Ok(None),
            Some(PropValue::Int(n)) if n >= 0 => Ok(Some(n)),
            Some(other) => Err(format!(
                "`{name}` must be a non-negative whole number, got {other:?}"
            )),
        }
    }

    fn string(&mut self, name: &str) -> Result<Option<String>, String> {
        match self.take(name) {
            None => Ok(None),
            Some(PropValue::Str(s)) => Ok(Some(s.to_ascii_lowercase())),
            Some(other) => Err(format!("`{name}` must be a string, got {other:?}")),
        }
    }

    /// A node argument: a string is an external key, a whole number a node id.
    fn node_ref(&mut self, name: &str) -> Result<NodeRef, String> {
        match self.take(name) {
            Some(PropValue::Str(s)) => Ok(NodeRef::Key(s)),
            Some(PropValue::Int(n)) if n >= 0 => Ok(NodeRef::Id(NodeId(n as u64))),
            Some(other) => Err(format!(
                "`{name}` must be an external key (a string) or a node id, got {other:?}"
            )),
            None => Err(format!("shortest_path needs a `{name}` argument")),
        }
    }

    fn finish(self, algo: &str) -> Result<(), String> {
        match self.values.first() {
            None => Ok(()),
            Some((name, _)) => Err(format!("`{algo}` has no argument named `{name}`")),
        }
    }
}

/// If `e` pins `var`'s external key to one or more literals — `key(n) = "k"`
/// or `key(n) IN ["a", "b"]` — return those keys. `None` when the shape
/// doesn't match at all, so a caller can test before committing.
fn key_seek_keys(
    e: &PExpr,
    var: Option<&str>,
    params: &crate::Params,
) -> Option<Result<Vec<String>, String>> {
    /// The key string a value expression denotes, if it is one.
    fn as_key(e: &PExpr, params: &crate::Params) -> Option<Result<String, String>> {
        match e {
            PExpr::Lit(PropValue::Str(s)) => Some(Ok(s.clone())),
            PExpr::Param(name) => Some(match crate::resolve_param(params, name) {
                Ok(PropValue::Str(s)) => Ok(s),
                Ok(other) => Err(format!("`key()` compares against a string, got {other:?}")),
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }
    let is_key = |e: &PExpr| matches!(e, PExpr::ExternalKey { var: v } if Some(v.as_str()) == var);

    let values: Vec<&PExpr> = match e {
        PExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
        } if is_key(lhs) => vec![rhs],
        PExpr::Compare {
            op: CmpOp::Eq,
            lhs,
            rhs,
        } if is_key(rhs) => vec![lhs],
        PExpr::In { lhs, list } if is_key(lhs) && !list.is_empty() => list.iter().collect(),
        _ => return None,
    };
    let mut keys = Vec::with_capacity(values.len());
    for v in values {
        match as_key(v, params)? {
            Ok(k) => keys.push(k),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(Ok(keys))
}

/// Split a conjunction into its top-level terms so each can be pushed down
/// independently. Non-`AND` expressions come back as a single term.
fn split_and(e: PExpr) -> Vec<PExpr> {
    use dr_strange_core::compute::expr::LogicOp;
    match e {
        PExpr::Logic {
            op: LogicOp::And,
            lhs,
            rhs,
        } => {
            let mut v = split_and(*lhs);
            v.extend(split_and(*rhs));
            v
        }
        other => vec![other],
    }
}

/// The set of pattern variables an expression mentions.
fn referenced_vars(e: &PExpr) -> BTreeSet<String> {
    fn go(e: &PExpr, out: &mut BTreeSet<String>) {
        match e {
            PExpr::Prop { var, .. }
            | PExpr::HasLabel { var, .. }
            | PExpr::ExternalKey { var }
            | PExpr::Similarity { var, .. }
            | PExpr::Distance { var, .. } => {
                out.insert(var.clone());
            }
            // score()/hops() read the row channel, not a variable's node.
            PExpr::Lit(_) | PExpr::Param(_) | PExpr::Score | PExpr::Hops => {}
            PExpr::In { lhs, list } => {
                go(lhs, out);
                for e in list {
                    go(e, out);
                }
            }
            PExpr::IsNull(x) | PExpr::Not(x) | PExpr::Neg(x) => go(x, out),
            PExpr::Compare { lhs, rhs, .. }
            | PExpr::Logic { lhs, rhs, .. }
            | PExpr::Arith { lhs, rhs, .. } => {
                go(lhs, out);
                go(rhs, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    go(e, &mut out);
    out
}

/// Drop the variable qualifiers (the slot is already fixed by pushdown) and map
/// straight onto core's `Expr`. Fallible only because `similarity`/`distance`
/// may embed a text argument via `embedder`.
fn compile_expr(
    e: &PExpr,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<Expr, String> {
    let sub = |x: &PExpr| compile_expr(x, embedder, params).map(Box::new);
    Ok(match e {
        PExpr::Lit(v) => Expr::Literal(v.clone()),
        PExpr::Param(name) => Expr::Literal(crate::resolve_param(params, name)?),
        PExpr::Prop { key, .. } => Expr::Property(key.clone()),
        PExpr::HasLabel { label, .. } => Expr::HasLabel(label.clone()),
        PExpr::ExternalKey { .. } => Expr::ExternalKey,
        // `x IN [a, b]` is sugar for `x = a OR x = b`; an empty list is
        // constantly false. (A source-anchored `key(n) IN […]` never reaches
        // here — it became a `SeekKeys` seek.)
        PExpr::In { lhs, list } => {
            let lhs = compile_expr(lhs, embedder, params)?;
            let mut out: Option<Expr> = None;
            for item in list {
                let eq = Expr::Compare {
                    op: CmpOp::Eq,
                    lhs: Box::new(lhs.clone()),
                    rhs: sub(item)?,
                };
                out = Some(match out {
                    None => eq,
                    Some(prev) => Expr::Logic {
                        op: LogicOp::Or,
                        lhs: Box::new(prev),
                        rhs: Box::new(eq),
                    },
                });
            }
            out.unwrap_or(Expr::Literal(PropValue::Bool(false)))
        }
        PExpr::IsNull(x) => Expr::IsNull(sub(x)?),
        PExpr::Not(x) => Expr::Not(sub(x)?),
        // Fold `-literal` to a literal; otherwise `0 - x` (core has no negate).
        PExpr::Neg(x) => match compile_expr(x, embedder, params)? {
            Expr::Literal(PropValue::Int(n)) => Expr::Literal(PropValue::Int(-n)),
            Expr::Literal(PropValue::Float(f)) => Expr::Literal(PropValue::Float(-f)),
            other => Expr::Arith {
                op: ArithOp::Sub,
                lhs: Box::new(Expr::Literal(PropValue::Int(0))),
                rhs: Box::new(other),
            },
        },
        PExpr::Compare { op, lhs, rhs } => Expr::Compare {
            op: *op,
            lhs: sub(lhs)?,
            rhs: sub(rhs)?,
        },
        PExpr::Logic { op, lhs, rhs } => Expr::Logic {
            op: *op,
            lhs: sub(lhs)?,
            rhs: sub(rhs)?,
        },
        PExpr::Arith { op, lhs, rhs } => Expr::Arith {
            op: *op,
            lhs: sub(lhs)?,
            rhs: sub(rhs)?,
        },
        PExpr::Score => Expr::Score,
        PExpr::Hops => Expr::Hops,
        PExpr::Similarity {
            property,
            query,
            metric,
            ..
        } => Expr::Similarity {
            property: property.clone(),
            query: resolve(query, embedder)?,
            metric: *metric,
        },
        PExpr::Distance {
            property,
            query,
            metric,
            ..
        } => Expr::Distance {
            property: property.clone(),
            query: resolve(query, embedder)?,
            metric: *metric,
        },
    })
}
