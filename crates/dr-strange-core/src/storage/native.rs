//! Native LSM storage engine (arch/01 v2) — the hand-rolled alternative to
//! redb, selected by the `native-backend` feature.
//!
//! **Phase 1 (this module): the memtable + WAL foundation.** Every logical
//! table shares one keyspace, keys prefixed with a one-byte table id. Writes
//! land in an in-memory, versioned memtable (a `BTreeMap`) and are made durable
//! by a write-ahead log; on open the WAL is replayed to reconstruct the
//! memtable. SST flush and compaction (which bound memory and WAL size) land in
//! later phases — until then the whole dataset lives in the memtable and the
//! WAL grows unbounded, but the transactional/MVCC/durability semantics are the
//! final ones.
//!
//! **MVCC by sequence number.** Each committed write transaction gets one
//! monotonically increasing sequence number; every key version it writes is
//! stamped with it. A read transaction pins the latest committed sequence at
//! `begin_read` and only ever sees versions at or below it — so a reader opened
//! before a commit keeps seeing the old value afterwards, with no locks held
//! across its lifetime (versions are never mutated in place, only superseded).
//!
//! **Atomic, durable commit.** A whole transaction is one length-prefixed,
//! CRC32-checked WAL batch record, `fsync`ed before the memtable is updated. A
//! crash mid-append leaves a torn tail that fails its checksum and is discarded
//! on replay — so a commit is all-or-nothing.
//!
//! The on-disk form is a **directory** (not a single file): the WAL lives at
//! `<path>/wal`, and future SSTs alongside it.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::engine::{
    KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction, prefix_successor,
};

/// A memtable entry: a value, or a tombstone marking a deletion.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    Put(Vec<u8>),
    Del,
}

/// Memtable key: `(table, user_key, Reverse(seq))`. The `Reverse` orders a
/// key's versions newest-first, so the first version with `seq <= snapshot`
/// found in a forward scan is the visible one.
type MemKey = (u8, Vec<u8>, Reverse<u64>);

/// The versioned memtable plus the high-water sequence of committed data. Kept
/// together under one lock so a reader sees `committed_seq` consistent with the
/// versions backing it.
#[derive(Default)]
struct Store {
    mem: BTreeMap<MemKey, Op>,
    committed_seq: u64,
}

/// The native LSM engine. `Send + Sync`: reads take a shared lock on `store`,
/// the single writer is serialized by `write_gate`, and the WAL has its own
/// lock so an in-flight `fsync` doesn't block readers.
pub struct NativeEngine {
    store: RwLock<Store>,
    wal: Mutex<BufWriter<File>>,
    write_gate: Mutex<()>,
    #[allow(dead_code)] // used once SST files land beside the WAL (later phase)
    dir: PathBuf,
}

impl NativeEngine {
    /// Open (creating if absent) the engine directory at `path` and replay its
    /// WAL into a fresh memtable.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let wal_path = dir.join("wal");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)?;

        // Replay: rebuild the memtable and find where the last intact record
        // ends, then truncate any torn tail so the next append is clean.
        let mut store = Store::default();
        let valid_len = replay(&mut file, &mut store)?;
        file.set_len(valid_len)?;
        file.seek(SeekFrom::Start(valid_len))?;

        Ok(Self {
            store: RwLock::new(store),
            wal: Mutex::new(BufWriter::new(file)),
            write_gate: Mutex::new(()),
            dir,
        })
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
        // Hold the gate for the whole transaction — one writer at a time.
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

/// Newest version of `(table, key)` visible at `snapshot`, from the memtable.
fn visible(mem: &BTreeMap<MemKey, Op>, table: u8, key: &[u8], snapshot: u64) -> Option<Vec<u8>> {
    let lo = (table, key.to_vec(), Reverse(u64::MAX));
    let hi = (table, key.to_vec(), Reverse(0u64));
    for ((_, _, Reverse(seq)), op) in mem.range(lo..=hi) {
        if *seq <= snapshot {
            return match op {
                Op::Put(v) => Some(v.clone()),
                Op::Del => None,
            };
        }
    }
    None
}

/// Collect the visible-at-`snapshot` `(key, value)` pairs of `table` in
/// `[start, end)`, in key order — deduping each user key to its newest visible
/// version and dropping tombstones.
fn visible_range(
    mem: &BTreeMap<MemKey, Op>,
    table: u8,
    start: &[u8],
    end: Option<&[u8]>,
    snapshot: u64,
) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let lo = Bound::Included((table, start.to_vec(), Reverse(u64::MAX)));
    let hi = match end {
        // Exclude every version of the end key.
        Some(e) => Bound::Excluded((table, e.to_vec(), Reverse(u64::MAX))),
        // End of this table = start of the next table id.
        None => Bound::Excluded((table + 1, Vec::new(), Reverse(u64::MAX))),
    };

    let mut out = BTreeMap::new();
    let mut cur: Option<&[u8]> = None;
    for ((_, user, Reverse(seq)), op) in mem.range((lo, hi)) {
        if cur == Some(user.as_slice()) {
            continue; // already resolved this user key to its newest visible
        }
        if *seq > snapshot {
            continue; // version too new for this snapshot; try older ones
        }
        cur = Some(user.as_slice());
        if let Op::Put(v) = op {
            out.insert(user.clone(), v.clone());
        }
    }
    out
}

pub struct NativeReadTxn<'a> {
    engine: &'a NativeEngine,
    snapshot: u64,
}

impl ReadTransaction for NativeReadTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        Ok(visible(&store.mem, table as u8, key, self.snapshot))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        let out = visible_range(&store.mem, table as u8, start, end, self.snapshot);
        Ok(Box::new(out.into_iter().map(Ok)))
    }
}

pub struct NativeWriteTxn<'a> {
    engine: &'a NativeEngine,
    _gate: MutexGuard<'a, ()>,
    /// Committed high-water at `begin_write`; the gate keeps it fixed, so the
    /// commit sequence is exactly `snapshot + 1`.
    snapshot: u64,
    /// Staged mutations, last-write-wins per key (no seq yet — assigned at
    /// commit). Read-your-own-writes reads these before the committed store.
    buf: BTreeMap<(u8, Vec<u8>), Op>,
}

impl ReadTransaction for NativeWriteTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(op) = self.buf.get(&(table as u8, key.to_vec())) {
            return Ok(match op {
                Op::Put(v) => Some(v.clone()),
                Op::Del => None,
            });
        }
        let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
        Ok(visible(&store.mem, table as u8, key, self.snapshot))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        // Committed view, then overlay the transaction's own staged writes.
        let mut out = {
            let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
            visible_range(&store.mem, table as u8, start, end, self.snapshot)
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
        // Drop staged writes under the prefix.
        self.buf
            .retain(|(bt, k), _| !(*bt == t && k.starts_with(prefix)));
        // Tombstone every committed key under the prefix visible at snapshot.
        let end = prefix_successor(prefix);
        let victims: Vec<Vec<u8>> = {
            let store = self.engine.store.read().unwrap_or_else(|e| e.into_inner());
            visible_range(&store.mem, t, prefix, end.as_deref(), self.snapshot)
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
            return Ok(()); // nothing to persist; sequence does not advance
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

        // Durability first: append + fsync the WAL before the data is visible.
        {
            let mut wal = self.engine.wal.lock().unwrap_or_else(|e| e.into_inner());
            append_batch(&mut *wal, &batch)?;
            wal.flush()?;
            wal.get_ref().sync_all()?;
        }

        // Then publish to the memtable and advance the high-water sequence.
        let mut store = self.engine.store.write().unwrap_or_else(|e| e.into_inner());
        for ((t, k), op) in self.buf {
            store.mem.insert((t, k, Reverse(seq)), op);
        }
        store.committed_seq = seq;
        Ok(())
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
            break; // truncated body
        }
        let body = &bytes[body_start..body_end];
        if crc32(body) != crc {
            break; // torn / corrupt record
        }
        let Ok(batch) = postcard::from_bytes::<WalBatch>(body) else {
            break;
        };
        for op in batch.ops {
            let entry = op.value.map_or(Op::Del, Op::Put);
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

/// Bitwise CRC-32 (IEEE 802.3 polynomial) — no dependency, and the WAL isn't a
/// throughput bottleneck at this phase.
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
