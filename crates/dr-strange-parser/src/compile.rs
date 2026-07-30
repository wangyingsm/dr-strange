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

use dr_strange_core::compute::expr::ArithOp;
use dr_strange_core::{Expr, LogicalPlan, PropValue, SortKey, Source, Step};

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

pub fn compile(q: Query, embedder: Option<&dyn Embedder>) -> Result<LogicalPlan, String> {
    // Derive the source + first node from MATCH or SEARCH. Both consume their
    // first node's label into the source (ScanLabel / VectorTopK), so it never
    // becomes a HasLabel filter.
    let (source, first_node): (Source, &NodePat) = match &q.source {
        QuerySource::Match(p) => {
            let src = match &p.first.label {
                Some(l) => Source::ScanLabel(l.clone()),
                None => Source::ScanAll,
            };
            (src, &p.first)
        }
        QuerySource::Search(s) => {
            let src = Source::VectorTopK {
                label: s.node.label.clone(),
                property: s.property.clone(),
                query: resolve(&s.query, embedder)?,
                metric: s.metric,
                k: s.k,
            };
            (src, &s.node)
        }
    };

    // The hops after the source, in path order: the MATCH pattern's
    // relationships (if any), then any BEAM clauses.
    let mut hops: Vec<(Hop, &NodePat)> = Vec::new();
    if let QuerySource::Match(p) = &q.source {
        hops.extend(p.rest.iter().map(|(r, n)| (Hop::Rel(r), n)));
    }
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

    // WHERE conjuncts, each pushed down to its single variable's slot.
    let mut slot_filters: Vec<Vec<Expr>> = vec![Vec::new(); nodes.len()];
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
            slot_filters[slot].push(compile_expr(&conj, embedder)?);
        }
    }

    let mut steps: Vec<Step> = Vec::new();
    steps.extend(slot_filters[0].drain(..).map(Step::Filter));

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
        steps.extend(slot_filters[idx].drain(..).map(Step::Filter));
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
                expr: compile_expr(&ok.expr, embedder)?,
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
            | PExpr::Similarity { var, .. }
            | PExpr::Distance { var, .. } => {
                out.insert(var.clone());
            }
            // score()/hops() read the row channel, not a variable's node.
            PExpr::Lit(_) | PExpr::Score | PExpr::Hops => {}
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
fn compile_expr(e: &PExpr, embedder: Option<&dyn Embedder>) -> Result<Expr, String> {
    let sub = |x: &PExpr| compile_expr(x, embedder).map(Box::new);
    Ok(match e {
        PExpr::Lit(v) => Expr::Literal(v.clone()),
        PExpr::Prop { key, .. } => Expr::Property(key.clone()),
        PExpr::HasLabel { label, .. } => Expr::HasLabel(label.clone()),
        PExpr::IsNull(x) => Expr::IsNull(sub(x)?),
        PExpr::Not(x) => Expr::Not(sub(x)?),
        // Fold `-literal` to a literal; otherwise `0 - x` (core has no negate).
        PExpr::Neg(x) => match compile_expr(x, embedder)? {
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
