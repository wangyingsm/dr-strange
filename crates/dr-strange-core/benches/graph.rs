//! Micro-benchmarks (arch/01 §7, arch/02 §5): the numbers that gate the
//! deferred optimizations — the moka `CachedReader` (is a decode-cache worth
//! it on traversal?) and the HNSW graph sidecar (how much does rebuild-on-open
//! cost?). Run with `cargo bench`.
//!
//! Covered: insert throughput, point lookup, 1-hop and 2-hop expansion
//! (memory vs redb), and vector search brute-force vs the declared HNSW index.
//! External-DB comparison (Kùzu/Neo4j) is a separate, later effort.
//!
//! **Backend comparison.** The `redb`-labelled `Database` benches run against
//! whichever on-disk backend the features select, so the native LSM engine is
//! measured against redb by running the suite twice:
//! ```text
//!   cargo bench --bench graph                          # redb (baseline)
//!   cargo bench --bench graph --features native-backend  # native (reports Δ)
//! ```
//! Indicative (1 machine, data resident in the memtable): the native engine is
//! ~3.5× faster on insert (40.4ms → 11.6ms / 1k nodes) and ~2.7× on point
//! lookup (1.42µs → 518ns), and on par for 2-hop expansion. The SST read path
//! (bloom/block-cache) only engages past the flush threshold and isn't measured
//! by this small-data suite.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use dr_strange_core::{Database, Dir, Metric, NodeId, PropDesc, PropValue, Properties};
use tempfile::TempDir;

fn int_props(v: i64) -> Properties {
    [("v".to_string(), PropDesc::new(PropValue::Int(v)))]
        .into_iter()
        .collect()
}

fn emb_props(v: Vec<f32>) -> Properties {
    [("emb".to_string(), PropDesc::new(PropValue::Vector(v)))]
        .into_iter()
        .collect()
}

/// A fresh redb database in a temp dir (kept alive alongside the handle).
fn open_redb() -> (Database, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("bench.drsg")).unwrap();
    (db, dir)
}

fn insert_nodes(db: &Database, n: usize) {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    for i in 0..n {
        txn.create_node(&["N"], int_props(i as i64)).unwrap();
    }
    txn.commit().unwrap();
}

/// A star-of-stars: `root` → `fanout` mids, each mid → `fanout` leaves. Gives
/// `fanout` 1-hop and `fanout²` 2-hop neighbours from `root`.
fn build_expand_graph(db: &Database, fanout: usize) -> NodeId {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let root = txn.create_node(&["Root"], Properties::new()).unwrap();
    for _ in 0..fanout {
        let mid = txn.create_node(&["Mid"], Properties::new()).unwrap();
        txn.create_edge(root, mid, "E", Properties::new()).unwrap();
        for _ in 0..fanout {
            let leaf = txn.create_node(&["Leaf"], Properties::new()).unwrap();
            txn.create_edge(mid, leaf, "E", Properties::new()).unwrap();
        }
    }
    txn.commit().unwrap();
    root
}

/// `n` random 16-d vectors; optionally declare the HNSW index.
fn build_vectors(db: &Database, n: usize, with_index: bool) -> Vec<f32> {
    let plane = db.plane("startup").unwrap();
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        ((rng >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    };
    let mut txn = plane.write().unwrap();
    let mut query = Vec::new();
    for i in 0..n {
        let v: Vec<f32> = (0..16).map(|_| next()).collect();
        if i == 0 {
            query = v.clone();
        }
        txn.create_node(&["Doc"], emb_props(v)).unwrap();
    }
    txn.commit().unwrap();
    if with_index {
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
    }
    query
}

fn bench_insert(c: &mut Criterion) {
    let mut g = c.benchmark_group("insert_1k_nodes");
    g.sample_size(20);
    g.bench_function("memory", |b| {
        b.iter_batched(
            || Database::in_memory().unwrap(),
            |db| insert_nodes(&db, 1000),
            BatchSize::SmallInput,
        )
    });
    g.bench_function("redb", |b| {
        b.iter_batched(
            open_redb,
            |(db, _dir)| insert_nodes(&db, 1000),
            BatchSize::SmallInput,
        )
    });
    g.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let mut g = c.benchmark_group("point_lookup");
    let mem = Database::in_memory().unwrap();
    insert_nodes(&mem, 10_000);
    let (redb, _dir) = open_redb();
    insert_nodes(&redb, 10_000);

    for (name, db) in [("memory", &mem), ("redb", &redb)] {
        let plane = db.plane("startup").unwrap();
        g.bench_function(name, |b| {
            b.iter(|| black_box(plane.node(NodeId(5000)).unwrap()))
        });
    }
    g.finish();
}

fn bench_expand(c: &mut Criterion) {
    // fanout 32 → 32 one-hop, 1024 two-hop neighbours.
    let mem = Database::in_memory().unwrap();
    let mem_root = build_expand_graph(&mem, 32);
    let (redb, _dir) = open_redb();
    let redb_root = build_expand_graph(&redb, 32);

    let mut one = c.benchmark_group("expand_1hop");
    for (name, db, root) in [("memory", &mem, mem_root), ("redb", &redb, redb_root)] {
        let plane = db.plane("startup").unwrap();
        one.bench_function(name, |b| {
            b.iter(|| black_box(plane.neighbors(root, Dir::Out, None).unwrap().len()))
        });
    }
    one.finish();

    let mut two = c.benchmark_group("expand_2hop");
    for (name, db, root) in [("memory", &mem, mem_root), ("redb", &redb, redb_root)] {
        let plane = db.plane("startup").unwrap();
        two.bench_function(name, |b| {
            b.iter(|| {
                black_box(
                    plane
                        .query()
                        .seek_ids([root])
                        .expand(Dir::Out, None)
                        .expand(Dir::Out, None)
                        .count()
                        .unwrap(),
                )
            })
        });
    }
    two.finish();
}

fn bench_vector_search(c: &mut Criterion) {
    // 5000 vectors: brute-force (no index) vs the declared HNSW index.
    let brute = Database::in_memory().unwrap();
    let q = build_vectors(&brute, 5000, false);
    let indexed = Database::in_memory().unwrap();
    let _ = build_vectors(&indexed, 5000, true);

    let mut g = c.benchmark_group("vector_top10_of_5k");
    let bp = brute.plane("startup").unwrap();
    g.bench_function("brute_force", |b| {
        b.iter(|| {
            black_box(
                bp.query()
                    .vector_top_k(Some("Doc"), "emb", q.clone(), Metric::L2, 10)
                    .ids()
                    .unwrap(),
            )
        })
    });
    let ip = indexed.plane("startup").unwrap();
    g.bench_function("hnsw", |b| {
        b.iter(|| {
            black_box(
                ip.query()
                    .vector_top_k(Some("Doc"), "emb", q.clone(), Metric::L2, 10)
                    .ids()
                    .unwrap(),
            )
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_insert, bench_lookup, bench_expand, bench_vector_search
}
criterion_main!(benches);
