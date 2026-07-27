//! M2 query engine, exercised through the public builder API (arch/03,
//! arch/04 §3) on both backends, plus a plan serde round-trip and a proptest
//! checking `Filter` against a naive model.

use dr_strange_core::{Database, Dir, NodeId, PropValue, Properties, SortKey, has_label, p};
use proptest::prelude::*;

fn prop_i(v: i64) -> Properties {
    [(
        "year".to_string(),
        dr_strange_core::PropDesc::new(PropValue::Int(v)),
    )]
    .into_iter()
    .collect()
}

/// A small citation graph in the startup plane:
///   p1(2020) -CITES-> p2(2019), p3(2021)
///   p2(2019) -CITES-> p3(2021)
///   a1 -AUTHORED-> p1
/// Nodes are keyed ("p1".."p3","a1") for stable seeking.
fn build_fixture(db: &Database) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let p1 = txn
        .create_node_with_key("p1", &["Paper"], prop_i(2020))
        .unwrap();
    let p2 = txn
        .create_node_with_key("p2", &["Paper"], prop_i(2019))
        .unwrap();
    let p3 = txn
        .create_node_with_key("p3", &["Paper"], prop_i(2021))
        .unwrap();
    let a1 = txn
        .create_node_with_key("a1", &["Person"], Properties::new())
        .unwrap();
    txn.create_edge(p1, p2, "CITES", Properties::new()).unwrap();
    txn.create_edge(p1, p3, "CITES", Properties::new()).unwrap();
    txn.create_edge(p2, p3, "CITES", Properties::new()).unwrap();
    txn.create_edge(a1, p1, "AUTHORED", Properties::new())
        .unwrap();
    txn.commit().unwrap();
}

fn key_id(db: &Database, key: &str) -> NodeId {
    db.plane("startup")
        .unwrap()
        .node_by_key(key)
        .unwrap()
        .unwrap()
        .id
}

fn years(nodes: &[dr_strange_core::NodeRecord]) -> Vec<i64> {
    nodes
        .iter()
        .map(|n| match &n.properties["year"].value {
            PropValue::Int(y) => *y,
            other => panic!("expected int year, got {other:?}"),
        })
        .collect()
}

fn run_query_suite(db: &Database) {
    build_fixture(db);
    let plane = db.plane("startup").unwrap();
    let (p1, p2, p3) = (key_id(db, "p1"), key_id(db, "p2"), key_id(db, "p3"));

    // scan by label
    assert_eq!(plane.query().scan_label("Paper").count().unwrap(), 3);
    assert_eq!(plane.query().scan_label("Person").count().unwrap(), 1);
    assert_eq!(plane.query().scan_label("Ghost").count().unwrap(), 0);

    // scan all
    assert_eq!(plane.query().scan_all().count().unwrap(), 4);

    // filter on a property
    let recent = plane
        .query()
        .scan_label("Paper")
        .filter(p("year").ge(2020))
        .ids()
        .unwrap();
    assert_eq!(recent, vec![p1, p3]); // 2020, 2021 (id order)

    // filter by label over a full scan
    assert_eq!(
        plane
            .query()
            .scan_all()
            .filter(has_label("Paper"))
            .count()
            .unwrap(),
        3
    );

    // 1-hop expand from a seeded node
    let mut cited = plane
        .query()
        .seek_keys(["p1"])
        .expand_out("CITES")
        .ids()
        .unwrap();
    cited.sort();
    assert_eq!(cited, {
        let mut v = vec![p2, p3];
        v.sort();
        v
    });

    // expand + filter: p1's citations from 2021 onward → p3
    let recent_cites = plane
        .query()
        .seek_keys(["p1"])
        .expand_out("CITES")
        .filter(p("year").ge(2021))
        .ids()
        .unwrap();
    assert_eq!(recent_cites, vec![p3]);

    // variable-length expand + distinct: everything reachable from p1 in 1..=2
    // CITES hops = {p2, p3} (p3 reached directly and via p2).
    let mut reach = plane
        .query()
        .seek_keys(["p1"])
        .expand_var(Dir::Out, Some("CITES"), 1, 2)
        .distinct()
        .ids()
        .unwrap();
    reach.sort();
    assert_eq!(reach, {
        let mut v = vec![p2, p3];
        v.sort();
        v
    });

    // in-direction expand: who cites p3? p1 and p2
    assert_eq!(
        plane
            .query()
            .seek_keys(["p3"])
            .expand_in("CITES")
            .count()
            .unwrap(),
        2
    );

    // sort ascending / descending by year
    let asc = plane
        .query()
        .scan_label("Paper")
        .sort_asc(p("year"))
        .nodes()
        .unwrap();
    assert_eq!(years(&asc), vec![2019, 2020, 2021]);
    let desc = plane
        .query()
        .scan_label("Paper")
        .sort_desc(p("year"))
        .nodes()
        .unwrap();
    assert_eq!(years(&desc), vec![2021, 2020, 2019]);

    // sort + limit = top-k
    let top1 = plane
        .query()
        .scan_label("Paper")
        .sort_desc(p("year"))
        .limit(1)
        .nodes()
        .unwrap();
    assert_eq!(years(&top1), vec![2021]);

    // skip + limit
    let middle = plane
        .query()
        .scan_label("Paper")
        .sort_asc(p("year"))
        .skip(1)
        .limit(1)
        .nodes()
        .unwrap();
    assert_eq!(years(&middle), vec![2020]);

    // select projects expressions per row
    let projected = plane
        .query()
        .scan_label("Paper")
        .sort_asc(p("year"))
        .select(&[p("year"), p("year").ge(2020)])
        .unwrap();
    assert_eq!(
        projected,
        vec![
            vec![PropValue::Int(2019), PropValue::Bool(false)],
            vec![PropValue::Int(2020), PropValue::Bool(true)],
            vec![PropValue::Int(2021), PropValue::Bool(true)],
        ]
    );

    // multi-key sort composes (explicit SortKey list)
    let by_label_then_year = plane
        .query()
        .scan_all()
        .filter(has_label("Paper"))
        .sort_by(vec![SortKey {
            expr: p("year"),
            descending: true,
        }])
        .ids()
        .unwrap();
    assert_eq!(by_label_then_year, vec![p3, p1, p2]);
}

#[test]
fn query_suite_memory() {
    run_query_suite(&Database::in_memory().unwrap());
}

#[test]
fn query_suite_redb() {
    let dir = tempfile::tempdir().unwrap();
    run_query_suite(&Database::open(dir.path().join("query.drsg")).unwrap());
}

#[test]
fn built_plan_is_serde_roundtrippable() {
    // The plan a builder produces must survive JSON round-trip unchanged —
    // that's the wire-protocol readiness the arch promises (arch/00 §2).
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let q = plane
        .query()
        .scan_label("Paper")
        .expand_out("CITES")
        .filter(p("year").ge(2020))
        .sort_desc(p("year"))
        .limit(5);
    let json = serde_json::to_string(q.plan()).unwrap();
    let back: dr_strange_core::LogicalPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(q.plan(), &back);
}

#[test]
fn queries_are_empty_on_a_fresh_db() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    assert_eq!(plane.query().scan_all().count().unwrap(), 0);
    assert!(plane.query().scan_label("X").ids().unwrap().is_empty());
    assert!(
        plane
            .query()
            .seek_keys(["missing"])
            .nodes()
            .unwrap()
            .is_empty()
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// `scan_all().filter(year >= t)` must match a naive model over the same
    /// random node set — the executor's Filter agrees with brute force.
    #[test]
    fn filter_matches_naive_model(
        vals in prop::collection::vec(-5i64..5, 0..30),
        threshold in -6i64..6,
    ) {
        let db = Database::in_memory().unwrap();
        let plane = db.plane("startup").unwrap();

        // create one node per value; node ids are sequential from 1, in
        // creation order, so the model set is naturally in scan order.
        let mut expected = Vec::new();
        {
            let mut txn = plane.write().unwrap();
            for &v in &vals {
                let id = txn.create_node(&["N"], prop_i(v)).unwrap();
                if v >= threshold {
                    expected.push(id);
                }
            }
            txn.commit().unwrap();
        }

        let got = plane
            .query()
            .scan_all()
            .filter(p("year").ge(threshold))
            .ids()
            .unwrap();
        prop_assert_eq!(got, expected);
    }
}
