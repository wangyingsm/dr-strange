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
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use moka::sync::Cache;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, backend};
use crate::storage::engine::{
    KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction, prefix_successor,
};

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
    write_gate: Mutex<()>,
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
        Self::open_with_threshold(path, DEFAULT_FLUSH_THRESHOLD)
    }

    pub(crate) fn open_with_threshold(
        path: impl AsRef<Path>,
        flush_threshold: usize,
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
            write_gate: Mutex::new(()),
            _lock: lock,
            dir,
            flush_threshold,
            readers: Mutex::new(BTreeMap::new()),
            blocks,
            next_sst_id: AtomicU64::new(next_id),
            // Unbounded history by default (keep every version): time-travel to
            // any past commit "just works". A host caps it via `set_retention`.
            retain_commits: AtomicU64::new(0),
        })
    }

    /// Flush the memtable to a new SST and rotate the WAL, if the memtable is
    /// over threshold. Called from `commit` while holding the store write lock
    /// (so single-writer). The SST is made durable before the WAL is truncated.
    fn maybe_flush(&self, store: &mut Store) -> Result<()> {
        if store.mem_bytes < self.flush_threshold || store.mem.is_empty() {
            return Ok(());
        }
        let n = store.next_sst;
        store.next_sst += 1;
        let path = self.dir.join(format!("sst-{n:06}"));
        sst::write(&path, &store.mem, store.committed_seq)?;
        let s = self.open_sst(&path)?;
        store.ssts.push(s);
        store.mem.clear();
        store.mem_bytes = 0;

        // The flushed records are now durable in the SST → drop them from the
        // WAL. (Not fsynced: if the truncation is lost, the next open just
        // replays records already captured by the SST, which is idempotent.)
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        wal.flush()?;
        let f = wal.get_mut();
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
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

        // Merge every run into one map (a later run's version wins), then drop
        // what no reader needs.
        let mut merged: BTreeMap<MemKey, Op> = BTreeMap::new();
        for run in &runs {
            run.load_into(&mut merged)?;
        }
        let kept = gc_versions(merged, min_snap);
        let max_seq = kept
            .keys()
            .map(|(_, _, Reverse(s))| *s)
            .max()
            .unwrap_or(committed_seq);

        let path = self.dir.join(format!("sst-{next:06}"));
        sst::write(&path, &kept, max_seq)?;
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

/// Reclaim dead versions from a merged run. Processing each key newest-first,
/// keep versions down to and including the first at or below `min_snapshot`
/// (the "floor" a reader at that snapshot would see); drop everything older. A
/// key whose only survivor is a tombstone at/below the floor is dropped
/// entirely — this is the bottom run, so nothing older can resurface.
fn gc_versions(merged: BTreeMap<MemKey, Op>, min_snapshot: u64) -> BTreeMap<MemKey, Op> {
    let mut out = BTreeMap::new();
    let mut group: Vec<(u64, Op)> = Vec::new();
    let mut cur: Option<(u8, Vec<u8>)> = None;

    for ((table, key, Reverse(seq)), op) in merged {
        let k = (table, key);
        if cur.as_ref() != Some(&k) {
            if let Some((t, key)) = cur.take() {
                emit_group(t, &key, &group, min_snapshot, &mut out);
            }
            group.clear();
            cur = Some(k);
        }
        group.push((seq, op)); // newest-first: merged iterates seq DESC per key
    }
    if let Some((t, key)) = cur {
        emit_group(t, &key, &group, min_snapshot, &mut out);
    }
    out
}

/// Emit the surviving versions of one key (its `group`, newest-first).
fn emit_group(
    table: u8,
    key: &[u8],
    group: &[(u64, Op)],
    min_snapshot: u64,
    out: &mut BTreeMap<MemKey, Op>,
) {
    let mut keep = 0;
    for (seq, _) in group {
        keep += 1;
        if *seq <= min_snapshot {
            break; // floor reached; older versions are unreachable
        }
    }
    let kept = &group[..keep];
    // A lone tombstone at/below the floor leaves nothing to shadow → drop it.
    if let [(seq, Op::Del)] = kept
        && *seq <= min_snapshot
    {
        return;
    }
    for (seq, op) in kept {
        out.insert((table, key.to_vec(), Reverse(*seq)), op.clone());
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
        let gate = self.write_gate.lock().unwrap_or_else(|e| e.into_inner());
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
    _gate: MutexGuard<'a, ()>,
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
        let batch = WalBatchRef {
            seq,
            ops: self
                .buf
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
            let mut wal = self.engine.wal.lock().unwrap_or_else(|e| e.into_inner());
            append_batch(&mut *wal, &batch)?;
            wal.flush()?;
            wal.get_ref().sync_all()?;
        }

        // Publish to the memtable, advance the sequence, then flush if large.
        {
            let mut store = self.engine.store.write().unwrap_or_else(|e| e.into_inner());
            for ((t, k), op) in self.buf {
                store.mem_bytes += 1 + k.len() + 8 + op.value_len();
                store.mem.insert((t, k, Reverse(seq)), op);
            }
            store.committed_seq = seq;
            self.engine.maybe_flush(&mut store)?;
        }
        // Compact outside the store lock (heavy merge I/O shouldn't block reads).
        self.engine.maybe_compact()
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

fn append_batch(w: &mut impl Write, batch: &WalBatchRef<'_>) -> Result<()> {
    let body = postcard::to_stdvec(batch).map_err(backend)?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
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
