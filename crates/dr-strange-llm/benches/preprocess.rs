//! Preprocessing throughput (ROADMAP §11): how fast Rust source becomes facts,
//! now that the parser is a **wasm plugin** rather than native code.
//!
//! The metric is **MiB of source per second** — the number that decides
//! whether "digest this repository" is a thing you do while waiting or a thing
//! you schedule. Criterion's `Throughput::Bytes` gets the corpus's exact byte
//! count, so it reports MiB/s directly.
//!
//! The corpus is this workspace's own `crates/` — real code, with the deep
//! module trees and re-export facades the resolution passes cost time on.
//!
//! Needs the built plugin: point `DRSG_RUST_PLUGIN_WASM` at `rust.wasm` (built
//! in the extensions repo with `cargo build --target wasm32-wasip2 --release`).
//! Without it the bench prints how to get one and measures nothing, rather
//! than silently reporting an empty number.
//!
//! Two groups:
//! - `tree` — `route_tree` through the plugin: chunked parallel `parse` calls
//!   plus one `assemble`. What `drsg digest <dir>` actually runs, and the
//!   number to quote.
//! - `document` — one file at a time through the single-document path, each
//!   call paying parse + assemble for a single file. The per-call overhead
//!   floor, and the regression canary for the boundary itself.
//!
//! Slice 1's native baseline on this corpus: 23 MiB/s parallel (tree),
//! 7.7 MiB/s sequential per-file. Measured through the plugin (1 machine,
//! release, 2026-08-14): **7.5 MiB/s tree, 3.7 MiB/s per-document** — the
//! sandbox costs ~3×, which is ~220 ms for this workspace and trivial
//! beside a single model call. The first measurement said 4.5; pre-linking
//! the component, per-file chunks and MessagePack partials bought it to 8.6,
//! and line provenance (proc-macro2's span-locations) then cost ~13% of
//! that back — paid knowingly: a fact you cannot jump to is half a fact.
//! The per-document number is the wasm instruction floor (~2.1× native
//! sequential); the remaining tree gap is the serial `assemble` tail, which
//! `RUST_LOG=dr_strange_llm=debug` splits out per run.

// The whole bench needs the sandbox; without the feature it is an empty main
// rather than a build error, so `--no-default-features --all-targets` stays a
// build anyone can run.
#[cfg(feature = "plugins")]
mod imp {
    use std::path::{Path, PathBuf};

    use criterion::{Criterion, Throughput, criterion_group};
    use dr_strange_llm::preprocess::{
        Host, Limits, LocalFiles, Plugins, WasmPlugin, route_document, route_tree,
    };

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
        let Ok(wasm) = std::env::var("DRSG_RUST_PLUGIN_WASM") else {
            eprintln!(
                "preprocess bench: set DRSG_RUST_PLUGIN_WASM to a built rust.wasm \
                 (extensions repo: cargo build --manifest-path plugins/rust/component/Cargo.toml \
                 --target wasm32-wasip2 --release) — skipping"
            );
            return;
        };
        let Some(root) = corpus_root() else {
            eprintln!("preprocess bench: no `crates/` directory found — skipping");
            return;
        };

        let plugin = WasmPlugin::load(Path::new(&wasm), Vec::new(), Limits::default())
            .expect("load the rust plugin");
        let plugins = Plugins::from_handlers(vec![Box::new(plugin)]);

        let host = LocalFiles::new(&root).expect("open the corpus");
        let files = corpus(&host);
        let total: usize = files.iter().map(|(_, b)| b.len()).sum();
        if total == 0 {
            eprintln!("preprocess bench: corpus is empty — skipping");
            return;
        }
        eprintln!(
            "preprocess bench: {} files, {:.2} MiB of Rust, via {wasm}",
            files.len(),
            total as f64 / (1024.0 * 1024.0),
        );

        // The whole tree — chunked parallel parse, one assemble. The real path.
        let mut group = c.benchmark_group("preprocess/tree");
        group.throughput(Throughput::Bytes(total as u64));
        group.sample_size(10); // a whole workspace per iteration
        group.bench_function("rust-wasm", |b| {
            b.iter(|| {
                let out = route_tree(&host, Some("rust"), &plugins).expect("route the tree");
                std::hint::black_box(out.nodes.len() + out.edges.len());
            })
        });
        group.finish();

        // One file at a time: parse + assemble per call — the boundary's floor.
        let mut group = c.benchmark_group("preprocess/document");
        group.throughput(Throughput::Bytes(total as u64));
        group.sample_size(10);
        group.bench_function("rust-wasm", |b| {
            b.iter(|| {
                for (path, bytes) in &files {
                    let out = route_document(path, bytes, Some("rust"), &host, &plugins)
                        .expect("parse a file");
                    std::hint::black_box(out.nodes.len());
                }
            })
        });
        group.finish();
    }

    criterion_group!(benches, bench);
}

#[cfg(feature = "plugins")]
fn main() {
    imp::benches();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
}

#[cfg(not(feature = "plugins"))]
fn main() {}
