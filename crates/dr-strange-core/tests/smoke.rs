//! M0 vertical slice (arch/00 §5): create node → get node → 1-hop expand,
//! through the public API, on both backends, with redb persistence.

use dr_strange_core::{Database, Dir, Error, PlaneId, PropDesc, PropValue, Properties};

fn props(entries: &[(&str, PropValue)]) -> Properties {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
        .collect()
}

fn vertical_slice(db: &Database) {
    let plane = db.plane("startup").expect("startup plane always exists");
    assert_eq!(plane.id(), PlaneId::STARTUP);

    // -- write: two nodes and an edge
    let mut txn = plane.write().unwrap();
    let alice = txn
        .create_node(
            &["Person", "Author"],
            props(&[
                ("name", PropValue::Str("Alice".into())),
                ("embedding", PropValue::Vector(vec![0.1, 0.2, 0.3])),
            ]),
        )
        .unwrap();
    let paper = txn
        .create_node(&["Paper"], props(&[("year", PropValue::Int(2026))]))
        .unwrap();
    let authored = txn
        .create_edge(alice, paper, "AUTHORED", Properties::new())
        .unwrap();
    txn.commit().unwrap();

    // -- read back: labels, properties, descriptions
    let rec = plane.node(alice).unwrap().expect("alice exists");
    assert_eq!(rec.labels, vec!["Person".to_string(), "Author".to_string()]);
    assert_eq!(
        rec.properties.get("name").map(|p| &p.value),
        Some(&PropValue::Str("Alice".into()))
    );
    assert_eq!(
        rec.properties.get("embedding").map(|p| &p.value),
        Some(&PropValue::Vector(vec![0.1, 0.2, 0.3]))
    );

    // -- 1-hop expansion, all four ways
    let out = plane.neighbors(alice, Dir::Out, None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, paper);
    assert_eq!(out[0].edge, authored);

    let out_typed = plane.neighbors(alice, Dir::Out, Some("AUTHORED")).unwrap();
    assert_eq!(out_typed, out);

    let inbound = plane.neighbors(paper, Dir::In, None).unwrap();
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].node, alice);

    // unknown edge type is empty, not an error (soft schema)
    assert!(
        plane
            .neighbors(alice, Dir::Out, Some("NEVER_SEEN"))
            .unwrap()
            .is_empty()
    );

    // -- uncommitted writes vanish
    {
        let mut txn = plane.write().unwrap();
        txn.create_node(&["Ghost"], Properties::new()).unwrap();
        // dropped
    }

    // -- planes are separate rooms
    let side = db.create_plane("scratch", Properties::new()).unwrap();
    assert!(
        side.node(alice).unwrap().is_none(),
        "alice is not in scratch"
    );
    let mut txn = side.write().unwrap();
    let bob = txn.create_node(&["Person"], Properties::new()).unwrap();
    // cross-plane edge rejected: alice lives in startup, not scratch
    let err = txn
        .create_edge(bob, alice, "KNOWS", Properties::new())
        .unwrap_err();
    assert!(matches!(err, Error::PlaneMismatch(_)), "got: {err:?}");
    drop(txn);

    assert!(matches!(db.plane("nope").unwrap_err(), Error::NotFound(_)));
    assert!(matches!(
        db.create_plane("scratch", Properties::new()).unwrap_err(),
        Error::PlaneExists(_)
    ));
}

#[test]
fn m0_slice_memory() {
    let db = Database::in_memory().unwrap();
    vertical_slice(&db);
}

#[test]
fn m0_slice_redb() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("smoke.drsg")).unwrap();
    vertical_slice(&db);
}

#[test]
fn m0_redb_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.drsg");

    let (alice, paper) = {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let alice = txn
            .create_node(
                &["Person"],
                props(&[("name", PropValue::Str("Alice".into()))]),
            )
            .unwrap();
        let paper = txn.create_node(&["Paper"], Properties::new()).unwrap();
        txn.create_edge(alice, paper, "AUTHORED", Properties::new())
            .unwrap();
        txn.commit().unwrap();
        (alice, paper)
    };

    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    let rec = plane.node(alice).unwrap().expect("persisted");
    assert_eq!(rec.labels, vec!["Person".to_string()]);
    let out = plane.neighbors(alice, Dir::Out, Some("AUTHORED")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, paper);
}

#[test]
fn opening_a_non_drsg_file_fails_with_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.redb");
    {
        // a valid redb file that is NOT a dr-strange database
        let eng = redb_smoke::make_foreign_redb(&path);
        drop(eng);
    }
    let err = Database::open(&path).unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "got: {err:?}");
}

mod redb_smoke {
    use std::path::Path;

    pub fn make_foreign_redb(path: &Path) -> redb::Database {
        let db = redb::Database::create(path).unwrap();
        let t = redb::TableDefinition::<&[u8], &[u8]>::new("meta");
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn.open_table(t).unwrap();
            table.insert(&b"magic"[..], &b"NOPE"[..]).unwrap();
        }
        txn.commit().unwrap();
        db
    }
}
