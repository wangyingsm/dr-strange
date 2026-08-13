//! Preprocessing throughput (ROADMAP §11): how fast Rust source becomes facts.
//!
//! The metric is **MiB of source per second**, because that is the number that
//! decides whether "digest this repository" is a thing you do while waiting or
//! a thing you schedule. Criterion's `Throughput::Bytes` is given the exact
//! byte count of the corpus, so it reports MiB/s directly rather than leaving a
//! ratio to be worked out from an elapsed time.
//!
//! The corpus is **this workspace's own `crates/`** — real code, with the long
//! functions, deep module trees, macro invocations and re-export facades that a
//! synthetic fixture would not have, and which are exactly what the resolution
//! passes cost time on. It is found by walking up from `CARGO_MANIFEST_DIR`, so
//! the bench runs from wherever cargo starts it; if it is ever run against a
//! checkout without sources, it says so instead of reporting a fast zero.
//!
//! Two groups, measuring two different things — and **not** two points on one
//! scale, since they differ in more than one way at once:
//!
//! - `parse` — one file at a time through the single-document path, from bytes
//!   already in memory and on one thread. Close to `syn`'s own cost plus the
//!   item walk.
//! - `tree` — the whole corpus through `route_tree`: what `drsg digest <dir>`
//!   actually runs. Reads each file from disk, parses in parallel across cores,
//!   then does every cross-file pass — alias building, import resolution, call
//!   resolution.
//!
//! `tree` comes out *faster per byte* than `parse` even though it does strictly
//! more work, because it is parallel and `parse` is not. So the gap between
//! them is not the price of resolution; it is parallelism minus that price,
//! and this bench does not separate the two. `tree` is the number to quote to
//! a user, `parse` the one to watch for a regression in the parser itself.
//!
//! Indicative (1 machine, 1.68 MiB corpus): `parse` 7.7 MiB/s, `tree` 22.9
//! MiB/s — a workspace of this size preprocessed in ~73 ms, against the several
//! seconds a single model call would take.
//!
//! Run with `cargo bench -p dr-strange-llm --bench preprocess`.

use std::path::{Path, PathBuf};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dr_strange_llm::preprocess::{Host, LocalFiles, Plugins, route_document, route_tree};

/// The workspace's `crates/` directory, found by walking up from this package.
fn corpus_root() -> Option<PathBuf> {
    let mut dir: &Path = Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("crates");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
}

/// Every `.rs` file the host will answer for, with its bytes.
fn corpus(host: &LocalFiles) -> Vec<(String, Vec<u8>)> {
    host.list(".rs")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| {
            let bytes = host.read(&path).ok()?;
            Some((path, bytes))
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let Some(root) = corpus_root() else {
        eprintln!("preprocess bench: no `crates/` directory found — skipping");
        return;
    };
    let host = LocalFiles::new(&root).expect("open the corpus");
    let files = corpus(&host);
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    if total == 0 {
        eprintln!("preprocess bench: corpus is empty — skipping");
        return;
    }
    let opts = Plugins::builtin();
    eprintln!(
        "preprocess bench: {} files, {:.2} MiB of Rust",
        files.len(),
        total as f64 / (1024.0 * 1024.0),
    );

    // Per-file parsing, with no cross-file resolution: the floor.
    let mut group = c.benchmark_group("preprocess/parse");
    group.throughput(Throughput::Bytes(total as u64));
    group.bench_function("rust", |b| {
        b.iter(|| {
            for (path, bytes) in &files {
                let out =
                    route_document(path, bytes, Some("rust"), &host, &opts).expect("parse a file");
                std::hint::black_box(out.nodes.len());
            }
        })
    });
    group.finish();

    // The whole tree at once — what `drsg digest <dir>` actually runs, adding
    // the parallel fan-out and every cross-file resolution pass.
    let mut group = c.benchmark_group("preprocess/tree");
    group.throughput(Throughput::Bytes(total as u64));
    group.sample_size(20); // a whole workspace per iteration is not a microsecond
    group.bench_function("rust", |b| {
        b.iter(|| {
            let out = route_tree(&host, Some("rust"), &opts).expect("route the tree");
            std::hint::black_box(out.nodes.len() + out.edges.len());
        })
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
