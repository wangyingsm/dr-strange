//! Write statements, end to end: parse → apply against an in-memory database →
//! assert the graph changed as expected.

use dr_strange_core::{Database, Dir, PlaneHandle, PropValue};
use dr_strange_parser::{ParseError, Statement, WriteSummary, parse, parse_statement};

/// Parse a write and apply it to `startup`, returning the summary.
fn write(db: &Database, q: &str) -> WriteSummary {
    match parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")) {
        Statement::Write(w) => w
            .apply(&db.plane("startup").unwrap())
            .unwrap_or_else(|e| panic!("apply `{q}`: {e}")),
        Statement::Read(_) => panic!("expected a write for `{q}`"),
    }
}

fn plane(db: &Database) -> PlaneHandle<'_> {
    db.plane("startup").unwrap()
}

#[test]
fn create_nodes_edge_and_props() {
    let db = Database::in_memory().unwrap();
    let s = write(
        &db,
        r#"CREATE (a:Person {key:"alice", age:30, name:"Alice", active:true}),
                 (b:Person {key:"bob"}),
                 (a)-[:KNOWS {since:2020}]->(b)"#,
    );
    // a and b are created once; the third path reuses both by variable.
    assert_eq!(s.nodes_created, 2);
    assert_eq!(s.edges_created, 1);

    let p = plane(&db);
    let alice = p.node_by_key("alice").unwrap().unwrap();
    assert!(alice.labels.iter().any(|l| l == "Person"));
    assert_eq!(alice.properties["age"].value, PropValue::Int(30));
    assert_eq!(
        alice.properties["name"].value,
        PropValue::Str("Alice".into())
    );
    assert_eq!(alice.properties["active"].value, PropValue::Bool(true));
    // `key` became the external key, not a property.
    assert!(!alice.properties.contains_key("key"));

    // The KNOWS edge goes alice → bob.
    let bob = p.node_by_key("bob").unwrap().unwrap();
    let out = p.neighbors(alice.id, Dir::Out, None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, bob.id);
}

#[test]
fn incoming_edge_direction() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (a:N {key:"a"}), (b:N {key:"b"}), (a)<-[:REF]-(b)"#,
    );
    let p = plane(&db);
    let a = p.node_by_key("a").unwrap().unwrap();
    let b = p.node_by_key("b").unwrap().unwrap();
    // `(a)<-[:REF]-(b)` is b → a.
    let out = p.neighbors(b.id, Dir::Out, None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, a.id);
}

#[test]
fn create_with_negative_and_vector_props() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (n:Doc {key:"d", score:-1.5, emb:[0.0, 1.0]})"#,
    );
    let p = plane(&db);
    let d = p.node_by_key("d").unwrap().unwrap();
    assert_eq!(d.properties["score"].value, PropValue::Float(-1.5));
    assert_eq!(d.properties["emb"].value, PropValue::Vector(vec![0.0, 1.0]));
}

#[test]
fn anonymous_and_unlabeled_nodes() {
    let db = Database::in_memory().unwrap();
    let s = write(&db, "CREATE (), (:Tag), (n)");
    assert_eq!(s.nodes_created, 3);
    assert_eq!(s.edges_created, 0);
}

#[test]
fn rejects_undirected_create_edge() {
    // Undirected / two-headed edges aren't creatable (edges are directed).
    assert!(matches!(
        parse_statement(r#"CREATE (a {key:"a"})-[:R]-(b {key:"b"})"#),
        Err(ParseError::Syntax(_))
    ));
}

// ---- MATCH … SET / REMOVE / DELETE ----------------------------------------

#[test]
fn set_prop_label_and_merge() {
    let db = Database::in_memory().unwrap();
    write(&db, r#"CREATE (a:Person {key:"alice", age:30})"#);
    let s = write(
        &db,
        r#"MATCH (n:Person) WHERE n.age = 30 SET n.age = 31, n:VIP, n += {city:"NYC", zip:10001}"#,
    );
    assert_eq!(s.props_set, 3); // age + city + zip
    assert_eq!(s.labels_set, 1); // VIP

    let a = plane(&db).node_by_key("alice").unwrap().unwrap();
    assert_eq!(a.properties["age"].value, PropValue::Int(31));
    assert_eq!(a.properties["city"].value, PropValue::Str("NYC".into()));
    assert!(a.labels.iter().any(|l| l == "VIP"));
    assert!(a.labels.iter().any(|l| l == "Person")); // original label kept
}

#[test]
fn remove_prop_and_label() {
    let db = Database::in_memory().unwrap();
    write(&db, r#"CREATE (a:Person {key:"a", tmp:1})"#);
    write(&db, "MATCH (n:Person) SET n:Archived");
    let s = write(&db, "MATCH (n:Person) REMOVE n.tmp, n:Archived");
    assert_eq!(s.props_set, 1);
    assert_eq!(s.labels_set, 1);

    let a = plane(&db).node_by_key("a").unwrap().unwrap();
    assert!(!a.properties.contains_key("tmp"));
    assert!(!a.labels.iter().any(|l| l == "Archived"));
    assert!(a.labels.iter().any(|l| l == "Person"));
}

#[test]
fn delete_unconnected_node() {
    let db = Database::in_memory().unwrap();
    write(&db, r#"CREATE (a:N {key:"a", tag:"x"})"#);
    let s = write(&db, r#"MATCH (n:N) WHERE n.tag = "x" DELETE n"#);
    assert_eq!(s.nodes_deleted, 1);
    assert!(plane(&db).node_by_key("a").unwrap().is_none());
}

#[test]
fn plain_delete_refuses_connected_node_detach_cascades() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (a:N {key:"a", tag:"x"})-[:R]->(b:N {key:"b", tag:"y"})"#,
    );

    // Plain DELETE of the connected node errors (and doesn't commit).
    let stmt = parse_statement(r#"MATCH (n:N) WHERE n.tag = "x" DELETE n"#).unwrap();
    let e = match stmt {
        Statement::Write(w) => w.apply(&plane(&db)).unwrap_err(),
        Statement::Read(_) => panic!("expected write"),
    };
    assert!(e.contains("DETACH"), "{e}");
    assert!(plane(&db).node_by_key("a").unwrap().is_some());

    // DETACH DELETE cascades: node a and its edge go, b remains.
    let s = write(&db, r#"MATCH (n:N) WHERE n.tag = "x" DETACH DELETE n"#);
    assert_eq!(s.nodes_deleted, 1);
    let p = plane(&db);
    assert!(p.node_by_key("a").unwrap().is_none());
    assert!(p.node_by_key("b").unwrap().is_some());
}

#[test]
fn match_create_anchors_new_nodes_to_matched_rows() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (a:Person {key:"alice", active:true}),
                 (b:Person {key:"bob", active:true}),
                 (c:Person {key:"carol", active:false})"#,
    );
    // Give every active person a fresh gold Badge — runs once per matched row.
    let s = write(
        &db,
        r#"MATCH (p:Person) WHERE p.active = true CREATE (p)-[:HAS]->(badge:Badge {name:"gold"})"#,
    );
    assert_eq!(s.nodes_created, 2); // 2 badges (the matched persons are anchored, not recreated)
    assert_eq!(s.edges_created, 2);

    let p = plane(&db);
    let alice = p.node_by_key("alice").unwrap().unwrap();
    assert_eq!(
        p.neighbors(alice.id, Dir::Out, Some("HAS")).unwrap().len(),
        1
    );
    let carol = p.node_by_key("carol").unwrap().unwrap(); // inactive → untouched
    assert!(
        p.neighbors(carol.id, Dir::Out, Some("HAS"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(p.catalog().unwrap().node_count, 5); // 3 persons + 2 badges
}

#[test]
fn rejects_mutation_of_non_terminal_variable() {
    // `a` is not the pattern's terminal variable (`n` is).
    let e = parse_statement(r#"MATCH (a:N)-[:R]->(n:N) SET a.x = 1"#).unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
}

#[test]
fn rejects_match_write_with_anonymous_terminal() {
    let e = parse_statement(r#"MATCH (a:N)-[:R]->() SET x.p = 1"#).unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
}

// ---- MERGE (upsert) -------------------------------------------------------

#[test]
fn merge_creates_when_absent() {
    let db = Database::in_memory().unwrap();
    let s = write(
        &db,
        r#"MERGE (a:Person {key:"alice", age:30})
             ON CREATE SET a.created = true
             ON MATCH SET a.seen = true"#,
    );
    assert_eq!(s.nodes_created, 1);
    let a = plane(&db).node_by_key("alice").unwrap().unwrap();
    assert!(a.labels.iter().any(|l| l == "Person"));
    assert_eq!(a.properties["age"].value, PropValue::Int(30)); // inline prop written
    assert_eq!(a.properties["created"].value, PropValue::Bool(true)); // ON CREATE
    assert!(!a.properties.contains_key("seen")); // ON MATCH skipped
}

#[test]
fn merge_binds_when_present() {
    let db = Database::in_memory().unwrap();
    write(&db, r#"CREATE (a:Person {key:"bob", age:40})"#);
    let s = write(
        &db,
        r#"MERGE (a:Person {key:"bob", age:99})
             ON CREATE SET a.created = true
             ON MATCH SET a.seen = true"#,
    );
    assert_eq!(s.nodes_created, 0); // already existed → bound, not created
    let b = plane(&db).node_by_key("bob").unwrap().unwrap();
    assert_eq!(b.properties["age"].value, PropValue::Int(40)); // inline age:99 NOT reapplied
    assert!(b.properties.contains_key("seen")); // ON MATCH applied
    assert!(!b.properties.contains_key("created")); // ON CREATE skipped
}

#[test]
fn merge_is_idempotent() {
    let db = Database::in_memory().unwrap();
    assert_eq!(write(&db, r#"MERGE (a:Tag {key:"t"})"#).nodes_created, 1);
    assert_eq!(write(&db, r#"MERGE (a:Tag {key:"t"})"#).nodes_created, 0);
    assert_eq!(plane(&db).catalog().unwrap().node_count, 1);
}

#[test]
fn merge_requires_a_key() {
    let e = parse_statement(r#"MERGE (a:Person {name:"x"})"#).unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
    assert!(e.to_string().contains("key"), "{e}");
}

#[test]
fn merge_on_set_must_reference_the_merge_variable() {
    let e = parse_statement(r#"MERGE (a:Person {key:"k"}) ON CREATE SET b.x = 1"#).unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
}

#[test]
fn create_duplicate_key_errors_and_rolls_back() {
    let db = Database::in_memory().unwrap();
    // Two nodes claiming the same external key — the second create fails; the
    // whole statement's txn is never committed.
    let stmt = parse_statement(r#"CREATE (a {key:"x"}), (b {key:"x"})"#).unwrap();
    let err = match stmt {
        Statement::Write(w) => w.apply(&plane(&db)).unwrap_err(),
        Statement::Read(_) => panic!("expected write"),
    };
    assert!(!err.is_empty(), "duplicate key should error");
    assert!(plane(&db).node_by_key("x").unwrap().is_none()); // rolled back
}

#[test]
fn read_parse_rejects_a_write() {
    // The read-only `parse` refuses a write with a clear message.
    let e = parse("CREATE (n:Person)").unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
    assert!(e.to_string().contains("write statement"));
}
