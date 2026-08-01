//! Hybrid retrieval fusion (ROADMAP §2) end-to-end through `plane.hybrid()`:
//! vector + BM25 keyword + graph-proximity channels fused into one ranking.

use dr_strange_core::{Database, Language, Metric, NodeId, PropDesc, PropValue, Properties};

/// A Doc with both an `emb` vector and a `body` text property.
fn doc(body: &str, emb: Vec<f32>) -> Properties {
    [
        (
            "body".to_string(),
            PropDesc::new(PropValue::Str(body.into())),
        ),
        ("emb".to_string(), PropDesc::new(PropValue::Vector(emb))),
    ]
    .into_iter()
    .collect()
}

/// Four docs on a line in 2-D embedding space; d0 links to d3. d0 is strong on
/// both text ("graph") and vector (query [0,0]); d3 matches neither but is d0's
/// neighbour, so only the graph channel can surface it.
fn setup() -> (Database, [NodeId; 4]) {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let ids = {
        let mut txn = plane.write().unwrap();
        let d0 = txn
            .create_node(
                &["Doc"],
                doc("graph databases store connected data", vec![0.0, 0.0]),
            )
            .unwrap();
        let d1 = txn
            .create_node(
                &["Doc"],
                doc("vector search finds similar items", vec![1.0, 0.0]),
            )
            .unwrap();
        let d2 = txn
            .create_node(
                &["Doc"],
                doc("graph graph structure and graph queries", vec![6.0, 0.0]),
            )
            .unwrap();
        let d3 = txn
            .create_node(
                &["Doc"],
                doc("entirely unrelated subject matter", vec![0.0, 4.0]),
            )
            .unwrap();
        txn.create_edge(d0, d3, "LINKS", Properties::new()).unwrap();
        txn.commit().unwrap();
        [d0, d1, d2, d3]
    };
    plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    plane
        .ensure_keyword_index("Doc", "body", Language::English)
        .unwrap();
    (db, ids)
}

#[test]
fn fuses_three_channels_and_reports_breakdown() {
    let (db, [d0, _d1, _d2, d3]) = setup();
    let plane = db.plane("startup").unwrap();

    let hits = plane
        .hybrid()
        .label("Doc")
        .vector("emb", vec![0.0, 0.0], Metric::L2)
        .keyword("body", "graph")
        .graph(1, 0.5)
        .k(10)
        .run()
        .unwrap();

    // d0 is best on vector AND keyword (and is a seed) → ranks first.
    assert_eq!(hits[0].node, d0);
    let top = &hits[0];
    assert!(top.vector.is_some() && top.keyword.is_some() && top.graph.is_some());

    // d3 has no "graph" text; it surfaces ONLY because it neighbours d0 — so it
    // carries a graph contribution but no keyword one.
    let d3hit = hits.iter().find(|h| h.node == d3).expect("d3 surfaced");
    assert!(d3hit.graph.is_some(), "graph proximity reached d3");
    assert!(d3hit.keyword.is_none(), "d3 has no keyword match");
}

#[test]
fn keyword_channel_requires_a_label() {
    let (db, _) = setup();
    let plane = db.plane("startup").unwrap();
    // No `.label(..)` → the keyword channel can't resolve its index.
    let err = plane.hybrid().keyword("body", "graph").run();
    assert!(err.is_err());
}

#[test]
fn vector_only_matches_plain_vector_ranking() {
    let (db, [d0, _d1, _d2, _d3]) = setup();
    let plane = db.plane("startup").unwrap();
    let hits = plane
        .hybrid()
        .label("Doc")
        .vector("emb", vec![0.0, 0.0], Metric::L2)
        .k(2)
        .run()
        .unwrap();
    assert_eq!(hits[0].node, d0, "closest embedding leads");
    assert!(
        hits.iter()
            .all(|h| h.keyword.is_none() && h.graph.is_none())
    );
}

#[test]
fn weights_shift_the_ranking() {
    let (db, [d0, _d1, d2, _d3]) = setup();
    let plane = db.plane("startup").unwrap();
    // Keyword-only weighting: the graph-dense text (d2, "graph" ×3) should beat
    // d0 on pure BM25 even though d0 is the vector winner.
    let hits = plane
        .hybrid()
        .label("Doc")
        .vector("emb", vec![0.0, 0.0], Metric::L2)
        .keyword("body", "graph")
        .weights(dr_strange_core::HybridWeights {
            vector: 0.0,
            keyword: 1.0,
            graph: 0.0,
        })
        .k(10)
        .run()
        .unwrap();
    let rank = |id: NodeId| hits.iter().position(|h| h.node == id).unwrap();
    assert!(
        rank(d2) < rank(d0),
        "BM25-only ranks the graph-dense doc first"
    );
}
