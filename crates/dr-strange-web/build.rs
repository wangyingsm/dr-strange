//! Guarantees `frontend/dist/` exists at compile time so `rust-embed`'s
//! folder macro always has something to embed — even on a fresh checkout
//! where the Svelte bundle has not been built yet (`cargo build` must work
//! without a JS toolchain present). The real bundle is produced by
//! `just web-build` (bun + Vite); this only backfills a placeholder page
//! that tells the operator how to build the UI.

use std::path::Path;

fn main() {
    let dist = Path::new("frontend/dist");
    println!("cargo:rerun-if-changed=frontend/dist");

    let index = dist.join("index.html");
    if !index.exists() {
        std::fs::create_dir_all(dist).expect("create frontend/dist");
        std::fs::write(
            &index,
            "<!doctype html><meta charset=\"utf-8\">\
             <title>dr-strange</title>\
             <body style=\"font:14px system-ui;padding:2rem;max-width:40rem\">\
             <h1>dr-strange web UI not built</h1>\
             <p>The JSON-RPC backend is live at <code>/rpc</code> and \
             <code>/ws</code>. To build the dashboard, run \
             <code>just web-build</code> (needs <code>bun</code>) and restart \
             <code>drsg serve</code>.</p>",
        )
        .expect("write placeholder index.html");
    }
}
