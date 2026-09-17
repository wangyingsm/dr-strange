//! Compile and execute parsed write statements. Unlike reads (which compile to
//! a `LogicalPlan` the surfaces run), writes are imperative — core mutates
//! through `WriteTxn`, not a serializable plan — so the query-language runtime
//! applies them here, in one transaction committed atomically.
//!
//! A `MATCH … SET/REMOVE/DELETE` is **find-then-mutate**: the `MATCH` compiles
//! to a read plan whose terminal node is the bound variable; we run it to get
//! the ids, then apply the ops to each. A standalone `CREATE` just builds nodes
//! and edges.

use ahash::{AHashMap, AHashSet};

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
    /// Values for `$name` placeholders, resolved when props are written.
    params: crate::Params,
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
pub fn compile(mut ast: WriteAst, params: crate::Params) -> Result<WriteStatement, String> {
    let has_label_ops = ast.ops.iter().any(is_label_op);
    resolve_keys(&mut ast.ops, &params)?;

    let binding = match ast.match_clause {
        // Standalone: only CREATE / MERGE are allowed with no MATCH.
        None => {
            for op in &ast.ops {
                match op {
                    WriteOp::Create(_) => {}
                    WriteOp::Merge(m) => validate_merge(m)?,
                    _ => {
                        return Err(
                            "SET / REMOVE / DELETE require a MATCH to select nodes".to_string()
                        );
                    }
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
                    // CREATE after MATCH runs once per matched row: nodes that
                    // reuse the matched variable anchor to it; other vars are
                    // new. No var restriction (unlike SET/REMOVE/DELETE).
                    WriteOp::Create(_) => {}
                    WriteOp::Merge(m) => validate_merge_after_match(m, &var)?,
                    WriteOp::Set(items) => check_var(items.iter().map(set_item_var), &var)?,
                    WriteOp::Remove(items) => check_var(items.iter().map(remove_item_var), &var)?,
                    WriteOp::Delete { vars, .. } => {
                        check_var(vars.iter().map(String::as_str), &var)?
                    }
                }
            }
            // Compile the MATCH to a read plan returning the terminal node.
            let query = Query {
                source: QuerySource {
                    kind: SourceKind::Match,
                    first: m.pattern.first,
                    rest: m.pattern.rest,
                },
                beams: Vec::new(),
                where_clause: m.where_clause,
                // One row per matched node, not per path: a pattern that
                // reaches a node twice (`(a)-->(n)`, two `a`s) would otherwise
                // delete it twice — the second time a missing node — and the
                // ops can't tell the rows apart anyway, since only the
                // terminal variable is theirs to mutate.
                ret: Return {
                    distinct: true,
                    items: vec![ReturnItem::Star],
                },
                order_by: Vec::new(),
                skip: None,
                limit: None,
                // A write never time-travels: it targets the current state.
                as_of: None,
            };
            let plan = crate::compile::compile(query, None, &params)?;
            Some((var, plan))
        }
    };

    Ok(WriteStatement {
        binding,
        ops: ast.ops,
        has_label_ops,
        params,
    })
}

/// Resolve every `{key: $param}` to its string now, so a missing or non-string
/// key is the statement's error rather than one row's, and the run-time path
/// reads plain literals ([`literal_key`]).
fn resolve_keys(ops: &mut [WriteOp], params: &crate::Params) -> Result<(), String> {
    fn resolve_node(cn: &mut CreateNode, params: &crate::Params) -> Result<(), String> {
        if let Some(Val::Param(name)) = &cn.key {
            match crate::resolve_param(params, name)? {
                s @ PropValue::Str(_) => cn.key = Some(Val::Lit(s)),
                other => {
                    return Err(format!(
                        "`key:` must be a string to serve as the external key; `${name}` is {other:?}"
                    ));
                }
            }
        }
        Ok(())
    }
    for op in ops {
        let paths: Vec<&mut CreatePath> = match op {
            WriteOp::Create(paths) => paths.iter_mut().collect(),
            WriteOp::Merge(m) => vec![&mut m.path],
            _ => continue,
        };
        for path in paths {
            resolve_node(&mut path.first, params)?;
            for (_, node) in &mut path.rest {
                resolve_node(node, params)?;
            }
        }
    }
    Ok(())
}

/// The node's external key, which [`resolve_keys`] has made a literal.
fn literal_key(cn: &CreateNode) -> Option<&str> {
    match &cn.key {
        Some(Val::Lit(PropValue::Str(s))) => Some(s),
        _ => None,
    }
}

/// A MERGE upserts by external key, so every node needs a string `key:` —
/// unless it re-names a node earlier in the path (`(a)-[:R]->(b)<-[:S]-(a)`),
/// which is already resolved. ON CREATE/MATCH SET is only for a single-node
/// MERGE and references its variable.
fn validate_merge(m: &MergeClause) -> Result<(), String> {
    let nodes = std::iter::once(&m.path.first).chain(m.path.rest.iter().map(|(_, n)| n));
    let mut bound: AHashSet<&str> = AHashSet::new();
    for n in nodes {
        let rebound = n.var.as_deref().is_some_and(|v| !bound.insert(v));
        if n.key.is_none() && !rebound {
            return Err(
                "MERGE needs a `key:` on every node to upsert on, e.g. MERGE (n:Label {key:\"…\"})"
                    .to_string(),
            );
        }
    }
    if !m.path.rest.is_empty() && (!m.on_create.is_empty() || !m.on_match.is_empty()) {
        return Err(
            "ON CREATE / ON MATCH SET is only supported for a single-node MERGE".to_string(),
        );
    }
    let var = m.path.first.var.as_deref();
    for it in m.on_create.iter().chain(&m.on_match) {
        match var {
            Some(v) if set_item_var(it) == v => {}
            _ => {
                return Err(
                    "ON CREATE / ON MATCH SET must reference the MERGE variable".to_string()
                );
            }
        }
    }
    Ok(())
}

/// A MERGE after MATCH must extend the matched node with a relationship: the
/// first node anchors to the matched variable, every later node needs a key,
/// and ON CREATE/MATCH SET isn't supported here.
fn validate_merge_after_match(m: &MergeClause, terminal: &str) -> Result<(), String> {
    if m.path.first.var.as_deref() != Some(terminal) {
        return Err(format!(
            "MERGE after MATCH must start from the matched variable `{terminal}`, \
             e.g. MATCH ({terminal}) MERGE ({terminal})-[:R]->(b {{key:\"…\"}})"
        ));
    }
    if m.path.rest.is_empty() {
        return Err(
            "MERGE after MATCH must extend the matched node with a relationship".to_string(),
        );
    }
    for (_, n) in &m.path.rest {
        if n.key.is_none() {
            return Err("MERGE needs a `key:` on every non-anchor node".to_string());
        }
    }
    if !m.on_create.is_empty() || !m.on_match.is_empty() {
        return Err("ON CREATE / ON MATCH SET isn't supported for a MERGE after MATCH".to_string());
    }
    Ok(())
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

fn resolve_val(v: &Val, params: &crate::Params) -> Result<PropValue, String> {
    match v {
        Val::Lit(p) => Ok(p.clone()),
        Val::Param(name) => crate::resolve_param(params, name),
    }
}

fn props_of(entries: &[(String, Val)], params: &crate::Params) -> Result<Properties, String> {
    entries
        .iter()
        .map(|(k, v)| Ok((k.clone(), PropDesc::new(resolve_val(v, params)?))))
        .collect()
}

fn execute(plane: &PlaneHandle<'_>, stmt: &WriteStatement) -> Result<WriteSummary, String> {
    let mut staged = Staged::new(&stmt.params);

    // Find-then-mutate: run the MATCH read plan to get the target ids.
    let ids: Vec<NodeId> = match &stmt.binding {
        Some((_, plan)) => plane
            .query_from_plan(plan.clone())
            .ids()
            .map_err(|e| e.to_string())?,
        None => Vec::new(),
    };

    // Preloaded rather than read per op: a label SET/REMOVE rewrites the whole
    // set, so the ops need the one the store holds now.
    if stmt.has_label_ops {
        for id in &ids {
            if let Some(n) = plane.node(*id).map_err(|e| e.to_string())? {
                staged.labels.insert(id.0, n.labels);
            }
        }
    }

    let rows = Rows { stmt, ids: &ids };
    let mut txn = plane.write().map_err(|e| e.to_string())?;
    for op in &stmt.ops {
        match op {
            WriteOp::Create(paths) => {
                run_create(&mut txn, paths, rows, &mut staged)?;
            }
            WriteOp::Merge(m) => {
                run_merge(plane, &mut txn, m, rows, &mut staged)?;
            }
            WriteOp::Set(items) => {
                for id in &ids {
                    for it in items {
                        apply_set(&mut txn, *id, it, &mut staged)?;
                    }
                }
            }
            WriteOp::Remove(items) => {
                for id in &ids {
                    for it in items {
                        apply_remove(&mut txn, *id, it, &mut staged)?;
                    }
                }
            }
            WriteOp::Delete { detach, .. } => {
                run_delete(plane, &mut txn, rows, *detach, &mut staged)?;
            }
        }
    }

    txn.commit().map_err(|e| e.to_string())?;
    Ok(staged.summary)
}

/// `CREATE`, standalone or after a `MATCH`.
///
/// Standalone builds once, with variables scoped to the clause. After a match
/// it runs once per matched row with the matched variable pre-bound, so `(a)`
/// anchors to that row's node rather than creating a fresh one.
fn run_create<'a>(
    txn: &mut WriteTxn<'_>,
    paths: &'a [CreatePath],
    rows: Rows<'a>,
    staged: &mut Staged<'a>,
) -> Result<(), String> {
    match &rows.stmt.binding {
        None => {
            let mut vars: AHashMap<&'a str, NodeId> = AHashMap::new();
            for path in paths {
                create_path(txn, path, &mut vars, staged)?;
            }
        }
        Some((bound, _)) => {
            for id in rows.ids {
                let mut vars: AHashMap<&'a str, NodeId> = AHashMap::new();
                vars.insert(bound.as_str(), *id);
                for path in paths {
                    create_path(txn, path, &mut vars, staged)?;
                }
            }
        }
    }
    Ok(())
}

/// `MERGE`, standalone or once per matched row — the same anchoring rule as
/// [`run_create`]. [`Staged::keyed`] outlives the rows on purpose: a keyed node
/// two rows both name must resolve to one node, and `node_by_key` cannot see a
/// create this transaction has not committed.
fn run_merge<'a>(
    plane: &PlaneHandle<'_>,
    txn: &mut WriteTxn<'_>,
    m: &'a MergeClause,
    rows: Rows<'a>,
    staged: &mut Staged<'a>,
) -> Result<(), String> {
    match &rows.stmt.binding {
        None => {
            let mut vars: AHashMap<&'a str, NodeId> = AHashMap::new();
            merge_path(plane, txn, m, &mut vars, staged)?;
        }
        Some((bound, _)) => {
            for id in rows.ids {
                let mut vars: AHashMap<&'a str, NodeId> = AHashMap::new();
                vars.insert(bound.as_str(), *id);
                merge_path(plane, txn, m, &mut vars, staged)?;
            }
        }
    }
    Ok(())
}

/// `DELETE`, and `DETACH DELETE`.
///
/// Plain `DELETE` refuses a node that still has relationships, which is
/// Cypher's own semantics; `DETACH DELETE` cascades, core deleting the
/// incident edges along with the node.
fn run_delete(
    plane: &PlaneHandle<'_>,
    txn: &mut WriteTxn<'_>,
    rows: Rows<'_>,
    detach: bool,
    staged: &mut Staged<'_>,
) -> Result<(), String> {
    for id in rows.ids {
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
        staged.summary.nodes_deleted += 1;
        staged.labels.remove(&id.0);
    }
    Ok(())
}

fn apply_set(
    txn: &mut WriteTxn<'_>,
    id: NodeId,
    it: &SetItem,
    staged: &mut Staged<'_>,
) -> Result<(), String> {
    match it {
        SetItem::Prop { key, value, .. } => {
            txn.set_prop(id, key, PropDesc::new(resolve_val(value, staged.params)?))
                .map_err(|e| e.to_string())?;
            staged.summary.props_set += 1;
        }
        SetItem::Merge { props, .. } => {
            for (k, v) in props {
                txn.set_prop(id, k, PropDesc::new(resolve_val(v, staged.params)?))
                    .map_err(|e| e.to_string())?;
                staged.summary.props_set += 1;
            }
        }
        SetItem::Label { label, .. } => {
            let set = staged.labels.entry(id.0).or_default();
            if !set.iter().any(|l| l == label) {
                set.push(label.clone());
                let refs: Vec<&str> = set.iter().map(String::as_str).collect();
                txn.set_labels(id, &refs).map_err(|e| e.to_string())?;
                staged.summary.labels_set += 1;
            }
        }
    }
    Ok(())
}

fn apply_remove(
    txn: &mut WriteTxn<'_>,
    id: NodeId,
    it: &RemoveItem,
    staged: &mut Staged<'_>,
) -> Result<(), String> {
    match it {
        RemoveItem::Prop { key, .. } => {
            txn.remove_prop(id, key).map_err(|e| e.to_string())?;
            staged.summary.props_set += 1;
        }
        RemoveItem::Label { label, .. } => {
            let set = staged.labels.entry(id.0).or_default();
            let before = set.len();
            set.retain(|l| l != label);
            if set.len() != before {
                let refs: Vec<&str> = set.iter().map(String::as_str).collect();
                txn.set_labels(id, &refs).map_err(|e| e.to_string())?;
                staged.summary.labels_set += 1;
            }
        }
    }
    Ok(())
}

// ---- MERGE ----------------------------------------------------------------

/// Execute a MERGE (single node or path): upsert each keyed node by external
/// key, apply ON CREATE/MATCH SET to a single node, and ensure each edge.
fn merge_path<'a>(
    plane: &PlaneHandle<'_>,
    txn: &mut WriteTxn<'_>,
    m: &'a MergeClause,
    vars: &mut AHashMap<&'a str, NodeId>,
    staged: &mut Staged<'a>,
) -> Result<(), String> {
    let (first_id, created) = upsert_merge_node(plane, txn, &m.path.first, vars, staged)?;
    // ON CREATE / ON MATCH SET apply to a single-node MERGE's node.
    let items = if created { &m.on_create } else { &m.on_match };
    for it in items {
        apply_set(txn, first_id, it, staged)?;
    }
    let mut prev = first_id;
    for (rel, node) in &m.path.rest {
        let (cur, _) = upsert_merge_node(plane, txn, node, vars, staged)?;
        ensure_edge(plane, txn, (prev, cur), rel, staged)?;
        prev = cur;
    }
    Ok(())
}

/// Find a node by external key (and bind it) or create it. Reuse order: an
/// already-bound variable (a matched anchor / an earlier node in this path),
/// then a node upserted earlier *in this statement* by the same key
/// ([`Staged::keyed`] — needed because `node_by_key` can't see the uncommitted
/// create, e.g. a shared MERGE target across matched rows), then the committed
/// store. Returns `(id, created)`.
fn upsert_merge_node<'a>(
    plane: &PlaneHandle<'_>,
    txn: &mut WriteTxn<'_>,
    cn: &'a CreateNode,
    vars: &mut AHashMap<&'a str, NodeId>,
    staged: &mut Staged<'a>,
) -> Result<(NodeId, bool), String> {
    if let Some(v) = &cn.var
        && let Some(&id) = vars.get(v.as_str())
    {
        return Ok((id, false));
    }
    let key = literal_key(cn).ok_or("MERGE node needs a `key:` to upsert on")?;

    if let Some(&id) = staged.keyed.get(key) {
        if let Some(v) = &cn.var {
            vars.insert(v.as_str(), id);
        }
        return Ok((id, false));
    }

    let (id, created) = match plane.node_by_key(key).map_err(|e| e.to_string())? {
        Some(found) => {
            let id = found.id;
            staged.labels.entry(id.0).or_insert(found.labels);
            (id, false)
        }
        None => {
            let label_refs: Vec<&str> = cn.label.as_deref().into_iter().collect();
            let id = txn
                .create_node_with_key(key, &label_refs, props_of(&cn.props, staged.params)?)
                .map_err(|e| e.to_string())?;
            staged.summary.nodes_created += 1;
            staged
                .labels
                .entry(id.0)
                .or_insert_with(|| cn.label.clone().into_iter().collect());
            (id, true)
        }
    };
    staged.keyed.insert(key, id);
    if let Some(v) = &cn.var {
        vars.insert(v.as_str(), id);
    }
    Ok((id, created))
}

/// A statement in flight: what it has written so far, and the params it
/// resolves values from.
///
/// One transaction spans every op, and the committed store shows none of its
/// work until commit — so each op reads here what the ones before it made.
struct Staged<'a> {
    /// `$name` values, for every op that resolves one.
    params: &'a crate::Params,
    /// Keyed nodes this statement upserted or created, so one key names one
    /// node however many rows or clauses reach it.
    keyed: AHashMap<&'a str, NodeId>,
    /// The `(src, dst, type)` triples created, which `ensure_edge` must check
    /// on top of the committed store.
    edges: AHashSet<(NodeId, NodeId, String)>,
    /// Labels per node id: a label SET/REMOVE is read-modify-write, since
    /// core's `set_labels` replaces the whole set.
    labels: AHashMap<u64, Vec<String>>,
    /// The counts returned to the caller.
    summary: WriteSummary,
}

impl<'a> Staged<'a> {
    /// An empty statement, resolving values from `params`.
    fn new(params: &'a crate::Params) -> Self {
        Self {
            params,
            keyed: AHashMap::new(),
            edges: AHashSet::new(),
            labels: AHashMap::new(),
            summary: WriteSummary::default(),
        }
    }
}

/// The nodes a `MATCH` bound, and the statement that named them.
///
/// The two travel together: `ids` is empty exactly when the statement has no
/// binding, and every op that runs per row needs both — the ids to anchor on
/// and the binding to say which variable they are.
#[derive(Clone, Copy)]
struct Rows<'a> {
    stmt: &'a WriteStatement,
    ids: &'a [NodeId],
}

/// Ensure a directed edge of `rel.ty` exists between the `(prev, cur)` pair —
/// MERGE is idempotent, so a matching edge is not duplicated, whether it was
/// committed earlier or created a clause ago in this same statement.
fn ensure_edge(
    plane: &PlaneHandle<'_>,
    txn: &mut WriteTxn<'_>,
    (prev, cur): (NodeId, NodeId),
    rel: &CreateRel,
    staged: &mut Staged<'_>,
) -> Result<(), String> {
    // `->` is prev→cur; `<-` is cur→prev.
    let (src, dst) = match rel.dir {
        Dir::In => (cur, prev),
        _ => (prev, cur),
    };
    if staged.edges.contains(&(src, dst, rel.ty.clone())) {
        return Ok(());
    }
    let existing = plane
        .neighbors(src, Dir::Out, Some(&rel.ty))
        .map_err(|e| e.to_string())?;
    if existing.iter().any(|n| n.node == dst) {
        return Ok(());
    }
    txn.create_edge(src, dst, &rel.ty, props_of(&rel.props, staged.params)?)
        .map_err(|e| e.to_string())?;
    staged.edges.insert((src, dst, rel.ty.clone()));
    staged.summary.edges_created += 1;
    Ok(())
}

// ---- CREATE ---------------------------------------------------------------

fn get_or_create<'a>(
    txn: &mut WriteTxn<'_>,
    cn: &'a CreateNode,
    vars: &mut AHashMap<&'a str, NodeId>,
    staged: &mut Staged<'a>,
) -> Result<NodeId, String> {
    if let Some(v) = &cn.var
        && let Some(&id) = vars.get(v.as_str())
    {
        return Ok(id); // same variable → the same node
    }
    let labels: Vec<&str> = cn.label.as_deref().into_iter().collect();
    let props = props_of(&cn.props, staged.params)?;
    let id = match literal_key(cn) {
        Some(k) => {
            let id = txn
                .create_node_with_key(k, &labels, props)
                .map_err(|e| e.to_string())?;
            // A MERGE later in this statement upserts on the same key; the
            // store won't show it the node until commit, so tell it here.
            staged.keyed.insert(k, id);
            id
        }
        None => txn.create_node(&labels, props).map_err(|e| e.to_string())?,
    };
    staged.summary.nodes_created += 1;
    if let Some(v) = &cn.var {
        vars.insert(v.as_str(), id);
    }
    Ok(id)
}

fn create_path<'a>(
    txn: &mut WriteTxn<'_>,
    path: &'a CreatePath,
    vars: &mut AHashMap<&'a str, NodeId>,
    staged: &mut Staged<'a>,
) -> Result<(), String> {
    let mut prev = get_or_create(txn, &path.first, vars, staged)?;
    for (rel, node) in &path.rest {
        let cur = get_or_create(txn, node, vars, staged)?;
        // `->` is prev→cur; `<-` is cur→prev (the parser rejects undirected).
        let (src, dst) = match rel.dir {
            Dir::In => (cur, prev),
            _ => (prev, cur),
        };
        txn.create_edge(src, dst, &rel.ty, props_of(&rel.props, staged.params)?)
            .map_err(|e| e.to_string())?;
        // CREATE always adds; it records the edge so a MERGE later in the
        // same statement sees it.
        staged.edges.insert((src, dst, rel.ty.clone()));
        staged.summary.edges_created += 1;
        prev = cur;
    }
    Ok(())
}
