//! Smoke test for the subscriber setup. It installs a *process-global*
//! subscriber, so everything lives in one test (one process = one global) and
//! logs to a throwaway dir under the OS temp so nothing lands in the repo.

use std::path::PathBuf;

#[test]
fn init_installs_subscriber_and_is_idempotent() {
    let dir: PathBuf = std::env::temp_dir().join(format!("drsg-log-test-{}", std::process::id()));

    // SAFETY: this runs at the very start of the test binary, before any other
    // thread could read the environment (Rust 2024 makes `set_var` unsafe).
    unsafe {
        std::env::set_var("DRSG_LOG_DIR", &dir);
    }

    let guard = dr_strange_log::init("drsg-test");
    tracing::info!(marker = "coverage", "hello from the log smoke test");

    // The rolling appender creates its directory eagerly on construction.
    assert!(dir.exists(), "log dir {dir:?} should have been created");

    // A second init can't replace the global subscriber — exercises the
    // already-initialized branch (it warns and returns a fresh guard).
    let _second = dr_strange_log::init("drsg-test");

    // Dropping the guard flushes and stops the non-blocking file writer.
    drop(guard);
}
