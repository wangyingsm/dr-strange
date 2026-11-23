//! Node expiry (`$expires_at`, #114): a plane used as a cache hides what has
//! expired, on every read, without a sweeper walking it.
//!
//! The reads a node can hide from are not one code path — `node`, `node_by_key`,
//! the scans behind every query, `neighbors`, vector search, keyword search,
//! and a time-travelling read of any of them — so each is asserted here rather
//! than trusting that one filter covers them all.

use dr_strange_core::{
    Database, Dir, EXPIRES_AT, Language, Metric, PlaneHandle, PropDesc, PropValue, Properties,
};

/// An instant comfortably in the past, and one comfortably ahead.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn past() -> i64 {
    now_ms() - 60_000
}
fn future() -> i64 {
    now_ms() + 3_600_000
}

/// `emb` (a 2-wide vector), `body` (text) and optionally an expiry.
fn doc(v: [f32; 2], body: &str, expires_at: Option<i64>) -> Properties {
    let mut p = Properties::new();
    p.insert("emb".into(), PropDesc::new(PropValue::Vector(v.to_vec())));
    p.insert("body".into(), PropDesc::new(PropValue::Str(body.into())));
    if let Some(at) = expires_at {
        p.insert(EXPIRES_AT.into(), PropDesc::new(PropValue::Int(at)));
    }
    p
}

fn indexed(plane: &PlaneHandle<'_>) {
    plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    plane
        .ensure_keyword_index("Doc", "body", Language::English)
        .unwrap();
}

/// The reporter's shape: a cache entry that has expired, and a live one that
/// is a worse match. Returns `(expired, live)`.
fn plane_with_an_expired_entry(
    db: &Database,
) -> (dr_strange_core::NodeId, dr_strange_core::NodeId) {
    let plane = db.plane("startup").unwrap();
    indexed(&plane);
    let mut w = plane.write().unwrap();
    let stale = w
        .create_node_with_key(
            "stale",
            &["Doc"],
            doc([1.0, 0.0], "graph question", Some(past())),
        )
        .unwrap();
    let live = w
        .create_node_with_key("live", &["Doc"], doc([0.0, 1.0], "graph question", None))
        .unwrap();
    w.commit().unwrap();
    (stale, live)
}

#[test]
fn an_expired_node_is_invisible_to_every_read() {
    let db = Database::in_memory().unwrap();
    let (stale, live) = plane_with_an_expired_entry(&db);
    let plane = db.plane("startup").unwrap();

    // The record itself, by id and by key.
    assert!(plane.node(stale).unwrap().is_none(), "node(id)");
    assert!(plane.node_by_key("stale").unwrap().is_none(), "node_by_key");
    assert!(plane.node(live).unwrap().is_some(), "the live one is there");

    // Every query over the plane: the scans behind `ScanAll`/`ScanLabel` are
    // where an id would otherwise leak without its record being read.
    assert_eq!(plane.query().scan_all().ids().unwrap(), vec![live]);
    assert_eq!(plane.query().scan_label("Doc").ids().unwrap(), vec![live]);
    assert_eq!(plane.query().scan_all().count().unwrap(), 1);

    // Both search channels, asked for more than there are.
    let hits = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![1.0, 0.0], Metric::L2, 10)
        .ids()
        .unwrap();
    assert_eq!(hits, vec![live], "vector search");
    let kw: Vec<_> = plane
        .keyword_search("Doc", "body", "graph", 10)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(kw, vec![live], "keyword search");
}

/// The issue's own complaint: an expired entry outranking a live one. With
/// `k = 1` the expired node *is* the nearest, so exclusion has to happen
/// before `k` is decided — filtering after would answer nothing at all.
#[test]
fn expiry_excludes_before_k_not_after() {
    let db = Database::in_memory().unwrap();
    let (_stale, live) = plane_with_an_expired_entry(&db);
    let plane = db.plane("startup").unwrap();

    let hits = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", vec![1.0, 0.0], Metric::L2, 1)
        .ids()
        .unwrap();
    assert_eq!(hits, vec![live], "k=1 answers the live node, not nothing");

    let kw: Vec<_> = plane
        .keyword_search("Doc", "body", "graph", 1)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(kw, vec![live], "and the same on the keyword channel");
}

/// An expiry still ahead of now hides nothing.
#[test]
fn an_unexpired_expiry_hides_nothing() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    indexed(&plane);
    let id = {
        let mut w = plane.write().unwrap();
        let id = w
            .create_node(&["Doc"], doc([1.0, 0.0], "graph", Some(future())))
            .unwrap();
        w.commit().unwrap();
        id
    };

    assert!(plane.node(id).unwrap().is_some());
    assert_eq!(plane.query().scan_all().ids().unwrap(), vec![id]);
    assert_eq!(
        plane
            .query()
            .vector_top_k(Some("Doc"), "emb", vec![1.0, 0.0], Metric::L2, 1)
            .ids()
            .unwrap(),
        vec![id]
    );
}

/// An edge onto an expired node leads nowhere a read can see, in both
/// directions, while the rest of the adjacency is untouched.
#[test]
fn an_edge_to_an_expired_node_is_not_a_neighbour() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let (from, stale, live) = {
        let mut w = plane.write().unwrap();
        let from = w.create_node(&["Doc"], Properties::new()).unwrap();
        let stale = w
            .create_node(&["Doc"], doc([1.0, 0.0], "x", Some(past())))
            .unwrap();
        let live = w.create_node(&["Doc"], Properties::new()).unwrap();
        w.create_edge(from, stale, "MENTIONS", Properties::new())
            .unwrap();
        w.create_edge(from, live, "MENTIONS", Properties::new())
            .unwrap();
        w.commit().unwrap();
        (from, stale, live)
    };

    let out: Vec<_> = plane
        .neighbors(from, Dir::Out, None)
        .unwrap()
        .into_iter()
        .map(|n| n.node)
        .collect();
    assert_eq!(out, vec![live]);
    assert!(
        plane.neighbors(stale, Dir::In, None).unwrap().is_empty()
            || plane.node(stale).unwrap().is_none(),
        "the expired node is not reachable as a record either"
    );
}

/// A plane nobody has given an expiry is left exactly as it was: the flag that
/// turns filtering on is never set, so no read pays for the mechanism.
#[test]
fn a_plane_without_expiry_is_untouched() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    indexed(&plane);
    let ids = {
        let mut w = plane.write().unwrap();
        let a = w
            .create_node(&["Doc"], doc([1.0, 0.0], "graph", None))
            .unwrap();
        let b = w
            .create_node(&["Doc"], doc([0.0, 1.0], "graph", None))
            .unwrap();
        w.commit().unwrap();
        vec![a, b]
    };
    assert_eq!(plane.query().scan_all().ids().unwrap(), ids);
    assert_eq!(
        plane
            .query()
            .vector_top_k(Some("Doc"), "emb", vec![1.0, 0.0], Metric::L2, 2)
            .ids()
            .unwrap()
            .len(),
        2
    );
}

/// Expiry needs no write to take effect, which is what makes it work on a
/// follower: a database opened read-only — where nothing can run a sweeper —
/// hides the same node. `open_read_only` is the native backend's, so this is
/// the one case here that names a backend.
#[cfg(feature = "native-backend")]
#[test]
fn expiry_needs_no_write_to_take_effect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let db = Database::open(&path).unwrap();
        plane_with_an_expired_entry(&db);
    }
    let db = Database::open_read_only(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    assert!(plane.node_by_key("stale").unwrap().is_none());
    assert!(plane.node_by_key("live").unwrap().is_some());
    assert_eq!(plane.query().scan_all().ids().unwrap().len(), 1);
}

/// Rewriting a key whose node has expired is an ordinary overwrite: the row is
/// still there and still owns the key — expiry hides a node, it does not free
/// its name — so `update` lands on it and the entry is live again with
/// whatever expiry the new write carries.
#[test]
fn rewriting_an_expired_key_brings_it_back() {
    use dr_strange_core::{BulkNode, OnConflict};

    let db = Database::in_memory().unwrap();
    let (stale, _live) = plane_with_an_expired_entry(&db);
    let plane = db.plane("startup").unwrap();
    assert!(
        plane.node_by_key("stale").unwrap().is_none(),
        "hidden first"
    );

    {
        let mut w = plane.write().unwrap();
        w.bulk_upsert(
            vec![BulkNode {
                external_key: Some("stale"),
                labels: &["Doc"],
                props: doc([1.0, 0.0], "graph question", Some(future())),
            }],
            Vec::new(),
            OnConflict::Update,
        )
        .unwrap();
        w.commit().unwrap();
    }

    let back = plane.node_by_key("stale").unwrap().expect("live again");
    assert_eq!(back.id, stale, "the same row, with its edges intact");
    assert_eq!(
        plane
            .query()
            .vector_top_k(Some("Doc"), "emb", vec![1.0, 0.0], Metric::L2, 1)
            .ids()
            .unwrap(),
        vec![stale],
        "and searchable again"
    );
}

/// Expiry is not history: naming a revision at which the node was still in the
/// plane does not bring it back, because the record at that revision carries
/// the same absolute timestamp. (An expiry *added* later applies from the
/// revision that added it — before that, the history honestly holds a node
/// with no expiry at all.)
#[cfg(feature = "native-backend")]
#[test]
fn expiry_is_not_history() {
    use dr_strange_core::AsOf;

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("db")).unwrap();
    let plane = db.plane("startup").unwrap();
    indexed(&plane);

    {
        let mut w = plane.write().unwrap();
        w.create_node_with_key("stale", &["Doc"], doc([1.0, 0.0], "graph", Some(past())))
            .unwrap();
        w.commit().unwrap();
    }
    let seq_before = db.commit_seq().unwrap();
    // A later commit, so the revision above is genuinely in the past.
    {
        let mut w = plane.write().unwrap();
        w.create_node(&["Doc"], doc([0.0, 1.0], "graph", None))
            .unwrap();
        w.commit().unwrap();
    }

    let then = plane.as_of(AsOf::Seq(seq_before)).unwrap();
    assert!(
        then.node_by_key("stale").unwrap().is_none(),
        "an expired node stays hidden at the revision it was written in"
    );
    assert!(
        then.query().scan_all().ids().unwrap().is_empty(),
        "and in a query against that revision"
    );
}
