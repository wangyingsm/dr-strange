//! Storage-layer graph tests (arch/01). One `MemoryEngine`-backed harness
//! (`with_db`) exercises the re-exported API from `meta`/`node`/`edge`; a
//! couple of tests reach into `meta`'s raw counter helpers to corrupt or
//! inspect the KV directly.

use super::meta::{ID_BATCH_SIZE, get_u64, put_u64};
use super::*;
use crate::error::{Error, Result};
use crate::storage::engine::{
    ReadTransaction, StorageEngine, TableId, WriteTransaction, prefix_successor,
};
use crate::storage::memory::MemoryEngine;
use crate::storage::{codec, keys};
use crate::types::{Dir, EdgeId, NodeId, PlaneId, PropDesc, PropValue, Properties};

/// Runs `f` in an initialized write transaction and commits.
fn with_db<T>(f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T>) -> T {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let out = f(&mut txn).unwrap();
    txn.commit().unwrap();
    out
}

#[test]
fn init_is_idempotent() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    init(&mut txn).unwrap(); // second init on same data: verify, not clobber
    // the startup plane exists exactly once
    assert_eq!(
        plane_id_by_name(&txn, DEFAULT_PLANE_NAME).unwrap(),
        Some(PlaneId::STARTUP)
    );
}

#[test]
fn init_rejects_bad_magic() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    txn.put(TableId::Meta, keys::META_MAGIC, b"NOPE").unwrap();
    assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
}

/// A corrupted database must surface `Corrupt` errors, never panic.
#[test]
fn corrupted_meta_errors_cleanly() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();

    // garbage node-id counter (wrong width)
    txn.put(TableId::Meta, keys::META_NEXT_NODE_ID, b"xx")
        .unwrap();
    assert!(matches!(
        create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()),
        Err(Error::Corrupt(_))
    ));
    put_u64(&mut txn, keys::META_NEXT_NODE_ID, 1).unwrap();

    // missing counter
    txn.delete(TableId::Meta, keys::META_NEXT_EDGE_TYPE_ID)
        .unwrap();
    assert!(matches!(
        intern_edge_type(&mut txn, "T"),
        Err(Error::Corrupt(_))
    ));

    // garbage dictionary entry (wrong width)
    txn.put(TableId::Meta, &keys::dict_label_key("Bad"), b"toolong")
        .unwrap();
    assert!(matches!(
        intern_label(&mut txn, "Bad"),
        Err(Error::Corrupt(_))
    ));

    // reverse dictionary entry with invalid utf-8
    let id = intern_label(&mut txn, "Ok").unwrap();
    txn.put(TableId::Meta, &keys::dict_label_rev_key(id), &[0xFF, 0xFE])
        .unwrap();
    assert!(matches!(resolve_label(&txn, id), Err(Error::Corrupt(_))));

    // garbage plane-name entry (wrong width)
    txn.put(TableId::PlaneNames, &keys::plane_name_key("bad"), b"12345")
        .unwrap();
    assert!(matches!(
        plane_id_by_name(&txn, "bad"),
        Err(Error::Corrupt(_))
    ));

    // garbage node record body
    let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    txn.put(
        TableId::Nodes,
        &keys::node_key(PlaneId::STARTUP, n),
        &[0xFF; 3],
    )
    .unwrap();
    assert!(matches!(
        get_node(&txn, PlaneId::STARTUP, n),
        Err(Error::Corrupt(_))
    ));

    // node referencing a label id with no dictionary entry
    let m = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    txn.put(
        TableId::Nodes,
        &keys::node_key(PlaneId::STARTUP, m),
        &codec::encode_node_record(None, &[4040], &Properties::new()),
    )
    .unwrap();
    assert!(matches!(
        get_node(&txn, PlaneId::STARTUP, m),
        Err(Error::Corrupt(_))
    ));

    // malformed adjacency key (wrong length)
    txn.put(TableId::AdjFwd, b"short", b"").unwrap();
    let mut prefix_hit = keys::adj_prefix(PlaneId::STARTUP, NodeId(0)).to_vec();
    prefix_hit.clear(); // scan whole table via empty prefix
    let _ = prefix_hit;
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    // craft a bad entry under a's own prefix so neighbors() parses it
    let mut bad_key = keys::adj_prefix(PlaneId::STARTUP, a).to_vec();
    bad_key.push(0xAB);
    txn.put(TableId::AdjFwd, &bad_key, b"").unwrap();
    assert!(matches!(
        neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn init_rejects_future_format_version() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    txn.put(
        TableId::Meta,
        keys::META_FORMAT_VERSION,
        &(FORMAT_VERSION + 1).to_be_bytes(),
    )
    .unwrap();
    assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
}

#[test]
fn init_rejects_missing_version() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    txn.delete(TableId::Meta, keys::META_FORMAT_VERSION)
        .unwrap();
    assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
}

#[test]
fn interning_is_stable_and_distinct() {
    with_db(|txn| {
        let a1 = intern_label(txn, "Person")?;
        let a2 = intern_label(txn, "Person")?;
        let b = intern_label(txn, "Paper")?;
        assert_eq!(a1, a2, "same name → same id");
        assert_ne!(a1, b, "different names → different ids");
        assert_eq!(resolve_label(txn, a1)?, "Person");
        assert_eq!(resolve_label(txn, b)?, "Paper");

        // labels and edge types are separate dictionaries
        let e = intern_edge_type(txn, "Person")?;
        assert_eq!(lookup_edge_type(txn, "Person")?, Some(e));
        assert_eq!(lookup_edge_type(txn, "KNOWS")?, None);
        Ok(())
    });
}

#[test]
fn resolving_a_dangling_label_id_is_corrupt() {
    with_db(|txn| {
        assert!(matches!(resolve_label(txn, 999), Err(Error::Corrupt(_))));
        Ok(())
    });
}

#[test]
fn ids_are_sequential_within_and_across_transactions() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n1 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let n2 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    assert_eq!(n2.0, n1.0 + 1);
    txn.commit().unwrap();

    let mut txn = eng.begin_write().unwrap();
    let n3 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    assert_eq!(n3.0, n2.0 + 1);
    txn.commit().unwrap();
}

#[test]
fn aborted_transaction_ids_may_be_reused() {
    // Counter bumps roll back with the transaction: an id handed out by
    // an aborted txn was never committed, so reuse is safe. This test
    // documents that semantic.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    txn.commit().unwrap();

    let mut txn = eng.begin_write().unwrap();
    let ghost = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    drop(txn); // abort

    let mut txn = eng.begin_write().unwrap();
    let real = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    txn.commit().unwrap();
    assert_eq!(ghost, real);
}

#[test]
fn node_with_no_labels_and_no_props() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    assert!(rec.labels.is_empty());
    assert!(rec.properties.is_empty());
}

#[test]
fn duplicate_labels_are_preserved_as_given() {
    // Soft schema: storage does not deduplicate; documents behavior.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &["A", "A"], &Properties::new()).unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    assert_eq!(rec.labels, vec!["A".to_string(), "A".to_string()]);
}

#[test]
fn unicode_names_survive() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let plane = create_plane(&mut txn, "研究-λ", &Properties::new()).unwrap();
    let n = create_node(&mut txn, plane, &["实体", "Ünïcodé"], &Properties::new()).unwrap();
    assert_eq!(plane_id_by_name(&txn, "研究-λ").unwrap(), Some(plane));
    let rec = get_node(&txn, plane, n).unwrap().unwrap();
    assert_eq!(rec.labels, vec!["实体".to_string(), "Ünïcodé".to_string()]);
}

#[test]
fn parallel_edges_coexist() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e1 = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        b,
        "CITES",
        &Properties::new(),
    )
    .unwrap();
    let e2 = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        b,
        "CITES",
        &Properties::new(),
    )
    .unwrap();
    assert_ne!(e1, e2);
    let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("CITES")).unwrap();
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|n| n.node == b));
}

#[test]
fn typed_neighbors_filter_by_edge_type() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let c = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        b,
        "KNOWS",
        &Properties::new(),
    )
    .unwrap();
    create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        c,
        "CITES",
        &Properties::new(),
    )
    .unwrap();

    let knows = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("KNOWS")).unwrap();
    assert_eq!(knows.len(), 1);
    assert_eq!(knows[0].node, b);
    let all = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn self_loop_appears_in_both_directions() {
    // A self-loop writes one adj_fwd and one adj_rev entry, so Dir::Both
    // reports it twice (once per direction). Documents behavior.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    create_edge(&mut txn, PlaneId::STARTUP, a, a, "SELF", &Properties::new()).unwrap();
    assert_eq!(
        neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        neighbors(&txn, PlaneId::STARTUP, a, Dir::In, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        neighbors(&txn, PlaneId::STARTUP, a, Dir::Both, None)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn neighbors_of_unknown_node_is_empty() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let out = neighbors(&txn, PlaneId::STARTUP, NodeId(999), Dir::Both, None).unwrap();
    assert!(out.is_empty());
}

#[test]
fn edge_with_missing_endpoint_reports_which_side() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();

    let err = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        NodeId(999),
        "X",
        &Properties::new(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::PlaneMismatch(m) if m.contains("dst")),
        "got: {err}"
    );

    let err = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        NodeId(999),
        a,
        "X",
        &Properties::new(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::PlaneMismatch(m) if m.contains("src")),
        "got: {err}"
    );
}

#[test]
fn adjacency_is_isolated_per_node_and_per_plane() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();

    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

    let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
    let y = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
    create_edge(&mut txn, p2, x, y, "T", &Properties::new()).unwrap();

    // b has no out-edges; a's expansion does not leak plane 2's edges
    assert!(
        neighbors(&txn, PlaneId::STARTUP, b, Dir::Out, None)
            .unwrap()
            .is_empty()
    );
    let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, b);
    // and node ids are globally unique across planes
    assert_ne!(a, x);
    assert_ne!(b, y);
}

#[test]
fn plane_ids_are_distinct_and_names_unique() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p1 = create_plane(&mut txn, "p1", &Properties::new()).unwrap();
    let p2 = create_plane(&mut txn, "p2", &Properties::new()).unwrap();
    assert_ne!(p1, p2);
    assert_ne!(p1, PlaneId::STARTUP);
    assert!(matches!(
        create_plane(&mut txn, "p1", &Properties::new()),
        Err(Error::PlaneExists(_))
    ));
    assert_eq!(plane_id_by_name(&txn, "p1").unwrap(), Some(p1));
    assert_eq!(plane_id_by_name(&txn, "absent").unwrap(), None);
}

#[test]
fn get_node_in_wrong_plane_is_none() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();
    assert!(get_node(&txn, p2, n).unwrap().is_none());
    assert!(get_node(&txn, PlaneId::STARTUP, n).unwrap().is_some());
}

// ---- scan sources --------------------------------------------------

#[test]
fn scan_all_returns_plane_nodes_in_id_order() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &["A"], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &["B"], &Properties::new()).unwrap();
    let _x = create_node(&mut txn, p2, &["A"], &Properties::new()).unwrap();

    assert_eq!(scan_all(&txn, PlaneId::STARTUP).unwrap(), vec![a, b]);
    // other plane's nodes don't leak in
    assert_eq!(scan_all(&txn, p2).unwrap().len(), 1);
}

#[test]
fn scan_label_filters_by_label_and_plane() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
    let a1 = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
    let _p = create_node(&mut txn, PlaneId::STARTUP, &["Person"], &Properties::new()).unwrap();
    let a2 = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
    create_node(&mut txn, p2, &["Paper"], &Properties::new()).unwrap();

    assert_eq!(
        scan_label(&txn, PlaneId::STARTUP, "Paper").unwrap(),
        vec![a1, a2]
    );
    // a label the plane doesn't use here
    assert!(
        scan_label(&txn, PlaneId::STARTUP, "Org")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scan_label_unknown_label_is_empty_not_error() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    assert!(
        scan_label(&txn, PlaneId::STARTUP, "NeverInterned")
            .unwrap()
            .is_empty()
    );
    assert_eq!(lookup_label(&txn, "NeverInterned").unwrap(), None);
}

#[test]
fn scan_label_reflects_deletes() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &["Paper"], &Properties::new()).unwrap();
    delete_node(&mut txn, PlaneId::STARTUP, a).unwrap();
    assert_eq!(
        scan_label(&txn, PlaneId::STARTUP, "Paper").unwrap(),
        vec![b]
    );
}

// ---- external keys -----------------------------------------------

#[test]
fn external_key_roundtrips_and_is_stored_on_the_node() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node_with_key(
        &mut txn,
        PlaneId::STARTUP,
        "arxiv:2406.01234",
        &["Paper"],
        &Properties::new(),
    )
    .unwrap();

    assert_eq!(
        node_id_by_external_key(&txn, PlaneId::STARTUP, "arxiv:2406.01234").unwrap(),
        Some(n)
    );
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    assert_eq!(rec.external_key.as_deref(), Some("arxiv:2406.01234"));
    let by_key = get_node_by_external_key(&txn, PlaneId::STARTUP, "arxiv:2406.01234")
        .unwrap()
        .unwrap();
    assert_eq!(by_key.id, n);

    // plain create_node leaves it unset
    let plain = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    assert_eq!(
        get_node(&txn, PlaneId::STARTUP, plain)
            .unwrap()
            .unwrap()
            .external_key,
        None
    );
}

#[test]
fn duplicate_external_key_in_same_plane_is_conflict() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
    assert!(matches!(
        create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn same_external_key_allowed_in_different_planes() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
    let a = create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
    let b = create_node_with_key(&mut txn, p2, "k", &[], &Properties::new()).unwrap();
    assert_ne!(a, b);
    assert_eq!(
        node_id_by_external_key(&txn, PlaneId::STARTUP, "k").unwrap(),
        Some(a)
    );
    assert_eq!(node_id_by_external_key(&txn, p2, "k").unwrap(), Some(b));
}

#[test]
fn unknown_external_key_is_none_not_error() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    assert_eq!(
        node_id_by_external_key(&txn, PlaneId::STARTUP, "nope").unwrap(),
        None
    );
    assert!(
        get_node_by_external_key(&txn, PlaneId::STARTUP, "nope")
            .unwrap()
            .is_none()
    );
}

// ---- deletes -------------------------------------------------------

#[test]
fn delete_edge_removes_record_and_both_adjacency_entries() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

    delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap();

    assert!(get_edge(&txn, PlaneId::STARTUP, e).unwrap().is_none());
    assert!(
        neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        neighbors(&txn, PlaneId::STARTUP, b, Dir::In, None)
            .unwrap()
            .is_empty()
    );
    // both nodes are untouched
    assert!(get_node(&txn, PlaneId::STARTUP, a).unwrap().is_some());
    assert!(get_node(&txn, PlaneId::STARTUP, b).unwrap().is_some());
}

#[test]
fn delete_edge_is_idempotent() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    delete_edge(&mut txn, PlaneId::STARTUP, EdgeId(999)).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
    delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap();
    delete_edge(&mut txn, PlaneId::STARTUP, e).unwrap(); // second delete: still Ok
}

#[test]
fn get_edge_resolves_type_name_and_endpoints() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        a,
        b,
        "CITES",
        &Properties::new(),
    )
    .unwrap();
    let rec = get_edge(&txn, PlaneId::STARTUP, e).unwrap().unwrap();
    assert_eq!(rec.src, a);
    assert_eq!(rec.dst, b);
    assert_eq!(rec.ty, "CITES");
    assert!(
        get_edge(&txn, PlaneId::STARTUP, EdgeId(999))
            .unwrap()
            .is_none()
    );
}

#[test]
fn delete_node_cascades_to_incident_edges_both_directions() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    // x --T--> center --T--> y, plus a self-loop on center
    let center = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();
    let x = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let y = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e_in = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        x,
        center,
        "T",
        &Properties::new(),
    )
    .unwrap();
    let e_out = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        center,
        y,
        "T",
        &Properties::new(),
    )
    .unwrap();
    let e_self = create_edge(
        &mut txn,
        PlaneId::STARTUP,
        center,
        center,
        "T",
        &Properties::new(),
    )
    .unwrap();

    delete_node(&mut txn, PlaneId::STARTUP, center).unwrap();

    assert!(get_node(&txn, PlaneId::STARTUP, center).unwrap().is_none());
    for e in [e_in, e_out, e_self] {
        assert!(
            get_edge(&txn, PlaneId::STARTUP, e).unwrap().is_none(),
            "edge {e:?} should have been cascade-deleted"
        );
    }
    // x and y survive, and now have no dangling adjacency to `center`
    assert!(get_node(&txn, PlaneId::STARTUP, x).unwrap().is_some());
    assert!(get_node(&txn, PlaneId::STARTUP, y).unwrap().is_some());
    assert!(
        neighbors(&txn, PlaneId::STARTUP, x, Dir::Out, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        neighbors(&txn, PlaneId::STARTUP, y, Dir::In, None)
            .unwrap()
            .is_empty()
    );

    // label_idx entry is gone: scanning label "L" finds no nodes
    let lid = intern_label(&mut txn, "L").unwrap();
    let prefix = keys::label_idx_key(PlaneId::STARTUP, lid, NodeId(0));
    let scan_prefix = &prefix[..8]; // plane · label, dropping the node-id suffix
    let end = prefix_successor(scan_prefix);
    assert_eq!(
        txn.range(TableId::LabelIdx, scan_prefix, end.as_deref())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn delete_node_removes_external_key_and_node_plane_entries() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
    delete_node(&mut txn, PlaneId::STARTUP, n).unwrap();
    assert_eq!(
        node_id_by_external_key(&txn, PlaneId::STARTUP, "k").unwrap(),
        None
    );
    // the key is free again
    let n2 =
        create_node_with_key(&mut txn, PlaneId::STARTUP, "k", &[], &Properties::new()).unwrap();
    assert_ne!(n, n2);
}

#[test]
fn delete_node_is_idempotent() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    delete_node(&mut txn, PlaneId::STARTUP, NodeId(999)).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    delete_node(&mut txn, PlaneId::STARTUP, n).unwrap();
    delete_node(&mut txn, PlaneId::STARTUP, n).unwrap(); // second delete: still Ok
}

#[test]
fn deleting_node_does_not_affect_other_planes_or_nodes() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
    let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();

    delete_node(&mut txn, PlaneId::STARTUP, a).unwrap();

    assert!(get_node(&txn, PlaneId::STARTUP, b).unwrap().is_some());
    assert!(get_node(&txn, p2, x).unwrap().is_some());
}

// ---- drop_plane ------------------------------------------------------

#[test]
fn drop_plane_wipes_everything_and_frees_the_name() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
    let a = create_node_with_key(&mut txn, p, "k", &["L"], &Properties::new()).unwrap();
    let b = create_node(&mut txn, p, &[], &Properties::new()).unwrap();
    let e = create_edge(&mut txn, p, a, b, "T", &Properties::new()).unwrap();

    drop_plane(&mut txn, p).unwrap();

    assert_eq!(plane_id_by_name(&txn, "scratch").unwrap(), None);
    assert!(get_node(&txn, p, a).unwrap().is_none());
    assert!(get_node(&txn, p, b).unwrap().is_none());
    assert!(get_edge(&txn, p, e).unwrap().is_none());
    assert_eq!(node_id_by_external_key(&txn, p, "k").unwrap(), None);

    // the name is free again, and reuse gets a fresh plane id
    let p2 = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
    assert_ne!(p, p2);
}

#[test]
fn drop_plane_does_not_touch_other_planes() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p1 = create_plane(&mut txn, "p1", &Properties::new()).unwrap();
    let p2 = create_plane(&mut txn, "p2", &Properties::new()).unwrap();
    let a = create_node(&mut txn, p1, &[], &Properties::new()).unwrap();
    let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
    let startup_node = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();

    drop_plane(&mut txn, p1).unwrap();

    assert!(get_node(&txn, p1, a).unwrap().is_none());
    assert!(get_node(&txn, p2, x).unwrap().is_some());
    assert!(
        get_node(&txn, PlaneId::STARTUP, startup_node)
            .unwrap()
            .is_some()
    );
    assert_eq!(plane_id_by_name(&txn, "p2").unwrap(), Some(p2));
}

#[test]
fn drop_plane_rejects_startup() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    assert!(matches!(
        drop_plane(&mut txn, PlaneId::STARTUP),
        Err(Error::InvalidArgument(_))
    ));
    // still there
    assert_eq!(
        plane_id_by_name(&txn, DEFAULT_PLANE_NAME).unwrap(),
        Some(PlaneId::STARTUP)
    );
}

#[test]
fn plane_properties_and_rename() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();

    let mut props = Properties::new();
    props.insert(
        "source".into(),
        PropDesc::described("where this came from", PropValue::Str("arxiv".into())),
    );
    let p = create_plane(&mut txn, "paper-1", &props).unwrap();

    // read back name + props
    let (name, read_props) = read_plane(&txn, p).unwrap().unwrap();
    assert_eq!(name, "paper-1");
    assert_eq!(read_props, props);

    // replace properties
    let mut props2 = Properties::new();
    props2.insert(
        "status".into(),
        PropDesc::new(PropValue::Str("merged".into())),
    );
    set_plane_properties(&mut txn, p, &props2).unwrap();
    let (_, after) = read_plane(&txn, p).unwrap().unwrap();
    assert_eq!(after, props2);

    // rename: name lookup moves, id + props stay
    rename_plane(&mut txn, p, "paper-1-final").unwrap();
    assert_eq!(plane_id_by_name(&txn, "paper-1").unwrap(), None);
    assert_eq!(plane_id_by_name(&txn, "paper-1-final").unwrap(), Some(p));
    let (renamed, still) = read_plane(&txn, p).unwrap().unwrap();
    assert_eq!(renamed, "paper-1-final");
    assert_eq!(still, props2);

    // rename to same name is a no-op
    rename_plane(&mut txn, p, "paper-1-final").unwrap();

    // errors: taken name, absent plane, startup, and props on absent plane
    create_plane(&mut txn, "taken", &Properties::new()).unwrap();
    assert!(matches!(
        rename_plane(&mut txn, p, "taken"),
        Err(Error::PlaneExists(_))
    ));
    assert!(matches!(
        rename_plane(&mut txn, PlaneId(999), "x"),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        rename_plane(&mut txn, PlaneId::STARTUP, "x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        set_plane_properties(&mut txn, PlaneId(999), &Properties::new()),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn drop_plane_is_idempotent_for_absent_plane() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    drop_plane(&mut txn, PlaneId(9999)).unwrap();
}

#[test]
fn drop_plane_leaves_dictionaries_and_startup_intact() {
    // Labels/edge-type dictionaries are global; dropping a plane must
    // not corrupt them even though it heavily uses the same label ids.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let p = create_plane(&mut txn, "scratch", &Properties::new()).unwrap();
    let s = create_node(&mut txn, PlaneId::STARTUP, &["Shared"], &Properties::new()).unwrap();
    create_node(&mut txn, p, &["Shared"], &Properties::new()).unwrap();

    drop_plane(&mut txn, p).unwrap();

    let rec = get_node(&txn, PlaneId::STARTUP, s).unwrap().unwrap();
    assert_eq!(rec.labels, vec!["Shared".to_string()]);
}

// ---- property mutation ------------------------------------------------

#[test]
fn set_node_prop_inserts_and_overwrites() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();

    set_node_prop(
        &mut txn,
        PlaneId::STARTUP,
        n,
        "name",
        PropDesc::new(PropValue::Str("Alice".into())),
    )
    .unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    assert_eq!(
        rec.properties.get("name").map(|p| &p.value),
        Some(&PropValue::Str("Alice".into()))
    );

    // overwrite
    set_node_prop(
        &mut txn,
        PlaneId::STARTUP,
        n,
        "name",
        PropDesc::described("updated", PropValue::Str("Bob".into())),
    )
    .unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    let p = rec.properties.get("name").unwrap();
    assert_eq!(p.value, PropValue::Str("Bob".into()));
    assert_eq!(p.description.as_deref(), Some("updated"));

    // labels and external key untouched by a prop write
    assert_eq!(rec.labels, vec!["L".to_string()]);
    assert_eq!(rec.external_key, None);
}

#[test]
fn remove_node_prop_shrinks_and_is_idempotent_on_missing_key() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    set_node_prop(
        &mut txn,
        PlaneId::STARTUP,
        n,
        "draft",
        PropDesc::new(PropValue::Bool(true)),
    )
    .unwrap();

    remove_node_prop(&mut txn, PlaneId::STARTUP, n, "draft").unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
    assert!(rec.properties.is_empty());

    // removing again, or removing a key that never existed: not an error
    remove_node_prop(&mut txn, PlaneId::STARTUP, n, "draft").unwrap();
    remove_node_prop(&mut txn, PlaneId::STARTUP, n, "never_existed").unwrap();
}

#[test]
fn node_prop_mutation_on_missing_node_is_not_found() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    assert!(matches!(
        set_node_prop(
            &mut txn,
            PlaneId::STARTUP,
            NodeId(999),
            "k",
            PropDesc::new(PropValue::Null)
        ),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        remove_node_prop(&mut txn, PlaneId::STARTUP, NodeId(999), "k"),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn set_edge_prop_inserts_and_overwrites_without_disturbing_adjacency() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

    set_edge_prop(
        &mut txn,
        PlaneId::STARTUP,
        e,
        "weight",
        PropDesc::new(PropValue::Float(0.5)),
    )
    .unwrap();
    let rec = get_edge(&txn, PlaneId::STARTUP, e).unwrap().unwrap();
    assert_eq!(
        rec.properties.get("weight").map(|p| &p.value),
        Some(&PropValue::Float(0.5))
    );
    assert_eq!(rec.src, a);
    assert_eq!(rec.dst, b);
    assert_eq!(rec.ty, "T");

    // adjacency is untouched
    let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("T")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].edge, e);
}

#[test]
fn remove_edge_prop_shrinks_and_is_idempotent_on_missing_key() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    let e = create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();
    set_edge_prop(
        &mut txn,
        PlaneId::STARTUP,
        e,
        "w",
        PropDesc::new(PropValue::Int(1)),
    )
    .unwrap();

    remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "w").unwrap();
    assert!(
        get_edge(&txn, PlaneId::STARTUP, e)
            .unwrap()
            .unwrap()
            .properties
            .is_empty()
    );
    remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "w").unwrap();
    remove_edge_prop(&mut txn, PlaneId::STARTUP, e, "never_existed").unwrap();
}

#[test]
fn edge_prop_mutation_on_missing_edge_is_not_found() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    assert!(matches!(
        set_edge_prop(
            &mut txn,
            PlaneId::STARTUP,
            EdgeId(999),
            "k",
            PropDesc::new(PropValue::Null)
        ),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        remove_edge_prop(&mut txn, PlaneId::STARTUP, EdgeId(999), "k"),
        Err(Error::NotFound(_))
    ));
}

// ---- IdAllocator -------------------------------------------------------

#[test]
fn id_allocator_hands_out_sequential_distinct_ids() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let mut ids = IdAllocator::new();

    let mut nodes = Vec::new();
    for _ in 0..(ID_BATCH_SIZE * 2 + 5) {
        nodes.push(ids.next_node_id(&mut txn).unwrap());
    }
    for w in nodes.windows(2) {
        assert_eq!(
            w[1].0,
            w[0].0 + 1,
            "ids must stay sequential across refills"
        );
    }
    let unique: std::collections::BTreeSet<_> = nodes.iter().collect();
    assert_eq!(unique.len(), nodes.len());

    // node and edge counters are independent
    let e1 = ids.next_edge_id(&mut txn).unwrap();
    let e2 = ids.next_edge_id(&mut txn).unwrap();
    assert_eq!(e2.0, e1.0 + 1);
}

#[test]
fn id_allocator_refill_only_touches_meta_once_per_batch() {
    // Indirect but precise: the meta counter should equal
    // start + ID_BATCH_SIZE after a single allocation (one refill),
    // not start + 1 (which unbatched next_id would produce).
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let before = get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap();

    let mut ids = IdAllocator::new();
    let first = ids.next_node_id(&mut txn).unwrap();
    assert_eq!(first.0, before);

    let after_one_alloc = get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap();
    assert_eq!(after_one_alloc, before + ID_BATCH_SIZE);

    // draining the rest of the batch must not move the counter again
    for _ in 1..ID_BATCH_SIZE {
        ids.next_node_id(&mut txn).unwrap();
    }
    assert_eq!(
        get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap(),
        before + ID_BATCH_SIZE
    );

    // the (ID_BATCH_SIZE+1)-th allocation triggers a second refill
    ids.next_node_id(&mut txn).unwrap();
    assert_eq!(
        get_u64(&txn, keys::META_NEXT_NODE_ID).unwrap().unwrap(),
        before + ID_BATCH_SIZE * 2
    );
}

#[test]
fn id_allocator_reservation_rolls_back_with_an_aborted_transaction() {
    // The counter bump from reserving a batch is part of the write
    // transaction; if the whole transaction aborts, the reservation
    // never happened as far as any later transaction can tell — so no
    // ids are wasted by an abort, only by a commit that under-uses its
    // batch (documented in `id_allocator_commit_can_waste_a_partial_batch`).
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    txn.commit().unwrap();

    let mut txn = eng.begin_write().unwrap();
    let mut ids = IdAllocator::new();
    let ghost = ids.next_node_id(&mut txn).unwrap();
    drop(txn); // abort — the batch reservation is discarded too

    let mut txn = eng.begin_write().unwrap();
    let mut ids = IdAllocator::new();
    let real = ids.next_node_id(&mut txn).unwrap();
    txn.commit().unwrap();
    assert_eq!(ghost, real);
}

#[test]
fn id_allocator_commit_can_waste_a_partial_batch() {
    // Accepted tradeoff, documented on `IdAllocator`: a committed
    // transaction that only partially drains its reserved batch loses
    // the unused tail — ids stay unique/monotonic, just not dense.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    txn.commit().unwrap();

    let mut txn = eng.begin_write().unwrap();
    let mut ids = IdAllocator::new();
    let first = ids.next_node_id(&mut txn).unwrap(); // reserves a full batch
    txn.commit().unwrap(); // only 1 of ID_BATCH_SIZE used

    let mut txn = eng.begin_write().unwrap();
    let mut ids = IdAllocator::new();
    let next = ids.next_node_id(&mut txn).unwrap();
    assert_eq!(
        next.0,
        first.0 + ID_BATCH_SIZE,
        "the rest of the first batch should be permanently skipped"
    );
}

#[test]
fn id_allocator_ids_are_still_usable_to_create_real_nodes() {
    // End-to-end: insert_node with an allocator-supplied id behaves
    // exactly like create_node with an unbatched one.
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let mut ids = IdAllocator::new();

    let id = ids.next_node_id(&mut txn).unwrap();
    insert_node(
        &mut txn,
        PlaneId::STARTUP,
        id,
        None,
        &["L"],
        &Properties::new(),
    )
    .unwrap();
    let rec = get_node(&txn, PlaneId::STARTUP, id).unwrap().unwrap();
    assert_eq!(rec.labels, vec!["L".to_string()]);

    let eid = ids.next_edge_id(&mut txn).unwrap();
    let other = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
    insert_edge(
        &mut txn,
        PlaneId::STARTUP,
        eid,
        id,
        other,
        "T",
        &Properties::new(),
    )
    .unwrap();
    let out = neighbors(&txn, PlaneId::STARTUP, id, Dir::Out, Some("T")).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].edge, eid);
}

#[test]
fn insert_node_rejects_duplicate_external_key_even_with_preallocated_id() {
    let eng = MemoryEngine::new();
    let mut txn = eng.begin_write().unwrap();
    init(&mut txn).unwrap();
    let mut ids = IdAllocator::new();
    let id1 = ids.next_node_id(&mut txn).unwrap();
    insert_node(
        &mut txn,
        PlaneId::STARTUP,
        id1,
        Some("k"),
        &[],
        &Properties::new(),
    )
    .unwrap();

    let id2 = ids.next_node_id(&mut txn).unwrap();
    assert!(matches!(
        insert_node(
            &mut txn,
            PlaneId::STARTUP,
            id2,
            Some("k"),
            &[],
            &Properties::new()
        ),
        Err(Error::Conflict(_))
    ));
}
