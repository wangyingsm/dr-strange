//! Stages the embedded web assets in `OUT_DIR` so `rust-embed`'s folder macro
//! always has something to embed — even on a fresh checkout where the Svelte
//! bundle has not been built yet (`cargo build` must work without a JS
//! toolchain present). The real bundle is produced by `just web-build` (bun +
//! Vite) and copied here; without one, a placeholder page tells the operator
//! how to build the UI. Unstyled on purpose: every response carries a
//! Content-Security-Policy without `'unsafe-inline'`, so an inline `style`
//! attribute here would only be dropped by the browser.
//!
//! Staging rather than embedding `frontend/dist` directly: `cargo publish`
//! rejects a build script that writes anywhere outside `OUT_DIR`, and
//! backfilling the placeholder into the source tree is exactly that.

use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = "<!doctype html><meta charset=\"utf-8\">\
     <title>dr-strange</title>\
     <body>\
     <h1>dr-strange web UI not built</h1>\
     <p>The JSON-RPC backend is live at <code>/rpc</code> and \
     <code>/ws</code>. To build the dashboard, run \
     <code>just web-build</code> (needs <code>bun</code>) from a checkout and \
     restart <code>drsg serve</code>.</p>";

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("web-assets");
    let dist = Path::new("frontend/dist");
    println!("cargo:rerun-if-changed=frontend/dist");

    let _ = std::fs::remove_dir_all(&out);
    if dist.join("index.html").exists() {
        copy_dir(dist, &out).expect("stage frontend/dist");
    } else {
        std::fs::create_dir_all(&out).expect("create the asset staging directory");
        std::fs::write(out.join("index.html"), PLACEHOLDER).expect("write placeholder index.html");
    }
}
