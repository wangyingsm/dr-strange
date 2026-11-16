//! The join, against facts alone and against a plane that already holds the
//! topology.

use dr_strange_core::{Database, PropDesc, PropValue, Properties};

use super::*;

fn props(pairs: &[(&str, &str)]) -> Properties {
    let mut props = Properties::new();
    for (name, value) in pairs {
        props.insert(
            (*name).into(),
            PropDesc::new(PropValue::Str((*value).to_string())),
        );
    }
    props
}

fn node(key: &str, label: &str, pairs: &[(&str, &str)]) -> DigestNode {
    DigestNode {
        key: key.into(),
        label: label.into(),
        extra_labels: Vec::new(),
        props: props(pairs),
    }
}

fn target(job: &str, address: &str, host: &str) -> DigestNode {
    node(
        &format!("{job}/target/{address}"),
        "ScrapeTarget",
        &[("address", address), ("host", host)],
    )
}

fn scrapes(edges: &[DigestEdge]) -> Vec<(String, String)> {
    edges
        .iter()
        .filter(|e| e.ty == SCRAPES)
        .map(|e| (e.src.clone(), e.dst.clone()))
        .collect()
}

fn prop_of(props: &Properties, name: &str) -> Option<String> {
    match props.get(name).map(|d| &d.value) {
        Some(PropValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// One service answers to the name, so the edge is drawn — and says it was a
/// name that matched, not an address.
#[test]
fn one_name_is_one_edge_marked_for_what_it_is() {
    let mut nodes = vec![
        node("shop/service/api", "Workload", &[("name", "api")]),
        target("cfg/job/api", "api:9090", "api"),
    ];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, None).unwrap();

    assert_eq!(found.matched, 1);
    assert_eq!(
        scrapes(&edges),
        vec![(
            "cfg/job/api/target/api:9090".to_string(),
            "shop/service/api".to_string()
        )]
    );
    let props = &edges[0].props;
    assert_eq!(prop_of(props, "matched_by").as_deref(), Some("name"));
    assert_eq!(prop_of(props, "confidence").as_deref(), Some("low"));
}

/// Two things answer to one name, so nothing is chosen. The candidates are
/// recorded on the target instead.
#[test]
fn two_names_are_candidates_and_no_edge() {
    let mut nodes = vec![
        node("prod/Service/api", "Service", &[("name", "api")]),
        node("shop/service/api", "Workload", &[("name", "api")]),
        target("cfg/job/api", "api:9090", "api"),
    ];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, None).unwrap();

    assert_eq!(found.ambiguous, 1);
    assert_eq!(found.matched, 0);
    assert!(scrapes(&edges).is_empty(), "a guess was drawn as an edge");
    let candidates = nodes[2].props.get("candidates").map(|d| d.value.clone());
    assert_eq!(
        candidates,
        Some(PropValue::List(vec![
            PropValue::Str("prod/Service/api".into()),
            PropValue::Str("shop/service/api".into()),
        ]))
    );
}

/// An external host, an IP, a service this tree does not declare: counted, and
/// left as the address it is.
#[test]
fn an_unmatched_host_is_counted_and_left_alone() {
    let mut nodes = vec![
        node("shop/service/api", "Workload", &[("name", "api")]),
        target("cfg/job/node", "10.0.3.11:9100", "10.0.3.11"),
    ];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, None).unwrap();

    assert_eq!(found.unmatched, 1);
    assert!(scrapes(&edges).is_empty());
    assert!(nodes[1].props.get("candidates").is_none());
}

/// An address that names a key is joined to it exactly, and said to be exact.
#[test]
fn an_address_that_names_a_key_is_joined_to_it() {
    let key = "cfg/job/api/target/api.prod.svc.cluster.local:9090";
    let mut nodes = vec![
        node("prod/Service/api", "Service", &[("name", "api")]),
        node(
            key,
            "ScrapeTarget",
            &[
                ("address", "api.prod.svc.cluster.local:9090"),
                ("host", "api.prod.svc.cluster.local"),
                ("names", "prod/Service/api"),
            ],
        ),
    ];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, None).unwrap();

    assert_eq!(found.matched, 1);
    assert_eq!(
        scrapes(&edges),
        vec![(key.to_string(), "prod/Service/api".to_string())]
    );
    assert_eq!(
        prop_of(&edges[0].props, "matched_by").as_deref(),
        Some("dns")
    );
    assert_eq!(
        prop_of(&edges[0].props, "confidence").as_deref(),
        Some("exact")
    );
}

/// The defect the first end-to-end run found: an address naming a service this
/// tree does not declare is unmatched, not an edge into nothing — which the
/// digest refuses outright.
#[test]
fn an_address_naming_a_service_the_tree_lacks_is_unmatched() {
    let key = "cfg/job/api/target/api.prod.svc.cluster.local:9090";
    let mut nodes = vec![node(
        key,
        "ScrapeTarget",
        &[
            ("host", "api.prod.svc.cluster.local"),
            ("names", "prod/Service/api"),
        ],
    )];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, None).unwrap();

    assert_eq!(found.unmatched, 1);
    assert!(edges.is_empty(), "an edge was drawn to a key nothing holds");
}

/// And the same address against a plane that does hold it.
#[test]
fn an_address_naming_a_service_in_the_plane_is_joined() {
    let db = Database::in_memory().unwrap();
    db.create_plane("code_ops", Properties::new()).unwrap();
    let plane = db.plane("code_ops").unwrap();
    {
        let mut txn = plane.write().unwrap();
        txn.create_node_with_key("prod/Service/api", &["Service"], props(&[("name", "api")]))
            .unwrap();
        txn.commit().unwrap();
    }
    let key = "cfg/job/api/target/api.prod.svc.cluster.local:9090";
    let mut nodes = vec![node(
        key,
        "ScrapeTarget",
        &[
            ("host", "api.prod.svc.cluster.local"),
            ("names", "prod/Service/api"),
        ],
    )];
    let mut edges = Vec::new();

    assert_eq!(
        join(&mut nodes, &mut edges, Some(&plane)).unwrap().matched,
        1
    );
    assert_eq!(
        scrapes(&edges),
        vec![(key.to_string(), "prod/Service/api".to_string())]
    );
}

/// Nothing to do, and the plane is not read for it.
#[test]
fn a_tree_without_scrape_targets_joins_nothing() {
    let mut nodes = vec![node("shop/service/api", "Workload", &[("name", "api")])];
    let mut edges = Vec::new();
    assert_eq!(
        join(&mut nodes, &mut edges, None).unwrap(),
        Joined::default()
    );
    assert!(edges.is_empty());
}

/// The case the facts alone cannot answer: an incremental fold re-reads the
/// configuration and nothing else, so the service it must reach is in the
/// plane rather than in hand.
#[test]
fn a_service_already_in_the_plane_is_found() {
    let db = Database::in_memory().unwrap();
    db.create_plane("code_ops", Properties::new()).unwrap();
    let plane = db.plane("code_ops").unwrap();
    {
        let mut txn = plane.write().unwrap();
        txn.create_node_with_key("shop/service/api", &["Workload"], props(&[("name", "api")]))
            .unwrap();
        txn.commit().unwrap();
    }

    // Only the Prometheus facts, as a fold of one changed file would carry.
    let mut nodes = vec![target("cfg/job/api", "api:9090", "api")];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, Some(&plane)).unwrap();

    assert_eq!(found.matched, 1);
    assert_eq!(
        scrapes(&edges),
        vec![(
            "cfg/job/api/target/api:9090".to_string(),
            "shop/service/api".to_string()
        )]
    );
}

/// A node in the plane and one in hand under the same name are two candidates,
/// not one match counted twice.
#[test]
fn the_plane_and_the_facts_are_one_set_of_candidates() {
    let db = Database::in_memory().unwrap();
    db.create_plane("code_ops", Properties::new()).unwrap();
    let plane = db.plane("code_ops").unwrap();
    {
        let mut txn = plane.write().unwrap();
        txn.create_node_with_key("prod/Service/api", &["Service"], props(&[("name", "api")]))
            .unwrap();
        txn.commit().unwrap();
    }

    let mut nodes = vec![
        node("shop/service/api", "Workload", &[("name", "api")]),
        target("cfg/job/api", "api:9090", "api"),
    ];
    let mut edges = Vec::new();

    let found = join(&mut nodes, &mut edges, Some(&plane)).unwrap();

    assert_eq!(found.ambiguous, 1);
    assert!(scrapes(&edges).is_empty());
}

/// The same facts joined twice — a re-digest of an unchanged tree — draw the
/// same one edge, not two.
#[test]
fn joining_twice_draws_one_edge() {
    let mut nodes = vec![
        node("shop/service/api", "Workload", &[("name", "api")]),
        target("cfg/job/api", "api:9090", "api"),
    ];
    let mut edges = Vec::new();
    join(&mut nodes, &mut edges, None).unwrap();
    let again = join(&mut nodes, &mut edges, None).unwrap();
    assert_eq!(again, Joined::default());
    assert_eq!(scrapes(&edges).len(), 1);
}
