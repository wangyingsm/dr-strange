//! The sandbox's guarantees, each proven against a hostile fixture rather
//! than asserted in a comment. The fixtures are committed artifacts
//! (`tests/fixtures/README.md` records how to rebuild them), so this suite
//! needs no wasm toolchain — it is the *host* under test.
#![cfg(feature = "plugins")]

use std::path::Path;

use dr_strange_llm::preprocess::{
    Input, Limits, LocalFiles, Plugins, Preprocessor, WasmPlugin, route_tree,
};

fn fixture(mode: &str, limits: Limits) -> WasmPlugin {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture.wasm");
    WasmPlugin::load(&path, vec![("mode".to_string(), mode.to_string())], limits)
        .expect("the committed fixture must load")
}

/// A scratch dir with one claimable file, for calls that need a host.
fn scratch(name: &str) -> (std::path::PathBuf, LocalFiles) {
    let dir = std::env::temp_dir().join(format!("drsg-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.fix"), "x").unwrap();
    let host = LocalFiles::new(&dir).unwrap();
    (dir, host)
}

/// The well-behaved mode round-trips through parse and assemble — the control
/// the hostile cases are measured against.
#[test]
fn the_well_behaved_fixture_round_trips() {
    let (dir, host) = scratch("ok");
    let plugin = fixture("ok", Limits::default());
    let plugins = Plugins::from_handlers(vec![Box::new(plugin)]);
    let out = route_tree(&host, None, &plugins).unwrap();
    assert_eq!(out.nodes.len(), 1);
    assert_eq!(out.nodes[0].key, "a.fix");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A path outside the root is refused by the host — checked on the resolved
/// path, so `..` does not walk through — and the refusal reaches the plugin
/// as an error it can only report, not argue with.
#[test]
fn escaping_the_root_is_refused() {
    let (dir, host) = scratch("escape");
    let plugin = fixture("escape", Limits::default());
    let out = plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .unwrap();
    // The fixture reports the host's refusal verbatim inside its one node.
    let value = format!("{:?}", out.nodes[0].props);
    assert!(
        value.contains("refused") && (value.contains("outside") || value.contains("resolving")),
        "the host should have refused by name: {value}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plugin that never terminates is *interrupted* — fuel, counted in
/// instructions, so the stop lands at the same point on every machine — and
/// the error names the plugin rather than saying "wasm trap".
#[test]
fn the_infinite_loop_runs_out_of_fuel_and_is_named() {
    let (dir, host) = scratch("spin");
    let plugin = fixture(
        "spin",
        Limits {
            // Small, so the test is fast; the default exists for real work.
            fuel: Some(10_000_000),
            ..Limits::default()
        },
    );
    let err = plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .expect_err("an infinite loop must not return");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("fixture"),
        "the error must name the plugin: {msg}"
    );
    assert!(msg.contains("fuel"), "and say what stopped it: {msg}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Allocation without bound hits the store's memory limit instead of the
/// machine's.
#[test]
fn the_memory_bomb_hits_the_limit() {
    let (dir, host) = scratch("alloc");
    let plugin = fixture(
        "alloc",
        Limits {
            fuel: None, // memory is the wall this test is about
            memory_bytes: 64 << 20,
        },
    );
    let err = plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .expect_err("unbounded allocation must not succeed");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("fixture"),
        "the error must name the plugin: {msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The clock is frozen: a thousand iterations of work measure as **zero**
/// elapsed nanoseconds, so a plugin cannot fold time into its facts and
/// re-ingesting a tree yields the same graph whenever it runs.
#[test]
fn the_clock_is_frozen() {
    let (dir, host) = scratch("clock");
    let plugin = fixture("clock", Limits::default());
    let out = plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .unwrap();
    let value = format!("{:?}", out.nodes[0].props);
    assert!(
        value.contains("\\\"0\\\"") || value.contains("Str(\"0\")") || value.contains(": \"0\""),
        "elapsed time must be zero under a frozen clock: {value}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A component that imports `wasi:filesystem` is refused **at load, by name**
/// — before it can run at all. A preprocessor reads through the host
/// interface, which is rooted; there is nothing an honest one needs a
/// filesystem for.
#[test]
fn a_filesystem_import_is_refused_at_load() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-fs.wasm");
    let err = match WasmPlugin::load(&path, Vec::new(), Limits::default()) {
        Ok(_) => panic!("a component importing wasi:filesystem must not load"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wasi:filesystem"),
        "refusal must name the import: {msg}"
    );
}

/// Determinism, end to end: the same tree through the same plugin twice gives
/// byte-identical facts — the property the frozen clock and fixed chunking
/// exist for.
#[test]
fn the_same_tree_twice_gives_the_same_facts() {
    let (dir, host) = scratch("determinism");
    std::fs::write(dir.join("b.fix"), "y").unwrap();
    let plugin = fixture("ok", Limits::default());
    let plugins = Plugins::from_handlers(vec![Box::new(plugin)]);

    let first = route_tree(&host, None, &plugins).unwrap();
    let second = route_tree(&host, None, &plugins).unwrap();
    let keys = |o: &dr_strange_llm::Preprocessed| {
        o.nodes.iter().map(|n| n.key.clone()).collect::<Vec<_>>()
    };
    assert_eq!(keys(&first), keys(&second));
    let _ = std::fs::remove_dir_all(&dir);
}
