//! Native LSM storage engine (arch/01 v2) — the hand-rolled alternative to
//! redb, selected by the `native-backend` feature.
//!
//! **Structure.** A write lands in the write-ahead log (durability) and a
//! versioned in-memory *memtable*. When the memtable grows past a threshold it
//! is flushed to an immutable, sorted **SST** file ([`sst`]) and the WAL is
//! rotated. A read merges the live memtable over the SSTs, newest run first.
//! On open, the SSTs are loaded and the (un-flushed) WAL tail is replayed.
//!
//! **MVCC by sequence number.** Each committed transaction gets one
//! monotonically increasing sequence; every version it writes is stamped with
//! it. A reader pins the latest committed sequence at `begin_read` and only
//! sees versions at or below it — so a reader opened before a commit keeps the
//! old value, holding no locks across its lifetime. (SSTs are immutable and, in
//! this phase, never removed, so an old snapshot's data is always reachable;
//! reclaiming SSTs no live reader needs is a later phase.)
//!
//! **Atomic, durable commit.** A whole transaction is one length-prefixed,
//! CRC32-checked WAL batch record, `fsync`ed before the memtable is published.
//! A flush writes its SST (temp file + fsync + rename) before truncating the
//! WAL, so a crash never loses committed data — at worst the next open replays
//! WAL records already captured in an SST, which is idempotent.
//!
//! The on-disk form is a **directory**: the WAL at `<path>/wal`, SSTs at
//! `<path>/sst-NNNNNN`.

mod sst;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use moka::sync::Cache;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, backend};
use crate::storage::engine::{
    KvPair, ReadTransaction, ReplicatedBatch, ReplicatedOp, StorageEngine, TableId, WalObserver,
    WriteTransaction, prefix_successor,
};

/// Serializes writers within one process, with an optional deadline.
///
/// A plain `Mutex<()>` would be shorter, but `std`'s has no timed acquire and
/// an unbounded one is a hazard as soon as several clients share a server
/// (arch/08 §4.2): a long `bulk_load` blocks every other writer with no way to
/// say why. A `Condvar` over a `held` flag gives the same mutual exclusion plus
/// a bound. Poisoning is ignored throughout — the flag is a plain `bool` that
/// no panic can leave torn, and refusing every future write because one writer
/// panicked would turn a transient bug into a dead database.
struct WriteGate {
    held: Mutex<bool>,
    free: Condvar,
}

impl WriteGate {
    fn new() -> Self {
        Self {
            held: Mutex::new(false),
            free: Condvar::new(),
        }
    }

    /// Take the writer slot, waiting at most `timeout` (`None` = forever).
    fn acquire(&self, timeout: Option<Duration>) -> Result<WriteGuard<'_>> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        match timeout {
            None => {
                while *held {
                    held = self.free.wait(held).unwrap_or_else(|e| e.into_inner());
                }
            }
            Some(limit) => {
                // Deadline, not a per-wait budget: a spurious wake-up must not
                // silently restart the clock and turn a bounded wait into an
                // unbounded one.
                let deadline = Instant::now() + limit;
                while *held {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(timed_out(limit));
                    }
                    let (guard, wait) = self
                        .free
                        .wait_timeout(held, left)
                        .unwrap_or_else(|e| e.into_inner());
                    held = guard;
                    // Re-check the flag rather than trusting the timeout flag
                    // alone: waking exactly as the slot frees is a success.
                    if wait.timed_out() && *held {
                        return Err(timed_out(limit));
                    }
                }
            }
        }
        *held = true;
        Ok(WriteGuard { gate: self })
    }
}

fn timed_out(limit: Duration) -> Error {
    Error::Timeout(format!(
        "waited {limit:?} for the write transaction slot; another writer still holds it. \
         Long-running writes (bulk_load, digest) hold it for their whole transaction."
    ))
}

/// Releases the writer slot on drop and wakes one waiter.
struct WriteGuard<'a> {
    gate: &'a WriteGate,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        let mut held = self.gate.held.lock().unwrap_or_else(|e| e.into_inner());
        *held = false;
        // One waiter: the slot is exclusive, so waking the rest only to have
        // them re-sleep is wasted work.
        self.gate.free.notify_one();
    }
}

/// Shared cache of decoded-on-read SST data blocks, keyed by `(sst id, block
/// offset)` and byte-weighted, so repeated reads of a hot block skip the
/// `pread`. Ids are unique per engine instance (a monotonic counter), and the
/// cache is dropped with the engine, so a compacted-away run's entries age out.
pub(super) type BlockCache = Cache<(u64, u64), Arc<Vec<u8>>>;

/// Byte budget for the block cache.
const BLOCK_CACHE_BYTES: u64 = 32 * 1024 * 1024;

fn new_block_cache() -> Arc<BlockCache> {
    Arc::new(
        Cache::builder()
            .max_capacity(BLOCK_CACHE_BYTES)
            .weigher(|_k, v: &Arc<Vec<u8>>| v.len().min(u32::MAX as usize) as u32)
            .build(),
    )
}

/// Flush the memtable to an SST once its live bytes exceed this. SST + WAL then
/// bound memory and log growth; below it everything stays in the memtable.
const DEFAULT_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;

/// Compact (merge all runs into one) once this many SSTs accumulate — bounds the
/// number of runs a read must consult and lets shadowed versions/tombstones be
/// reclaimed. A single-tier "merge everything" policy; leveled/size-tiered
/// compaction (less write amplification) is a later refinement.
const COMPACTION_TRIGGER: usize = 4;

/// A memtable entry: a value, or a tombstone marking a deletion.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Put(Vec<u8>),
    Del,
}

impl Op {
    /// The put value, or `None` for a tombstone — how a read reports "deleted".
    fn into_value(self) -> Option<Vec<u8>> {
        match self {
            Op::Put(v) => Some(v),
            Op::Del => None,
        }
    }

    /// Heap bytes this op's value pins (memtable size accounting).
    fn value_len(&self) -> usize {
        match self {
            Op::Put(v) => v.len(),
            Op::Del => 0,
        }
    }
}

/// Memtable key: `(table, user_key, Reverse(seq))`. `Reverse` orders a key's
/// versions newest-first, so the first version with `seq <= snapshot` in a
/// forward scan is the visible one.
type MemKey = (u8, Vec<u8>, Reverse<u64>);

/// The versioned memtable, the SSTs it has been flushed into, and the committed
/// high-water sequence — kept under one lock so a reader sees them consistently.
#[derive(Default)]
struct Store {
    mem: BTreeMap<MemKey, Op>,
    /// Approximate live byte size of `mem`, for the flush threshold.
    mem_bytes: usize,
    committed_seq: u64,
    /// Flushed runs, oldest first (a later run shadows an earlier one).
    ssts: Vec<Arc<sst::Sst>>,
    /// Next SST file number.
    next_sst: u64,
}

/// Take the directory's exclusive advisory lock, or say who already has it.
///
/// `sst::list` only matches `sst-<n>`, so the lock file is inert to the rest of
/// the directory.
///
/// A filesystem that cannot lock at all (some network mounts) is warned about
/// and allowed through. Refusing there would make the database unusable on
/// those mounts, and an unenforceable guarantee is not worth a working
/// installation. A lock that is *held* is always refused.
fn lock_directory(dir: &Path) -> Result<File> {
    let path = dir.join("LOCK");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Fully qualified, and via `fs4` rather than std: `File::try_lock` is
    // inherent from Rust 1.89 and would shadow the trait, but this crate
    // promises 1.85. Drop the dependency and call std's directly whenever the
    // MSRV moves past 1.89.
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(fs4::TryLockError::WouldBlock) => Err(Error::Conflict(format!(
            concat!(
                "the database at {} is already open by another process. ",
                "One process at a time may open it directly; ",
                "run `drsg serve` and share that instance instead."
            ),
            dir.display()
        ))),
        Err(fs4::TryLockError::Error(e)) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "cannot take the database lock on this filesystem; \
                 a concurrent open will not be detected"
            );
            Ok(file)
        }
    }
}

/// The native LSM engine. `Send + Sync`: reads take a shared lock on `store`,
/// the single writer is serialized by `write_gate`, and the WAL has its own
/// lock so an in-flight `fsync` doesn't block readers.
pub struct NativeEngine {
    store: RwLock<Store>,
    wal: Mutex<BufWriter<File>>,
    write_gate: WriteGate,
    /// How long `begin_write` waits for the gate, in milliseconds; `0` means
    /// wait forever (the default). Atomic so a server can set it through the
    /// shared `Arc<Database>` after open, without an options struct threaded
    /// through every constructor.
    write_timeout_ms: AtomicU64,
    /// Exclusive advisory lock on `<dir>/LOCK`, held for the engine's lifetime
    /// and released when this handle drops.
    ///
    /// `write_gate` serializes the writer *within* one process; this is what
    /// stops a second process opening the same directory. Without it each
    /// process gets its own WAL offset and its own `next_sst` counter and they
    /// quietly overwrite each other — measured before this existed: two
    /// concurrent imports of 200 nodes each left a database holding 200, with
    /// `check` reporting it healthy. Silent loss that survives the integrity
    /// scan is the worst kind, so the second open is refused instead.
    _lock: File,
    dir: PathBuf,
    flush_threshold: usize,
    /// Live read snapshots (snapshot → reader count), so compaction never
    /// reclaims a version some open reader could still need. Registered under
    /// the store read lock at `begin_read` (see the ordering note there).
    readers: Mutex<BTreeMap<u64, usize>>,
    /// Shared SST block cache + the monotonic id counter that keys it.
    blocks: Arc<BlockCache>,
    next_sst_id: AtomicU64,
    /// History-retention window for time-travel (ROADMAP §4): how many commits
    /// back stay queryable. `0` ⇒ unbounded (never GC across versions — any
    /// past snapshot is reachable, disk grows with history). `n > 0` ⇒ keep
    /// versions down to `committed_seq - n`; older snapshots are compacted away.
    /// Compaction floors its GC at `min(oldest live reader, this)`.
    retain_commits: AtomicU64,
    /// Set once at open by a `serve --follow` replica (arch/01 §9): rejects
    /// `begin_write` outright for the engine's whole lifetime — a replica
    /// never gets promoted, so this needs no runtime setter.
    /// [`Self::apply_replicated`] bypasses it — it's the one write path a
    /// replica is meant to use.
    read_only: bool,
    /// Registered by the web layer once a follower may be subscribed;
    /// invoked synchronously right after each commit's WAL fsync, same
    /// contract as `api::ChangeObserver` ("cheap and non-blocking — the web
    /// layer just forwards into a broadcast channel").
    wal_observer: Mutex<Option<WalObserver>>,
    /// The error from the most recent post-commit maintenance pass
    /// (flush/compaction) that failed, cleared once a later pass succeeds.
    /// Maintenance runs after the batch is durable and published, so its
    /// failure cannot be reported through `commit` without lying about a
    /// write that did land; it is logged and kept here for `check`/stats.
    last_maintenance_error: Mutex<Option<String>>,
    /// Test seam: parks a flush after its SST is written but before the swap,
    /// so a test can prove readers get through that window.
    #[cfg(test)]
    flush_pause: test_hooks::Pause,
    /// Test seam: makes the next flush's SST write fail, standing in for a
    /// disk that still takes the log but refuses a new file.
    #[cfg(test)]
    sst_write_fault: test_hooks::Fault,
    /// Test seam: makes the next WAL truncation's fsync fail, after the
    /// truncate itself took effect.
    #[cfg(test)]
    wal_sync_fault: test_hooks::Fault,
}

impl NativeEngine {
    /// Open an SST, giving it a fresh cache id + a handle to the shared block
    /// cache.
    fn open_sst(&self, path: &Path) -> Result<Arc<sst::Sst>> {
        let id = self.next_sst_id.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(sst::Sst::open(path, id, self.blocks.clone())?))
    }
}

impl NativeEngine {
    /// Open (creating if absent) the engine directory at `path`: load its SSTs,
    /// then replay the WAL tail into the memtable.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, DEFAULT_FLUSH_THRESHOLD, false)
    }

    /// Open in read-only mode (`serve --follow`, arch/01 §9): `begin_write`
    /// refuses for this handle's whole lifetime. [`Self::apply_replicated`]
    /// is unaffected — it's how a replica applies what it follows.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, DEFAULT_FLUSH_THRESHOLD, true)
    }

    /// Test-only: the conformance suite wants a tiny flush threshold to
    /// exercise SST rotation without writing megabytes of fixtures.
    #[cfg(test)]
    pub(crate) fn open_with_threshold(
        path: impl AsRef<Path>,
        flush_threshold: usize,
    ) -> Result<Self> {
        Self::open_with_options(path, flush_threshold, false)
    }

    fn open_with_options(
        path: impl AsRef<Path>,
        flush_threshold: usize,
        read_only: bool,
    ) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        // Before anything else touches the directory.
        let lock = lock_directory(&dir)?;

        let mut store = Store {
            next_sst: sst::next_number(&dir),
            ..Store::default()
        };
        let blocks = new_block_cache();
        let mut next_id = 0u64;
        // SSTs first (oldest→newest), tracking the highest sequence they hold.
        for sst_path in sst::list(&dir) {
            let s = sst::Sst::open(&sst_path, next_id, blocks.clone())?;
            next_id += 1;
            store.committed_seq = store.committed_seq.max(s.max_seq);
            store.ssts.push(Arc::new(s));
        }

        // Then the WAL tail (records not yet folded into an SST).
        let wal_path = dir.join("wal");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)?;
        let valid_len = replay(&mut file, &mut store)?;
        file.set_len(valid_len)?;
        file.seek(SeekFrom::Start(valid_len))?;

        Ok(Self {
            store: RwLock::new(store),
            wal: Mutex::new(BufWriter::new(file)),
            write_gate: WriteGate::new(),
            write_timeout_ms: AtomicU64::new(0),
            _lock: lock,
            dir,
            flush_threshold,
            readers: Mutex::new(BTreeMap::new()),
            blocks,
            next_sst_id: AtomicU64::new(next_id),
            // Unbounded history by default (keep every version): time-travel to
            // any past commit "just works". A host caps it via `set_retention`.
            retain_commits: AtomicU64::new(0),
            read_only,
            wal_observer: Mutex::new(None),
            last_maintenance_error: Mutex::new(None),
            #[cfg(test)]
            flush_pause: test_hooks::Pause::default(),
            #[cfg(test)]
            sst_write_fault: test_hooks::Fault::default(),
            #[cfg(test)]
            wal_sync_fault: test_hooks::Fault::default(),
        })
    }

    /// The error from the latest failed flush/compaction, if the most recent
    /// maintenance pass failed. A commit whose batch is durable returns `Ok`
    /// even when the maintenance it triggered fails (the write is safe; the
    /// memtable/WAL just stay larger than intended until the next commit
    /// retries), so this is where that failure is surfaced.
    pub fn last_maintenance_error(&self) -> Option<String> {
        self.last_maintenance_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Flush the memtable to a new SST and rotate the WAL, if the memtable is
    /// over threshold. Called from `commit` after the batch is published and
    /// the store write lock released, still under the write gate.
    ///
    /// The SST is written under the store *read* lock, so readers keep going
    /// through the write+fsync. That is sound because the memtable only ever
    /// changes inside `durable_commit`, and the gate makes this the only
    /// `durable_commit` in flight: between publish and the swap below the
    /// memtable is effectively immutable, and the SST is an exact copy of it
    /// stamped with the same sequences. The swap itself — run in, memtable
    /// out — is the only step that takes the write lock, and it is a few
    /// pointer moves; a reader before or after it sees the same versions. The
    /// SST is made durable before the WAL is truncated.
    fn maybe_flush(&self) -> Result<()> {
        let (path, n) = {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            if store.mem_bytes < self.flush_threshold || store.mem.is_empty() {
                return Ok(());
            }
            let n = store.next_sst;
            let path = self.dir.join(format!("sst-{n:06}"));
            #[cfg(test)]
            self.sst_write_fault.trip()?;
            sst::write(&path, &store.mem, store.committed_seq)?;
            #[cfg(test)]
            self.flush_pause.wait();
            (path, n)
        };
        let s = self.open_sst(&path)?;
        {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            // A failed attempt (write error above, or open_sst) leaves the
            // number unclaimed and the memtable in place; the retry the next
            // commit makes overwrites the same name via rename.
            store.next_sst = n + 1;
            store.ssts.push(s);
            store.mem.clear();
            store.mem_bytes = 0;
        }

        // The flushed records are now durable in the SST → drop them from the
        // WAL. Losing the truncation itself would be harmless (the next open
        // would replay records already captured by the SST, which is
        // idempotent), but the truncate is a metadata change and the SST
        // rename before it another, so the directory is fsynced to make the
        // new file list durable in the same order the code produced it.
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        wal.flush()?;
        let f = wal.get_mut();
        f.set_len(0)?;
        // The cursor moves the instant the file is cut, before anything that
        // can fail: a commit is Ok even when its maintenance fails, so were the
        // fsync below to fail with the cursor still at the old end, every later
        // batch would be appended behind a zero-filled hole that replay reads
        // as an empty torn tail — durable commits, lost on the next open.
        f.seek(SeekFrom::Start(0))?;
        f.sync_all()?;
        #[cfg(test)]
        self.wal_sync_fault.trip()?;
        sync_dir(&self.dir)?;
        Ok(())
    }

    /// Set the time-travel retention window: keep the last `keep_commits`
    /// commits queryable (`None` ⇒ unbounded, the default). Takes effect on the
    /// next compaction; it never resurrects versions an earlier compaction has
    /// already reclaimed.
    pub fn set_retention(&self, keep_commits: Option<u64>) {
        self.retain_commits
            .store(keep_commits.unwrap_or(0), Ordering::Relaxed);
    }

    /// The latest committed sequence — the newest snapshot a reader can pin.
    pub fn committed_seq(&self) -> u64 {
        self.store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .committed_seq
    }

    /// The oldest sequence retention keeps queryable at `committed_seq`: the
    /// floor below which time-travel is refused. `0` under unbounded retention.
    fn retention_floor(&self, committed_seq: u64) -> u64 {
        match self.retain_commits.load(Ordering::Relaxed) {
            0 => 0, // unbounded — keep everything reachable
            keep => committed_seq.saturating_sub(keep),
        }
    }

    /// The oldest sequence still retained (a reader may pin any snapshot in
    /// `[retained_floor, committed_seq]`).
    pub fn retained_floor(&self) -> u64 {
        self.retention_floor(self.committed_seq())
    }

    /// A read transaction pinned to a past commit sequence (time-travel / AS
    /// OF, ROADMAP §4). Like [`begin_read`](StorageEngine::begin_read) it
    /// registers the snapshot under the store read lock, so a concurrent
    /// compaction honours it and won't reclaim versions it needs. Errors if
    /// `snapshot` is in the future (beyond the latest commit) or older than the
    /// retained history.
    pub fn begin_read_at(&self, snapshot: u64) -> Result<NativeReadTxn<'_>> {
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        let committed = store.committed_seq;
        if snapshot > committed {
            return Err(Error::InvalidArgument(format!(
                "cannot read as of {snapshot}: the latest commit is {committed}"
            )));
        }
        let floor = self.retention_floor(committed);
        if snapshot < floor {
            return Err(Error::InvalidArgument(format!(
                "cannot read as of {snapshot}: older than the retained history \
                 (oldest retained commit is {floor})"
            )));
        }
        *self
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(snapshot)
            .or_insert(0) += 1;
        drop(store);
        Ok(NativeReadTxn {
            engine: self,
            snapshot,
        })
    }

    /// The oldest sequence any live reader can still observe — the floor below
    /// which older versions are unreachable and may be reclaimed. With no
    /// readers, that's the committed sequence (everything below the newest
    /// version per key is dead), unless retention pins history further back.
    fn min_snapshot(&self, committed_seq: u64) -> u64 {
        let reader_floor = {
            let readers = self.readers.lock().unwrap_or_else(|e| e.into_inner());
            readers
                .keys()
                .next()
                .copied()
                .unwrap_or(committed_seq)
                .min(committed_seq)
        };
        // Retention lowers the floor so bounded history (or an unbounded 0) keeps
        // more than live readers alone would.
        reader_floor.min(self.retention_floor(committed_seq))
    }

    /// If enough runs have accumulated, merge them all into one, dropping
    /// versions no reader can reach. Called from `commit` (single writer) after
    /// the store lock is released, so the heavy merge I/O doesn't block readers.
    fn maybe_compact(&self) -> Result<()> {
        // Snapshot the runs to merge under a brief read lock.
        let (runs, committed_seq, next) = {
            let store = self.store.read().unwrap_or_else(|e| e.into_inner());
            if store.ssts.len() <= COMPACTION_TRIGGER {
                return Ok(());
            }
            (store.ssts.clone(), store.committed_seq, store.next_sst)
        };
        let min_snap = self.min_snapshot(committed_seq);

        // Stream the runs through a k-way merge (a later run's version wins)
        // and the version GC straight into the new file: memory is one block
        // per run plus one key's version group, not the sum of the runs. The
        // merged run is stamped with the newest sequence any input held, which
        // stays right when GC drops the newest entry of an all-dead key.
        let max_seq = runs
            .iter()
            .map(|r| r.max_seq)
            .max()
            .unwrap_or(committed_seq);
        let count_hint = runs.iter().map(|r| r.count).sum::<u64>();
        let count_hint = usize::try_from(count_hint).unwrap_or(usize::MAX);
        let path = self.dir.join(format!("sst-{next:06}"));
        {
            let merged = MergeIter::new(runs.iter().map(|r| r.entries()));
            let kept = gc_versions(merged, min_snap);
            sst::write_sorted(&path, kept, count_hint, max_seq)?;
        }
        let merged_sst = self.open_sst(&path)?;

        // Swap the merged runs out for the single new run. Single writer ⇒ the
        // first `runs.len()` entries are exactly the ones we merged.
        let old = {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            let tail = store.ssts.split_off(runs.len());
            let mut fresh = Vec::with_capacity(1 + tail.len());
            fresh.push(merged_sst);
            fresh.extend(tail);
            let old = std::mem::replace(&mut store.ssts, fresh);
            store.next_sst = next + 1;
            old
        };

        // Delete the merged runs' files. Their `Arc`s drop here (readers never
        // retain a run across a call), closing the handles first.
        let paths: Vec<PathBuf> = old.iter().map(|s| s.path.clone()).collect();
        drop(old);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }
}

/// Make a directory's entry list durable: on unix a rename or truncate is
/// only guaranteed to survive a crash once the *directory* is fsynced too,
/// otherwise a just-renamed SST can vanish while the WAL that held its records
/// has already been cut. Windows has no directory fsync (NTFS journals
/// metadata), so this is a no-op there.
pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// The k-way merge of several runs' entry sweeps into one `(table, key, seq
/// DESC)` stream — compaction's input. Each sweep keeps one block resident and
/// the heap holds one entry per run, so the merge's memory is proportional to
/// the number of runs, not to their size. Runs are given oldest first; should
/// two runs ever carry the same `(table, key, seq)`, the later run's entry is
/// the one emitted (what the memtable-based merge used to produce by
/// overwriting) and the earlier one is dropped.
struct MergeIter<I: Iterator<Item = Result<(MemKey, Op)>>> {
    runs: Vec<I>,
    heap: BinaryHeap<Reverse<MergeHead>>,
    last: Option<MemKey>,
    /// A sweep's error is delivered once and ends the merge.
    done: bool,
}

/// A run's current front entry. Ordered by key, then by *later run first*, so
/// equal keys pop in the order that lets the newest run win.
struct MergeHead {
    key: MemKey,
    run: usize,
    op: Op,
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for MergeHead {}
impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| other.run.cmp(&self.run))
    }
}

impl<I: Iterator<Item = Result<(MemKey, Op)>>> MergeIter<I> {
    /// `runs` oldest first. Priming reads one block per run; a read error
    /// there is reported by the first `next`.
    fn new(runs: impl IntoIterator<Item = I>) -> Self {
        Self {
            runs: runs.into_iter().collect(),
            heap: BinaryHeap::new(),
            last: None,
            done: false,
        }
    }

    /// Pull `run`'s next entry onto the heap (nothing if the run is drained).
    fn advance(&mut self, run: usize) -> Result<()> {
        if let Some(next) = self.runs[run].next() {
            let (key, op) = next?;
            self.heap.push(Reverse(MergeHead { key, run, op }));
        }
        Ok(())
    }

    fn fail(&mut self, e: Error) -> Option<Result<(MemKey, Op)>> {
        self.done = true;
        self.heap.clear();
        Some(Err(e))
    }
}

impl<I: Iterator<Item = Result<(MemKey, Op)>>> Iterator for MergeIter<I> {
    type Item = Result<(MemKey, Op)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.last.is_none() && self.heap.is_empty() {
            // First call: prime every run.
            for run in 0..self.runs.len() {
                if let Err(e) = self.advance(run) {
                    return self.fail(e);
                }
            }
        }
        loop {
            let Reverse(MergeHead { key, run, op }) = self.heap.pop()?;
            if let Err(e) = self.advance(run) {
                return self.fail(e);
            }
            if self.last.as_ref() == Some(&key) {
                continue; // an older run's copy of an entry already emitted
            }
            self.last = Some(key.clone());
            return Some(Ok((key, op)));
        }
    }
}

/// Reclaim dead versions from a merged stream, itself streaming: `merged` is
/// in `(table, key, seq DESC)` order, so a key's versions arrive together
/// newest-first. Keep versions down to and including the first at or below
/// `min_snapshot` (the "floor" a reader at that snapshot would see); drop
/// everything older. A key whose only survivor is a tombstone at/below the
/// floor is dropped entirely — this is the bottom run, so nothing older can
/// resurface. Memory is one key's version group; an input error is passed
/// through and ends the stream.
fn gc_versions<I: Iterator<Item = Result<(MemKey, Op)>>>(
    merged: I,
    min_snapshot: u64,
) -> GcIter<I> {
    GcIter {
        merged,
        min_snapshot,
        cur: None,
        group: Vec::new(),
        out: VecDeque::new(),
        done: false,
    }
}

/// See [`gc_versions`].
struct GcIter<I> {
    merged: I,
    min_snapshot: u64,
    /// The key whose versions `group` is collecting.
    cur: Option<(u8, Vec<u8>)>,
    group: Vec<(u64, Op)>,
    /// Survivors of the last closed group, drained before more input is read.
    out: VecDeque<(MemKey, Op)>,
    done: bool,
}

impl<I: Iterator<Item = Result<(MemKey, Op)>>> Iterator for GcIter<I> {
    type Item = Result<(MemKey, Op)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.out.pop_front() {
                return Some(Ok(item));
            }
            if self.done {
                return None;
            }
            match self.merged.next() {
                Some(Err(e)) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Some(Ok(((table, key, Reverse(seq)), op))) => {
                    let same = matches!(&self.cur, Some((t, k)) if *t == table && *k == key);
                    if !same {
                        if let Some((t, k)) = self.cur.take() {
                            emit_group(t, k, &mut self.group, self.min_snapshot, &mut self.out);
                        }
                        self.cur = Some((table, key));
                    }
                    self.group.push((seq, op)); // newest-first per key
                }
                None => {
                    self.done = true;
                    if let Some((t, k)) = self.cur.take() {
                        emit_group(t, k, &mut self.group, self.min_snapshot, &mut self.out);
                    }
                }
            }
        }
    }
}

/// Emit the surviving versions of one key (its `group`, newest-first), then
/// leave `group` empty for the next key. The key is owned so the last surviving
/// version takes it without a copy; only a key with several survivors clones
/// it, and keys are small next to values.
fn emit_group(
    table: u8,
    key: Vec<u8>,
    group: &mut Vec<(u64, Op)>,
    min_snapshot: u64,
    out: &mut VecDeque<(MemKey, Op)>,
) {
    let mut keep = 0;
    for (seq, _) in group.iter() {
        keep += 1;
        if *seq <= min_snapshot {
            break; // floor reached; older versions are unreachable
        }
    }
    group.truncate(keep);
    // A lone tombstone at/below the floor leaves nothing to shadow → drop it.
    if let [(seq, Op::Del)] = group.as_slice()
        && *seq <= min_snapshot
    {
        group.clear();
        return;
    }
    let last = group.len().saturating_sub(1);
    let mut key = Some(key);
    for (i, (seq, op)) in group.drain(..).enumerate() {
        let k = if i == last {
            key.take()
                .expect("the key is handed out once, to the last survivor")
        } else {
            key.as_ref()
                .expect("still held until the last survivor")
                .clone()
        };
        out.push_back(((table, k, Reverse(seq)), op));
    }
}

#[cfg(test)]
mod test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex, mpsc};

    /// An optional rendezvous a code path waits at when armed. Arriving there
    /// is announced on a channel first, so a test can learn the path is parked
    /// without touching any lock the path might be holding.
    #[derive(Default)]
    pub(super) struct Pause(Mutex<Option<(mpsc::Sender<()>, Arc<Barrier>)>>);

    impl Pause {
        /// Arm the pause; the returned receiver fires when it is reached, and
        /// `b.wait()` from the test then releases it.
        pub(super) fn arm(&self, b: Arc<Barrier>) -> mpsc::Receiver<()> {
            let (tx, rx) = mpsc::channel();
            *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((tx, b));
            rx
        }

        pub(super) fn wait(&self) {
            let armed = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((tx, b)) = armed {
                let _ = tx.send(());
                b.wait();
            }
        }
    }

    /// A one-shot injected I/O failure: once armed, the next `trip` returns
    /// an error and disarms, so a test can fail a chosen step of a code path
    /// deterministically on every platform and uid (unlike permission bits,
    /// which root ignores).
    #[derive(Default)]
    pub(super) struct Fault(AtomicBool);

    impl Fault {
        pub(super) fn arm(&self) {
            self.0.store(true, Ordering::SeqCst);
        }

        pub(super) fn trip(&self) -> crate::Result<()> {
            if self.0.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("injected fault").into());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    fn put(v: &str) -> Op {
        Op::Put(v.as_bytes().to_vec())
    }

    fn merged(entries: &[(u8, &str, u64, Op)]) -> BTreeMap<MemKey, Op> {
        entries
            .iter()
            .map(|(t, k, seq, op)| ((*t, k.as_bytes().to_vec(), Reverse(*seq)), op.clone()))
            .collect()
    }

    /// Run the streaming GC over an in-memory merged run and collect it.
    fn gc(m: BTreeMap<MemKey, Op>, min_snapshot: u64) -> BTreeMap<MemKey, Op> {
        gc_versions(m.into_iter().map(Ok), min_snapshot)
            .collect::<Result<_>>()
            .unwrap()
    }

    fn seqs(out: &BTreeMap<MemKey, Op>, key: &str) -> Vec<u64> {
        out.keys()
            .filter(|(_, k, _)| k == key.as_bytes())
            .map(|(_, _, Reverse(s))| *s)
            .collect()
    }

    #[test]
    fn keeps_versions_down_to_the_floor_and_drops_the_rest() {
        let m = merged(&[
            (0, "k", 9, put("v9")),
            (0, "k", 6, put("v6")),
            (0, "k", 4, put("v4")),
            (0, "k", 2, put("v2")),
        ]);
        let out = gc(m, 5);
        // Above the floor: 9, 6. The first at/below it (4) is what a reader
        // pinned at 5 sees, so it stays; 2 is unreachable.
        assert_eq!(seqs(&out, "k"), vec![9, 6, 4]);
        assert_eq!(out[&(0, b"k".to_vec(), Reverse(4))], put("v4"));
    }

    #[test]
    fn a_lone_tombstone_below_the_floor_vanishes_but_a_shadowing_one_stays() {
        let m = merged(&[
            (0, "gone", 3, Op::Del),
            (0, "gone", 1, put("x")),
            (0, "live", 8, Op::Del),
            (0, "live", 7, put("y")),
        ]);
        let out = gc(m, 5);
        assert!(seqs(&out, "gone").is_empty(), "nothing older can resurface");
        // A tombstone above the floor still shadows the version a pinned
        // reader at 5 would otherwise see.
        assert_eq!(seqs(&out, "live"), vec![8, 7]);
    }

    #[test]
    fn keys_are_grouped_per_table_and_every_survivor_keeps_its_value() {
        let m = merged(&[
            (0, "a", 2, put("n")),
            (1, "a", 2, put("e")),
            (1, "a", 1, put("old")),
        ]);
        let out = gc(m, 10);
        assert_eq!(out.len(), 2, "the same key in two tables is two keys");
        assert_eq!(out[&(0, b"a".to_vec(), Reverse(2))], put("n"));
        assert_eq!(out[&(1, b"a".to_vec(), Reverse(2))], put("e"));
    }

    #[test]
    fn an_unbounded_retention_floor_of_zero_keeps_every_version() {
        // `retain_commits = 0` ⇒ `retention_floor` = 0 ⇒ compaction's floor is
        // 0. Sequences start at 1, so nothing is at/below it: every version
        // and every tombstone must survive, or time-travel to an old commit
        // would silently read the wrong value.
        let m = merged(&[
            (0, "k", 9, put("v9")),
            (0, "k", 6, put("v6")),
            (0, "k", 1, put("v1")),
            (0, "gone", 3, Op::Del),
            (0, "gone", 1, put("x")),
        ]);
        let before = m.clone();
        let out = gc(m, 0);
        assert_eq!(out, before, "floor 0 must be a no-op");
    }

    #[test]
    fn the_streaming_merge_equals_the_map_merge_and_the_later_run_wins() {
        // Three runs with interleaved keys and a version spread; a key that
        // appears in every run; one (table, key, seq) duplicated across runs
        // to pin the "later run wins" rule the old overwrite gave for free.
        let runs = [
            merged(&[
                (0, "a", 1, put("a1")),
                (0, "c", 2, put("c2")),
                (1, "a", 3, put("ta3")),
                (0, "dup", 4, put("old")),
            ]),
            merged(&[
                (0, "a", 5, Op::Del),
                (0, "b", 6, put("b6")),
                (0, "dup", 4, put("new")),
            ]),
            merged(&[(0, "a", 7, put("a7")), (0, "c", 8, put("c8"))]),
        ];
        let mut expected = BTreeMap::new();
        for r in &runs {
            expected.extend(r.clone());
        }
        let streamed: Vec<(MemKey, Op)> =
            MergeIter::new(runs.iter().map(|r| r.clone().into_iter().map(Ok)))
                .collect::<Result<_>>()
                .unwrap();
        assert!(
            streamed.windows(2).all(|w| w[0].0 < w[1].0),
            "merge output must be strictly ordered"
        );
        let streamed: BTreeMap<MemKey, Op> = streamed.into_iter().collect();
        assert_eq!(streamed, expected);
        assert_eq!(streamed[&(0, b"dup".to_vec(), Reverse(4))], put("new"));
        // And the GC on top still sees whole version groups.
        let out = gc_versions(
            MergeIter::new(runs.iter().map(|r| r.clone().into_iter().map(Ok))),
            6,
        )
        .collect::<Result<BTreeMap<_, _>>>()
        .unwrap();
        // Table 0's "a": 7 above the floor, 5 is the floor version; table 1's
        // lone (1, "a", 3) is its own group and survives as that key's floor.
        assert_eq!(seqs(&out, "a"), vec![7, 5, 3]);
        assert!(!out.contains_key(&(0, b"a".to_vec(), Reverse(1))));
        assert!(out.contains_key(&(1, b"a".to_vec(), Reverse(3))));
    }

    #[test]
    fn a_failing_sweep_ends_the_merge_with_its_error() {
        let good = merged(&[(0, "a", 1, put("x")), (0, "z", 2, put("y"))]);
        let bad: Vec<Result<(MemKey, Op)>> = vec![
            Ok(((0, b"m".to_vec(), Reverse(3)), put("m"))),
            Err(Error::Corrupt("boom".into())),
        ];
        let out: Vec<Result<(MemKey, Op)>> = MergeIter::new(vec![
            good.into_iter().map(Ok).collect::<Vec<_>>().into_iter(),
            bad.into_iter(),
        ])
        .collect();
        let errs = out.iter().filter(|r| r.is_err()).count();
        assert_eq!(errs, 1, "exactly one error, then the merge ends");
        assert!(matches!(out.last(), Some(Err(Error::Corrupt(_)))));
        // Through the GC and into the writer, that error aborts the file.
        let dir = std::env::temp_dir().join(format!("drs-merge-error-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let r = sst::write_sorted(
            &dir.join("sst-000001"),
            gc_versions(
                vec![Err::<(MemKey, Op), _>(Error::Corrupt("boom".into()))].into_iter(),
                0,
            ),
            1,
            1,
        );
        assert!(matches!(r, Err(Error::Corrupt(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod engine_tests {
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
}

impl StorageEngine for NativeEngine {
    type ReadTxn<'a> = NativeReadTxn<'a>;
    type WriteTxn<'a> = NativeWriteTxn<'a>;

    fn begin_read(&self) -> Result<NativeReadTxn<'_>> {
        // Read the snapshot AND register it while holding the store read lock.
        // A commit sets `committed_seq` under the store *write* lock, so any
        // reader whose snapshot is below a commit's sequence necessarily
        // registered before that commit — hence before the compaction that
        // commit may run. Compaction can therefore trust the reader set to bound
        // what it reclaims. (A reader that registers later sees the new
        // `committed_seq` and only needs the newest versions, which survive.)
        let store = self.store.read().unwrap_or_else(|e| e.into_inner());
        let snapshot = store.committed_seq;
        *self
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(snapshot)
            .or_insert(0) += 1;
        drop(store);
        Ok(NativeReadTxn {
            engine: self,
            snapshot,
        })
    }

    fn begin_write(&self) -> Result<NativeWriteTxn<'_>> {
        if self.read_only {
            return Err(Error::ReadOnly(
                "this database is a read-only replica (serve --follow)".into(),
            ));
        }
        self.begin_write_unchecked()
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) {
        // Saturating: a timeout longer than ~584 million years is the caller
        // meaning "forever", and `0` already encodes that. Sub-millisecond
        // values round up to 1ms rather than to 0, so a small timeout never
        // silently becomes an unbounded wait.
        let ms = match timeout {
            None => 0,
            Some(d) => u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1),
        };
        self.write_timeout_ms.store(ms, Ordering::Relaxed);
    }
}

// ---- merged reads over memtable + SSTs -------------------------------------

/// Newest memtable version of `(table, key)` visible at `snapshot`.
fn mem_op(mem: &BTreeMap<MemKey, Op>, table: u8, key: &[u8], snapshot: u64) -> Option<Op> {
    let lo = (table, key.to_vec(), Reverse(u64::MAX));
    let hi = (table, key.to_vec(), Reverse(0u64));
    for ((_, _, Reverse(seq)), op) in mem.range(lo..=hi) {
        if *seq <= snapshot {
            return Some(op.clone());
        }
    }
    None
}

/// Newest-visible `Op` per memtable user key in `[start, end)`, written into
/// `out` (shadowing whatever the SSTs contributed).
fn mem_range_ops(
    mem: &BTreeMap<MemKey, Op>,
    table: u8,
    start: &[u8],
    end: Option<&[u8]>,
    snapshot: u64,
    out: &mut BTreeMap<Vec<u8>, Op>,
) {
    let lo = Bound::Included((table, start.to_vec(), Reverse(u64::MAX)));
    let hi = match end {
        Some(e) => Bound::Excluded((table, e.to_vec(), Reverse(u64::MAX))),
        None => Bound::Excluded((table + 1, Vec::new(), Reverse(u64::MAX))),
    };
    let mut cur: Option<Vec<u8>> = None;
    for ((_, user, Reverse(seq)), op) in mem.range((lo, hi)) {
        if cur.as_deref() == Some(user.as_slice()) {
            continue;
        }
        if *seq > snapshot {
            continue;
        }
        cur = Some(user.clone());
        out.insert(user.clone(), op.clone());
    }
}

/// Value of `(table, key)` visible at `snapshot`, merging memtable over SSTs
/// (newest run first). The first source with a version wins — a tombstone there
/// means "deleted", shadowing older runs.
fn committed_get(store: &Store, table: u8, key: &[u8], snapshot: u64) -> Result<Option<Vec<u8>>> {
    if let Some(op) = mem_op(&store.mem, table, key, snapshot) {
        return Ok(op.into_value());
    }
    for s in store.ssts.iter().rev() {
        if let Some(op) = s.get(table, key, snapshot)? {
            return Ok(op.into_value());
        }
    }
    Ok(None)
}

/// Visible `(key, value)` pairs of `table` in `[start, end)` at `snapshot`,
/// merged across all runs. Applying oldest→newest with the memtable last means
/// a newer run's `Put`/`Del` overrides an older one; surviving tombstones are
/// then dropped.
fn committed_values(
    store: &Store,
    table: u8,
    start: &[u8],
    end: Option<&[u8]>,
    snapshot: u64,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut ops: BTreeMap<Vec<u8>, Op> = BTreeMap::new();
    for s in store.ssts.iter() {
        s.range(table, start, end, snapshot, &mut ops)?;
    }
    mem_range_ops(&store.mem, table, start, end, snapshot, &mut ops);
    Ok(ops
        .into_iter()
        .filter_map(|(k, op)| op.into_value().map(|v| (k, v)))
        .collect())
}

pub struct NativeReadTxn<'a> {
    engine: &'a NativeEngine,
    snapshot: u64,
}

impl Drop for NativeReadTxn<'_> {
    fn drop(&mut self) {
        let mut readers = self
            .engine
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let std::collections::btree_map::Entry::Occupied(mut e) = readers.entry(self.snapshot) {
            *e.get_mut() -= 1;
            if *e.get() == 0 {
                e.remove();
            }
        }
    }
}

impl ReadTransaction for NativeReadTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        committed_get(&store, table as u8, key, self.snapshot)
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        let out = committed_values(&store, table as u8, start, end, self.snapshot)?;
        Ok(Box::new(out.into_iter().map(Ok)))
    }
}

pub struct NativeWriteTxn<'a> {
    engine: &'a NativeEngine,
    _gate: WriteGuard<'a>,
    snapshot: u64,
    /// Staged mutations, last-write-wins per key (seq assigned at commit).
    buf: BTreeMap<(u8, Vec<u8>), Op>,
}

impl ReadTransaction for NativeWriteTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(op) = self.buf.get(&(table as u8, key.to_vec())) {
            return Ok(op.clone().into_value());
        }
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        committed_get(&store, table as u8, key, self.snapshot)
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        let mut out = {
            let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
            committed_values(&store, table as u8, start, end, self.snapshot)?
        };
        let in_range = |k: &[u8]| k >= start && end.is_none_or(|e| k < e);
        for ((t, user), op) in &self.buf {
            if *t != table as u8 || !in_range(user) {
                continue;
            }
            match op {
                Op::Put(v) => {
                    out.insert(user.clone(), v.clone());
                }
                Op::Del => {
                    out.remove(user);
                }
            }
        }
        Ok(Box::new(out.into_iter().map(Ok)))
    }
}

impl WriteTransaction for NativeWriteTxn<'_> {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()> {
        self.buf
            .insert((table as u8, key.to_vec()), Op::Put(value.to_vec()));
        Ok(())
    }

    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()> {
        self.buf.insert((table as u8, key.to_vec()), Op::Del);
        Ok(())
    }

    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()> {
        let t = table as u8;
        self.buf
            .retain(|(bt, k), _| !(*bt == t && k.starts_with(prefix)));
        let end = prefix_successor(prefix);
        let victims: Vec<Vec<u8>> = {
            let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
            committed_values(&store, t, prefix, end.as_deref(), self.snapshot)?
                .into_keys()
                .collect()
        };
        for k in victims {
            self.buf.insert((t, k), Op::Del);
        }
        Ok(())
    }

    fn commit(self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let seq = self.snapshot + 1;
        self.engine.durable_commit(seq, self.buf).map(|_| ())
    }
}

impl NativeEngine {
    /// Append `ops` as one WAL batch at `seq`, fsync, notify a registered
    /// replication observer, then publish to the memtable and flush/compact
    /// as needed. Shared by [`NativeWriteTxn::commit`] (`seq = snapshot + 1`,
    /// a freshly allocated sequence) and [`Self::apply_replicated`] (`seq`
    /// taken verbatim from the source it's replicating, so a replica's
    /// commit sequence matches its master's exactly).
    ///
    /// Returns `Ok(seq)` as soon as the batch is durable *and* published: from
    /// that point the write has happened, and a caller that treated an `Err`
    /// as "nothing landed" (the API layer applies index/keyword events only
    /// after `Ok`) would drift from the KV. So the maintenance that follows
    /// (flush, compaction) never fails the commit; its error is logged and
    /// parked in [`Self::last_maintenance_error`], and the next commit simply
    /// retries — a failed flush leaves the memtable over threshold, a failed
    /// compaction leaves the runs in place.
    fn durable_commit(&self, seq: u64, ops: BTreeMap<(u8, Vec<u8>), Op>) -> Result<u64> {
        let batch = WalBatchRef {
            seq,
            ops: ops
                .iter()
                .map(|((t, k), op)| WalOpRef {
                    table: *t,
                    key: k,
                    value: match op {
                        Op::Put(v) => Some(v.as_slice()),
                        Op::Del => None,
                    },
                })
                .collect(),
        };

        // Durability first: append + fsync the WAL before publishing.
        {
            let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
            append_batch(&mut *wal, &batch)?;
            wal.flush()?;
            wal.get_ref().sync_all()?;
        }

        // Notify a replication subscriber, if any, before publishing — same
        // "durable before visible" ordering as the WAL fsync itself. Skipped
        // entirely (no clone of `ops`) when nobody's registered, so a master
        // with no followers pays nothing for this. The observer is called with
        // its slot's mutex *released*: an observer that blocks (a full channel,
        // a slow subscriber) must not wedge `set_wal_observer`, and one that
        // re-registers from inside the callback must not deadlock. Batches
        // still reach it strictly in commit order because every caller of
        // `durable_commit` holds the write gate for the whole call.
        {
            let observer = self
                .wal_observer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(obs) = observer {
                let replicated = ReplicatedBatch {
                    seq,
                    ops: ops
                        .iter()
                        .map(|((t, k), op)| ReplicatedOp {
                            table: TableId::from_index(*t)
                                .expect("table byte written by this engine is always valid"),
                            key: k.clone(),
                            value: match op {
                                Op::Put(v) => Some(v.clone()),
                                Op::Del => None,
                            },
                        })
                        .collect(),
                };
                obs(replicated);
            }
        }

        // Publish to the memtable and advance the sequence.
        {
            let mut store = self.store.write().unwrap_or_else(|e| e.into_inner());
            for ((t, k), op) in ops {
                store.mem_bytes += 1 + k.len() + 8 + op.value_len();
                store.mem.insert((t, k, Reverse(seq)), op);
            }
            store.committed_seq = seq;
        }
        // Flush and compact outside the store write lock (their I/O shouldn't
        // block reads); both are safe there because the gate is still held.
        let maintained = self.maybe_flush().and_then(|()| self.maybe_compact());
        self.record_maintenance(seq, maintained);
        Ok(seq)
    }

    /// Log and remember a post-publish maintenance failure (or clear the slot
    /// on success) — see [`Self::durable_commit`] for why it never fails the
    /// commit that triggered it.
    fn record_maintenance(&self, seq: u64, outcome: Result<()>) {
        let mut slot = self
            .last_maintenance_error
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match outcome {
            Ok(()) => *slot = None,
            Err(e) => {
                tracing::error!(
                    seq,
                    error = %e,
                    "post-commit flush/compaction failed; the commit is durable, \
                     maintenance is retried on the next commit"
                );
                *slot = Some(e.to_string());
            }
        }
    }

    /// The guts of `begin_write`, minus the `read_only` check — used by
    /// `begin_write` itself, and by `Engine::with_write_bootstrap` for the
    /// one-time-per-open plane/counters bootstrap (`Database::init`), which
    /// must succeed even when this engine was opened read-only: it's the
    /// engine's own setup, not a caller-initiated write.
    pub(crate) fn begin_write_unchecked(&self) -> Result<NativeWriteTxn<'_>> {
        let ms = self.write_timeout_ms.load(Ordering::Relaxed);
        let gate = self
            .write_gate
            .acquire((ms != 0).then(|| Duration::from_millis(ms)))?;
        let snapshot = self
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .committed_seq;
        Ok(NativeWriteTxn {
            engine: self,
            _gate: gate,
            snapshot,
            buf: BTreeMap::new(),
        })
    }

    /// Apply a batch replicated from a master's WAL (`serve --follow`,
    /// arch/01 §9), landing it at the master's own `seq` rather than
    /// allocating a new one — so a replica's KV content converges
    /// byte-for-byte with its source. Bypasses the public `begin_write` gate
    /// (and so ignores `read_only`) but still serializes through
    /// `write_gate`, exactly like a normal commit.
    pub fn apply_replicated(&self, batch: ReplicatedBatch) -> Result<()> {
        if batch.ops.is_empty() {
            return Ok(());
        }
        let ms = self.write_timeout_ms.load(Ordering::Relaxed);
        let _gate = self
            .write_gate
            .acquire((ms != 0).then(|| Duration::from_millis(ms)))?;
        // Under the gate, so the comparison is against a sequence no other
        // writer can move. Sequences must only advance: a batch landing at or
        // below `committed_seq` would be stamped older than versions already
        // visible (its ops would lose to them in the memtable, and a reader
        // pinned at the current sequence would see it appear mid-snapshot).
        // Refusing is safer than skipping — the follower treats an error as
        // "resync from a fresh snapshot", which re-captures the batch, while a
        // silent skip could drop it for good.
        let committed = self
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .committed_seq;
        if batch.seq <= committed {
            return Err(Error::Conflict(format!(
                "cannot apply replicated batch {}: this replica has already committed \
                 sequence {committed}; sequences must only advance (a full resync is needed)",
                batch.seq
            )));
        }
        let ops: BTreeMap<(u8, Vec<u8>), Op> = batch
            .ops
            .into_iter()
            .map(|op| {
                (
                    (op.table.index() as u8, op.key),
                    op.value.map_or(Op::Del, Op::Put),
                )
            })
            .collect();
        self.durable_commit(batch.seq, ops).map(|_| ())
    }

    /// Register (or clear, with `None`) the replication observer invoked
    /// after every commit's WAL fsync. `dr-strange-web` wires this to a
    /// broadcast channel once a follower may subscribe; see
    /// `api::Database::on_wal_commit`.
    pub fn set_wal_observer(&self, f: Option<WalObserver>) {
        *self.wal_observer.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }
}

// ---- WAL format ------------------------------------------------------------
//
// A record is `[u32 len][u32 crc32][postcard(WalBatch)]`, all little-endian.
// One record per committed transaction, so replay is all-or-nothing per commit.

// Borrowing (append) vs owning (replay) structs, agreeing on field order and
// wire types (`&[u8]`/`Vec<u8>` serialize identically), so a commit serializes
// its staged ops without copying every key and value.

#[derive(Deserialize)]
struct WalBatch {
    seq: u64,
    ops: Vec<WalOp>,
}

#[derive(Deserialize)]
struct WalOp {
    table: u8,
    key: Vec<u8>,
    /// `None` is a tombstone (delete).
    value: Option<Vec<u8>>,
}

/// ⚠ Borrowed append twin of [`WalBatch`] — field order must match it.
#[derive(Serialize)]
struct WalBatchRef<'a> {
    seq: u64,
    ops: Vec<WalOpRef<'a>>,
}

/// ⚠ Borrowed append twin of [`WalOp`] — field order must match it.
#[derive(Serialize)]
struct WalOpRef<'a> {
    table: u8,
    key: &'a [u8],
    /// `None` is a tombstone (delete).
    value: Option<&'a [u8]>,
}

/// Largest WAL record body the `u32` length prefix can describe.
const MAX_WAL_RECORD_LEN: usize = u32::MAX as usize;

/// The length-prefix value for a record body of `len` bytes, or a typed error
/// when it does not fit: `len as u32` would silently wrap for a >4 GiB batch
/// and write a record whose prefix disagrees with its body, which replay would
/// then treat as a torn tail — losing the commit *after* reporting it durable.
fn wal_record_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| {
        Error::InvalidArgument(format!(
            "transaction too large for one WAL record: {len} bytes serialized, \
             the limit is {MAX_WAL_RECORD_LEN} bytes; split the write into smaller batches"
        ))
    })
}

fn append_batch(w: &mut impl Write, batch: &WalBatchRef<'_>) -> Result<()> {
    let body = postcard::to_stdvec(batch).map_err(backend)?;
    // Checked before the first byte is written, so an oversize batch leaves
    // the WAL exactly as it was.
    let len = wal_record_len(body.len())?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&crc32(&body).to_le_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

/// Replay every intact record into `store`, returning the byte length of the
/// valid prefix (where the next append begins). Stops at the first short or
/// checksum-failing record — a crash-torn tail — leaving it to be truncated.
fn replay(file: &mut File, store: &mut Store) -> Result<u64> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    let mut pos = 0usize;
    let mut valid = 0u64;
    while pos + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let body_start = pos + 8;
        let Some(body_end) = body_start.checked_add(len) else {
            break;
        };
        if body_end > bytes.len() {
            break;
        }
        let body = &bytes[body_start..body_end];
        if crc32(body) != crc {
            break;
        }
        let Ok(batch) = postcard::from_bytes::<WalBatch>(body) else {
            break;
        };
        for op in batch.ops {
            let entry = op.value.map_or(Op::Del, Op::Put);
            store.mem_bytes += 1 + op.key.len() + 8 + entry.value_len();
            store
                .mem
                .insert((op.table, op.key, Reverse(batch.seq)), entry);
        }
        store.committed_seq = store.committed_seq.max(batch.seq);
        pos = body_end;
        valid = body_end as u64;
    }
    Ok(valid)
}

/// Bitwise CRC-32 (IEEE 802.3 polynomial) — no dependency; used by the WAL and
/// SST footers. Not a throughput bottleneck at this phase.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
