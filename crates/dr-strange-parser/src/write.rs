//! Execute a parsed [`WriteAst`] against a plane. Unlike reads (which compile
//! to a `LogicalPlan` the surfaces run), writes are imperative — core exposes
//! mutation through `WriteTxn`, not a serializable plan — so the query-language
//! runtime applies them here, in one transaction committed atomically.

use std::collections::HashMap;

use dr_strange_core::{NodeId, PlaneHandle, PropDesc, PropValue, Properties, WriteTxn};

use crate::ast::*;

/// What a write statement changed. Returned to the caller so a surface can
/// report `2 nodes, 1 edge created`, etc.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteSummary {
    pub nodes_created: u64,
    pub edges_created: u64,
    pub props_set: u64,
    pub labels_set: u64,
    pub nodes_deleted: u64,
    pub edges_deleted: u64,
}

/// Apply a write statement to `plane` in a single committed transaction.
pub fn execute(plane: &PlaneHandle<'_>, stmt: &WriteAst) -> Result<WriteSummary, String> {
    let mut txn = plane.write().map_err(|e| e.to_string())?;
    let mut summary = WriteSummary::default();
    // Variables bound within the statement (a CREATE var → its new node id), so
    // `(a)…(a)` and edge endpoints resolve to the same node.
    let mut vars: HashMap<&str, NodeId> = HashMap::new();

    for op in &stmt.ops {
        match op {
            WriteOp::Create(paths) => {
                for path in paths {
                    create_path(&mut txn, path, &mut vars, &mut summary)?;
                }
            }
        }
    }

    txn.commit().map_err(|e| e.to_string())?;
    Ok(summary)
}

fn props_of(entries: &[(String, PropValue)]) -> Properties {
    entries
        .iter()
        .map(|(k, v)| (k.clone(), PropDesc::new(v.clone())))
        .collect()
}

/// Create (or, if the variable is already bound, reuse) a node.
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
            dr_strange_core::Dir::In => (cur, prev),
            _ => (prev, cur),
        };
        txn.create_edge(src, dst, &rel.ty, props_of(&rel.props))
            .map_err(|e| e.to_string())?;
        summary.edges_created += 1;
        prev = cur;
    }
    Ok(())
}
