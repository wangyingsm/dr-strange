//! A write transaction can be made to give up waiting for the writer slot.
//!
//! `write_gate` serializes writers within one process, and until now it waited
//! forever. That is right for an embedded database — the caller is the only
//! writer, so waiting is just correctness — and wrong as soon as several
//! clients share one `drsg serve` (arch/08 §4.2): one long `bulk_load` or
//! `digest` holds the slot for its whole transaction, blocking every other
//! writer with no way to report why. `set_write_timeout` bounds that wait.
//!
//! These tests hold a real write transaction open on one thread while another
//! tries to begin one, which is exactly the contention a shared server meets.

#![cfg(feature = "native-backend")]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use dr_strange_core::{Database, Error, Properties};

/// A scratch directory that cleans up after itself.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-wtimeout-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(dir: &Dir) -> Database {
    let db = Database::open(&dir.0).expect("open");
    db.create_plane("p", Properties::new()).expect("plane");
    db
}

#[test]
fn a_blocked_writer_gives_up_and_says_why() {
    let dir = Dir::new("expires");
    let db = std::sync::Arc::new(open(&dir));
    db.set_write_timeout(Some(Duration::from_millis(150)));

    let plane = db.plane("p").expect("plane");
    let held = plane.write().expect("first writer takes the slot");

    // A second writer, while the first still holds an open transaction.
    let other = db.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let plane = other.plane("p").expect("plane");
        let start = Instant::now();
        let outcome = plane.write().err();
        tx.send(()).ok();
        (outcome, start.elapsed())
    });

    rx.recv_timeout(Duration::from_secs(10))
        .expect("the blocked writer must fail rather than hang");
    let (err, waited) = waiter.join().expect("waiter thread");

    let err = err.expect("the second writer must not acquire the slot");
    assert!(
        matches!(err, Error::Timeout(_)),
        "a bounded wait that expires is a Timeout, not {err:?}"
    );
    let msg = err.to_string();
    // The message is what a stuck agent's operator reads, so it has to name
    // the cause rather than just the fact.
    assert!(
        msg.contains("another writer"),
        "the message should say what is holding it: {msg}"
    );
    assert!(
        msg.contains("bulk_load"),
        "the message should hint at the usual culprits: {msg}"
    );
    // It waited roughly the budget: not returning instantly (which would mean
    // the bound was ignored), not far beyond it (which would mean unbounded).
    assert!(
        waited >= Duration::from_millis(100),
        "gave up after only {waited:?} — the wait was not honoured"
    );
    assert!(
        waited < Duration::from_secs(5),
        "waited {waited:?} — the bound did not apply"
    );

    drop(held);
}

#[test]
fn the_slot_is_handed_over_when_the_holder_finishes() {
    let dir = Dir::new("handover");
    let db = std::sync::Arc::new(open(&dir));
    db.set_write_timeout(Some(Duration::from_secs(30)));

    let plane = db.plane("p").expect("plane");
    let mut held = plane.write().expect("first writer");
    held.create_node_with_key("first", &["L"], Properties::new())
        .expect("write");

    let other = db.clone();
    let waiter = thread::spawn(move || {
        let plane = other.plane("p").expect("plane");
        // Blocks until the first commits, then must succeed — a generous
        // timeout must not turn into a spurious failure.
        let mut txn = plane.write().expect("second writer after handover");
        txn.create_node_with_key("second", &["L"], Properties::new())
            .expect("write");
        txn.commit().expect("commit");
    });

    // Give the waiter time to actually block before releasing it, so this
    // exercises the wake-up path rather than an uncontended acquire.
    thread::sleep(Duration::from_millis(200));
    held.commit().expect("first commit");

    waiter.join().expect("waiter thread");

    let plane = db.plane("p").expect("plane");
    for key in ["first", "second"] {
        assert!(
            plane.node_by_key(key).expect("lookup").is_some(),
            "{key} should have been written"
        );
    }
}

#[test]
fn unbounded_is_the_default_and_can_be_restored() {
    let dir = Dir::new("default");
    let db = std::sync::Arc::new(open(&dir));

    // No call to `set_write_timeout`: an embedded caller keeps the old
    // behaviour, so a slow writer is waited out rather than failed.
    let plane = db.plane("p").expect("plane");
    let held = plane.write().expect("holder");

    let other = db.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let plane = other.plane("p").expect("plane");
        let txn = plane.write().expect("must wait, not fail");
        tx.send(()).ok();
        drop(txn);
    });

    // It must still be waiting well past any plausible default bound.
    assert!(
        rx.recv_timeout(Duration::from_millis(600)).is_err(),
        "the default must wait, not time out"
    );
    drop(held);
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the waiter should proceed once the slot frees");
    waiter.join().expect("waiter thread");

    // Setting a bound and clearing it again returns to waiting.
    db.set_write_timeout(Some(Duration::from_millis(50)));
    db.set_write_timeout(None);
    let plane = db.plane("p").expect("plane");
    let held = plane.write().expect("holder");
    let other = db.clone();
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let plane = other.plane("p").expect("plane");
        let txn = plane.write().expect("cleared bound must wait again");
        tx.send(()).ok();
        drop(txn);
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "clearing the timeout must restore the unbounded wait"
    );
    drop(held);
    rx.recv_timeout(Duration::from_secs(10)).expect("proceeds");
    waiter.join().expect("waiter thread");
}

#[test]
fn a_sub_millisecond_bound_does_not_become_unbounded() {
    let dir = Dir::new("submilli");
    let db = std::sync::Arc::new(open(&dir));
    // 0ms would collide with the sentinel for "wait forever", so a tiny
    // timeout must round up rather than down.
    db.set_write_timeout(Some(Duration::from_micros(10)));

    let plane = db.plane("p").expect("plane");
    let held = plane.write().expect("holder");

    let other = db.clone();
    let waiter = thread::spawn(move || {
        let plane = other.plane("p").expect("plane");
        plane.write().err()
    });
    let err = waiter
        .join()
        .expect("waiter thread")
        .expect("a 10µs bound must expire, not wait forever");
    assert!(
        matches!(err, Error::Timeout(_)),
        "expected Timeout: {err:?}"
    );

    drop(held);
}
