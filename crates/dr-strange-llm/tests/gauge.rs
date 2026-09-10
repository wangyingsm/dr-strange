//! The plugin memory gauge, alone in its own binary.
//!
//! `plugin_memory_bytes` counts what every plugin in the *process* holds, so
//! a sibling test that loads a fixture moves the figure between this one's
//! reads. Cargo gives each integration file its own process, which is the
//! only isolation a global counter can be given.
#![cfg(feature = "plugins")]

mod common;

use common::{fixture, scratch};
use dr_strange_llm::plugin_memory_bytes;
use dr_strange_llm::preprocess::{Input, Limits, Preprocessor};

/// What the plugins hold is a gauge the process can read: loading a plugin
/// adds its compiled image, a call adds the guest's memory while it runs and
/// takes it back when it returns, and dropping the plugin takes the image
/// back — so the figure a dashboard shows beside the resident set is what is
/// held *now*, not what was ever allocated.
#[test]
fn what_the_plugins_hold_is_a_gauge() {
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
