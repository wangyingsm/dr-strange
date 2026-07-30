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

#[test]
fn read_parse_rejects_a_write() {
    // The read-only `parse` refuses a write with a clear message.
    let e = parse("CREATE (n:Person)").unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
    assert!(e.to_string().contains("write statement"));
}
