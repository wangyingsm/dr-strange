//! Persistent-cache coherence through the public API (arch/02 §3, §6). The
//! query path caches decoded records across queries; a committed write must
//! make the next query see fresh data — the seq stamp, end to end.

use dr_strange_core::{BulkEdge, BulkNode, Database, PropDesc, PropValue, Properties};

fn prop_x(v: i64) -> Properties {
    let mut p = Properties::new();
    p.insert(
        "x".into(),
        PropDesc {
            description: None,
            value: PropValue::Int(v),
        },
    );
    p
}

/// Read node `key`'s `x` through the *query path* (which uses the cache).
fn query_x(db: &Database, key: &str) -> i64 {
    let recs = db
        .plane("startup")
        .unwrap()
        .query()
        .seek_keys([key])
        .nodes()
        .unwrap();
    match &recs[0].properties["x"].value {
        PropValue::Int(i) => *i,
        other => panic!("unexpected {other:?}"),
    }
}

fn check_snapshot_isolation(db: &Database) {
    let id = {
        let p = db.plane("startup").unwrap();
        let mut t = p.write().unwrap();
        let id = t.create_node_with_key("a", &["N"], prop_x(1)).unwrap();
        t.commit().unwrap();
        id
    };

    // Query 1 populates the cache at this snapshot.
    assert_eq!(query_x(db, "a"), 1);
    assert_eq!(query_x(db, "a"), 1); // cross-query hit, same value

    // Commit a change — this bumps the commit sequence.
    {
        let p = db.plane("startup").unwrap();
        let mut t = p.write().unwrap();
        t.set_prop(
            id,
            "x",
            PropDesc {
                description: None,
                value: PropValue::Int(2),
            },
        )
        .unwrap();
        t.commit().unwrap();
    }

    // Query 2 is on a newer snapshot: the cached x=1 entry is stamped with the
    // old seq, so it's a miss → fresh read → the new value. No stale serve.
    assert_eq!(query_x(db, "a"), 2);
}

#[test]
fn write_invalidates_cache_memory() {
    check_snapshot_isolation(&Database::in_memory().unwrap());
}

#[test]
fn write_invalidates_cache_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    check_snapshot_isolation(&Database::open(dir.path().join("g.drsg")).unwrap());
}

/// Sizing measurement for the cross-query win (arch/02 §5); not run by
/// default. `cargo test -p dr-strange-core --test cache -- --ignored --nocapture`.
/// The same record-reading query, re-run on a static snapshot: the first call
/// decodes + populates the cache, later calls serve fat records from the L2.
#[test]
#[ignore]
fn bench_cross_query_warm_cache() {
    use std::time::Instant;

    fn fat_props(i: usize) -> Properties {
        let mut p = Properties::new();
        for j in 0..12 {
            p.insert(
                format!("field_{j}"),
                PropDesc {
                    description: Some(format!("description of field {j} of node {i}")),
                    value: PropValue::Str(format!("value-{i}-{j}-lorem-ipsum-dolor-sit-amet")),
                },
            );
        }
        p
    }

    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("g.drsg")).unwrap();
    let n = 5000usize;
    {
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let keys: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
        let props: Vec<Properties> = (0..n).map(fat_props).collect();
        let nodes: Vec<BulkNode> = keys
            .iter()
            .zip(props)
            .map(|(k, p)| BulkNode {
                external_key: Some(k),
                labels: &["N"],
                props: p,
            })
            .collect();
        txn.bulk_load(nodes, Vec::<BulkEdge>::new()).unwrap();
        txn.commit().unwrap();
    }

    // A query that reads 500 fat node records.
    let query_keys: Vec<String> = (0..500).map(|i| format!("n{}", i * 7 % n)).collect();
    let run = || {
        db.plane("startup")
            .unwrap()
            .query()
            .seek_keys(query_keys.iter().cloned())
            .nodes()
            .unwrap()
            .len()
    };

    let iters = 200;
    let t = Instant::now();
    run(); // cold: decode + populate L2
    let cold = t.elapsed();
    let t = Instant::now();
    for _ in 0..iters {
        run(); // warm: L2 hits, no re-decode (static snapshot ⇒ same seq)
    }
    let warm = t.elapsed() / iters;
    println!(
        "cross-query 500 fat records: cold {:.1} µs, warm {:.1} µs → {:.2}x",
        cold.as_secs_f64() * 1e6,
        warm.as_secs_f64() * 1e6,
        cold.as_secs_f64() / warm.as_secs_f64(),
    );
}

#[test]
fn commit_seq_advances_with_writes() {
    let db = Database::in_memory().unwrap();
    let s0 = db.commit_seq().unwrap();
    let p = db.plane("startup").unwrap();
    let mut t = p.write().unwrap();
    t.create_node(&["N"], Properties::new()).unwrap();
    t.commit().unwrap();
    assert!(
        db.commit_seq().unwrap() > s0,
        "a committed write bumps the seq"
    );
}

/// Planes are a hard partition (arch/01): a node id from plane A looked up
/// through plane B is `None`, cache or no cache. Ids are global, so the L2 must
/// not hand plane B a record plane A's query decoded. Differential: plane B's
/// answer on a cold cache (the storage path) is the oracle; the same query
/// after plane A has warmed the L2 with that very id must agree.
fn check_cross_plane_isolation(db: &Database) {
    let a = db.plane("startup").unwrap();
    let b = db.create_plane("other", Properties::new()).unwrap();
    let (src, dst) = {
        let mut t = a.write().unwrap();
        let src = t.create_node_with_key("a", &["N"], prop_x(1)).unwrap();
        let dst = t.create_node_with_key("a2", &["N"], prop_x(2)).unwrap();
        t.create_edge(src, dst, "LINKS", Properties::new()).unwrap();
        t.commit().unwrap();
        (src, dst)
    };

    // Cold: nothing cached anywhere, so this is the uncached answer.
    let cold_nodes = b.query().seek_ids([src]).nodes().unwrap();
    let cold_hops = b
        .query()
        .seek_ids([src])
        .expand(dr_strange_core::Dir::Out, None)
        .ids()
        .unwrap();
    assert!(cold_nodes.is_empty(), "plane B does not own plane A's node");
    assert!(cold_hops.is_empty());

    // Warm the L2 with plane A's record and adjacency for exactly these ids.
    assert_eq!(query_x(db, "a"), 1);
    assert_eq!(
        a.query()
            .seek_ids([src])
            .expand(dr_strange_core::Dir::Out, None)
            .ids()
            .unwrap(),
        vec![dst]
    );

    // Warm: plane B must agree with its own cold answer, not with plane A.
    assert_eq!(b.query().seek_ids([src]).nodes().unwrap(), cold_nodes);
    assert_eq!(
        b.query()
            .seek_ids([src])
            .expand(dr_strange_core::Dir::Out, None)
            .ids()
            .unwrap(),
        cold_hops
    );
    // And plane A still sees its own data through the same cache.
    assert_eq!(query_x(db, "a"), 1);
}

#[test]
fn cache_does_not_leak_across_planes_memory() {
    check_cross_plane_isolation(&Database::in_memory().unwrap());
}

#[test]
fn cache_does_not_leak_across_planes_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    check_cross_plane_isolation(&Database::open(dir.path().join("g.drsg")).unwrap());
}
