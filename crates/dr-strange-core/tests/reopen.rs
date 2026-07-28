//! Durability / crash-recovery, part 2: committed state survives closing and
//! reopening the redb file (arch/01 §7). redb guarantees the file is
//! consistent to the last successful commit; these tests assert that *our*
//! layers — records, external keys, the vector index (rebuilt from the KV),
//! the catalog, plane properties — all come back intact across a reopen.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use dr_strange_core::{Database, Dir, Metric, NodeId, PropDesc, PropValue, Properties};
use proptest::prelude::*;

fn emb(v: Vec<f32>) -> Properties {
    [("emb".to_string(), PropDesc::new(PropValue::Vector(v)))]
        .into_iter()
        .collect()
}

#[test]
fn everything_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.drsg");

    // -- build: keyed docs with embeddings, a citation chain, a declared
    //    index, and a plane property.
    let (d0, d1, d2);
    {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        plane
            .set_properties(
                [(
                    "run".to_string(),
                    PropDesc::new(PropValue::Str("r1".into())),
                )]
                .into(),
            )
            .unwrap();
        let mut txn = plane.write().unwrap();
        d0 = txn
            .create_node_with_key("d0", &["Doc"], emb(vec![0.0, 0.0]))
            .unwrap();
        d1 = txn
            .create_node_with_key("d1", &["Doc"], emb(vec![1.0, 0.0]))
            .unwrap();
        d2 = txn
            .create_node_with_key("d2", &["Doc"], emb(vec![2.0, 0.0]))
            .unwrap();
        txn.create_edge(d0, d1, "CITES", Properties::new()).unwrap();
        txn.create_edge(d1, d2, "CITES", Properties::new()).unwrap();
        txn.commit().unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    } // db dropped → file closed

    // -- reopen and verify everything.
    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();

    // plane property
    assert_eq!(
        plane.properties().unwrap().get("run").map(|p| &p.value),
        Some(&PropValue::Str("r1".into()))
    );

    // records + external keys + labels + properties
    let n = plane.node_by_key("d1").unwrap().unwrap();
    assert_eq!(n.id, d1);
    assert_eq!(n.labels, vec!["Doc".to_string()]);
    assert_eq!(
        n.properties.get("emb").map(|p| &p.value),
        Some(&PropValue::Vector(vec![1.0, 0.0]))
    );

    // adjacency
    let out = plane.neighbors(d0, Dir::Out, Some("CITES")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, d1);

    // vector index was rebuilt from the KV on open — search works immediately
    let near = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![0.0, 0.0], Metric::L2, 2)
        .ids()
        .unwrap();
    assert_eq!(near, vec![d0, d1]);

    // catalog
    let cat = plane.catalog().unwrap();
    assert_eq!(cat.node_count, 3);
    assert_eq!(cat.edge_count, 2);
    assert_eq!(cat.labels["Doc"].count, 3);
    let _ = d2;
}

#[test]
fn deletes_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("del.drsg");
    {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let a = txn
            .create_node_with_key("a", &["N"], Properties::new())
            .unwrap();
        txn.create_node_with_key("b", &["N"], Properties::new())
            .unwrap();
        txn.delete_node(a).unwrap();
        txn.commit().unwrap();
    }
    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    // the delete persisted: a is gone (and its key freed), b remains
    assert!(plane.node_by_key("a").unwrap().is_none());
    assert!(plane.node_by_key("b").unwrap().is_some());
    assert_eq!(plane.catalog().unwrap().node_count, 1);
}

// ---- proptest: random batches with reopens between them ------------------

#[derive(Clone, Debug)]
enum Op {
    Create(u8), // create a keyed node (key = "k{n}") if not already alive
    Delete(u8), // delete the keyed node if alive
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0u8..8).prop_map(Op::Create),
        1 => (0u8..8).prop_map(Op::Delete),
    ]
}

/// Applies a batch to the db (one plane, one transaction), keeping the model
/// (`alive` = set of live keys) in step.
fn apply_batch(db: &Database, alive: &mut BTreeSet<u8>, batch: &[Op]) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    // Nodes created earlier in *this* transaction: a fresh node_by_key read
    // can't see them until commit, so track ids here (mirrors CLI import).
    let mut this_batch: BTreeMap<u8, NodeId> = BTreeMap::new();
    for op in batch {
        match op {
            Op::Create(k) if !alive.contains(k) => {
                let id = txn
                    .create_node_with_key(&format!("k{k}"), &["N"], Properties::new())
                    .unwrap();
                this_batch.insert(*k, id);
                alive.insert(*k);
            }
            Op::Delete(k) if alive.contains(k) => {
                let id = this_batch
                    .get(k)
                    .copied()
                    .unwrap_or_else(|| plane.node_by_key(&format!("k{k}")).unwrap().unwrap().id);
                txn.delete_node(id).unwrap();
                this_batch.remove(k);
                alive.remove(k);
            }
            _ => {}
        }
    }
    txn.commit().unwrap();
}

fn all_keys(db: &Database) -> BTreeMap<String, u64> {
    let plane = db.plane("startup").unwrap();
    let mut out = BTreeMap::new();
    for node in plane.query().scan_all().nodes().unwrap() {
        if let Some(key) = node.external_key {
            out.insert(key, node.id.0);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 20, ..ProptestConfig::default() })]

    /// Apply random batches of create/delete, reopening the redb file between
    /// each. After the final reopen, the live-key set must match the model.
    #[test]
    fn model_survives_reopens_between_batches(
        batches in prop::collection::vec(
            prop::collection::vec(op_strategy(), 0..6),
            1..6,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path: &Path = &dir.path().join("pbt.drsg");
        let mut alive: BTreeSet<u8> = BTreeSet::new();

        for batch in &batches {
            // A fresh open per batch — simulating close/crash/reopen cycles.
            let db = Database::open(path).unwrap();
            apply_batch(&db, &mut alive, batch);
            // still consistent immediately after commit, before closing
            prop_assert_eq!(all_keys(&db).len(), alive.len());
        }

        // Final reopen: committed state matches the model exactly.
        let db = Database::open(path).unwrap();
        let keys = all_keys(&db);
        let expected: BTreeSet<String> = alive.iter().map(|k| format!("k{k}")).collect();
        prop_assert_eq!(keys.keys().cloned().collect::<BTreeSet<_>>(), expected);
    }
}
