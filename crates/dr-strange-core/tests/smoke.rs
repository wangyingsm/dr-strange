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
fn database_and_records_are_send_and_sync() {
    // arch/04 §6: Database is Send + Sync (wrappers thread it freely).
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Database>();
    assert_send_sync::<dr_strange_core::NodeRecord>();
    assert_send_sync::<dr_strange_core::Error>();
}

fn stable_reads_while_writer_open(db: &Database) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let committed_before = txn.create_node(&["Seen"], Properties::new()).unwrap();
    drop(txn);
    let mut txn = plane.write().unwrap();
    let committed = txn.create_node(&["Seen"], Properties::new()).unwrap();
    txn.commit().unwrap();
    assert_eq!(committed_before, committed, "abort rolled back the counter");

    // While a write transaction is open with uncommitted changes, reads see
    // only committed state.
    let mut txn = plane.write().unwrap();
    let uncommitted = txn.create_node(&["Unseen"], Properties::new()).unwrap();
    assert!(plane.node(committed).unwrap().is_some());
    assert!(
        plane.node(uncommitted).unwrap().is_none(),
        "snapshot must not see uncommitted writes"
    );
    txn.commit().unwrap();
    assert!(plane.node(uncommitted).unwrap().is_some());
}

#[test]
fn stable_reads_memory() {
    let db = Database::in_memory().unwrap();
    stable_reads_while_writer_open(&db);
}

#[test]
fn stable_reads_redb() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("stable.drsg")).unwrap();
    stable_reads_while_writer_open(&db);
}

#[test]
fn new_plane_supports_full_slice_and_carries_props() {
    let db = Database::in_memory().unwrap();
    let plane = db
        .create_plane(
            "paper-2406.01234",
            [(
                "source".to_string(),
                PropDesc::described(
                    "arxiv id this plane was digested from",
                    PropValue::Str("2406.01234".into()),
                ),
            )]
            .into(),
        )
        .unwrap();

    let mut txn = plane.write().unwrap();
    let a = txn.create_node(&["Chunk"], Properties::new()).unwrap();
    let b = txn.create_node(&["Chunk"], Properties::new()).unwrap();
    txn.create_edge(a, b, "NEXT", Properties::new()).unwrap();
    txn.commit().unwrap();

    let out = plane.neighbors(a, Dir::Out, Some("NEXT")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, b);

    // same handle via lookup
    let again = db.plane("paper-2406.01234").unwrap();
    assert_eq!(again.id(), plane.id());
}

#[test]
fn plane_name_properties_and_rename_via_api() {
    let db = Database::in_memory().unwrap();
    let plane = db
        .create_plane(
            "run-1",
            [(
                "model".to_string(),
                PropDesc::new(PropValue::Str("gpt".into())),
            )]
            .into(),
        )
        .unwrap();

    assert_eq!(plane.name().unwrap(), "run-1");
    assert_eq!(
        plane.properties().unwrap().get("model").map(|p| &p.value),
        Some(&PropValue::Str("gpt".into()))
    );

    // replace properties
    plane
        .set_properties(
            [(
                "status".to_string(),
                PropDesc::new(PropValue::Str("done".into())),
            )]
            .into(),
        )
        .unwrap();
    let props = plane.properties().unwrap();
    assert!(props.contains_key("status") && !props.contains_key("model"));

    // rename: handle id stays valid, new name resolves
    plane.rename("run-1-final").unwrap();
    assert_eq!(plane.name().unwrap(), "run-1-final");
    assert_eq!(db.plane("run-1-final").unwrap().id(), plane.id());
    assert!(matches!(db.plane("run-1").unwrap_err(), Error::NotFound(_)));

    // startup can't be renamed
    let startup = db.plane("startup").unwrap();
    assert!(matches!(
        startup.rename("nope").unwrap_err(),
        Error::InvalidArgument(_)
    ));
}

#[test]
fn handles_have_useful_debug_output() {
    let db = Database::in_memory().unwrap();
    assert!(format!("{db:?}").contains("memory"));
    let dir = tempfile::tempdir().unwrap();
    let file_db = Database::open(dir.path().join("dbg.drsg")).unwrap();
    let dbg = format!("{file_db:?}");
    // The on-disk backend depends on the active feature (redb or native).
    assert!(dbg.contains("redb") || dbg.contains("native"), "got: {dbg}");
    let plane = db.plane("startup").unwrap();
    assert!(format!("{plane:?}").contains("PlaneHandle"));
}

#[test]
fn io_errors_convert_and_render() {
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
    let err: Error = io.into();
    assert!(matches!(err, Error::Io(_)));
    assert!(err.to_string().contains("nope"));
}

#[test]
fn errors_render_readable_messages() {
    let db = Database::in_memory().unwrap();
    let msg = db.plane("ghost").unwrap_err().to_string();
    assert!(msg.contains("ghost"), "got: {msg}");

    db.create_plane("dup", Properties::new()).unwrap();
    let msg = db
        .create_plane("dup", Properties::new())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("dup"), "got: {msg}");

    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let n = txn.create_node(&[], Properties::new()).unwrap();
    let msg = txn
        .create_edge(n, dr_strange_core::NodeId(4242), "X", Properties::new())
        .unwrap_err()
        .to_string();
    assert!(msg.contains("4242"), "got: {msg}");
}

// ---- M1: deletes, external keys, property mutation, batched ids ----------

fn m1_slice(db: &Database) {
    let plane = db.plane("startup").unwrap();

    // external keys
    let mut txn = plane.write().unwrap();
    let alice = txn
        .create_node_with_key(
            "person:alice",
            &["Person"],
            props(&[("name", PropValue::Str("Alice".into()))]),
        )
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(
        plane.node_by_key("person:alice").unwrap().map(|r| r.id),
        Some(alice)
    );
    assert!(plane.node_by_key("nobody").unwrap().is_none());

    let mut txn = plane.write().unwrap();
    let err = txn
        .create_node_with_key("person:alice", &["Person"], Properties::new())
        .unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "got: {err:?}");
    drop(txn);

    // property mutation
    let mut txn = plane.write().unwrap();
    txn.set_prop(
        alice,
        "affiliation",
        PropDesc::described("current employer", PropValue::Str("MIT".into())),
    )
    .unwrap();
    txn.commit().unwrap();
    let rec = plane.node(alice).unwrap().unwrap();
    assert_eq!(
        rec.properties.get("affiliation").map(|p| &p.value),
        Some(&PropValue::Str("MIT".into()))
    );

    let mut txn = plane.write().unwrap();
    txn.remove_prop(alice, "affiliation").unwrap();
    txn.commit().unwrap();
    assert!(
        !plane
            .node(alice)
            .unwrap()
            .unwrap()
            .properties
            .contains_key("affiliation")
    );

    // edge + edge property mutation
    let mut txn = plane.write().unwrap();
    let bob = txn.create_node(&["Person"], Properties::new()).unwrap();
    let knows = txn
        .create_edge(alice, bob, "KNOWS", Properties::new())
        .unwrap();
    txn.set_edge_prop(knows, "since", PropDesc::new(PropValue::Int(2020)))
        .unwrap();
    txn.commit().unwrap();
    let e = plane.edge(knows).unwrap().unwrap();
    assert_eq!(e.src, alice);
    assert_eq!(e.dst, bob);
    assert_eq!(
        e.properties.get("since").map(|p| &p.value),
        Some(&PropValue::Int(2020))
    );

    let mut txn = plane.write().unwrap();
    txn.remove_edge_prop(knows, "since").unwrap();
    txn.commit().unwrap();
    assert!(plane.edge(knows).unwrap().unwrap().properties.is_empty());

    // delete_edge
    let mut txn = plane.write().unwrap();
    txn.delete_edge(knows).unwrap();
    txn.commit().unwrap();
    assert!(plane.edge(knows).unwrap().is_none());
    assert!(
        plane
            .neighbors(alice, Dir::Out, Some("KNOWS"))
            .unwrap()
            .is_empty()
    );
    // both endpoints survive
    assert!(plane.node(alice).unwrap().is_some());
    assert!(plane.node(bob).unwrap().is_some());

    // delete_node cascades
    let mut txn = plane.write().unwrap();
    let carol = txn.create_node(&["Person"], Properties::new()).unwrap();
    txn.create_edge(carol, bob, "KNOWS", Properties::new())
        .unwrap();
    txn.commit().unwrap();
    let mut txn = plane.write().unwrap();
    txn.delete_node(carol).unwrap();
    txn.commit().unwrap();
    assert!(plane.node(carol).unwrap().is_none());
    assert!(
        plane
            .neighbors(bob, Dir::In, Some("KNOWS"))
            .unwrap()
            .is_empty()
    );

    // deleting an already-gone node/edge is not an error
    let mut txn = plane.write().unwrap();
    txn.delete_node(carol).unwrap();
    txn.delete_edge(knows).unwrap();
    txn.commit().unwrap();

    // batched ids: creating many nodes across ID_BATCH_SIZE (64) boundaries
    // still yields distinct, usable ids
    let mut txn = plane.write().unwrap();
    let mut created = Vec::new();
    for _ in 0..130 {
        created.push(txn.create_node(&[], Properties::new()).unwrap());
    }
    txn.commit().unwrap();
    let unique: std::collections::BTreeSet<_> = created.iter().collect();
    assert_eq!(unique.len(), created.len());
    for id in &created {
        assert!(plane.node(*id).unwrap().is_some());
    }
}

#[test]
fn m1_slice_memory() {
    let db = Database::in_memory().unwrap();
    m1_slice(&db);
}

#[test]
fn m1_slice_redb() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("m1.drsg")).unwrap();
    m1_slice(&db);
}

#[test]
fn drop_plane_via_database_wipes_the_plane() {
    let db = Database::in_memory().unwrap();
    let plane = db.create_plane("scratch", Properties::new()).unwrap();
    let mut txn = plane.write().unwrap();
    let n = txn.create_node(&["L"], Properties::new()).unwrap();
    txn.commit().unwrap();

    db.drop_plane(plane.id()).unwrap();

    assert!(matches!(
        db.plane("scratch").unwrap_err(),
        Error::NotFound(_)
    ));
    assert!(
        plane.node(n).unwrap().is_none(),
        "stale handle reads see nothing, not stale data"
    );

    // startup can't be dropped
    let startup = db.plane("startup").unwrap();
    assert!(matches!(
        db.drop_plane(startup.id()).unwrap_err(),
        Error::InvalidArgument(_)
    ));

    // dropping an id that was never a plane is a harmless no-op
    db.drop_plane(PlaneId(999_999)).unwrap();
}

#[test]
fn set_prop_on_missing_node_is_not_found() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let err = txn
        .set_prop(
            dr_strange_core::NodeId(99999),
            "k",
            PropDesc::new(PropValue::Null),
        )
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
}

// redb-specific: detecting a foreign redb file as corrupt is a redb behavior;
// the native backend's on-disk form is a directory, so this doesn't apply.
#[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
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

#[cfg(all(feature = "redb-backend", not(feature = "native-backend")))]
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
