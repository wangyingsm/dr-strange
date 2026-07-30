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

#[test]
fn read_parse_rejects_a_write() {
    // The read-only `parse` refuses a write with a clear message.
    let e = parse("CREATE (n:Person)").unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
    assert!(e.to_string().contains("write statement"));
}
