//! HNSW sidecar (arch/01 §5): the built vector-index graph is persisted beside
//! the database file and reloaded on open, so reopening a vector database skips
//! the rebuild-from-KV. The KV stays the source of truth — the sidecar is only
//! a cache, gated on the commit sequence — so these tests assert *correctness*
//! across reopen, not that any particular load path ran.

use dr_strange_core::{Database, Metric, PropDesc, PropValue, Properties};
use tempfile::TempDir;

fn emb(v: Vec<f32>) -> Properties {
    [("emb".to_string(), PropDesc::new(PropValue::Vector(v)))]
        .into_iter()
        .collect()
}

/// Nearest Doc's external key to `q`.
fn nearest(db: &Database, q: Vec<f32>) -> String {
    let plane = db.plane("startup").unwrap();
    let ids = plane
        .query()
        .vector_top_k(Some("Doc"), "emb", q, Metric::L2, 1)
        .ids()
        .unwrap();
    plane.node(ids[0]).unwrap().unwrap().external_key.unwrap()
}

/// Ad-hoc measurement of the open-time win: build an indexed vector graph,
/// then time reopening it with the sidecar (load) vs without (rebuild-from-KV).
/// `cargo test -p dr-strange-core --release --test sidecar -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, not an assertion"]
fn measure_open_with_sidecar_vs_rebuild() {
    use std::time::Instant;

    let n = 20_000usize;
    let dim = 64usize;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.drsg");
    let sidecar = dir.path().join("graph.drsg.hnsw");

    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    };
    {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        for _ in 0..n {
            let v: Vec<f32> = (0..dim).map(|_| next()).collect();
            txn.create_node(&["Doc"], emb(v)).unwrap();
        }
        txn.commit().unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    } // drop writes the sidecar

    assert!(sidecar.exists());
    let t = Instant::now();
    let _ = Database::open(&path).unwrap();
    let with_sidecar = t.elapsed();

    std::fs::remove_file(&sidecar).unwrap();
    let t = Instant::now();
    let _ = Database::open(&path).unwrap();
    let rebuild = t.elapsed();

    println!(
        "open {n}x{dim}: sidecar-load {with_sidecar:?} vs KV-rebuild {rebuild:?} \
         ({:.1}x faster)",
        rebuild.as_secs_f64() / with_sidecar.as_secs_f64()
    );
}

#[test]
fn sidecar_is_written_on_drop_and_reloaded_on_open() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.drsg");
    let sidecar = dir.path().join("graph.drsg.hnsw");

    // Build a small indexed graph, then close it.
    {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        txn.create_node_with_key("a", &["Doc"], emb(vec![0.0, 0.0]))
            .unwrap();
        txn.create_node_with_key("b", &["Doc"], emb(vec![9.0, 9.0]))
            .unwrap();
        txn.commit().unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
        assert_eq!(nearest(&db, vec![0.1, 0.1]), "a");
    }
    // Dropping the database wrote the sidecar next to the file.
    assert!(sidecar.exists(), "sidecar should be written on drop");

    // Reopen (loads the sidecar since nothing changed) — search still correct.
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(nearest(&db, vec![0.1, 0.1]), "a");
        assert_eq!(nearest(&db, vec![8.5, 8.5]), "b");

        // A write bumps the commit sequence, so the on-disk sidecar is now
        // stale; the next open must rebuild from the KV and reflect this node.
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        // Between a([0,0]) and b([9,9]) so a query there is unambiguously c.
        txn.create_node_with_key("c", &["Doc"], emb(vec![4.5, 4.5]))
            .unwrap();
        txn.commit().unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    }

    // Reopen once more: whether via a freshly-saved sidecar or a KV rebuild,
    // the new node is present and is nearest to its own location.
    {
        let db = Database::open(&path).unwrap();
        assert_eq!(nearest(&db, vec![4.5, 4.5]), "c");
    }
}
