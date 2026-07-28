//! Bulk-load equivalence + safety (arch/01 §2). The fast path must produce a
//! graph byte-for-byte equivalent to looping `create_node`/`create_edge` in
//! the same order — same ids, labels, edges — on both backends, and must still
//! reject dangling/duplicate keys.

use std::collections::HashMap;

use dr_strange_core::{BulkEdge, BulkNode, Database, Dir, PropDesc, PropValue, Properties};

const NODES: &[(&str, &[&str])] = &[
    ("a", &["Person"]),
    ("b", &["Person"]),
    ("c", &["Company"]),
    ("d", &["Person", "Admin"]),
    ("e", &["Topic"]),
];

const EDGES: &[(&str, &str, &str)] = &[
    ("a", "b", "KNOWS"),
    ("a", "c", "WORKS_AT"),
    ("b", "c", "WORKS_AT"),
    ("d", "a", "KNOWS"),
    ("d", "e", "ABOUT"),
];

fn props(name: &str) -> Properties {
    let mut p = Properties::new();
    p.insert(
        "name".into(),
        PropDesc {
            description: None,
            value: PropValue::Str(name.into()),
        },
    );
    p
}

fn build_incremental(db: &Database) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let mut ids = HashMap::new();
    for (k, labels) in NODES {
        ids.insert(*k, txn.create_node_with_key(k, labels, props(k)).unwrap());
    }
    for (s, d, ty) in EDGES {
        txn.create_edge(ids[s], ids[d], ty, Properties::new())
            .unwrap();
    }
    txn.commit().unwrap();
}

fn build_bulk(db: &Database) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let nodes: Vec<BulkNode> = NODES
        .iter()
        .map(|(k, labels)| BulkNode {
            external_key: Some(k),
            labels,
            props: props(k),
        })
        .collect();
    let edges: Vec<BulkEdge> = EDGES
        .iter()
        .map(|(s, d, ty)| BulkEdge {
            src_key: s,
            dst_key: d,
            ty,
            props: Properties::new(),
        })
        .collect();
    let stats = txn.bulk_load(nodes, edges).unwrap();
    assert_eq!(stats.nodes, NODES.len() as u64);
    assert_eq!(stats.edges, EDGES.len() as u64);
    txn.commit().unwrap();
}

type Dump = (
    Vec<(u64, Vec<String>, Option<String>)>,
    Vec<(u64, u64, String)>,
);

fn dump(db: &Database) -> Dump {
    let plane = db.plane("startup").unwrap();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for id in plane.query().scan_all().ids().unwrap() {
        let n = plane.node(id).unwrap().unwrap();
        nodes.push((id.0, n.labels.clone(), n.external_key.clone()));
        for hop in plane.neighbors(id, Dir::Out, None).unwrap() {
            let e = plane.edge(hop.edge).unwrap().unwrap();
            edges.push((e.src.0, e.dst.0, e.ty.clone()));
        }
    }
    nodes.sort();
    edges.sort();
    (nodes, edges)
}

#[test]
fn bulk_matches_incremental_memory() {
    let inc = Database::in_memory().unwrap();
    build_incremental(&inc);
    let bulk = Database::in_memory().unwrap();
    build_bulk(&bulk);
    assert_eq!(dump(&inc), dump(&bulk));
}

#[test]
fn bulk_matches_incremental_redb() {
    let dir = tempfile::tempdir().unwrap();
    let inc = Database::open(dir.path().join("inc.drsg")).unwrap();
    build_incremental(&inc);
    let bulk = Database::open(dir.path().join("bulk.drsg")).unwrap();
    build_bulk(&bulk);
    assert_eq!(dump(&inc), dump(&bulk));
}

#[test]
fn bulk_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.drsg");
    build_bulk(&Database::open(&path).unwrap());
    // Reopen: adjacency/records must be intact from the committed batch.
    let db = Database::open(&path).unwrap();
    let cat = db.plane("startup").unwrap().catalog().unwrap();
    assert_eq!(cat.node_count, NODES.len() as u64);
    assert_eq!(cat.edge_count, EDGES.len() as u64);
}

#[test]
fn bulk_rejects_dangling_edge_endpoint() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let nodes = vec![BulkNode {
        external_key: Some("a"),
        labels: &["Person"],
        props: Properties::new(),
    }];
    // "ghost" is neither in the batch nor pre-existing → rejected.
    let edges = vec![BulkEdge {
        src_key: "a",
        dst_key: "ghost",
        ty: "KNOWS",
        props: Properties::new(),
    }];
    assert!(txn.bulk_load(nodes, edges).is_err());
}

#[test]
fn bulk_rejects_duplicate_key_in_batch() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let nodes = vec![
        BulkNode {
            external_key: Some("dup"),
            labels: &["Person"],
            props: Properties::new(),
        },
        BulkNode {
            external_key: Some("dup"),
            labels: &["Person"],
            props: Properties::new(),
        },
    ];
    assert!(txn.bulk_load(nodes, vec![]).is_err());
}
