//! Compile and execute parsed write statements. Unlike reads (which compile to
//! a `LogicalPlan` the surfaces run), writes are imperative — core mutates
//! through `WriteTxn`, not a serializable plan — so the query-language runtime
//! applies them here, in one transaction committed atomically.
//!
//! A `MATCH … SET/REMOVE/DELETE` is **find-then-mutate**: the `MATCH` compiles
//! to a read plan whose terminal node is the bound variable; we run it to get
//! the ids, then apply the ops to each. A standalone `CREATE` just builds nodes
//! and edges.

use std::collections::HashMap;

use dr_strange_core::{
    Dir, LogicalPlan, NodeId, PlaneHandle, PropDesc, PropValue, Properties, WriteTxn,
};

use crate::ast::*;

/// What a write statement changed. Returned so a surface can report
/// `2 nodes, 1 edge created`, etc.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteSummary {
    pub nodes_created: u64,
    pub edges_created: u64,
    pub props_set: u64,
    pub labels_set: u64,
    pub nodes_deleted: u64,
    pub edges_deleted: u64,
}

/// A compiled, executable write statement: an optional bound `MATCH` (variable
/// name + its read plan) and the mutation ops. Build with [`compile`].
#[derive(Debug)]
pub struct WriteStatement {
    /// `Some((var, plan))` for `MATCH … SET/REMOVE/DELETE`; `None` for `CREATE`.
    binding: Option<(String, LogicalPlan)>,
    ops: Vec<WriteOp>,
    /// Whether any op adds/removes a label (so we preload current label sets).
    has_label_ops: bool,
}

impl WriteStatement {
    /// Execute against `plane` in one committed transaction.
    pub fn apply(&self, plane: &PlaneHandle<'_>) -> Result<WriteSummary, String> {
        execute(plane, self)
    }
}

/// The variable bound by a `MATCH` pattern — its terminal node's variable.
fn terminal_var(p: &Pattern) -> Option<&str> {
    match p.rest.last() {
        Some((_, n)) => n.var.as_deref(),
        None => p.first.var.as_deref(),
    }
}

fn set_item_var(it: &SetItem) -> &str {
    match it {
        SetItem::Prop { var, .. } | SetItem::Label { var, .. } | SetItem::Merge { var, .. } => var,
    }
}

fn remove_item_var(it: &RemoveItem) -> &str {
    match it {
        RemoveItem::Prop { var, .. } | RemoveItem::Label { var, .. } => var,
    }
}

fn is_label_op(op: &WriteOp) -> bool {
    match op {
        WriteOp::Set(items) => items.iter().any(|i| matches!(i, SetItem::Label { .. })),
        WriteOp::Remove(items) => items.iter().any(|i| matches!(i, RemoveItem::Label { .. })),
        _ => false,
    }
}

/// Validate a parsed write and compile its `MATCH` (if any) into a read plan.
pub fn compile(ast: WriteAst) -> Result<WriteStatement, String> {
    let has_label_ops = ast.ops.iter().any(is_label_op);

    let binding = match ast.match_clause {
        // Standalone: only CREATE is allowed with no MATCH.
        None => {
            for op in &ast.ops {
                if !matches!(op, WriteOp::Create(_)) {
                    return Err("SET / REMOVE / DELETE require a MATCH to select nodes".to_string());
                }
            }
            None
        }
        // MATCH …: only SET/REMOVE/DELETE, all referencing the terminal variable.
        Some(m) => {
            let var = terminal_var(&m.pattern)
                .ok_or("MATCH … SET/REMOVE/DELETE needs the last node to have a variable")?
                .to_string();
            for op in &ast.ops {
                match op {
                    WriteOp::Create(_) => {
                        return Err("CREATE after MATCH isn't supported yet".to_string());
                    }
                    WriteOp::Set(items) => check_var(items.iter().map(set_item_var), &var)?,
                    WriteOp::Remove(items) => check_var(items.iter().map(remove_item_var), &var)?,
                    WriteOp::Delete { vars, .. } => {
                        check_var(vars.iter().map(String::as_str), &var)?
                    }
                }
            }
            // Compile the MATCH to a read plan returning the terminal node.
            let query = Query {
                source: QuerySource::Match(m.pattern),
                beams: Vec::new(),
                where_clause: m.where_clause,
                ret: Return {
                    distinct: false,
                    item: ReturnItem::Star,
                },
                order_by: Vec::new(),
                skip: None,
                limit: None,
            };
            let plan = crate::compile::compile(query, None)?;
            Some((var, plan))
        }
    };

    Ok(WriteStatement {
        binding,
        ops: ast.ops,
        has_label_ops,
    })
}

fn check_var<'a>(vars: impl Iterator<Item = &'a str>, bound: &str) -> Result<(), String> {
    for v in vars {
        if v != bound {
            return Err(format!(
                "`{v}` is not the matched variable `{bound}`; a mutation may reference \
                 only the pattern's terminal variable"
            ));
        }
    }
    Ok(())
}

fn props_of(entries: &[(String, PropValue)]) -> Properties {
    entries
        .iter()
        .map(|(k, v)| (k.clone(), PropDesc::new(v.clone())))
        .collect()
}

fn execute(plane: &PlaneHandle<'_>, stmt: &WriteStatement) -> Result<WriteSummary, String> {
    let mut summary = WriteSummary::default();

    // Find-then-mutate: run the MATCH read plan to get the target ids.
    let ids: Vec<NodeId> = match &stmt.binding {
        Some((_, plan)) => plane
            .query_from_plan(plan.clone())
            .ids()
            .map_err(|e| e.to_string())?,
        None => Vec::new(),
    };

    // Preload current labels for the targets (label SET/REMOVE is
    // read-modify-write; core `set_labels` replaces the whole set).
    let mut labels: HashMap<u64, Vec<String>> = HashMap::new();
    if stmt.has_label_ops {
        for id in &ids {
            if let Some(n) = plane.node(*id).map_err(|e| e.to_string())? {
                labels.insert(id.0, n.labels);
            }
        }
    }

    let mut txn = plane.write().map_err(|e| e.to_string())?;
    let mut vars: HashMap<&str, NodeId> = HashMap::new();

    for op in &stmt.ops {
        match op {
            WriteOp::Create(paths) => {
                for path in paths {
                    create_path(&mut txn, path, &mut vars, &mut summary)?;
                }
            }
            WriteOp::Set(items) => {
                for id in &ids {
                    for it in items {
                        apply_set(&mut txn, *id, it, &mut labels, &mut summary)?;
                    }
                }
            }
            WriteOp::Remove(items) => {
                for id in &ids {
                    for it in items {
                        apply_remove(&mut txn, *id, it, &mut labels, &mut summary)?;
                    }
                }
            }
            WriteOp::Delete { detach, .. } => {
                for id in &ids {
                    // Plain DELETE refuses a node with relationships (Cypher
                    // semantics); DETACH DELETE cascades (core deletes incident
                    // edges with the node).
                    if !detach
                        && !plane
                            .neighbors(*id, Dir::Both, None)
                            .map_err(|e| e.to_string())?
                            .is_empty()
                    {
                        return Err(format!(
                            "cannot DELETE node {} — it still has relationships; use DETACH DELETE",
                            id.0
                        ));
                    }
                    txn.delete_node(*id).map_err(|e| e.to_string())?;
                    summary.nodes_deleted += 1;
                    labels.remove(&id.0);
                }
            }
        }
    }

    txn.commit().map_err(|e| e.to_string())?;
    Ok(summary)
}

fn apply_set(
    txn: &mut WriteTxn<'_>,
    id: NodeId,
    it: &SetItem,
    labels: &mut HashMap<u64, Vec<String>>,
    summary: &mut WriteSummary,
) -> Result<(), String> {
    match it {
        SetItem::Prop { key, value, .. } => {
            txn.set_prop(id, key, PropDesc::new(value.clone()))
                .map_err(|e| e.to_string())?;
            summary.props_set += 1;
        }
        SetItem::Merge { props, .. } => {
            for (k, v) in props {
                txn.set_prop(id, k, PropDesc::new(v.clone()))
                    .map_err(|e| e.to_string())?;
                summary.props_set += 1;
            }
        }
        SetItem::Label { label, .. } => {
            let set = labels.entry(id.0).or_default();
            if !set.iter().any(|l| l == label) {
                set.push(label.clone());
                let refs: Vec<&str> = set.iter().map(String::as_str).collect();
                txn.set_labels(id, &refs).map_err(|e| e.to_string())?;
                summary.labels_set += 1;
            }
        }
    }
    Ok(())
}

fn apply_remove(
    txn: &mut WriteTxn<'_>,
    id: NodeId,
    it: &RemoveItem,
    labels: &mut HashMap<u64, Vec<String>>,
    summary: &mut WriteSummary,
) -> Result<(), String> {
    match it {
        RemoveItem::Prop { key, .. } => {
            txn.remove_prop(id, key).map_err(|e| e.to_string())?;
            summary.props_set += 1;
        }
        RemoveItem::Label { label, .. } => {
            let set = labels.entry(id.0).or_default();
            let before = set.len();
            set.retain(|l| l != label);
            if set.len() != before {
                let refs: Vec<&str> = set.iter().map(String::as_str).collect();
                txn.set_labels(id, &refs).map_err(|e| e.to_string())?;
                summary.labels_set += 1;
            }
        }
    }
    Ok(())
}

// ---- CREATE ---------------------------------------------------------------

fn get_or_create<'a>(
    txn: &mut WriteTxn<'_>,
    cn: &'a CreateNode,
    vars: &mut HashMap<&'a str, NodeId>,
    summary: &mut WriteSummary,
) -> Result<NodeId, String> {
    if let Some(v) = &cn.var
        && let Some(&id) = vars.get(v.as_str())
    {
        return Ok(id); // same variable → the same node
    }
    let labels: Vec<&str> = cn.label.as_deref().into_iter().collect();
    let props = props_of(&cn.props);
    let id = match &cn.key {
        Some(k) => txn
            .create_node_with_key(k, &labels, props)
            .map_err(|e| e.to_string())?,
        None => txn.create_node(&labels, props).map_err(|e| e.to_string())?,
    };
    summary.nodes_created += 1;
    if let Some(v) = &cn.var {
        vars.insert(v.as_str(), id);
    }
    Ok(id)
}

fn create_path<'a>(
    txn: &mut WriteTxn<'_>,
    path: &'a CreatePath,
    vars: &mut HashMap<&'a str, NodeId>,
    summary: &mut WriteSummary,
) -> Result<(), String> {
    let mut prev = get_or_create(txn, &path.first, vars, summary)?;
    for (rel, node) in &path.rest {
        let cur = get_or_create(txn, node, vars, summary)?;
        // `->` is prev→cur; `<-` is cur→prev (the parser rejects undirected).
        let (src, dst) = match rel.dir {
            Dir::In => (cur, prev),
            _ => (prev, cur),
        };
        txn.create_edge(src, dst, &rel.ty, props_of(&rel.props))
            .map_err(|e| e.to_string())?;
        summary.edges_created += 1;
        prev = cur;
    }
    Ok(())
}
