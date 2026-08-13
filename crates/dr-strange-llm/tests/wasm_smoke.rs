//! End-to-end smoke for the wasm host: load a real component, describe it,
//! parse + assemble over a real directory, and read the facts back.
//!
//! Skips unless `DRSG_PLUGIN_WASM` names a component, so `cargo test` needs no
//! wasm toolchain; CI and the fixtures of task #17 make it unconditional later.
#![cfg(feature = "plugins")]

use dr_strange_llm::preprocess::{Host, Limits, LocalFiles, Preprocessor, WasmPlugin};

#[test]
fn a_component_parses_a_tree_through_the_sandbox() {
    let Ok(wasm) = std::env::var("DRSG_PLUGIN_WASM") else {
        eprintln!("DRSG_PLUGIN_WASM not set — skipping the wasm smoke test");
        return;
    };

    let dir = std::env::temp_dir().join(format!("drsg-wasm-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.toml"), "x = 1\n").unwrap();
    std::fs::write(dir.join("b.toml"), "y = 2\nz = 3\n").unwrap();

    let plugin = WasmPlugin::load(
        std::path::Path::new(&wasm),
        vec![("flavor".into(), "test".into())],
        Limits::default(),
    )
    .expect("load the component");

    let m = plugin.manifest();
    assert_eq!(m.name, "toml");
    assert_eq!(m.extensions, vec!["toml".to_string()]);

    let host = LocalFiles::new(&dir).unwrap();
    let paths = host.list(".toml").unwrap();
    assert_eq!(paths.len(), 2);

    let out = plugin
        .preprocess(
            &dr_strange_llm::preprocess::Input::Files { paths: &paths },
            &host,
        )
        .expect("parse + assemble through the sandbox");

    assert_eq!(out.nodes.len(), 2, "one Manifest node per file");
    assert_eq!(out.nodes[0].label, "Manifest");
    // The described property survived the JSON crossing.
    assert!(
        out.nodes[0].props.contains_key("path"),
        "properties should cross the boundary: {:?}",
        out.nodes[0].props
    );

    let _ = std::fs::remove_dir_all(&dir);
}
