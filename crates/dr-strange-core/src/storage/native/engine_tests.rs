//! The native engine end to end: commit durability, the WAL's torn-tail
//! replay, flush and compaction, and the crash windows around them.

use super::*;
use std::sync::mpsc;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-native-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn commit(e: &NativeEngine, key: &[u8], value: &[u8]) -> u64 {
    let mut w = e.begin_write().unwrap();
    w.put(TableId::Nodes, key, value).unwrap();
    let seq = w.snapshot + 1;
    w.commit().unwrap();
    seq
}

fn get(e: &NativeEngine, key: &[u8]) -> Option<Vec<u8>> {
    e.begin_read().unwrap().get(TableId::Nodes, key).unwrap()
}

/// Every version of `key` across the on-disk runs, newest first.
fn sst_versions(e: &NativeEngine, key: &[u8]) -> Vec<u64> {
    let store = e.store.read().unwrap_or_else(|e| e.into_inner());
    let mut all = BTreeMap::new();
    for run in &store.ssts {
        all.extend(run.entries().map(|e| e.unwrap()));
    }
    all.keys()
        .filter(|(_, k, _)| k == key)
        .map(|(_, _, Reverse(s))| *s)
        .collect()
}

#[test]
fn a_small_retention_window_reclaims_versions_below_it_on_compaction() {
    let dir = Dir::new("retention");
    // 1-byte threshold: every commit becomes its own SST, so the fifth
    // trips compaction (`> COMPACTION_TRIGGER` runs).
    let e = NativeEngine::open_with_threshold(&dir.0, 1).unwrap();
    e.set_retention(Some(2));
    let mut last = 0;
    for i in 1..=6u8 {
        last = commit(&e, b"k", &[i]);
    }
    let floor = last - 2;
    // The compaction ran at commit 5 with floor 3: versions 1 and 2 were
    // below what any reader (or retention) could reach.
    let versions = sst_versions(&e, b"k");
    assert!(
        !versions.contains(&1) && !versions.contains(&2),
        "versions below the retention floor must be reclaimed, got {versions:?}"
    );
    assert!(
        versions.contains(&(last - 1)) && versions.contains(&last),
        "recent versions stay, got {versions:?}"
    );
    // What retention promises is still readable.
    let at = e.begin_read_at(floor).unwrap();
    assert_eq!(
        at.get(TableId::Nodes, b"k").unwrap(),
        Some(vec![floor as u8])
    );
    assert!(matches!(
        e.begin_read_at(floor - 1),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(get(&e, b"k"), Some(vec![6]));
}

#[test]
fn a_torn_wal_tail_is_ignored_and_earlier_commits_survive() {
    let dir = Dir::new("torn");
    {
        let e = NativeEngine::open(&dir.0).unwrap();
        commit(&e, b"a", b"1");
        commit(&e, b"b", b"2");
    }
    // A crash mid-append: a header promising 100 body bytes, of which only
    // a handful made it to disk.
    {
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.0.join("wal"))
            .unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap();
        f.write_all(&0xdead_beefu32.to_le_bytes()).unwrap();
        f.write_all(b"partial").unwrap();
    }
    let torn_len = std::fs::metadata(dir.0.join("wal")).unwrap().len();

    let e = NativeEngine::open(&dir.0).unwrap();
    assert_eq!(e.committed_seq(), 2, "both intact commits replayed");
    assert_eq!(get(&e, b"a"), Some(b"1".to_vec()));
    assert_eq!(get(&e, b"b"), Some(b"2".to_vec()));
    assert!(
        std::fs::metadata(dir.0.join("wal")).unwrap().len() < torn_len,
        "the torn tail is cut so the next append starts on a record boundary"
    );

    // Writing after recovery appends cleanly, and a second reopen sees
    // everything.
    commit(&e, b"c", b"3");
    drop(e);
    let e = NativeEngine::open(&dir.0).unwrap();
    assert_eq!(e.committed_seq(), 3);
    assert_eq!(get(&e, b"c"), Some(b"3".to_vec()));
}

#[test]
fn wal_record_len_rejects_a_body_the_prefix_cannot_describe() {
    assert_eq!(wal_record_len(0).unwrap(), 0);
    assert_eq!(wal_record_len(MAX_WAL_RECORD_LEN).unwrap(), u32::MAX);
    #[cfg(target_pointer_width = "64")]
    {
        // Without the check this would wrap to 0 and write a record whose
        // prefix lies about its body.
        let err = wal_record_len(MAX_WAL_RECORD_LEN + 1).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        let err = wal_record_len(1 << 33).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    }
}

#[test]
fn a_commit_is_ok_once_durable_even_when_the_flush_it_triggers_fails() {
    // A commit's batch is durable (WAL fsync) and visible (published) before
    // any flush runs, so a flush failure must not turn into an `Err` that a
    // caller reads as "nothing landed". The SST write is failed through the
    // injection seam rather than directory permissions, which root ignores
    // and non-unix lacks: the WAL is already open, so its append + fsync
    // is unaffected — the shape of a disk that still takes the log but
    // refuses a new file.
    let dir = Dir::new("maint");
    let e = NativeEngine::open_with_threshold(&dir.0, 1).unwrap();
    commit(&e, b"a", b"1");
    assert_eq!(e.last_maintenance_error(), None);

    e.sst_write_fault.arm();
    let mut w = e.begin_write().unwrap();
    w.put(TableId::Nodes, b"b", b"2").unwrap();
    w.commit()
        .expect("the batch is durable and published; commit is Ok");
    assert_eq!(get(&e, b"b"), Some(b"2".to_vec()), "published");
    assert_eq!(e.committed_seq(), 2);
    let err = e
        .last_maintenance_error()
        .expect("the failed flush is remembered");
    assert!(err.contains("injected fault"), "{err}");
    {
        let store = e.store.read().unwrap_or_else(|e| e.into_inner());
        assert!(!store.mem.is_empty(), "the memtable stays for the retry");
        assert_eq!(store.ssts.len(), 1, "only the first commit's run exists");
    }

    // Once the cause clears, the next commit retries maintenance and
    // the slot resets.
    commit(&e, b"c", b"3");
    assert_eq!(e.last_maintenance_error(), None);
    let store = e.store.read().unwrap_or_else(|e| e.into_inner());
    assert!(
        store.mem.is_empty(),
        "the retried flush emptied the memtable"
    );
    assert_eq!(store.ssts.len(), 2);
    drop(store);

    // And nothing was lost across a reopen.
    drop(e);
    let e = NativeEngine::open(&dir.0).unwrap();
    assert_eq!(get(&e, b"b"), Some(b"2".to_vec()));
    assert_eq!(get(&e, b"c"), Some(b"3".to_vec()));
}

#[test]
fn commits_after_a_failed_wal_truncation_fsync_start_at_offset_zero() {
    // The flush cuts the WAL once its SST is durable. If the fsync of that
    // cut fails, the commit that triggered the flush is still Ok and the
    // engine keeps taking commits — so the write cursor must already be
    // at 0, or every later batch lands behind a hole of zeros that replay
    // reads as an empty torn tail and the next open discards.
    let dir = Dir::new("wal-cursor");
    // Threshold so that one big value flushes and a small one does not.
    let e = NativeEngine::open_with_threshold(&dir.0, 64).unwrap();
    e.wal_sync_fault.arm();
    commit(&e, b"big", &[7u8; 128]); // flushes; the truncate's fsync fails
    let err = e
        .last_maintenance_error()
        .expect("the failed truncation fsync is remembered");
    assert!(err.contains("injected fault"), "{err}");
    assert_eq!(get(&e, b"big"), Some(vec![7u8; 128]));

    commit(&e, b"small", b"v"); // no flush: lives only in the WAL
    assert_eq!(e.last_maintenance_error(), None);
    {
        let mut wal = e.wal.lock().unwrap_or_else(|e| e.into_inner());
        wal.flush().unwrap();
        let f = wal.get_mut();
        let len = f.metadata().unwrap().len();
        let mut head = [0u8; 4];
        f.seek(SeekFrom::Start(0)).unwrap();
        f.read_exact(&mut head).unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        assert!(
            len < 64,
            "the WAL holds one small record, not a hole: {len}"
        );
        assert_ne!(head, [0u8; 4], "the record's length prefix is at offset 0");
    }

    drop(e);
    let e = NativeEngine::open(&dir.0).unwrap();
    assert_eq!(get(&e, b"big"), Some(vec![7u8; 128]), "from the run");
    assert_eq!(
        get(&e, b"small"),
        Some(b"v".to_vec()),
        "the commit after the failed fsync survives a reopen"
    );
    assert_eq!(e.committed_seq(), 2);
}

#[test]
fn a_reader_is_served_while_a_flush_writes_its_sst() {
    // The flush's write+fsync used to run under the store write lock, so
    // every reader stalled for the whole of it. Park a flush right after
    // its SST write (the point where that lock used to be held) and check
    // a reader on another thread completes — and sees the just-committed
    // value — while the flush is parked.
    let dir = Dir::new("flush-readers");
    let e = Arc::new(NativeEngine::open_with_threshold(&dir.0, 1).unwrap());
    let gate = Arc::new(std::sync::Barrier::new(2));
    let parked = e.flush_pause.arm(gate.clone());

    let writer = {
        let e = e.clone();
        std::thread::spawn(move || commit(&e, b"k", b"v")) // parks inside its flush
    };
    // Learn the flush is parked without taking any store lock ourselves
    // (that would hang, not fail, if the flush still held the writer).
    parked
        .recv_timeout(Duration::from_secs(5))
        .expect("the commit reached its flush");

    let (rtx, rrx) = mpsc::channel();
    {
        let e = e.clone();
        std::thread::spawn(move || {
            rtx.send(get(&e, b"k")).unwrap();
        });
    }
    let read = rrx.recv_timeout(Duration::from_secs(2));
    gate.wait(); // release the flush whatever happened, so nothing hangs
    assert_eq!(writer.join().unwrap(), 1);
    assert_eq!(
        read.ok(),
        Some(Some(b"v".to_vec())),
        "a reader must not wait on a flush's I/O"
    );
    // And the flush completed normally once released.
    let store = e.store.read().unwrap_or_else(|e| e.into_inner());
    assert!(store.mem.is_empty() && store.ssts.len() == 1);
}

#[test]
fn the_wal_observer_runs_outside_its_slots_mutex() {
    // An observer that blocks (a slow subscriber) must not wedge
    // `set_wal_observer`, which previously waited on the same mutex the
    // observer was invoked under.
    let dir = Dir::new("observer");
    let e = Arc::new(NativeEngine::open(&dir.0).unwrap());
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    e.set_wal_observer(Some(Arc::new(move |_batch| {
        let _ = entered_tx.send(());
        let _ = go_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
    })));

    let committer = {
        let e = e.clone();
        std::thread::spawn(move || commit(&e, b"a", b"1"))
    };
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the observer is reached");

    // While the observer is parked mid-callback, re-registering must go
    // through; the deadline is what turns the old deadlock into a failure.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let swapper = {
        let e = e.clone();
        std::thread::spawn(move || {
            e.set_wal_observer(None);
            let _ = done_tx.send(());
        })
    };
    let swapped = done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    // Release the observer either way so the threads can be joined.
    let _ = go_tx.send(());
    committer.join().unwrap();
    swapper.join().unwrap();
    assert!(
        swapped,
        "set_wal_observer blocked behind a running observer"
    );
    assert_eq!(get(&e, b"a"), Some(b"1".to_vec()));
}
