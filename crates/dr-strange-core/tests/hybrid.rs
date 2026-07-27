//! M3 hybrid graph+vector search through the public builder API (arch/03
//! §4), on both backends. These exercise the exact (record-backed) path —
//! the declared HNSW index is a later acceleration, but results must match
//! this exact baseline.

use dr_strange_core::{
    Database, Dir, Metric, NodeId, NodeRecord, PropDesc, PropValue, Properties, distance, hops, p,
    score,
};

fn emb(v: Vec<f32>) -> Properties {
    [("emb".to_string(), PropDesc::new(PropValue::Vector(v)))]
        .into_iter()
        .collect()
}

/// Four "documents" on a line in 2-D embedding space plus a citation chain,
/// so nearest-neighbour order is obvious by construction.
///   d0=[0,0]  d1=[1,0]  d2=[2,0]  d3=[3,0]
///   d0 -CITES-> d1 -CITES-> d2 -CITES-> d3
fn build(db: &Database) -> Vec<NodeId> {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let mut ids = Vec::new();
    for (i, x) in [0.0f32, 1.0, 2.0, 3.0].into_iter().enumerate() {
        ids.push(
            txn.create_node_with_key(&format!("d{i}"), &["Doc"], emb(vec![x, 0.0]))
                .unwrap(),
        );
    }
    for w in ids.windows(2) {
        txn.create_edge(w[0], w[1], "CITES", Properties::new())
            .unwrap();
    }
    txn.commit().unwrap();
    ids
}

fn ids_of(nodes: &[NodeRecord]) -> Vec<NodeId> {
    nodes.iter().map(|n| n.id).collect()
}

fn run_hybrid_suite(db: &Database) {
    let ids = build(db);
    let (d0, d1, d2, d3) = (ids[0], ids[1], ids[2], ids[3]);
    let plane = db.plane("startup").unwrap();

    // --- VectorTopK: nearest to [0,0] is d0, then d1 ---
    let near = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 2)
        .nodes()
        .unwrap();
    assert_eq!(ids_of(&near), vec![d0, d1]);

    // scores are present and descending (closer = higher similarity)
    let scored = plane
        .query()
        .vector_top_k(None, "emb", vec![0.0, 0.0], Metric::L2, 3)
        .scored_nodes()
        .unwrap();
    assert_eq!(scored.len(), 3);
    assert!(scored[0].1.unwrap() >= scored[1].1.unwrap());
    assert!(scored[1].1.unwrap() >= scored[2].1.unwrap());

    // --- seed-then-expand: nearest doc to [3,0] is d3; expand back over CITES
    //     (in-direction) to its citer d2. One plan, score survives expansion.
    let expanded = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![3.0, 0.0], Metric::L2, 1) // -> d3
        .expand_in("CITES") // who cites d3? d2
        .ids()
        .unwrap();
    assert_eq!(expanded, vec![d2]);

    // --- FrontierTopK: graph-constrained vector search. Frontier = things
    //     d0 reaches in 1..=3 CITES hops = {d1,d2,d3}; rank by closeness to
    //     [2,0] → d2 first. No client-side join.
    let frontier = plane
        .query()
        .seek_ids([d0])
        .expand_var(Dir::Out, Some("CITES"), 1, 3)
        .distinct()
        .frontier_top_k("emb", vec![2.0, 0.0], Metric::L2, 2)
        .ids()
        .unwrap();
    assert_eq!(frontier[0], d2); // closest to [2,0]
    assert_eq!(frontier.len(), 2);

    // --- ExpandBeam: from d0, walk toward [3,0]; width 1 greedily follows the
    //     chain d1 -> d2 -> d3.
    let beam = plane
        .query()
        .seek_ids([d0])
        .expand_beam(
            Dir::Out,
            Some("CITES"),
            "emb",
            vec![3.0, 0.0],
            Metric::L2,
            1,
            3,
        )
        .ids()
        .unwrap();
    assert_eq!(beam, vec![d1, d2, d3]);

    // --- traverse-then-rerank with fusion: rank the citation chain from d0
    //     by 0.6*similarity(to [3,0]) + 0.4*(-hops), i.e. prefer close AND
    //     shallow. Just assert it runs and ranks d3 or d2 near the top.
    let fused = plane
        .query()
        .seek_ids([d0])
        .expand_var(Dir::Out, Some("CITES"), 1, 3)
        .distinct()
        .sort_desc(
            similarity_expr()
                .mul(dr_strange_core::lit(0.6))
                .add(hops().mul(dr_strange_core::lit(-0.4))),
        )
        .ids()
        .unwrap();
    assert_eq!(fused.len(), 3);
    assert!(fused.contains(&d1) && fused.contains(&d2) && fused.contains(&d3));

    // sort_by_score after a vector search = most similar first
    let by_score = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 4)
        .sort_by_score()
        .ids()
        .unwrap();
    assert_eq!(by_score, vec![d0, d1, d2, d3]);
}

fn similarity_expr() -> dr_strange_core::Expr {
    // similarity(emb, [3,0], L2)
    dr_strange_core::similarity("emb", vec![3.0, 0.0], Metric::L2)
}

#[test]
fn hybrid_suite_memory() {
    run_hybrid_suite(&Database::in_memory().unwrap());
}

#[test]
fn hybrid_suite_redb() {
    let dir = tempfile::tempdir().unwrap();
    run_hybrid_suite(&Database::open(dir.path().join("hybrid.drsg")).unwrap());
}

#[test]
fn vector_search_ignores_nodes_without_the_property() {
    // A node lacking `emb`, or with a wrong-dimension vector, is simply not a
    // candidate — never an error (soft-schema total semantics).
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let good = txn.create_node(&["Doc"], emb(vec![0.0, 0.0])).unwrap();
    txn.create_node(&["Doc"], Properties::new()).unwrap(); // no emb
    txn.create_node(&["Doc"], emb(vec![1.0])).unwrap(); // wrong dim
    txn.commit().unwrap();

    let hits = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 10)
        .ids()
        .unwrap();
    assert_eq!(hits, vec![good]);
}

#[test]
fn empty_vector_search_is_empty_not_error() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    assert!(
        plane
            .query()
            .vector_top_k(Some("Doc"), "emb", vec![0.0], Metric::Cosine, 5)
            .nodes()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fusion_expr_reads_score_channel() {
    let db = Database::in_memory().unwrap();
    let ids = build(&db);
    let plane = db.plane("startup").unwrap();
    // project the raw score channel: VectorTopK seeds it, so select(score())
    // returns non-null floats.
    let rows = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 2)
        .select(&[
            score(),
            p("emb").is_null(),
            distance("emb", vec![0.0, 0.0], Metric::L2),
        ])
        .unwrap();
    assert_eq!(rows.len(), 2);
    // first row is d0 (exact match): score high, distance ~0
    assert!(matches!(rows[0][0], PropValue::Float(_)));
    assert_eq!(rows[0][1], PropValue::Bool(false));
    assert!(matches!(rows[0][2], PropValue::Float(x) if x.abs() < 1e-6));
    let _ = ids;
}

// ---- declared HNSW index (registry) --------------------------------------

/// A declared index must return the same top-k as the exact path, and must
/// stay coherent as nodes are created, updated, and deleted afterward.
fn run_index_suite(db: &Database) {
    let ids = build(db);
    let (d0, d1) = (ids[0], ids[1]);
    let plane = db.plane("startup").unwrap();

    // Declare an index over the existing docs, then query it.
    plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    let via_index = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 2)
        .ids()
        .unwrap();
    assert_eq!(via_index, vec![d0, d1]); // same as exact

    // idempotent re-declare; conflicting metric errors
    plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    assert!(
        plane
            .ensure_vector_index("Doc", "emb", Metric::Cosine)
            .is_err()
    );

    // Coherence — insert: a new doc near [10,0] becomes the nearest to [9,0].
    let mut txn = plane.write().unwrap();
    let d_new = txn.create_node(&["Doc"], emb(vec![10.0, 0.0])).unwrap();
    txn.commit().unwrap();
    let near_10 = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![9.0, 0.0], Metric::L2, 1)
        .ids()
        .unwrap();
    assert_eq!(near_10, vec![d_new]);

    // Coherence — update: move d0 far away; it's no longer nearest to [0,0].
    let mut txn = plane.write().unwrap();
    txn.set_prop(
        d0,
        "emb",
        PropDesc::new(PropValue::Vector(vec![100.0, 0.0])),
    )
    .unwrap();
    txn.commit().unwrap();
    let near_origin = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 1)
        .ids()
        .unwrap();
    assert_eq!(near_origin, vec![d1]); // d1=[1,0] now closest

    // Coherence — delete: remove d1; nearest to [0,0] is now d2=[2,0].
    let mut txn = plane.write().unwrap();
    txn.delete_node(d1).unwrap();
    txn.commit().unwrap();
    let after_delete = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 1)
        .ids()
        .unwrap();
    assert_eq!(after_delete, vec![ids[2]]);
}

#[test]
fn index_suite_memory() {
    run_index_suite(&Database::in_memory().unwrap());
}

#[test]
fn index_suite_redb() {
    let dir = tempfile::tempdir().unwrap();
    run_index_suite(&Database::open(dir.path().join("index.drsg")).unwrap());
}

#[test]
fn index_rebuilds_from_kv_on_reopen() {
    // The declaration is durable; the index is reconstructed from the KV on
    // open, so a query works immediately after reopening (redb only).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen-index.drsg");
    let (d0, d1);
    {
        let db = Database::open(&path).unwrap();
        let ids = build(&db);
        (d0, d1) = (ids[0], ids[1]);
        db.plane("startup")
            .unwrap()
            .ensure_vector_index("Doc", "emb", Metric::L2)
            .unwrap();
    }
    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    let via_rebuilt = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 2)
        .ids()
        .unwrap();
    assert_eq!(via_rebuilt, vec![d0, d1]);
}

#[test]
fn aborted_write_does_not_touch_the_index() {
    let db = Database::in_memory().unwrap();
    build(&db);
    let plane = db.plane("startup").unwrap();
    plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();

    // Insert a would-be-nearest doc but drop the txn without committing.
    {
        let mut txn = plane.write().unwrap();
        txn.create_node(&["Doc"], emb(vec![9.0, 0.0])).unwrap();
        // dropped — not committed
    }
    // The index must not have learned about it.
    let near_9 = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![9.0, 0.0], Metric::L2, 1)
        .nodes()
        .unwrap();
    // nearest committed doc to [9,0] is d3=[3,0], not the aborted [9,0] one
    assert_eq!(
        near_9[0].properties["emb"].value,
        PropValue::Vector(vec![3.0, 0.0])
    );
}
