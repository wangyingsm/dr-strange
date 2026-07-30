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
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::engine::{
    KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction, prefix_successor,
};

/// Flush the memtable to an SST once its live bytes exceed this. SST + WAL then
/// bound memory and log growth; below it everything stays in the memtable.
const DEFAULT_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;

/// A memtable entry: a value, or a tombstone marking a deletion.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Put(Vec<u8>),
    Del,
}

fn op_value(op: Op) -> Option<Vec<u8>> {
    match op {
        Op::Put(v) => Some(v),
        Op::Del => None,
    }
}

fn op_bytes(op: &Op) -> usize {
    match op {
        Op::Put(v) => v.len(),
        Op::Del => 0,
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

/// The native LSM engine. `Send + Sync`: reads take a shared lock on `store`,
/// the single writer is serialized by `write_gate`, and the WAL has its own
/// lock so an in-flight `fsync` doesn't block readers.
pub struct NativeEngine {
    store: RwLock<Store>,
    wal: Mutex<BufWriter<File>>,
    write_gate: Mutex<()>,
    dir: PathBuf,
    flush_threshold: usize,
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

        let mut store = Store {
            next_sst: sst::next_number(&dir),
            ..Store::default()
        };
        // SSTs first (oldest→newest), tracking the highest sequence they hold.
        for sst_path in sst::list(&dir) {
            let s = sst::Sst::open(&sst_path)?;
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
            dir,
            flush_threshold,
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
        store.ssts.push(Arc::new(sst::Sst::open(&path)?));
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
}

impl StorageEngine for NativeEngine {
    type ReadTxn<'a> = NativeReadTxn<'a>;
    type WriteTxn<'a> = NativeWriteTxn<'a>;

    fn begin_read(&self) -> Result<NativeReadTxn<'_>> {
        let snapshot = self
            .store
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .committed_seq;
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
        return Ok(op_value(op));
    }
    for s in store.ssts.iter().rev() {
        if let Some(op) = s.get(table, key, snapshot)? {
            return Ok(op_value(op));
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
        .filter_map(|(k, op)| op_value(op).map(|v| (k, v)))
        .collect())
}

pub struct NativeReadTxn<'a> {
    engine: &'a NativeEngine,
    snapshot: u64,
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
            return Ok(op_value(op.clone()));
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
        let batch = WalBatch {
            seq,
            ops: self
                .buf
                .iter()
                .map(|((t, k), op)| WalOp {
                    table: *t,
                    key: k.clone(),
                    value: match op {
                        Op::Put(v) => Some(v.clone()),
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
        let mut store = self.engine.store.write().unwrap_or_else(|e| e.into_inner());
        for ((t, k), op) in self.buf {
            store.mem_bytes += 1 + k.len() + 8 + op_bytes(&op);
            store.mem.insert((t, k, Reverse(seq)), op);
        }
        store.committed_seq = seq;
        self.engine.maybe_flush(&mut store)
    }
}

// ---- WAL format ------------------------------------------------------------
//
// A record is `[u32 len][u32 crc32][postcard(WalBatch)]`, all little-endian.
// One record per committed transaction, so replay is all-or-nothing per commit.

#[derive(Serialize, Deserialize)]
struct WalBatch {
    seq: u64,
    ops: Vec<WalOp>,
}

#[derive(Serialize, Deserialize)]
struct WalOp {
    table: u8,
    key: Vec<u8>,
    /// `None` is a tombstone (delete).
    value: Option<Vec<u8>>,
}

fn append_batch(w: &mut impl Write, batch: &WalBatch) -> Result<()> {
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
            store.mem_bytes += 1 + op.key.len() + 8 + op_bytes(&entry);
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
