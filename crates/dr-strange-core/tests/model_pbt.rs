//! Property-based model test (arch/01 §7), using `proptest`.
//!
//! `proptest` generates a `Vec<Op>` and, crucially for a stateful test this
//! long, **shrinks** a failing run to a minimal operation sequence and
//! persists it under `proptest-regressions/` — so a divergence at step 287
//! comes back as the two or three ops that actually cause it, not a 400-step
//! seed to bisect by hand.
//!
//! Node/edge references in a pre-generated op vector can't be real ids yet,
//! so each `Op` carries an *index* that is resolved modulo the set of
//! ever-created ids at run time (the standard trick for stateful proptest).
//! Resolving against ever-created — not just alive — ids means deletes
//! naturally re-hit already-dead ids too, exercising idempotent delete.
//!
//! One `Harness` drives N planes at once against a single model:
//! - N = 1  → the model is the oracle for the real engine.
//! - N = 2  → memory vs redb differential; every op must produce identical
//!   results (including allocated ids, which holds only because id
//!   allocation is counter-based and content-independent — arch/01 §2).

use std::collections::BTreeMap;

use dr_strange_core::{
    Database, Dir, EdgeId, Error, NodeId, PlaneHandle, PropDesc, PropValue, Properties,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const LABEL_POOL: &[&str] = &["Person", "Paper", "Org"];
const EDGE_TYPE_POOL: &[&str] = &["KNOWS", "CITES", "WORKS_AT"];
const KEY_POOL: u8 = 6; // small on purpose: forces external-key collisions

#[derive(Clone, Debug)]
enum Op {
    CreateNode { labels: Vec<u8> },
    CreateNodeWithKey { key: u8, labels: Vec<u8> },
    CreateEdge { src: usize, dst: usize, ty: u8 },
    DeleteNode { idx: usize },
    DeleteEdge { idx: usize },
    SetProp { idx: usize, val: i64 },
    RemoveProp { idx: usize },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let labels = prop::collection::vec(0u8..LABEL_POOL.len() as u8, 0..3);
    prop_oneof![
        // weights ≈ the hand-rolled distribution: creation-heavy, so the
        // graph actually grows enough for deletes/mutations to bite.
        5 => labels.clone().prop_map(|labels| Op::CreateNode { labels }),
        2 => (0u8..KEY_POOL, labels.clone())
            .prop_map(|(key, labels)| Op::CreateNodeWithKey { key, labels }),
        4 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), 0u8..EDGE_TYPE_POOL.len() as u8)
            .prop_map(|(s, d, ty)| Op::CreateEdge { src: s.index(usize::MAX), dst: d.index(usize::MAX), ty }),
        3 => any::<prop::sample::Index>().prop_map(|i| Op::DeleteNode { idx: i.index(usize::MAX) }),
        2 => any::<prop::sample::Index>().prop_map(|i| Op::DeleteEdge { idx: i.index(usize::MAX) }),
        2 => (any::<prop::sample::Index>(), any::<i64>())
            .prop_map(|(i, val)| Op::SetProp { idx: i.index(usize::MAX), val }),
        1 => any::<prop::sample::Index>().prop_map(|i| Op::RemoveProp { idx: i.index(usize::MAX) }),
    ]
}

// -------------------------------------------------------------- model -----

#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeModel {
    labels: Vec<String>,
    external_key: Option<String>,
    tag: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeModel {
    src: NodeId,
    dst: NodeId,
    ty: String,
    tag: Option<i64>,
}

#[derive(Default)]
struct Model {
    nodes: BTreeMap<NodeId, NodeModel>,
    edges: BTreeMap<EdgeId, EdgeModel>,
    ext_keys: BTreeMap<String, NodeId>,
    all_nodes: Vec<NodeId>,
    all_edges: Vec<EdgeId>,
}

fn labels_of(indices: &[u8]) -> Vec<String> {
    indices
        .iter()
        .map(|&i| LABEL_POOL[i as usize].to_string())
        .collect()
}

fn tag_of(props: &Properties) -> Result<Option<i64>, TestCaseError> {
    match props.get("tag").map(|p| &p.value) {
        None => Ok(None),
        Some(PropValue::Int(v)) => Ok(Some(*v)),
        Some(other) => Err(TestCaseError::fail(format!(
            "unexpected tag value: {other:?}"
        ))),
    }
}

/// Drives one or more planes (all on the same op stream) against `model`.
struct Harness<'a> {
    planes: Vec<PlaneHandle<'a>>,
    model: Model,
}

impl<'a> Harness<'a> {
    fn new(planes: Vec<PlaneHandle<'a>>) -> Self {
        Self {
            planes,
            model: Model::default(),
        }
    }

    /// Runs `f(plane)` on every plane and asserts they all return the same
    /// `Ok` value (or all the same error kind); returns the canonical result.
    fn on_all<T, F>(&self, mut f: F) -> Result<Result<T, Error>, TestCaseError>
    where
        T: PartialEq + std::fmt::Debug,
        F: FnMut(&PlaneHandle<'a>) -> Result<T, Error>,
    {
        let mut results = self.planes.iter().map(&mut f);
        let first = results.next().expect("at least one plane");
        for other in results {
            match (&first, &other) {
                (Ok(a), Ok(b)) => {
                    prop_assert!(a == b, "backends diverged: {a:?} vs {b:?}");
                }
                (Err(a), Err(b)) => {
                    prop_assert!(
                        std::mem::discriminant(a) == std::mem::discriminant(b),
                        "backends diverged on error kind: {a:?} vs {b:?}"
                    );
                }
                (a, b) => {
                    return Err(TestCaseError::fail(format!(
                        "backends diverged (ok/err): {a:?} vs {b:?}"
                    )));
                }
            }
        }
        Ok(first)
    }

    fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::CreateNode { labels } => {
                let labels = labels_of(labels);
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let id = self
                    .on_all(|p| {
                        let mut t = p.write()?;
                        let id = t.create_node(&refs, Properties::new())?;
                        t.commit()?;
                        Ok(id)
                    })?
                    .expect("create_node is infallible here");
                self.model.nodes.insert(
                    id,
                    NodeModel {
                        labels,
                        external_key: None,
                        tag: None,
                    },
                );
                self.model.all_nodes.push(id);
            }
            Op::CreateNodeWithKey { key, labels } => {
                let key = format!("k{key}");
                let labels = labels_of(labels);
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let result = self.on_all(|p| {
                    let mut t = p.write()?;
                    match t.create_node_with_key(&key, &refs, Properties::new()) {
                        Ok(id) => {
                            t.commit()?;
                            Ok(id)
                        }
                        Err(e) => Err(e),
                    }
                })?;
                match result {
                    Ok(id) => {
                        prop_assert!(
                            !self.model.ext_keys.contains_key(&key),
                            "DB accepted key '{key}' the model already has bound"
                        );
                        self.model.nodes.insert(
                            id,
                            NodeModel {
                                labels,
                                external_key: Some(key.clone()),
                                tag: None,
                            },
                        );
                        self.model.ext_keys.insert(key, id);
                        self.model.all_nodes.push(id);
                    }
                    Err(Error::Conflict(_)) => {
                        prop_assert!(
                            self.model.ext_keys.contains_key(&key),
                            "DB rejected key '{key}' the model thinks is free"
                        );
                    }
                    Err(e) => return Err(TestCaseError::fail(format!("unexpected: {e}"))),
                }
            }
            Op::CreateEdge { src, dst, ty } => {
                if self.model.all_nodes.is_empty() {
                    return Ok(());
                }
                let src = self.model.all_nodes[src % self.model.all_nodes.len()];
                let dst = self.model.all_nodes[dst % self.model.all_nodes.len()];
                let ty_name = EDGE_TYPE_POOL[*ty as usize].to_string();
                // src/dst may be dead (deleted); the model predicts the outcome.
                let both_alive =
                    self.model.nodes.contains_key(&src) && self.model.nodes.contains_key(&dst);
                let result = self.on_all(|p| {
                    let mut t = p.write()?;
                    match t.create_edge(src, dst, &ty_name, Properties::new()) {
                        Ok(id) => {
                            t.commit()?;
                            Ok(id)
                        }
                        Err(e) => Err(e),
                    }
                })?;
                match result {
                    Ok(id) => {
                        prop_assert!(both_alive, "DB created an edge on a deleted endpoint");
                        self.model.edges.insert(
                            id,
                            EdgeModel {
                                src,
                                dst,
                                ty: ty_name,
                                tag: None,
                            },
                        );
                        self.model.all_edges.push(id);
                    }
                    Err(Error::PlaneMismatch(_)) => {
                        prop_assert!(!both_alive, "DB rejected an edge between two live nodes");
                    }
                    Err(e) => return Err(TestCaseError::fail(format!("unexpected: {e}"))),
                }
            }
            Op::DeleteNode { idx } => {
                if self.model.all_nodes.is_empty() {
                    return Ok(());
                }
                let id = self.model.all_nodes[idx % self.model.all_nodes.len()];
                self.on_all(|p| {
                    let mut t = p.write()?;
                    t.delete_node(id)?;
                    t.commit()?;
                    Ok(())
                })?
                .expect("delete_node is infallible");
                if let Some(nm) = self.model.nodes.remove(&id) {
                    if let Some(k) = nm.external_key {
                        self.model.ext_keys.remove(&k);
                    }
                    self.model.edges.retain(|_, e| e.src != id && e.dst != id);
                }
            }
            Op::DeleteEdge { idx } => {
                if self.model.all_edges.is_empty() {
                    return Ok(());
                }
                let id = self.model.all_edges[idx % self.model.all_edges.len()];
                self.on_all(|p| {
                    let mut t = p.write()?;
                    t.delete_edge(id)?;
                    t.commit()?;
                    Ok(())
                })?
                .expect("delete_edge is infallible");
                self.model.edges.remove(&id);
            }
            Op::SetProp { idx, val } => {
                let Some(id) = self.live_node(*idx) else {
                    return Ok(());
                };
                self.on_all(|p| {
                    let mut t = p.write()?;
                    t.set_prop(id, "tag", PropDesc::new(PropValue::Int(*val)))?;
                    t.commit()?;
                    Ok(())
                })?
                .expect("set_prop on a live node is infallible");
                self.model.nodes.get_mut(&id).unwrap().tag = Some(*val);
            }
            Op::RemoveProp { idx } => {
                let Some(id) = self.live_node(*idx) else {
                    return Ok(());
                };
                self.on_all(|p| {
                    let mut t = p.write()?;
                    t.remove_prop(id, "tag")?;
                    t.commit()?;
                    Ok(())
                })?
                .expect("remove_prop on a live node is infallible");
                self.model.nodes.get_mut(&id).unwrap().tag = None;
            }
        }
        self.verify()
    }

    /// Resolves an index to a *currently alive* node id, or `None` if there
    /// are none (used by prop mutations, which need a real target).
    fn live_node(&self, idx: usize) -> Option<NodeId> {
        if self.model.nodes.is_empty() {
            return None;
        }
        let live: Vec<NodeId> = self.model.nodes.keys().copied().collect();
        Some(live[idx % live.len()])
    }

    fn verify(&self) -> Result<(), TestCaseError> {
        for p in &self.planes {
            self.verify_plane(p)?;
        }
        Ok(())
    }

    fn verify_plane(&self, plane: &PlaneHandle) -> Result<(), TestCaseError> {
        for &id in &self.model.all_nodes {
            let real = plane.node(id).map_err(fail)?;
            match self.model.nodes.get(&id) {
                Some(nm) => {
                    let real =
                        real.ok_or_else(|| TestCaseError::fail(format!("missing node {id:?}")))?;
                    prop_assert_eq!(&real.labels, &nm.labels);
                    prop_assert_eq!(&real.external_key, &nm.external_key);
                    prop_assert_eq!(tag_of(&real.properties)?, nm.tag);
                }
                None => prop_assert!(real.is_none(), "node {id:?} should be deleted", id = id),
            }
        }
        for &id in &self.model.all_edges {
            let real = plane.edge(id).map_err(fail)?;
            match self.model.edges.get(&id) {
                Some(em) => {
                    let real =
                        real.ok_or_else(|| TestCaseError::fail(format!("missing edge {id:?}")))?;
                    prop_assert_eq!(real.src, em.src);
                    prop_assert_eq!(real.dst, em.dst);
                    prop_assert_eq!(&real.ty, &em.ty);
                    prop_assert_eq!(tag_of(&real.properties)?, em.tag);
                }
                None => prop_assert!(real.is_none(), "edge {id:?} should be deleted", id = id),
            }
        }
        // Adjacency as a multiset (order is not contractual).
        for &id in self.model.nodes.keys() {
            prop_assert_eq!(
                sorted_adj(plane, id, Dir::Out)?,
                self.model_adj(id, true),
                "out-neighbors mismatch for {:?}",
                id
            );
            prop_assert_eq!(
                sorted_adj(plane, id, Dir::In)?,
                self.model_adj(id, false),
                "in-neighbors mismatch for {:?}",
                id
            );
        }
        Ok(())
    }

    fn model_adj(&self, id: NodeId, outgoing: bool) -> Vec<(NodeId, EdgeId)> {
        let mut v: Vec<(NodeId, EdgeId)> = self
            .model
            .edges
            .iter()
            .filter(|(_, e)| if outgoing { e.src == id } else { e.dst == id })
            .map(|(&eid, e)| (if outgoing { e.dst } else { e.src }, eid))
            .collect();
        v.sort();
        v
    }
}

fn fail(e: Error) -> TestCaseError {
    TestCaseError::fail(format!("db error: {e}"))
}

fn sorted_adj(
    plane: &PlaneHandle,
    id: NodeId,
    dir: Dir,
) -> Result<Vec<(NodeId, EdgeId)>, TestCaseError> {
    let mut v: Vec<(NodeId, EdgeId)> = plane
        .neighbors(id, dir, None)
        .map_err(fail)?
        .into_iter()
        .map(|n| (n.node, n.edge))
        .collect();
    v.sort();
    Ok(v)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// The model is the oracle for the real engine (in-memory backend).
    #[test]
    fn model_matches_engine(ops in prop::collection::vec(op_strategy(), 0..160)) {
        let db = Database::in_memory().unwrap();
        let mut h = Harness::new(vec![db.plane("startup").unwrap()]);
        for op in &ops {
            h.apply(op)?;
        }
    }
}

proptest! {
    // redb spins a fresh file per case, so keep this smaller than the pure
    // in-memory run above.
    #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

    /// Memory and redb backends must agree bit-for-bit on the same script.
    #[test]
    fn memory_and_redb_agree(ops in prop::collection::vec(op_strategy(), 0..90)) {
        let mem = Database::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = Database::open(dir.path().join("pbt.drsg")).unwrap();
        let mut h = Harness::new(vec![
            mem.plane("startup").unwrap(),
            file.plane("startup").unwrap(),
        ]);
        for op in &ops {
            h.apply(op)?;
        }
    }
}
