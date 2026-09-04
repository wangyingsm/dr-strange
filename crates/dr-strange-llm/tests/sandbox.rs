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

/// A component may *import* `wasi:filesystem` — a guest toolchain's runtime
/// often does before the plugin's first line runs, which is why the import
/// alone is not refused — but the grant behind it is an **empty preopen
/// table**: there is no directory handle to read, probe, or enumerate, so
/// the read itself fails and the failure reaches the caller naming the
/// plugin.
#[test]
fn a_filesystem_import_is_granted_nothing() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-fs.wasm");
    let plugin = WasmPlugin::load(&path, Vec::new(), Limits::default())
        .expect("a filesystem import alone must not refuse the load");
    let (dir, host) = scratch("fs");
    let err = plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .expect_err("a read through an empty preopen table must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("grabby"),
        "the error must name the plugin: {msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A component that imports `wasi:sockets` is refused **at load, by name** —
/// before it can run at all. Unlike the filesystem import, which a guest
/// runtime plants before the plugin's first line runs, nothing needs sockets
/// to start: that import is intent.
#[test]
fn a_sockets_import_is_refused_at_load() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-net.wasm");
    let err = match WasmPlugin::load(&path, Vec::new(), Limits::default()) {
        Ok(_) => panic!("a component importing wasi:sockets must not load"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wasi:sockets"),
        "refusal must name the import: {msg}"
    );
}

/// `wasi:random` deals the **same bytes every run** — a guest runtime seeds
/// hash and map iteration order from it (Go does), so real entropy would make
/// the same tree emit facts in a different order on every ingest.
#[test]
fn entropy_is_dealt_from_a_fixed_deck() {
    let (dir, host) = scratch("rand");
    let plugin = fixture("rand", Limits::default());
    let draw = |plugin: &WasmPlugin| {
        let out = plugin
            .preprocess(
                &Input::Files {
                    paths: &["a.fix".to_string()],
                },
                &host,
            )
            .unwrap();
        format!("{:?}", out.nodes[0].props)
    };
    let first = draw(&plugin);
    let second = draw(&plugin);
    assert_eq!(
        first, second,
        "two draws in fresh stores must see identical entropy"
    );
    let _ = std::fs::remove_dir_all(&dir);
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

/// One file the plugin cannot get through is skipped; the rest of the tree
/// still lands. The real case was a `.pb.go` whose thousand-term string
/// concatenation walked the go plugin's printer off its stack: before this,
/// that single file refused the whole repository, and under `serve watch` it
/// refused every fold after it — the watcher stopped and the graph stayed
/// empty while the server kept answering.
#[test]
fn one_file_the_plugin_cannot_parse_is_skipped_not_the_tree() {
    let (dir, host) = scratch("stack-one");
    std::fs::write(dir.join("deep.fix"), "boom").unwrap();
    std::fs::write(dir.join("b.fix"), "y").unwrap();
    let plugin = fixture("stack", Limits::default());
    let out = plugin
        .preprocess(
            &Input::Files {
                paths: &[
                    "a.fix".to_string(),
                    "b.fix".to_string(),
                    "deep.fix".to_string(),
                ],
            },
            &host,
        )
        .expect("one impossible file must not refuse the other two");

    let keys: Vec<&str> = out.nodes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, vec!["a.fix", "b.fix"], "the good files still land");
    assert_eq!(out.report.skipped, 1);
    let note = out.report.notes.join(" ");
    assert!(
        note.contains("deep.fix") && note.contains("fixture"),
        "the report should name the file and the plugin: {note}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Failing on *every* file is a different animal — a plugin that does not work
/// here — and stays fatal, because a plane quietly missing every fact is the
/// worst of the available answers.
#[test]
fn a_plugin_that_fails_on_every_file_is_still_fatal() {
    let (dir, host) = scratch("stack-all");
    std::fs::write(dir.join("deep-1.fix"), "boom").unwrap();
    std::fs::write(dir.join("deep-2.fix"), "boom").unwrap();
    let plugin = fixture("stack", Limits::default());
    let err = plugin
        .preprocess(
            &Input::Files {
                paths: &["deep-1.fix".to_string(), "deep-2.fix".to_string()],
            },
            &host,
        )
        .expect_err("nothing got through, so nothing should be reported as fine");
    let said = format!("{err:#}");
    assert!(
        said.contains("all 2") && said.contains("fixture"),
        "the error should say the plugin failed on everything: {said}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the plugins hold is a gauge the process can read: loading a plugin
/// adds its compiled image, a call adds the guest's memory while it runs and
/// takes it back when it returns, and dropping the plugin takes the image
/// back — so the figure a dashboard shows beside the resident set is what is
/// held *now*, not what was ever allocated.
#[test]
fn what_the_plugins_hold_is_a_gauge() {
    use dr_strange_llm::plugin_memory_bytes;
    let (dir, host) = scratch("gauge");
    let before = plugin_memory_bytes();
    let plugin = fixture("ok", Limits::default());
    let loaded = plugin_memory_bytes();
    assert!(
        loaded > before,
        "a loaded plugin holds its compiled image: {before} -> {loaded}"
    );
    plugin
        .preprocess(
            &Input::Files {
                paths: &["a.fix".to_string()],
            },
            &host,
        )
        .unwrap();
    assert_eq!(
        plugin_memory_bytes(),
        loaded,
        "a finished call gives the guest's memory back"
    );
    drop(plugin);
    assert_eq!(
        plugin_memory_bytes(),
        before,
        "a dropped plugin gives its image back"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
