//! One process at a time may open a database directly (upstream issue #1).
//!
//! `write_gate` serializes the writer *within* a process. Nothing used to stop
//! a second process opening the same directory: each got its own WAL offset and
//! its own `next_sst` counter, and they overwrote each other's work. Measured
//! before the lock existed — two concurrent `drsg import` runs of 200 nodes
//! each left a database holding 200, with `drsg check` calling it healthy.
//! Silent loss that survives the integrity scan is the worst kind, so the
//! second open is refused instead.
//!
//! The lock is advisory and per-open-handle, so these in-process tests exercise
//! exactly the mechanism a second process hits.

#![cfg(feature = "native-backend")]

use dr_strange_core::{Database, Properties};

/// A scratch directory that cleans up after itself.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-excl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_second_open_is_refused_while_the_first_is_alive() {
    let dir = Dir::new("second");
    let first = Database::open(&dir.0).expect("first open");

    let err = Database::open(&dir.0).expect_err("the second open must be refused");
    let msg = err.to_string();
    // The message has to say what is wrong and what to do instead — this is
    // the error a user meets when two editors point an MCP server at one path.
    assert!(
        msg.contains("already open by another process"),
        "unhelpful message: {msg}"
    );
    assert!(
        msg.contains("drsg serve"),
        "the message should point at the supported way to share: {msg}"
    );
    assert!(
        msg.contains(&dir.0.display().to_string()),
        "the message should name the database: {msg}"
    );

    drop(first);
}

#[test]
fn closing_the_first_hands_the_database_over() {
    let dir = Dir::new("handover");
    let first = Database::open(&dir.0).expect("first open");
    assert!(Database::open(&dir.0).is_err());

    // Dropping the engine releases the lock, so the ordinary close-then-reopen
    // path — which every CLI invocation depends on — keeps working.
    drop(first);
    let second = Database::open(&dir.0).expect("reopen after close");
    drop(second);

    // And again, to be sure the lock is not merely leaked once.
    let third = Database::open(&dir.0).expect("reopen twice");
    drop(third);
}

#[test]
fn the_lock_file_is_not_mistaken_for_data() {
    let dir = Dir::new("inert");
    {
        let db = Database::open(&dir.0).expect("open");
        let plane = db
            .create_plane("p", Properties::new())
            .expect("create plane");
        let mut txn = plane.write().expect("begin");
        txn.create_node_with_key("k1", &["L"], Properties::new())
            .expect("write");
        txn.commit().expect("commit");
    }
    // The lock file sits in the same directory as the WAL and the SSTs;
    // `sst::list` matches `sst-<n>` only, so it must survive a reopen unnoticed.
    assert!(dir.0.join("LOCK").exists(), "the lock file should persist");
    let db = Database::open(&dir.0).expect("reopen with a LOCK file present");
    let plane = db.plane("p").expect("plane survives");
    assert!(
        plane.node_by_key("k1").expect("lookup").is_some(),
        "data written before the reopen must still be there"
    );
}
