use super::*;

/// A span in `service`, called by `parent`, at `start_ms`.
fn span(id: &str, parent: Option<&str>, service: &str, start_ms: i64) -> Span {
    Span {
        trace: "t".into(),
        id: id.into(),
        parent: parent.map(str::to_string),
        service: service.into(),
        start_ms,
        error: false,
    }
}

/// One string property, where it is one.
fn prop(props: &Properties, name: &str) -> String {
    match props.get(name).map(|d| &d.value) {
        Some(PropValue::Str(s)) => s.clone(),
        Some(PropValue::Int(n)) => n.to_string(),
        other => panic!("{name} is {other:?}"),
    }
}

#[test]
fn a_span_and_its_parent_in_another_service_are_an_edge() {
    let spans = [
        span("1", None, "gateway", 1_000),
        span("2", Some("1"), "checkout", 1_010),
    ];
    let folded = fold(&spans, &BTreeMap::new());

    assert_eq!(folded.edges.len(), 1);
    let edge = &folded.edges[0];
    assert_eq!(edge.src, "observed/gateway");
    assert_eq!(edge.dst, "observed/checkout");
    assert_eq!(edge.ty, "CALLS");
    assert_eq!(prop(&edge.props, "origin"), "observed");
    assert_eq!(prop(&edge.props, "calls"), "1");
    assert_eq!(prop(&edge.props, "errors"), "0");
    assert_eq!(prop(&edge.props, "_generated_by"), "otel");
    assert_eq!(
        folded.report,
        Report {
            spans: 2,
            services: 2,
            ..Default::default()
        }
    );
}

#[test]
fn many_spans_are_one_edge_that_counts_them() {
    let mut spans = vec![span("root", None, "gateway", 1_000)];
    for n in 0..5 {
        let mut s = span(&format!("c{n}"), Some("root"), "checkout", 2_000 + n * 10);
        s.error = n % 2 == 0;
        spans.push(s);
    }
    let folded = fold(&spans, &BTreeMap::new());

    assert_eq!(folded.edges.len(), 1);
    let props = &folded.edges[0].props;
    assert_eq!(prop(props, "calls"), "5");
    assert_eq!(prop(props, "errors"), "3");
    assert_eq!(prop(props, "first_seen"), "1970-01-01T00:00:02Z");
    assert_eq!(prop(props, "last_seen"), "1970-01-01T00:00:02Z");
    assert_eq!(prop(props, "last_seen_ts"), "2");
}

#[test]
fn a_parent_in_no_document_is_an_orphan_and_the_count_says_so() {
    let spans = [
        span("2", Some("gone"), "checkout", 1_000),
        span("3", Some("gone"), "checkout", 1_010),
    ];
    let folded = fold(&spans, &BTreeMap::new());

    assert!(folded.edges.is_empty());
    assert_eq!(folded.report.orphans, 2);
}

#[test]
fn a_call_inside_one_service_is_not_an_edge_between_two() {
    let spans = [
        span("1", None, "checkout", 1_000),
        span("2", Some("1"), "checkout", 1_010),
    ];
    let folded = fold(&spans, &BTreeMap::new());

    assert!(folded.edges.is_empty());
    assert_eq!(folded.report.internal, 1);
    assert_eq!(folded.report.services, 1);
}

#[test]
fn a_service_the_topology_declares_is_named_as_it_names_it() {
    let declared = BTreeMap::from([
        (
            "gateway".to_string(),
            Named::One("prod/Service/gateway".into()),
        ),
        (
            "checkout".to_string(),
            Named::One("prod/Service/checkout".into()),
        ),
    ]);
    let spans = [
        span("1", None, "gateway", 1_000),
        span("2", Some("1"), "checkout", 1_010),
    ];
    let folded = fold(&spans, &declared);

    assert_eq!(folded.edges[0].src, "prod/Service/gateway");
    assert_eq!(folded.edges[0].dst, "prod/Service/checkout");
    assert!(
        folded.nodes.is_empty(),
        "a declared service already has its node"
    );
    assert_eq!(folded.report.joined, 2);
}

#[test]
fn a_service_nothing_declares_gets_its_own_node() {
    let spans = [
        span("1", None, "gateway", 1_700_000_000_000),
        span("2", Some("1"), "checkout", 1_700_000_000_500),
    ];
    let folded = fold(&spans, &BTreeMap::new());

    assert_eq!(folded.nodes.len(), 2);
    let node = folded
        .nodes
        .iter()
        .find(|n| n.key == "observed/checkout")
        .expect("checkout");
    assert_eq!(node.label, "Service");
    assert_eq!(prop(&node.props, "name"), "checkout");
    assert_eq!(prop(&node.props, "origin"), "observed");
    assert_eq!(prop(&node.props, "spans"), "1");
    assert_eq!(prop(&node.props, "last_seen"), "2023-11-14T22:13:20Z");
}

#[test]
fn a_name_several_declared_nodes_answer_to_stays_observed_with_the_candidates() {
    let declared = BTreeMap::from([(
        "checkout".to_string(),
        Named::Many(vec![
            "prod/Service/checkout".into(),
            "staging/Service/checkout".into(),
        ]),
    )]);
    let spans = [
        span("1", None, "gateway", 1_000),
        span("2", Some("1"), "checkout", 1_010),
    ];
    let folded = fold(&spans, &declared);

    assert_eq!(folded.edges[0].dst, "observed/checkout");
    let node = folded
        .nodes
        .iter()
        .find(|n| n.key == "observed/checkout")
        .expect("checkout");
    assert_eq!(
        node.props.get("candidates").map(|d| &d.value),
        Some(&PropValue::List(vec![
            PropValue::Str("prod/Service/checkout".into()),
            PropValue::Str("staging/Service/checkout".into()),
        ]))
    );
    assert_eq!(folded.report.ambiguous, 1);
}

#[test]
fn a_span_with_no_time_leaves_the_window_off_rather_than_claiming_1970() {
    let spans = [
        span("1", None, "gateway", 0),
        span("2", Some("1"), "checkout", 0),
    ];
    let folded = fold(&spans, &BTreeMap::new());

    let props = &folded.edges[0].props;
    assert!(!props.contains_key("first_seen"));
    assert!(!props.contains_key("last_seen"));
    assert!(!props.contains_key("last_seen_ts"));
}

#[test]
fn two_names_the_same_node_answers_to_are_one_service_not_a_self_edge() {
    let declared = BTreeMap::from([
        ("checkout".to_string(), Named::One("prod/Service/x".into())),
        (
            "checkout-canary".to_string(),
            Named::One("prod/Service/x".into()),
        ),
    ]);
    let spans = [
        span("1", None, "checkout", 1_000),
        span("2", Some("1"), "checkout-canary", 1_010),
    ];
    let folded = fold(&spans, &declared);

    assert!(folded.edges.is_empty());
    assert_eq!(folded.report.internal, 1);
}

#[test]
fn nothing_read_is_nothing_said() {
    let folded = fold(&[], &BTreeMap::new());
    assert!(!folded.report.any());
    assert!(folded.nodes.is_empty() && folded.edges.is_empty());
}
