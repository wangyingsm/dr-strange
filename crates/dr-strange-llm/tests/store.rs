//! The plugin store's compiled artifacts: written at install, loaded instead
//! of compiling, and rebuilt from the verified wasm whenever they cannot be
//! trusted or used. Runs against the committed sandbox fixture, so it needs
//! no wasm toolchain.
#![cfg(feature = "plugins")]

use std::path::{Path, PathBuf};

use dr_strange_llm::preprocess::{Limits, PluginStore};

fn fixture_bytes() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture.wasm");
    std::fs::read(path).expect("the committed fixture exists")
}

fn fresh_store(tag: &str) -> (PathBuf, PluginStore) {
    let dir = std::env::temp_dir().join(format!("drsg-store-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = PluginStore::open(dir.clone()).unwrap();
    (dir, store)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn install_stores_the_compiled_form_and_load_uses_it() {
    let (dir, store) = fresh_store("compiled");
    let (entry, _) = store.install(&fixture_bytes(), "fixture").unwrap();

    let compiled = entry
        .compiled
        .clone()
        .expect("install records the artifact");
    assert_eq!(compiled, format!("{}-{}.cwasm", entry.name, entry.version));
    let artifact = std::fs::read(dir.join(&compiled)).expect("the artifact was written");
    assert_eq!(
        entry.compiled_sha256.as_deref(),
        Some(sha256_hex(&artifact).as_str()),
        "the pin is the artifact's own hash"
    );

    // A load through the artifact: nothing to recompile, so the registry is
    // left exactly as install wrote it.
    let before = std::fs::read_to_string(dir.join("registry.toml")).unwrap();
    let plugins = store
        .load_all(&Default::default(), &Limits::default())
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("registry.toml")).unwrap(),
        before
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_or_damaged_artifact_is_rebuilt_from_the_wasm() {
    let (dir, store) = fresh_store("rebuilt");
    let (entry, _) = store.install(&fixture_bytes(), "fixture").unwrap();
    let path = dir.join(entry.compiled.as_ref().unwrap());

    // Gone: the load still succeeds, and puts the artifact back.
    std::fs::remove_file(&path).unwrap();
    store
        .load_all(&Default::default(), &Limits::default())
        .unwrap();
    assert!(path.exists(), "a load without an artifact writes one");
    let recorded = store.list().unwrap().remove(0);
    assert_eq!(
        recorded.compiled_sha256.as_deref(),
        Some(sha256_hex(&std::fs::read(&path).unwrap()).as_str())
    );

    // Damaged: off its pin, so it is not run — rebuilt and re-pinned instead.
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    store
        .load_all(&Default::default(), &Limits::default())
        .unwrap();
    let rebuilt = std::fs::read(&path).unwrap();
    assert_ne!(rebuilt, bytes, "the damaged artifact was replaced");
    let recorded = store.list().unwrap().remove(0);
    assert_eq!(
        recorded.compiled_sha256.as_deref(),
        Some(sha256_hex(&rebuilt).as_str())
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fuel_off_compiles_and_leaves_the_artifact_alone() {
    let (dir, store) = fresh_store("unmetered");
    let (entry, _) = store.install(&fixture_bytes(), "fixture").unwrap();
    let path = dir.join(entry.compiled.as_ref().unwrap());
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    let unmetered = Limits {
        fuel: None,
        ..Limits::default()
    };
    let plugins = store.load_all(&Default::default(), &unmetered).unwrap();
    assert_eq!(plugins.len(), 1, "an unmetered load compiles from the wasm");
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before,
        "the metered artifact is neither used nor rewritten"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_takes_the_artifact_with_the_wasm() {
    let (dir, store) = fresh_store("remove");
    let (entry, _) = store.install(&fixture_bytes(), "fixture").unwrap();
    let artifact = dir.join(entry.compiled.as_ref().unwrap());
    assert!(artifact.exists());
    store.remove(&entry.name).unwrap();
    assert!(!dir.join(&entry.file).exists());
    assert!(!artifact.exists());

    let _ = std::fs::remove_dir_all(&dir);
}
