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

#[test]
fn the_store_installs_verifies_and_refuses_tampering() {
    let Ok(wasm) = std::env::var("DRSG_PLUGIN_WASM") else {
        eprintln!("DRSG_PLUGIN_WASM not set — skipping the registry smoke test");
        return;
    };
    use dr_strange_llm::preprocess::{PluginConfig, PluginStore, Plugins, route_tree};

    let dir = std::env::temp_dir().join(format!("drsg-store-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = PluginStore::open(dir.clone()).unwrap();
    let bytes = std::fs::read(&wasm).unwrap();

    // Install records what the component says it is, hashed.
    let (entry, replaced) = store.install(&bytes, &wasm).unwrap();
    assert_eq!(entry.name, "toml");
    assert_eq!(entry.extensions, vec!["toml".to_string()]);
    assert_eq!(entry.sha256.len(), 64);
    assert!(replaced.is_none());

    // Installing again is the upgrade path, and says what it replaced.
    let (_, replaced) = store.install(&bytes, &wasm).unwrap();
    assert_eq!(replaced.as_deref(), Some(entry.version.as_str()));

    // The loaded registry routes a real tree through the installed plugin.
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("a.toml"), "x = 1\n").unwrap();
    let plugins = Plugins::load(&PluginConfig {
        store_dir: Some(dir.clone()),
        ..Default::default()
    })
    .unwrap();
    let host = dr_strange_llm::preprocess::LocalFiles::new(&tree).unwrap();
    let out = route_tree(&host, None, &plugins).unwrap();
    assert_eq!(out.nodes.len(), 1, "the installed plugin claimed .toml");
    // The router stamped who produced it, name@version as everywhere else.
    assert!(
        out.nodes[0].props.contains_key("_generated_by"),
        "{:?}",
        out.nodes[0].props
    );

    // A file that changed since install is refused by hash, not run.
    let plugin_file = dir.join(&entry.file);
    let mut tampered = std::fs::read(&plugin_file).unwrap();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    std::fs::write(&plugin_file, &tampered).unwrap();
    let err = match Plugins::load(&PluginConfig {
        store_dir: Some(dir.clone()),
        ..Default::default()
    }) {
        Ok(_) => panic!("a tampered plugin must not load"),
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("changed on disk"),
        "unhelpful: {err:#}"
    );

    // Remove clears the record and the file.
    store.remove("toml").unwrap();
    assert!(store.list().unwrap().is_empty());
    assert!(!plugin_file.exists());
    let err = store.remove("toml").unwrap_err();
    assert!(format!("{err:#}").contains("no plugin named"), "{err:#}");

    let _ = std::fs::remove_dir_all(&dir);
}
