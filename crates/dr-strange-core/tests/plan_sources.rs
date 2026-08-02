//! The retrieval and algorithm plan sources (ROADMAP §7) executed against a
//! real database: keyword, hybrid and algorithm seeds each become rows with a
//! score, and — the point of making them plan *sources* rather than separate
//! calls — the pipeline keeps composing after them (expand, filter, sort,
//! limit). The query language compiles onto exactly these.

use dr_strange_core::{
    Algo, Database, Dir, GraphChannel, HybridSpec, HybridWeights, KeywordChannel, Language, NodeId,
    NodeRef, PropDesc, PropValue, Properties, SortKey, Source, VectorChannel, has_label, score,
};

fn props(pairs: &[(&str, PropValue)]) -> Properties {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
        .collect()
}

fn text(body: &str) -> Properties {
    props(&[("body", PropValue::Str(body.into()))])
}

/// Three docs citing one paper each, plus a keyword index on `Doc.body`:
///
/// ```text
///   doc-graph  ──CITES──▶ paper-a
///   doc-vector ──CITES──▶ paper-b
///   doc-mixed  ──CITES──▶ paper-a
/// ```
fn seed(db: &Database) -> [NodeId; 5] {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let g = txn
        .create_node_with_key("doc-graph", &["Doc"], text("graph databases store edges"))
        .unwrap();
    let v = txn
        .create_node_with_key(
            "doc-vector",
            &["Doc"],
            text("vector search finds neighbours"),
        )
        .unwrap();
    let m = txn
        .create_node_with_key(
            "doc-mixed",
            &["Doc"],
            text("a graph database indexes graph structure for graph queries"),
        )
        .unwrap();
    let pa = txn
        .create_node_with_key(
            "paper-a",
            &["Paper"],
            props(&[("year", PropValue::Int(2021))]),
        )
        .unwrap();
    let pb = txn
        .create_node_with_key(
            "paper-b",
            &["Paper"],
            props(&[("year", PropValue::Int(2019))]),
        )
        .unwrap();
    txn.create_edge(g, pa, "CITES", Properties::new()).unwrap();
    txn.create_edge(v, pb, "CITES", Properties::new()).unwrap();
    txn.create_edge(m, pa, "CITES", Properties::new()).unwrap();
    txn.commit().unwrap();

    plane
        .ensure_keyword_index("Doc", "body", Language::English)
        .unwrap();
    [g, v, m, pa, pb]
}

#[test]
fn keyword_source_seeds_scored_rows() {
    let db = Database::in_memory().unwrap();
    let [_g, _v, m, _pa, _pb] = seed(&db);
    let plane = db.plane("startup").unwrap();

    let rows = plane
        .query()
        .keyword_top_k("Doc", "body", "graph database", 5)
        .scored_nodes()
        .unwrap();

    assert_eq!(rows[0].0.id, m, "the graph-heavy doc ranks first");
    assert!(
        rows.iter().all(|(_, s)| s.unwrap() > 0.0),
        "every row carries its BM25 relevance in the score channel"
    );
}

#[test]
fn keyword_source_composes_with_a_typed_hop_and_filter() {
    let db = Database::in_memory().unwrap();
    let [_g, _v, _m, pa, _pb] = seed(&db);
    let plane = db.plane("startup").unwrap();

    // The composition a separate `keyword_search` call cannot express: seed by
    // relevance, hop to the cited papers, keep the recent ones.
    let rows = plane
        .query()
        .keyword_top_k("Doc", "body", "graph database", 5)
        .expand(Dir::Out, Some("CITES"))
        .filter(has_label("Paper"))
        .filter(dr_strange_core::p("year").ge(2020))
        .distinct()
        .nodes()
        .unwrap();

    assert_eq!(rows.iter().map(|n| n.id).collect::<Vec<_>>(), vec![pa]);
}

#[test]
fn keyword_source_without_an_index_is_empty() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();
    assert!(
        plane
            .query()
            .keyword_top_k("Doc", "title", "graph", 5)
            .nodes()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn hybrid_source_matches_the_builder_and_keeps_composing() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();

    let spec = HybridSpec {
        label: Some("Doc".into()),
        vector: None,
        keyword: Some(KeywordChannel {
            property: "body".into(),
            query: "graph database".into(),
        }),
        graph: Some(GraphChannel {
            hops: 1,
            decay: 0.5,
            seeds: 2,
        }),
        weights: HybridWeights::default(),
        candidates: 10,
        k: 5,
    };

    // Same query through the builder and as a plan source: same ranking.
    let via_builder = plane
        .hybrid()
        .label("Doc")
        .keyword("body", "graph database")
        .graph(1, 0.5)
        .candidates(10)
        .k(5)
        .run()
        .unwrap();
    let via_plan = plane.query().hybrid(spec).scored_nodes().unwrap();

    assert_eq!(
        via_builder.iter().map(|h| h.node).collect::<Vec<_>>(),
        via_plan.iter().map(|(n, _)| n.id).collect::<Vec<_>>()
    );
    assert!(!via_plan.is_empty());
    assert!(via_plan.iter().all(|(_, s)| s.is_some()));
}

#[test]
fn hybrid_source_rejects_a_keyword_channel_without_a_label() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();
    let spec = HybridSpec {
        label: None,
        vector: None,
        keyword: Some(KeywordChannel {
            property: "body".into(),
            query: "graph".into(),
        }),
        graph: None,
        weights: HybridWeights::default(),
        candidates: 10,
        k: 5,
    };
    assert!(plane.query().hybrid(spec).nodes().is_err());
}

#[test]
fn hybrid_vector_channel_flows_through_the_plan() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let near = txn
        .create_node(
            &["Item"],
            props(&[("emb", PropValue::Vector(vec![1.0, 0.0]))]),
        )
        .unwrap();
    let far = txn
        .create_node(
            &["Item"],
            props(&[("emb", PropValue::Vector(vec![0.0, 1.0]))]),
        )
        .unwrap();
    txn.commit().unwrap();

    let rows = plane
        .query()
        .hybrid(HybridSpec {
            label: Some("Item".into()),
            vector: Some(VectorChannel {
                property: "emb".into(),
                query: vec![1.0, 0.0],
                metric: dr_strange_core::Metric::Cosine,
            }),
            keyword: None,
            graph: None,
            weights: HybridWeights::default(),
            candidates: 10,
            k: 2,
        })
        .scored_nodes()
        .unwrap();

    assert_eq!(rows[0].0.id, near);
    assert_eq!(rows[1].0.id, far);
}

#[test]
fn pagerank_source_orders_by_rank_and_scores_each_row() {
    let db = Database::in_memory().unwrap();
    let [_g, _v, _m, pa, _pb] = seed(&db);
    let plane = db.plane("startup").unwrap();

    let rows = plane
        .query()
        .algo(
            None,
            Algo::PageRank {
                damping: 0.85,
                max_iters: 20,
                tolerance: 1e-6,
            },
        )
        .scored_nodes()
        .unwrap();

    assert_eq!(rows.len(), 5);
    // paper-a is cited twice — the most-pointed-at node ranks first.
    assert_eq!(rows[0].0.id, pa);
    let scores: Vec<f32> = rows.iter().map(|(_, s)| s.unwrap()).collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "rows arrive in rank order: {scores:?}"
    );
}

#[test]
fn algo_source_can_be_scoped_and_traversed_from() {
    let db = Database::in_memory().unwrap();
    let [_g, _v, _m, pa, pb] = seed(&db);
    let plane = db.plane("startup").unwrap();

    // Rank the docs, then follow their citations — an algorithm feeding a
    // traversal in one plan.
    let mut cited = plane
        .query()
        .algo(
            Some("Doc"),
            Algo::PageRank {
                damping: 0.85,
                max_iters: 20,
                tolerance: 1e-6,
            },
        )
        .expand(Dir::Out, Some("CITES"))
        .distinct()
        .nodes()
        .unwrap()
        .iter()
        .map(|n| n.id)
        .collect::<Vec<_>>();
    cited.sort_by_key(|n| n.0);
    assert_eq!(cited, vec![pa, pb]);
}

#[test]
fn components_source_groups_and_indexes_communities() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    // Two disjoint pairs: a—b and c—d.
    let mut txn = plane.write().unwrap();
    let a = txn.create_node(&["N"], Properties::new()).unwrap();
    let b = txn.create_node(&["N"], Properties::new()).unwrap();
    let c = txn.create_node(&["N"], Properties::new()).unwrap();
    let d = txn.create_node(&["N"], Properties::new()).unwrap();
    txn.create_edge(a, b, "R", Properties::new()).unwrap();
    txn.create_edge(c, d, "R", Properties::new()).unwrap();
    txn.commit().unwrap();

    let rows = plane
        .query()
        .algo(None, Algo::ConnectedComponents)
        .scored_nodes()
        .unwrap();

    assert_eq!(rows.len(), 4);
    let by_id = |id: NodeId| rows.iter().find(|(n, _)| n.id == id).unwrap().1.unwrap();
    assert_eq!(by_id(a), by_id(b), "a and b share a component");
    assert_eq!(by_id(c), by_id(d), "c and d share a component");
    assert_ne!(by_id(a), by_id(c), "the two components are distinct");
    // The index is dense and 0-based, and rows arrive grouped by it.
    let scores: Vec<f32> = rows.iter().map(|(_, s)| s.unwrap()).collect();
    assert_eq!(scores, vec![0.0, 0.0, 1.0, 1.0]);
}

#[test]
fn shortest_path_source_returns_the_path_in_order() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let a = txn
        .create_node_with_key("a", &["N"], Properties::new())
        .unwrap();
    let b = txn
        .create_node_with_key("b", &["N"], Properties::new())
        .unwrap();
    let c = txn
        .create_node_with_key("c", &["N"], Properties::new())
        .unwrap();
    txn.create_edge(a, b, "R", Properties::new()).unwrap();
    txn.create_edge(b, c, "R", Properties::new()).unwrap();
    txn.commit().unwrap();

    let rows = plane
        .query()
        .algo(
            None,
            Algo::ShortestPath {
                from: NodeRef::Key("a".into()),
                to: NodeRef::Id(c),
                dir: Dir::Out,
                weight: None,
            },
        )
        .scored_nodes()
        .unwrap();

    assert_eq!(
        rows.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
        vec![a, b, c]
    );
    assert_eq!(
        rows.iter().map(|(_, s)| s.unwrap()).collect::<Vec<_>>(),
        vec![0.0, 1.0, 2.0],
        "the score channel carries each node's position along the path"
    );
}

#[test]
fn shortest_path_source_is_empty_when_unresolvable() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();

    // An unknown key resolves to nothing rather than erroring.
    let unknown = plane
        .query()
        .algo(
            None,
            Algo::ShortestPath {
                from: NodeRef::Key("nope".into()),
                to: NodeRef::Key("paper-a".into()),
                dir: Dir::Out,
                weight: None,
            },
        )
        .nodes()
        .unwrap();
    assert!(unknown.is_empty());

    // So does a real endpoint pair with no route between them.
    let unreachable = plane
        .query()
        .algo(
            None,
            Algo::ShortestPath {
                from: NodeRef::Key("paper-a".into()),
                to: NodeRef::Key("doc-graph".into()),
                dir: Dir::Out,
                weight: None,
            },
        )
        .nodes()
        .unwrap();
    assert!(unreachable.is_empty());
}

#[test]
fn algo_source_sorts_and_limits_like_any_other() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();

    let top = plane
        .query()
        .algo(
            Some("Paper"),
            Algo::PageRank {
                damping: 0.85,
                max_iters: 20,
                tolerance: 1e-6,
            },
        )
        .sort_by(vec![SortKey {
            expr: score(),
            descending: true,
        }])
        .limit(1)
        .scored_nodes()
        .unwrap();
    assert_eq!(top.len(), 1);
}

#[test]
fn every_new_source_round_trips_through_plan_json() {
    let db = Database::in_memory().unwrap();
    seed(&db);
    let plane = db.plane("startup").unwrap();

    // What an SDK or the CLI's `drsg query` sends over the wire.
    for source in [
        Source::KeywordTopK {
            label: "Doc".into(),
            property: "body".into(),
            query: "graph".into(),
            k: 3,
        },
        Source::Algo {
            label: Some("Doc".into()),
            algo: Algo::ConnectedComponents,
        },
    ] {
        let plan = dr_strange_core::LogicalPlan::new(source);
        let json = serde_json::to_string(&plan).unwrap();
        let back: dr_strange_core::LogicalPlan = serde_json::from_str(&json).unwrap();
        assert!(plane.query_from_plan(back).nodes().is_ok());
    }
}
