//! Compile a [`Query`] AST into a core [`LogicalPlan`].
//!
//! The one real subtlety is that core's executor is a linear pipeline over a
//! single "current node" (arch/03 §2), while a Cypher pattern binds several
//! variables. We reconcile them by **filter pushdown**: the pattern's nodes are
//! visited in path order (node 0 is the source, node *i* is current right after
//! the *i*-th `Expand`), so a predicate that mentions exactly one variable is
//! placed at that variable's slot — where it *is* the current node. A predicate
//! spanning two variables can't be evaluated in this model, so it's rejected
//! with a clear message rather than silently given the wrong scope.

use std::collections::BTreeSet;

use ahash::AHashMap;

use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};
use dr_strange_core::types::Dir;
use dr_strange_core::{
    Agg, Algo, Binding, Expr, GraphChannel, HybridSpec, HybridWeights, KeywordChannel, LogicalPlan,
    LouvainOptions, NodeId, NodeRef, PageRankOptions, ProjExpr, ProjItem, Projection, PropValue,
    SortKey, Source, Step, TupleSortKey, VectorChannel,
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
    mut q: Query,
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

    let node_vars = bind_variables(&nodes)?;
    let rel_vars = bind_relationships(&hops, &node_vars)?;
    let vars = Vars {
        nodes: node_vars,
        rels: rel_vars,
        exact_through: exact_through(&hops),
    };
    // Taken rather than borrowed: the conjuncts are consumed into the slots,
    // and `q` is still read whole by `compile_return` below.
    let where_clause = q.where_clause.take();
    let mut slot_filters = push_down_where(where_clause, &vars, nodes.len())?;
    let source = key_seek_rewrite(source, &mut slot_filters[0], first_node, params)?;

    let mut steps: Vec<Step> = Vec::new();
    for e in slot_filters[0].drain(..) {
        let scope = Scope {
            vars: &vars,
            current: 0,
        };
        steps.push(Step::Filter(compile_expr(&e, embedder, params, &scope)?));
    }
    compile_hops(
        &mut steps,
        &hops,
        &nodes,
        &mut slot_filters,
        &vars,
        embedder,
        params,
    )?;

    let project = compile_return(&q, &mut steps, &vars, last, embedder, params)?;
    Ok(LogicalPlan {
        source,
        steps,
        project,
    })
}

/// Relationship variable → the hop it binds. A relationship is addressed by
/// which hop it is, so a variable is refused where that address would not be
/// one edge: on a variable-length hop (which walks several), bound twice, or
/// already naming a node.
fn bind_relationships<'a>(
    hops: &[(Hop<'a>, &NodePat)],
    var_slot: &AHashMap<&str, usize>,
) -> Result<AHashMap<&'a str, usize>, String> {
    let mut rel_slot: AHashMap<&'a str, usize> = AHashMap::new();
    for (hop, (h, _)) in hops.iter().enumerate() {
        let Hop::Rel(r) = h else { continue };
        let Some(v) = r.var.as_deref() else { continue };
        if r.var_len.is_some() {
            return Err(format!(
                "relationship variable `{v}` is on a variable-length hop, which walks \
                 several edges rather than one; bind a single hop instead"
            ));
        }
        if var_slot.contains_key(v) {
            return Err(format!(
                "`{v}` already names a node in this pattern; give the relationship \
                 another name"
            ));
        }
        if rel_slot.insert(v, hop).is_some() {
            return Err(format!(
                "relationship variable `{v}` is bound twice; reusing one isn't supported"
            ));
        }
    }
    Ok(rel_slot)
}

/// Variable → slot index. Reusing a variable would mean a graph constraint
/// (the same node reached two ways), which the linear model can't express, so
/// a second binding is refused rather than silently shadowing the first.
fn bind_variables<'a>(nodes: &[&'a NodePat]) -> Result<AHashMap<&'a str, usize>, String> {
    let mut var_slot: AHashMap<&'a str, usize> = AHashMap::new();
    for (idx, n) in nodes.iter().enumerate() {
        if let Some(v) = &n.var
            && var_slot.insert(v.as_str(), idx).is_some()
        {
            return Err(format!(
                "variable `{v}` is bound twice; reusing a pattern variable isn't supported yet"
            ));
        }
    }
    Ok(var_slot)
}

/// WHERE conjuncts, each pushed down to its single variable's slot.
///
/// Kept as parser expressions rather than compiled here, so the key-seek
/// rewrite can still recognise `key(n) = "…"` before the qualifier is dropped.
fn push_down_where(
    where_clause: Option<PExpr>,
    vars: &Vars<'_>,
    slots: usize,
) -> Result<Vec<Vec<PExpr>>, String> {
    let mut slot_filters: Vec<Vec<PExpr>> = vec![Vec::new(); slots];
    let Some(w) = where_clause else {
        return Ok(slot_filters);
    };
    for conj in split_and(w) {
        let named = referenced_vars(&conj);
        for v in &named {
            if vars.slot_of(v.as_str()).is_none() {
                return Err(format!("WHERE refers to unknown variable `{v}`"));
            }
        }
        // The row channels `score()` / `hops()` are only settled once the whole
        // path has been walked — `hops()` counts the hops taken so far, and a
        // BEAM rewrites `score()` — so a conjunct reading one goes on the last
        // slot, even mixed with a variable (`hops() = 2 OR a.x = 1`, which
        // cannot be split). At an earlier slot it would read hops() = 0 and a
        // score no MATCH has yet, and silently drop every row.
        //
        // Everything else goes to the latest slot its variables name: the
        // first point where the row has walked all of them, so a predicate
        // still filters as early as it can. One variable means its own slot;
        // several, the last of them, the earlier ones reached by `Expr::At`.
        let slot = match reads_row_channel(&conj) {
            true => slots - 1,
            false => named
                .iter()
                .filter_map(|v| vars.slot_of(v.as_str()))
                .max()
                // No variable at all: a constant, true or false for every row,
                // so the source is as good a place as any.
                .unwrap_or(0),
        };
        for v in &named {
            if !vars.readable_at(v.as_str(), slot) {
                return Err(format!(
                    "this condition is evaluated at slot {slot}, and `{v}` sits past a \
                     variable-length hop, where a row's own length decides which node \
                     that slot holds; split the condition so each part names only \
                     variables on one side of the hop"
                ));
            }
        }
        slot_filters[slot].push(conj);
    }
    Ok(slot_filters)
}

/// A `key(n) = "…"` / `key(n) IN […]` on a *scanned* source becomes a
/// `SeekKeys` seek — an index lookup instead of a scan-and-filter.
///
/// The scan's label survives as a `HasLabel` filter, since the seek is by key
/// alone. Only for `MATCH`: on a retrieval seed the predicate stays a filter,
/// because the seed order is what the query asked for.
fn key_seek_rewrite(
    source: Source,
    slot0: &mut Vec<PExpr>,
    first_node: &NodePat,
    params: &crate::Params,
) -> Result<Source, String> {
    if !matches!(source, Source::ScanAll | Source::ScanLabel(_)) {
        return Ok(source);
    }
    let var = first_node.var.as_deref();
    let Some(pos) = slot0
        .iter()
        .position(|e| key_seek_keys(e, var, params).is_some())
    else {
        return Ok(source);
    };
    let keys = key_seek_keys(&slot0[pos], var, params).expect("just matched")?;
    slot0.remove(pos);
    if let Source::ScanLabel(label) = &source {
        slot0.insert(
            0,
            PExpr::HasLabel {
                var: var.unwrap_or_default().to_string(),
                label: label.clone(),
            },
        );
    }
    Ok(Source::SeekKeys(keys))
}

/// One expansion per hop, in path order, each followed by the filters that
/// belong to the node it lands on.
#[allow(clippy::too_many_arguments)]
fn compile_hops(
    steps: &mut Vec<Step>,
    hops: &[(Hop<'_>, &NodePat)],
    nodes: &[&NodePat],
    slot_filters: &mut [Vec<PExpr>],
    vars: &Vars<'_>,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<(), String> {
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
                property: vector_property(b.property.as_deref()),
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
        let scope = Scope { vars, current: idx };
        for e in slot_filters[idx].drain(..) {
            steps.push(Step::Filter(compile_expr(&e, embedder, params, &scope)?));
        }
    }
    Ok(())
}

/// `RETURN` decides what the query *is*: naming a node (`n`, `*`) keeps the
/// rows themselves and the tail operators apply to them; anything else
/// projects the rows into a table, and the tail moves onto the projection.
fn compile_return(
    q: &Query,
    steps: &mut Vec<Step>,
    vars: &Vars<'_>,
    last: usize,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<Option<Projection>, String> {
    let scope = Scope {
        vars,
        current: last,
    };
    // A relationship variable is a *value*, not a row: `RETURN r` projects a
    // column, so only `*` and node variables keep the rows themselves.
    let node_rows = q.ret.items.iter().any(|i| match i {
        ReturnItem::Star => true,
        ReturnItem::Var(v) => !vars.rels.contains_key(v.as_str()),
        _ => false,
    });
    if node_rows && q.ret.items.len() > 1 {
        return Err(
            "RETURN mixes a node with columns of values; a node isn't a value, \
             so return its properties instead (`RETURN n.name, count(*)`)"
                .to_string(),
        );
    }
    if !node_rows {
        return projection(q, &scope, embedder, params).map(Some);
    }

    if let Some(ReturnItem::Var(v)) = q.ret.items.first() {
        let slot = *vars
            .nodes
            .get(v.as_str())
            .ok_or_else(|| format!("RETURN refers to unknown variable `{v}`"))?;
        // The pipeline ends on the last pattern node, so returning the *rows*
        // of an earlier one is the one thing a projection can't stand in for —
        // it would have to un-expand them.
        if slot != last {
            return Err(format!(
                "RETURN must name the pattern's last variable; returning an earlier \
                 variable (`{v}`) isn't supported yet — project its values instead \
                 (`RETURN {v}.name`)"
            ));
        }
    }
    if q.ret.distinct {
        steps.push(Step::Distinct);
    }
    if !q.order_by.is_empty() {
        steps.push(Step::Sort(compile_sort(
            &q.order_by,
            vars,
            last,
            embedder,
            params,
        )?));
    }
    if let Some(s) = q.skip {
        steps.push(Step::Skip(s));
    }
    if let Some(l) = q.limit {
        steps.push(Step::Limit(l));
    }
    Ok(None)
}

/// The table form of `RETURN`: one column per item, with the tail operators
/// riding on the projection rather than on the rows.
fn projection(
    q: &Query,
    scope: &Scope<'_, '_>,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<Projection, String> {
    let mut items = Vec::with_capacity(q.ret.items.len());
    for item in &q.ret.items {
        items.push(match item {
            ReturnItem::Value { expr, name } => {
                ProjItem::value(name.clone(), compile_expr(expr, embedder, params, scope)?)
            }
            ReturnItem::Agg {
                func,
                arg,
                distinct,
                name,
            } => {
                let arg = arg
                    .as_ref()
                    .map(|e| compile_expr(e, embedder, params, scope))
                    .transpose()?;
                ProjItem::agg(
                    name.clone(),
                    Agg {
                        func: *func,
                        arg,
                        distinct: *distinct,
                    },
                )
            }
            // A relationship is a value, so it projects as one column named
            // after the variable. Node variables never reach here — they are
            // what `node_rows` matched on.
            ReturnItem::Var(v) => match scope.hop_of(v) {
                Some(hop) => ProjItem::value(v.clone(), scope.read_hop(hop, Expr::EdgeValue)),
                None => unreachable!("a node RETURN never projects"),
            },
            ReturnItem::Star => unreachable!("a node RETURN never projects"),
        });
    }
    let order_by = order_columns(&q.order_by, &items, embedder, params, scope)?;
    Ok(Projection {
        items,
        // Over tuples, not node ids.
        distinct: q.ret.distinct,
        order_by,
        // On the tail: a LIMIT before an aggregate would change what the
        // aggregate is over.
        skip: q.skip,
        limit: q.limit,
    })
}

/// `ORDER BY` over node rows: keys evaluated on the row's current node.
fn compile_sort(
    keys: &[OrderKey],
    vars: &Vars<'_>,
    last: usize,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
) -> Result<Vec<SortKey>, String> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let e = match &key.target {
            SortTarget::Expr(e) => e,
            SortTarget::Name(_) => {
                return Err(format!(
                    "ORDER BY `{}`: a bare name orders by a RETURN alias, and this query \
                     returns nodes rather than columns",
                    key.text
                ));
            }
            SortTarget::Agg { .. } => {
                return Err(format!(
                    "ORDER BY `{}`: an aggregate orders a projection, and this query \
                     returns nodes rather than columns (`RETURN n.name, {}`)",
                    key.text, key.text
                ));
            }
        };
        for v in referenced_vars(e) {
            let slot = vars
                .slot_of(v.as_str())
                .ok_or_else(|| format!("ORDER BY refers to unknown variable `{v}`"))?;
            if slot != last {
                return Err("ORDER BY may reference only the returned (last) variable".to_string());
            }
        }
        out.push(SortKey {
            expr: compile_expr(
                e,
                embedder,
                params,
                &Scope {
                    vars,
                    current: last,
                },
            )?,
            descending: key.descending,
        });
    }
    Ok(out)
}

/// `ORDER BY` over a projection: each key names one of the returned columns.
///
/// By name first — an alias, or an unaliased item's own text — then by the
/// expression, so `RETURN n.year AS y ORDER BY n.year` finds column `y`.
fn order_columns(
    keys: &[OrderKey],
    items: &[ProjItem],
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
    scope: &Scope,
) -> Result<Vec<TupleSortKey>, String> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let by_name = items.iter().position(|c| c.name == key.text);
        let column = match (by_name, &key.target) {
            (Some(i), _) => i,
            (None, SortTarget::Expr(e)) => {
                let compiled = ProjExpr::Value(compile_expr(e, embedder, params, scope)?);
                items
                    .iter()
                    .position(|c| c.expr == compiled)
                    .ok_or_else(|| unknown_column(&key.text, items))?
            }
            // `ORDER BY count(*)` when the column was aliased (or spelled
            // `COUNT(*)`): the same fold is the same column.
            (
                None,
                SortTarget::Agg {
                    func,
                    arg,
                    distinct,
                },
            ) => {
                let arg = arg
                    .as_ref()
                    .map(|e| compile_expr(e, embedder, params, scope))
                    .transpose()?;
                let compiled = ProjExpr::Agg(Agg {
                    func: *func,
                    arg,
                    distinct: *distinct,
                });
                items
                    .iter()
                    .position(|c| c.expr == compiled)
                    .ok_or_else(|| unknown_column(&key.text, items))?
            }
            (None, SortTarget::Name(_)) => return Err(unknown_column(&key.text, items)),
        };
        out.push(TupleSortKey {
            column,
            descending: key.descending,
        });
    }
    Ok(out)
}

fn unknown_column(text: &str, items: &[ProjItem]) -> String {
    let names: Vec<&str> = items.iter().map(|c| c.name.as_str()).collect();
    format!(
        "ORDER BY `{text}` must name a returned column: {}",
        names.join(", ")
    )
}

/// The vector property a `NEAR` clause searches when `ON <prop>` is omitted.
/// This is the property the digest pipeline stores node embeddings under, and
/// what the dashboard prefers when a label has several vector properties — so
/// it is the convention across the system, not a guess local to the parser.
const DEFAULT_VECTOR_PROPERTY: &str = "embedding";

/// Resolve an optional `ON <prop>` against that convention.
fn vector_property(property: Option<&str>) -> String {
    property.unwrap_or(DEFAULT_VECTOR_PROPERTY).to_string()
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
            property: vector_property(property.as_deref()),
            query: resolve(query, embedder)?,
            metric: *metric,
            k: *k,
        },
        SourceKind::Keyword { property, query, k } => Source::KeywordTopK {
            // BM25 indexes are declared per `(label, property)`, so unlike a
            // vector search there is nothing to search without a label — nor
            // any conventional property name to fall back on.
            label: label.ok_or_else(|| {
                "a keyword SEARCH needs a label — the BM25 index is declared on \
                 (label, property), e.g. SEARCH (d:Doc) ON body MATCHING \"…\""
                    .to_string()
            })?,
            property: property.clone().ok_or_else(|| {
                "a keyword SEARCH needs ON <property> — keyword properties follow \
                 no convention, so there is nothing to default to. \
                 (Only NEAR defaults, to `embedding`.)"
                    .to_string()
            })?,
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
                property: vector_property(v.property.as_deref()),
                query: resolve(&v.query, embedder)?,
                metric: v.metric,
            })
        }
        None => None,
    };
    let keyword = match &h.keyword {
        Some(k) => {
            if let Some(w) = k.weight {
                weights.keyword = w;
            }
            Some(KeywordChannel {
                property: k.property.clone().ok_or_else(|| {
                    "the HYBRID KEYWORD channel needs ON <property> — keyword \
                     properties follow no convention, so there is nothing to \
                     default to. (Only NEAR defaults, to `embedding`.)"
                        .to_string()
                })?,
                query: k.query.clone(),
            })
        }
        None => None,
    };
    let graph = h.graph.as_ref().map(|g| {
        if let Some(w) = g.weight {
            weights.graph = w;
        }
        GraphChannel {
            hops: g.hops,
            decay: g.decay.unwrap_or(DEFAULT_GRAPH_DECAY),
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
/// Per-hop decay for `GRAPH` when `DECAY` is omitted — the same default the
/// RPC (`graph_decay`), MCP and CLI (`--graph-decay`) surfaces already apply.
const DEFAULT_GRAPH_DECAY: f32 = 0.5;
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
                    .u32("iterations")?
                    .or(args.u32("max_iters")?)
                    .unwrap_or(d.max_iters),
                tolerance: args.float("tolerance")?.unwrap_or(d.tolerance),
            }
        }
        "components" | "connected_components" => Algo::ConnectedComponents,
        "louvain" => {
            let d = LouvainOptions::default();
            Algo::Louvain {
                max_levels: args.u32("max_levels")?.unwrap_or(d.max_levels),
                min_gain: args.float("min_gain")?.unwrap_or(d.min_gain),
            }
        }
        "shortest_path" => Algo::ShortestPath {
            from: args.node_ref("from")?,
            to: args.node_ref("to")?,
            // The direction is a keyword-like choice, so its case is free; the
            // weight names a property, whose case is the user's.
            dir: match args
                .string("dir")?
                .map(|s| s.to_ascii_lowercase())
                .as_deref()
            {
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

    /// A whole-number argument that must fit the option it sets: `1e12`
    /// iterations is refused, not silently truncated to whatever its low
    /// 32 bits spell.
    fn u32(&mut self, name: &str) -> Result<Option<u32>, String> {
        match self.int(name)? {
            None => Ok(None),
            Some(n) => u32::try_from(n)
                .map(Some)
                .map_err(|_| format!("`{name}` must be at most {}, got {n}", u32::MAX)),
        }
    }

    fn string(&mut self, name: &str) -> Result<Option<String>, String> {
        match self.take(name) {
            None => Ok(None),
            Some(PropValue::Str(s)) => Ok(Some(s)),
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

/// Whether an expression reads the row's `score()` or `hops()` channel —
/// values a path settles only at its last node.
fn reads_row_channel(e: &PExpr) -> bool {
    match e {
        PExpr::Score | PExpr::Hops => true,
        PExpr::Lit(_)
        | PExpr::Param(_)
        | PExpr::Prop { .. }
        | PExpr::HasLabel { .. }
        | PExpr::ExternalKey { .. }
        | PExpr::EdgeTypeOf { .. }
        | PExpr::EdgeDirOf { .. }
        | PExpr::Similarity { .. }
        | PExpr::Distance { .. } => false,
        PExpr::In { lhs, list } => reads_row_channel(lhs) || list.iter().any(reads_row_channel),
        PExpr::InValue { lhs, haystack } => reads_row_channel(lhs) || reads_row_channel(haystack),
        PExpr::IsNull(x) | PExpr::Not(x) | PExpr::Neg(x) => reads_row_channel(x),
        PExpr::StringMatch { lhs, rhs, .. }
        | PExpr::Compare { lhs, rhs, .. }
        | PExpr::Logic { lhs, rhs, .. }
        | PExpr::Arith { lhs, rhs, .. } => reads_row_channel(lhs) || reads_row_channel(rhs),
    }
}

/// The set of pattern variables an expression mentions.
fn referenced_vars(e: &PExpr) -> BTreeSet<String> {
    fn go(e: &PExpr, out: &mut BTreeSet<String>) {
        match e {
            PExpr::Prop { var, .. }
            | PExpr::HasLabel { var, .. }
            | PExpr::ExternalKey { var }
            | PExpr::EdgeTypeOf { var }
            | PExpr::EdgeDirOf { var }
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
            PExpr::InValue { lhs, haystack } => {
                go(lhs, out);
                go(haystack, out);
            }
            PExpr::StringMatch { lhs, rhs, .. } => {
                go(lhs, out);
                go(rhs, out);
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
/// Where an expression is evaluated: which pattern slot the row is on, and
/// what each variable is bound to.
///
/// A qualifier naming the current slot compiles away — there, the variable
/// *is* the row's current node. An earlier one becomes an [`Expr::At`], which
/// reads that slot's binding out of the trail the row already carries. This
/// is what lets a projection say `a.name, b.name` about two ends of a hop.
struct Scope<'a, 'b> {
    vars: &'a Vars<'b>,
    current: usize,
}

/// What a pattern's variables name: nodes by the slot they are current at,
/// relationships by the hop they are.
struct Vars<'a> {
    nodes: AHashMap<&'a str, usize>,
    rels: AHashMap<&'a str, usize>,
    /// The last slot whose place in a row's trail is certain.
    ///
    /// [`Expr::At`] addresses the trail, which a variable-length hop makes a
    /// different length per row, so past such a hop a pattern slot names
    /// different nodes in different rows. Only a term read at its own slot is
    /// safe there. [`usize::MAX`] when every hop walks one edge.
    exact_through: usize,
}

impl Vars<'_> {
    /// The slot a term about `var` can be evaluated at. A node's own slot; for
    /// a relationship, the slot just past its hop — the first point where the
    /// row has walked that edge and can read it.
    fn slot_of(&self, var: &str) -> Option<usize> {
        self.nodes
            .get(var)
            .copied()
            .or_else(|| self.rels.get(var).map(|hop| hop + 1))
    }

    /// Whether a term about `var` may be read from `at` instead of the
    /// variable's own slot. Its own slot always can; another only when the
    /// trail addresses it the same way in every row.
    fn readable_at(&self, var: &str, at: usize) -> bool {
        self.slot_of(var)
            .is_none_or(|own| own == at || own <= self.exact_through)
    }
}

/// The last slot a row's trail addresses the same way every time — the first
/// variable-length hop ends it, since from there on a row's length varies.
fn exact_through(hops: &[(Hop<'_>, &NodePat)]) -> usize {
    hops.iter()
        .position(|(h, _)| matches!(h, Hop::Rel(r) if r.var_len.is_some()))
        .unwrap_or(usize::MAX)
}

impl Scope<'_, '_> {
    /// `inner`, read against whatever node `var` names.
    fn read(&self, var: &str, inner: Expr) -> Result<Expr, String> {
        // An anonymous pattern node (`(:Paper)`) has no name to look up; it is
        // always the slot its predicate was placed at.
        if var.is_empty() {
            return Ok(inner);
        }
        if self.vars.rels.contains_key(var) {
            return Err(format!(
                "`{var}` is a relationship, not a node — read its properties \
                 (`{var}.key`), `type({var})` or `direction({var})`"
            ));
        }
        let slot = *self
            .vars
            .nodes
            .get(var)
            .ok_or_else(|| format!("unknown variable `{var}`"))?;
        Ok(match slot == self.current {
            true => inner,
            false => Expr::At {
                binding: Binding::Node(slot as u32),
                inner: Box::new(inner),
            },
        })
    }

    /// Which hop a relationship variable binds, if it names one.
    fn hop_of(&self, var: &str) -> Option<usize> {
        self.vars.rels.get(var).copied()
    }

    /// `inner`, read against the relationship bound at `hop`. Always through
    /// [`Expr::At`]: a hop's edge is addressed by index, never by being the
    /// row's current anything, so there is no slot for it to compile away at.
    fn read_hop(&self, hop: usize, inner: Expr) -> Expr {
        Expr::At {
            binding: Binding::Edge(hop as u32),
            inner: Box::new(inner),
        }
    }

    /// `inner`, read against the relationship `var` names — for the terms that
    /// only a relationship has, so a node variable here is the user's mistake.
    fn read_edge(&self, var: &str, inner: Expr) -> Result<Expr, String> {
        let hop = self.hop_of(var).ok_or_else(|| {
            format!(
                "`{var}` isn't a relationship variable; `type()` and `direction()` read \
                 a bound relationship, as in `-[{var}:TYPE]->`"
            )
        })?;
        Ok(self.read_hop(hop, inner))
    }
}

fn compile_expr(
    e: &PExpr,
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
    scope: &Scope,
) -> Result<Expr, String> {
    let sub = |x: &PExpr| compile_expr(x, embedder, params, scope).map(Box::new);
    Ok(match e {
        PExpr::Lit(v) => Expr::Literal(v.clone()),
        PExpr::Param(name) => Expr::Literal(crate::resolve_param(params, name)?),
        // A qualifier naming a relationship reads that hop's edge; everything
        // else is a node property.
        PExpr::Prop { var, key } => match scope.hop_of(var) {
            Some(hop) => scope.read_hop(hop, Expr::EdgeProperty(key.clone())),
            None => scope.read(var, Expr::Property(key.clone()))?,
        },
        PExpr::HasLabel { var, label } => scope.read(var, Expr::HasLabel(label.clone()))?,
        PExpr::ExternalKey { var } => scope.read(var, Expr::ExternalKey)?,
        PExpr::EdgeTypeOf { var } => scope.read_edge(var, Expr::EdgeType)?,
        PExpr::EdgeDirOf { var } => scope.read_edge(var, Expr::EdgeDir)?,
        // `x IN [a, b]` is sugar for `x = a OR x = b`; an empty list is
        // constantly false. (A source-anchored `key(n) IN […]` never reaches
        // here — it became a `SeekKeys` seek.)
        PExpr::In { lhs, list } => {
            let lhs = compile_expr(lhs, embedder, params, scope)?;
            let mut eqs = Vec::with_capacity(list.len());
            for item in list {
                eqs.push(Expr::Compare {
                    op: CmpOp::Eq,
                    lhs: Box::new(lhs.clone()),
                    rhs: sub(item)?,
                });
            }
            balanced(LogicOp::Or, eqs).unwrap_or(Expr::Literal(PropValue::Bool(false)))
        }
        // Not sugar, unlike `PExpr::In`: the haystack is a per-row value, so
        // it stays an operator the executor evaluates.
        PExpr::InValue { lhs, haystack } => Expr::In {
            needle: sub(lhs)?,
            haystack: sub(haystack)?,
        },
        PExpr::StringMatch { op, lhs, rhs } => Expr::StringMatch {
            op: *op,
            lhs: sub(lhs)?,
            rhs: sub(rhs)?,
        },
        PExpr::IsNull(x) => Expr::IsNull(sub(x)?),
        // `NOT` over a missing property: core's evaluator is two-valued (a
        // missing value makes the inner predicate false, so its negation
        // true), openCypher's is not (NOT null = null, which no filter
        // keeps). Guard with the leaves so both agree.
        PExpr::Not(x) => null_guarded(Expr::Not(sub(x)?), &[x], embedder, params, scope)?,
        // Fold `-literal` to a literal; otherwise `0 - x` (core has no negate).
        PExpr::Neg(x) => match compile_expr(x, embedder, params, scope)? {
            Expr::Literal(PropValue::Int(n)) => Expr::Literal(PropValue::Int(-n)),
            Expr::Literal(PropValue::Float(f)) => Expr::Literal(PropValue::Float(-f)),
            other => Expr::Arith {
                op: ArithOp::Sub,
                lhs: Box::new(Expr::Literal(PropValue::Int(0))),
                rhs: Box::new(other),
            },
        },
        PExpr::Compare { op, lhs, rhs } => {
            let cmp = Expr::Compare {
                op: *op,
                lhs: sub(lhs)?,
                rhs: sub(rhs)?,
            };
            match op {
                // `<>` against a missing value is true in core and null in
                // openCypher; `=` differs only when *both* sides can be
                // missing (Null = Null holds in core). The ordered
                // comparisons are already false on a missing operand.
                CmpOp::Ne => null_guarded(cmp, &[lhs, rhs], embedder, params, scope)?,
                CmpOp::Eq
                    if !is_non_null_constant(lhs, params) && !is_non_null_constant(rhs, params) =>
                {
                    null_guarded(cmp, &[lhs, rhs], embedder, params, scope)?
                }
                _ => cmp,
            }
        }
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
            var,
            property,
            query,
            metric,
        } => scope.read(
            var,
            Expr::Similarity {
                property: property.clone(),
                query: resolve(query, embedder)?,
                metric: *metric,
            },
        )?,
        PExpr::Distance {
            var,
            property,
            query,
            metric,
        } => scope.read(
            var,
            Expr::Distance {
                property: property.clone(),
                query: resolve(query, embedder)?,
                metric: *metric,
            },
        )?,
    })
}

/// A literal or parameter that is not `null` — an operand that can never be
/// missing, so needs no guard.
fn is_non_null_constant(e: &PExpr, params: &crate::Params) -> bool {
    match e {
        PExpr::Lit(v) => !matches!(v, PropValue::Null),
        PExpr::Param(name) => !matches!(
            crate::resolve_param(params, name),
            Ok(PropValue::Null) | Err(_)
        ),
        _ => false,
    }
}

/// The sub-expressions of `roots` that can evaluate to `Null`: property reads,
/// the external key, the row channels that may be unset, and a literal
/// `null` itself. Each distinct one once, in first-seen order.
fn nullable_leaves<'e>(roots: &[&'e PExpr]) -> Vec<&'e PExpr> {
    fn go<'e>(e: &'e PExpr, out: &mut Vec<&'e PExpr>) {
        match e {
            // An edge term reads `Null` where the hop was never walked, so it
            // is guarded like a property rather than assumed present.
            PExpr::Prop { .. }
            | PExpr::ExternalKey { .. }
            | PExpr::EdgeTypeOf { .. }
            | PExpr::EdgeDirOf { .. }
            | PExpr::Score
            | PExpr::Similarity { .. }
            | PExpr::Distance { .. }
            | PExpr::Lit(PropValue::Null)
            | PExpr::Param(_) => {
                if !out.contains(&e) {
                    out.push(e);
                }
            }
            // Never null: a non-null literal, a label test, a hop count, and
            // `IS NULL` itself (which is how null is asked about).
            PExpr::Lit(_) | PExpr::HasLabel { .. } | PExpr::Hops | PExpr::IsNull(_) => {}
            PExpr::In { lhs, list } => {
                go(lhs, out);
                for e in list {
                    go(e, out);
                }
            }
            PExpr::InValue { lhs, haystack } => {
                go(lhs, out);
                go(haystack, out);
            }
            PExpr::StringMatch { lhs, rhs, .. }
            | PExpr::Compare { lhs, rhs, .. }
            | PExpr::Logic { lhs, rhs, .. }
            | PExpr::Arith { lhs, rhs, .. } => {
                go(lhs, out);
                go(rhs, out);
            }
            PExpr::Not(x) | PExpr::Neg(x) => go(x, out),
        }
    }
    let mut out = Vec::new();
    for r in roots {
        go(r, &mut out);
    }
    out
}

/// `pred`, true only where every nullable leaf under `roots` is present:
/// `l1 IS NOT NULL AND l2 IS NOT NULL AND pred`. This is how the compiler
/// meets openCypher's null semantics on the predicates where a two-valued
/// evaluator says *true* for a missing value — `<>` and `NOT` — without
/// teaching the executor three-valued logic. It is exact for a predicate
/// over one property (the common case) and stricter than openCypher for a
/// compound one under `NOT`, where a false branch would have absorbed the
/// null: `NOT (n.a = 1 AND n.b = 2)` keeps a node with `b = 3` and no `a` in
/// openCypher, and drops it here. `lib.rs` documents that divergence.
fn null_guarded(
    pred: Expr,
    roots: &[&PExpr],
    embedder: Option<&dyn Embedder>,
    params: &crate::Params,
    scope: &Scope,
) -> Result<Expr, String> {
    let mut present = Vec::new();
    for leaf in nullable_leaves(roots) {
        // A param that doesn't resolve fails here as it would anywhere.
        present.push(compile_expr(leaf, embedder, params, scope)?.is_null().not());
    }
    Ok(match balanced(LogicOp::And, present) {
        None => pred,
        Some(guards) => guards.and(pred),
    })
}

/// `items` joined under `op`, or `None` when there are none.
///
/// Balanced, not left-deep: a list is as long as the query body allows
/// (`x IN [… × 100 000]` is under a megabyte), and core evaluates and drops
/// an `Expr` recursively on the machine stack, so a chain that long is a
/// stack overflow — a balanced tree of it is seventeen levels. The parser's
/// nesting budget bounds every *written* chain; this bounds the ones the
/// compiler makes. Pairwise rounds keep the first items leftmost, so a short
/// list reads as it did (`(a OR b) OR c`).
fn balanced(op: LogicOp, mut items: Vec<Expr>) -> Option<Expr> {
    while items.len() > 1 {
        let mut next = Vec::with_capacity(items.len().div_ceil(2));
        let mut it = items.into_iter();
        while let Some(lhs) = it.next() {
            next.push(match it.next() {
                Some(rhs) => Expr::Logic {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                None => lhs,
            });
        }
        items = next;
    }
    items.pop()
}
