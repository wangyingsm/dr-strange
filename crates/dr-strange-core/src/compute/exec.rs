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
//! node plus the trail of `(edge, node)` hops taken to reach it. `Filter`
//! and `Sort` address the current node.

use std::collections::HashSet;

use crate::cache::GraphReader;
use crate::compute::expr::{self, Expr};
use crate::compute::plan::{LogicalPlan, SortKey, Source, Step};
use crate::error::Result;
use crate::types::{Dir, EdgeId, NodeId, PropValue};

/// One row of the executor's stream: the current node and the path to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub head: NodeId,
    /// `(edge traversed, node reached)` for each hop, in order. Empty at a
    /// source. Carries path information for future path-returning queries.
    pub trail: Vec<(EdgeId, NodeId)>,
}

impl Row {
    fn start(head: NodeId) -> Self {
        Row {
            head,
            trail: Vec::new(),
        }
    }

    fn step(&self, edge: EdgeId, node: NodeId) -> Self {
        let mut trail = self.trail.clone();
        trail.push((edge, node));
        Row { head: node, trail }
    }
}

type RowIter<'r> = Box<dyn Iterator<Item = Result<Row>> + 'r>;

/// Builds the row stream for `plan` reading through `reader`. The returned
/// iterator borrows `reader` for its whole lifetime.
pub fn execute<'r>(plan: &LogicalPlan, reader: &'r dyn GraphReader) -> Result<RowIter<'r>> {
    let mut iter: RowIter<'r> = Box::new(source_rows(reader, &plan.source)?.into_iter().map(Ok));
    for step in &plan.steps {
        iter = apply_step(iter, step, reader)?;
    }
    Ok(iter)
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
    };
    Ok(ids.into_iter().map(Row::start).collect())
}

fn apply_step<'r>(
    iter: RowIter<'r>,
    step: &Step,
    reader: &'r dyn GraphReader,
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
            Box::new(iter.filter_map(move |rr| filter_one(reader, rr, &pred)))
        }
        Step::Skip(n) => Box::new(iter.skip(*n as usize)),
        Step::Limit(n) => Box::new(iter.take(*n as usize)),
        Step::Distinct => {
            let mut seen: HashSet<NodeId> = HashSet::new();
            Box::new(iter.filter_map(move |rr| match rr {
                Err(e) => Some(Err(e)),
                Ok(row) => seen.insert(row.head).then_some(Ok(row)),
            }))
        }
        // Barrier: drain, sort, re-emit.
        Step::Sort(keys) => Box::new(sort_rows(iter, keys, reader)?.into_iter().map(Ok)),
    })
}

fn expand_one(
    reader: &dyn GraphReader,
    rr: Result<Row>,
    dir: Dir,
    ty: &Option<String>,
) -> std::vec::IntoIter<Result<Row>> {
    let row = match rr {
        Ok(r) => r,
        Err(e) => return vec![Err(e)].into_iter(),
    };
    match reader.neighbors(row.head, dir, ty.as_deref()) {
        Err(e) => vec![Err(e)].into_iter(),
        Ok(ns) => ns
            .iter()
            .map(|n| Ok(row.step(n.edge, n.node)))
            .collect::<Vec<_>>()
            .into_iter(),
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

fn filter_one(reader: &dyn GraphReader, rr: Result<Row>, pred: &Expr) -> Option<Result<Row>> {
    match rr {
        Err(e) => Some(Err(e)),
        Ok(row) => match reader.node(row.head) {
            Err(e) => Some(Err(e)),
            Ok(node) => {
                if expr::is_true(&expr::eval(pred, node.as_deref())) {
                    Some(Ok(row))
                } else {
                    None
                }
            }
        },
    }
}

fn sort_rows(iter: RowIter<'_>, keys: &[SortKey], reader: &dyn GraphReader) -> Result<Vec<Row>> {
    // Decorate each row with its precomputed key values (one node lookup +
    // eval per row), sort, undecorate — so comparisons don't re-evaluate.
    let mut decorated: Vec<(Vec<PropValue>, Row)> = Vec::new();
    for rr in iter {
        let row = rr?;
        let node = reader.node(row.head)?;
        let vals = keys
            .iter()
            .map(|k| expr::eval(&k.expr, node.as_deref()))
            .collect();
        decorated.push((vals, row));
    }
    decorated.sort_by(|a, b| cmp_keys(&a.0, &b.0, keys));
    Ok(decorated.into_iter().map(|(_, row)| row).collect())
}

fn cmp_keys(a: &[PropValue], b: &[PropValue], keys: &[SortKey]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (i, key) in keys.iter().enumerate() {
        // Incomparable pairs (mismatched types, Null, NaN) sort as Equal,
        // keeping the sort total and stable rather than panicking.
        let ord = expr::partial_cmp(&a[i], &b[i]).unwrap_or(Ordering::Equal);
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
    use crate::compute::expr::{has_label, p};
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
}
