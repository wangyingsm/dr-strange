//! Property-based tests for the ordering algorithms — `toposort` (ROADMAP §1,
//! issue #98) and `cycles` (issue #99) — using `proptest`.
//!
//! Both verbs answer about the same structure from opposite sides, so the
//! temptation is to check them against each other. That is worth doing, but it
//! cannot be the whole test: one bug in the shared `Frame` would satisfy it
//! twice over. So the oracle here is a naive model written in this file —
//! plain adjacency lists and a recursive DFS over the generated edges, sharing
//! no code with the engine — and the agreement between the two verbs is one
//! property among several rather than the basis of all of them.
//!
//! Generated graphs are small (six nodes) and dense enough that a cycle is
//! common rather than rare, and duplicate edges and self-loops are generated
//! on purpose: parallel edges are what in-degree counting gets wrong, and a
//! self-loop is the shortest cycle there is.

use std::collections::{HashMap, HashSet};

use dr_strange_core::{
    CyclesOptions, Database, Dir, NodeId, Properties, TopoOrder, TopoSortOptions,
};
use proptest::prelude::*;

/// Nodes per generated graph. Small, so a case shrinks to something readable.
const NODES: usize = 6;
/// Two edge types, so the `edge_type` scope has something to exclude.
const TYPES: [&str; 2] = ["A", "B"];
/// Cap handed to `cycles`, low enough that the limit itself gets exercised.
const LIMIT: usize = 4;

/// A generated graph as `(source, target, edge type)` triples.
type Edges = Vec<(usize, usize, usize)>;

fn graph() -> impl Strategy<Value = Edges> {
    prop::collection::vec((0..NODES, 0..NODES, 0..TYPES.len()), 0..14)
}

/// Create the plane and return the node ids, indexed as the model indexes them.
fn build(db: &Database, edges: &Edges) -> Vec<NodeId> {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let ids: Vec<NodeId> = (0..NODES)
        .map(|_| txn.create_node(&["N"], Properties::new()).unwrap())
        .collect();
    for &(u, v, t) in edges {
        txn.create_edge(ids[u], ids[v], TYPES[t], Properties::new())
            .unwrap();
    }
    txn.commit().unwrap();
    ids
}

/// The model: out-adjacency over the generated edges, restricted to one edge
/// type when `ty` names one. Parallel edges collapse — nothing below counts
/// multiplicity.
fn adjacency(edges: &Edges, ty: Option<usize>) -> Vec<HashSet<usize>> {
    let mut adj = vec![HashSet::new(); NODES];
    for &(u, v, t) in edges {
        if ty.is_none_or(|want| want == t) {
            adj[u].insert(v);
        }
    }
    adj
}

/// Every node that has an edge into `v`.
fn predecessors(adj: &[HashSet<usize>], v: usize) -> Vec<usize> {
    (0..NODES).filter(|&u| adj[u].contains(&v)).collect()
}

/// Does a walk leave `from` and come back to it? The oracle for "is this node
/// on a cycle", and with `from` ranging over every node, for "is this graph
/// cyclic" — deliberately a different algorithm from the engine's.
fn returns_to(adj: &[HashSet<usize>], from: usize) -> bool {
    let mut seen = [false; NODES];
    let mut stack: Vec<usize> = adj[from].iter().copied().collect();
    while let Some(node) = stack.pop() {
        if node == from {
            return true;
        }
        if seen[node] {
            continue;
        }
        seen[node] = true;
        stack.extend(adj[node].iter().copied());
    }
    false
}

fn is_cyclic(adj: &[HashSet<usize>]) -> bool {
    (0..NODES).any(|v| returns_to(adj, v))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// Over a random graph, whole-plane and scoped to one edge type: an order
    /// exists exactly when the model says the scope is acyclic, it respects
    /// every edge, every generation is a longest-path depth, and each reported
    /// cycle is a real closed walk of distinct nodes.
    #[test]
    fn toposort_and_cycles_hold_against_a_naive_model(edges in graph()) {
        let db = Database::in_memory().unwrap();
        let ids = build(&db, &edges);
        let index: HashMap<NodeId, usize> =
            ids.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        let plane = db.plane("startup").unwrap();

        for scope in [None, Some(0)] {
            let adj = adjacency(&edges, scope);
            let cyclic = is_cyclic(&adj);
            let edge_type = scope.map(|t| TYPES[t].to_string());

            let order = plane.algo().toposort(&TopoSortOptions {
                edge_type: edge_type.clone(),
                ..Default::default()
            }).unwrap();
            match order {
                TopoOrder::Sorted(order) => {
                    prop_assert!(!cyclic, "ordered a cyclic scope {scope:?}: {edges:?}");
                    // Every node once: an order is a permutation, not a subset.
                    prop_assert_eq!(order.len(), NODES);
                    let at: HashMap<NodeId, usize> =
                        order.iter().enumerate().map(|(i, t)| (t.node, i)).collect();
                    prop_assert_eq!(at.len(), NODES, "a node placed twice");
                    // The defining property: an edge means source before target.
                    for (u, targets) in adj.iter().enumerate() {
                        for &v in targets {
                            prop_assert!(
                                at[&ids[u]] < at[&ids[v]],
                                "{u} -> {v} but the order disagrees: {edges:?}"
                            );
                        }
                    }
                    // What parallel scheduling relies on: a node's generation
                    // is one past its deepest predecessor, 0 with none.
                    let depth: HashMap<NodeId, usize> =
                        order.iter().map(|t| (t.node, t.generation)).collect();
                    for v in 0..NODES {
                        let want = predecessors(&adj, v)
                            .iter()
                            .map(|&u| depth[&ids[u]] + 1)
                            .max()
                            .unwrap_or(0);
                        prop_assert_eq!(
                            depth[&ids[v]], want,
                            "generation of {} is not its longest-path depth: {:?}", v, edges
                        );
                    }
                }
                TopoOrder::Cyclic(node) => {
                    prop_assert!(cyclic, "refused an acyclic scope {scope:?}: {edges:?}");
                    // The promise the refusal makes: this node is *on* a
                    // cycle, not merely downstream of one.
                    prop_assert!(
                        returns_to(&adj, index[&node]),
                        "witness {:?} is not on a cycle: {edges:?}",
                        index[&node]
                    );
                }
            }

            let found = plane.algo().cycles(&CyclesOptions {
                edge_type,
                limit: LIMIT,
            }).unwrap();
            prop_assert_eq!(
                found.is_empty(), !cyclic,
                "cycles disagrees with the model on scope {:?}: {:?}", scope, edges
            );
            prop_assert!(found.len() <= LIMIT, "limit exceeded: {found:?}");
            for cycle in &found {
                prop_assert!(cycle.len() >= 2, "{cycle:?}");
                prop_assert_eq!(cycle.first(), cycle.last(), "the list must close");
                // Every step is an edge the scope actually holds.
                for pair in cycle.windows(2) {
                    let (from, to) = (index[&pair[0]], index[&pair[1]]);
                    prop_assert!(
                        adj[from].contains(&to),
                        "{from} -> {to} is not an edge of this scope: {edges:?}"
                    );
                }
                // A cycle, not a walk that wanders through one.
                let once: HashSet<&NodeId> = cycle[..cycle.len() - 1].iter().collect();
                prop_assert_eq!(
                    once.len(),
                    cycle.len() - 1,
                    "repeated node in {:?}",
                    cycle
                );
            }
        }
    }

    /// Reading every edge backwards is still an ordering question: it has an
    /// answer exactly when the forward reading does, and that answer orders
    /// the reversed edges.
    #[test]
    fn toposort_in_orders_the_reversed_graph(edges in graph()) {
        let db = Database::in_memory().unwrap();
        let ids = build(&db, &edges);
        let plane = db.plane("startup").unwrap();
        let adj = adjacency(&edges, None);

        let forward = plane.algo().toposort(&TopoSortOptions::default()).unwrap();
        let backward = plane.algo().toposort(&TopoSortOptions {
            dir: Dir::In,
            ..Default::default()
        }).unwrap();

        match (forward, backward) {
            (TopoOrder::Sorted(_), TopoOrder::Sorted(back)) => {
                let at: HashMap<NodeId, usize> =
                    back.iter().enumerate().map(|(i, t)| (t.node, i)).collect();
                for (u, targets) in adj.iter().enumerate() {
                    for &v in targets {
                        prop_assert!(
                            at[&ids[v]] < at[&ids[u]],
                            "{u} -> {v} should put {v} first read backwards: {edges:?}"
                        );
                    }
                }
            }
            (TopoOrder::Cyclic(_), TopoOrder::Cyclic(_)) => {}
            (f, b) => prop_assert!(
                false,
                "direction changed whether an order exists: {f:?} vs {b:?} for {edges:?}"
            ),
        }
    }
}
