//! Digest pipeline against the deterministic mock provider (arch/07) — the
//! whole chunk→extract→embed→assemble→apply path, offline.

use dr_strange_core::{Database, Dir, PropValue};
use dr_strange_llm::{DigestOptions, MockProvider, digest};

const REPLY: &str = r#"{
  "entities": [
    {"key":"alice","label":"Person","properties":{"role":"engineer"},"description":"An engineer."},
    {"key":"acme","label":"Company","properties":{},"description":"A robotics company."}
  ],
  "relations": [
    {"src":"alice","dst":"acme","type":"WORKS_AT","description":"Alice works at Acme."},
    {"src":"alice","dst":"ghost","type":"KNOWS"}
  ]
}"#;

fn opts(embed: bool) -> DigestOptions {
    DigestOptions {
        source: "doc.txt".into(),
        model: "mock-1".into(),
        run_id: "run-42".into(),
        chunk_chars: 4000,
        embed,
    }
}

#[test]
fn digests_entities_relations_provenance_and_embeddings() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest("Alice is an engineer at Acme.", &mock, &mock, &opts(true)).unwrap();

    // Two entities; the WORKS_AT relation kept, the dangling KNOWS dropped.
    assert_eq!(result.report.entities, 2);
    assert_eq!(result.report.relations, 1);
    assert_eq!(result.report.dropped_relations, 1);
    assert_eq!(result.report.chat_requests, 1);

    // Nodes carry label, extracted props, a summary, provenance, and a vector.
    let alice = result.nodes.iter().find(|n| n.key == "alice").unwrap();
    assert_eq!(alice.label, "Person");
    assert!(matches!(&alice.props["role"].value, PropValue::Str(s) if s == "engineer"));
    assert!(alice.props.contains_key("summary"));
    assert!(matches!(&alice.props["embedding"].value, PropValue::Vector(v) if v.len() == 8));
    for key in ["_source", "_model", "_run"] {
        assert!(alice.props.contains_key(key), "missing provenance {key}");
    }
    assert!(matches!(&alice.props["_run"].value, PropValue::Str(s) if s == "run-42"));

    // The kept edge references entity keys.
    let e = &result.edges[0];
    assert_eq!(
        (e.src.as_str(), e.ty.as_str(), e.dst.as_str()),
        ("alice", "WORKS_AT", "acme")
    );
}

#[test]
fn apply_writes_the_graph() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest("Alice at Acme.", &mock, &mock, &opts(false)).unwrap();

    let db = Database::in_memory().unwrap();
    {
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let stats = result.apply(&mut txn).unwrap();
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.edges, 1);
        txn.commit().unwrap();
    }

    let plane = db.plane("startup").unwrap();
    let alice = plane.node_by_key("alice").unwrap().unwrap();
    let acme = plane.node_by_key("acme").unwrap().unwrap();
    let hops = plane.neighbors(alice.id, Dir::Out, None).unwrap();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].node, acme.id);
}

#[test]
fn no_embed_leaves_no_vectors() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest("Alice at Acme.", &mock, &mock, &opts(false)).unwrap();
    assert!(
        result
            .nodes
            .iter()
            .all(|n| !n.props.contains_key("embedding"))
    );
}
