//! What every sandbox suite needs before it can assert anything: the
//! committed fixture, and somewhere for a call to read from.

use std::path::Path;

use dr_strange_llm::preprocess::{Limits, LocalFiles, WasmPlugin};

/// The committed fixture, loaded in one of its modes.
pub fn fixture(mode: &str, limits: Limits) -> WasmPlugin {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture.wasm");
    WasmPlugin::load(&path, vec![("mode".to_string(), mode.to_string())], limits)
        .expect("the committed fixture must load")
}

/// A scratch dir with one claimable file, for calls that need a host.
pub fn scratch(name: &str) -> (std::path::PathBuf, LocalFiles) {
    let dir = std::env::temp_dir().join(format!("drsg-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.fix"), "x").unwrap();
    let host = LocalFiles::new(&dir).unwrap();
    (dir, host)
}
