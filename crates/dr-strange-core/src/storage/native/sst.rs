//! Sorted String Table (SST) — an immutable, sorted, on-disk run of the LSM
//! engine (arch/01 v2, Phase 2). A flush writes the whole memtable to one SST;
//! reads then merge the live memtable over the SSTs, newest run first.
//!
//! **Layout** (all integers little-endian):
//! ```text
//!   [data block]*     entries, sorted by (table, key, seq DESC)
//!   [index block]     one entry per data block: its first key + offset/len
//!   [bloom block]     Bloom filter over the run's (table, key) pairs
//!   [footer]          index+bloom offsets/lens, entry_count, max_seq, magic, crc32
//! ```
//! An **entry** is `table:u8, key_len:u32, key, seq:u64, flag:u8 (0=del,1=put),
//! val_len:u32, val`. Keys are compared structurally as `(table, key, seq DESC)`
//! — the same order the memtable's `BTreeMap` uses — so a block's first key in
//! the index is enough to binary-search to the run of a user key's versions.
//!
//! Reads go through `pread` (a `Mutex<File>` + seek/read, portable) a block at a
//! time; a block cache lands in a later phase.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::storage::native::{MemKey, Op};

const MAGIC: u32 = 0x5353_5244; // "DRSS"
// Footer: index_offset, index_len, bloom_offset, bloom_len, count, max_seq (6×u64)
// + magic, crc (2×u32).
const FOOTER_LEN: u64 = 8 * 6 + 4 * 2; // 56 bytes
/// Target uncompressed size of a data block before it's cut.
const BLOCK_TARGET: usize = 16 * 1024;
/// Bloom-filter bits per key (~1% false-positive at k=7).
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOOM_HASHES: u32 = 7;

/// A Bloom filter over a run's `(table, key)` pairs, so a point `get` for a key
/// absent from a run skips scanning it. Double-hashing (`h1 + i·h2`) derives the
/// `k` positions from one 64-bit FNV-1a hash.
struct Bloom {
    bits: Vec<u8>,
    m_bits: u64,
    k: u32,
}

impl Bloom {
    fn new(n: usize) -> Self {
        let m_bits = (((n.max(1) * BLOOM_BITS_PER_KEY) as u64) + 7) & !7; // round to a byte
        Bloom {
            bits: vec![0u8; (m_bits / 8) as usize],
            m_bits,
            k: BLOOM_HASHES,
        }
    }

    fn hashes(table: u8, key: &[u8]) -> (u64, u64) {
        let mut h = 0xcbf2_9ce4_8422_2325u64; // FNV-1a over [table] ++ key
        h ^= table as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        for &b in key {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (h, h.rotate_left(32) | 1) // (h1, odd h2)
    }

    fn add(&mut self, table: u8, key: &[u8]) {
        let (mut h1, h2) = Self::hashes(table, key);
        for _ in 0..self.k {
            let bit = h1 % self.m_bits;
            self.bits[(bit / 8) as usize] |= 1 << (bit % 8);
            h1 = h1.wrapping_add(h2);
        }
    }

    fn maybe_contains(&self, table: u8, key: &[u8]) -> bool {
        if self.m_bits == 0 {
            return true;
        }
        let (mut h1, h2) = Self::hashes(table, key);
        for _ in 0..self.k {
            let bit = h1 % self.m_bits;
            if self.bits[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
                return false;
            }
            h1 = h1.wrapping_add(h2);
        }
        true
    }

    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + self.bits.len());
        put_u64(&mut buf, self.m_bits);
        put_u32(&mut buf, self.k);
        buf.extend_from_slice(&self.bits);
        buf
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        let mut c = Cursor::new(buf);
        let m_bits = c.u64()?;
        let k = c.u32()?;
        let bits = c.bytes(buf.len() - c.pos)?.to_vec();
        if bits.len() as u64 != m_bits / 8 {
            return None;
        }
        Some(Bloom { bits, m_bits, k })
    }
}

/// One decoded key position `(table, key, seq)`, compared as `(table, key,
/// seq DESC)` to match the memtable order.
#[derive(PartialEq, Eq)]
struct KeyPos {
    table: u8,
    key: Vec<u8>,
    seq: u64,
}

impl KeyPos {
    /// `< target` in `(table, key, seq DESC)` order.
    fn cmp(&self, table: u8, key: &[u8], seq: u64) -> std::cmp::Ordering {
        (self.table, self.key.as_slice())
            .cmp(&(table, key))
            .then(seq.cmp(&self.seq)) // reversed: higher seq is "smaller"
    }
}

/// An index entry: a data block's first key and where the block lives.
struct BlockRef {
    first: KeyPos,
    offset: u64,
    len: u32,
}

pub(super) struct Sst {
    file: Mutex<File>,
    index: Vec<BlockRef>,
    bloom: Bloom,
    pub(super) max_seq: u64,
    /// This run's file, so compaction can delete it once merged away.
    pub(super) path: PathBuf,
}

// ---- writing ---------------------------------------------------------------

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn encode_entry(buf: &mut Vec<u8>, table: u8, key: &[u8], seq: u64, op: &Op) {
    buf.push(table);
    put_u32(buf, key.len() as u32);
    buf.extend_from_slice(key);
    put_u64(buf, seq);
    match op {
        Op::Put(v) => {
            buf.push(1);
            put_u32(buf, v.len() as u32);
            buf.extend_from_slice(v);
        }
        Op::Del => {
            buf.push(0);
            put_u32(buf, 0);
        }
    }
}

fn encode_index_key(buf: &mut Vec<u8>, k: &KeyPos) {
    buf.push(k.table);
    put_u32(buf, k.key.len() as u32);
    buf.extend_from_slice(&k.key);
    put_u64(buf, k.seq);
}

/// Write `entries` (already sorted, as a memtable `BTreeMap` is) to an SST at
/// `path`, stamped with `max_seq`. Writes to a temp file then renames, so a
/// crash mid-flush can't leave a half-written SST under the real name.
pub(super) fn write(
    path: &Path,
    entries: &std::collections::BTreeMap<MemKey, Op>,
    max_seq: u64,
) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;

    let mut index: Vec<(KeyPos, u64, u32)> = Vec::new();
    let mut block = Vec::new();
    let mut block_first: Option<KeyPos> = None;
    let mut offset = 0u64;
    let mut count = 0u64;
    let mut bloom = Bloom::new(entries.len());

    let flush_block = |block: &mut Vec<u8>,
                       first: &mut Option<KeyPos>,
                       offset: &mut u64,
                       file: &mut File,
                       index: &mut Vec<(KeyPos, u64, u32)>|
     -> Result<()> {
        if block.is_empty() {
            return Ok(());
        }
        file.write_all(block)?;
        let len = block.len() as u32;
        index.push((first.take().expect("block has a first key"), *offset, len));
        *offset += len as u64;
        block.clear();
        Ok(())
    };

    for ((table, key, std::cmp::Reverse(seq)), op) in entries {
        if block_first.is_none() {
            block_first = Some(KeyPos {
                table: *table,
                key: key.clone(),
                seq: *seq,
            });
        }
        encode_entry(&mut block, *table, key, *seq, op);
        bloom.add(*table, key);
        count += 1;
        if block.len() >= BLOCK_TARGET {
            flush_block(
                &mut block,
                &mut block_first,
                &mut offset,
                &mut file,
                &mut index,
            )?;
        }
    }
    flush_block(
        &mut block,
        &mut block_first,
        &mut offset,
        &mut file,
        &mut index,
    )?;

    // Index block.
    let index_offset = offset;
    let mut index_bytes = Vec::new();
    for (first, off, len) in &index {
        encode_index_key(&mut index_bytes, first);
        put_u64(&mut index_bytes, *off);
        put_u32(&mut index_bytes, *len);
    }
    file.write_all(&index_bytes)?;

    // Bloom block (right after the index).
    let bloom_offset = index_offset + index_bytes.len() as u64;
    let bloom_bytes = bloom.encode();
    file.write_all(&bloom_bytes)?;

    // Footer.
    let mut footer = Vec::with_capacity(FOOTER_LEN as usize);
    put_u64(&mut footer, index_offset);
    put_u64(&mut footer, index_bytes.len() as u64);
    put_u64(&mut footer, bloom_offset);
    put_u64(&mut footer, bloom_bytes.len() as u64);
    put_u64(&mut footer, count);
    put_u64(&mut footer, max_seq);
    put_u32(&mut footer, MAGIC);
    let crc = super::crc32(&footer);
    put_u32(&mut footer, crc);
    file.write_all(&footer)?;

    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---- reading ---------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn u32(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let v = u32::from_le_bytes(self.buf.get(self.pos..end)?.try_into().ok()?);
        self.pos = end;
        Some(v)
    }
    fn u64(&mut self) -> Option<u64> {
        let end = self.pos.checked_add(8)?;
        let v = u64::from_le_bytes(self.buf.get(self.pos..end)?.try_into().ok()?);
        self.pos = end;
        Some(v)
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
}

/// A decoded data-block entry.
struct Entry {
    table: u8,
    key: Vec<u8>,
    seq: u64,
    op: Op,
}

fn decode_entry(c: &mut Cursor<'_>) -> Option<Entry> {
    let table = c.u8()?;
    let key_len = c.u32()? as usize;
    let key = c.bytes(key_len)?.to_vec();
    let seq = c.u64()?;
    let flag = c.u8()?;
    let val_len = c.u32()? as usize;
    let val = c.bytes(val_len)?;
    let op = if flag == 1 {
        Op::Put(val.to_vec())
    } else {
        Op::Del
    };
    Some(Entry {
        table,
        key,
        seq,
        op,
    })
}

impl Sst {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        if file_len < FOOTER_LEN {
            return Err(Error::Corrupt("SST too short".into()));
        }
        // Footer.
        file.seek(SeekFrom::Start(file_len - FOOTER_LEN))?;
        let mut footer = vec![0u8; FOOTER_LEN as usize];
        file.read_exact(&mut footer)?;
        let mut c = Cursor::new(&footer);
        let bad = || Error::Corrupt("bad SST footer".into());
        let index_offset = c.u64().ok_or_else(bad)?;
        let index_len = c.u64().ok_or_else(bad)?;
        let bloom_offset = c.u64().ok_or_else(bad)?;
        let bloom_len = c.u64().ok_or_else(bad)?;
        let _count = c.u64().ok_or_else(bad)?;
        let max_seq = c.u64().ok_or_else(bad)?;
        let magic = c.u32().ok_or_else(bad)?;
        let crc = c.u32().ok_or_else(bad)?;
        if magic != MAGIC || super::crc32(&footer[..FOOTER_LEN as usize - 4]) != crc {
            return Err(Error::Corrupt("SST footer magic/crc mismatch".into()));
        }

        // Bloom block.
        file.seek(SeekFrom::Start(bloom_offset))?;
        let mut bbuf = vec![0u8; bloom_len as usize];
        file.read_exact(&mut bbuf)?;
        let bloom = Bloom::decode(&bbuf).ok_or_else(|| Error::Corrupt("bad SST bloom".into()))?;

        // Index block.
        file.seek(SeekFrom::Start(index_offset))?;
        let mut ibuf = vec![0u8; index_len as usize];
        file.read_exact(&mut ibuf)?;
        let mut ic = Cursor::new(&ibuf);
        let mut index = Vec::new();
        while ic.pos < ibuf.len() {
            let table = ic
                .u8()
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?;
            let key_len = ic
                .u32()
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?
                as usize;
            let key = ic
                .bytes(key_len)
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?
                .to_vec();
            let seq = ic
                .u64()
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?;
            let offset = ic
                .u64()
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?;
            let len = ic
                .u32()
                .ok_or_else(|| Error::Corrupt("bad SST index".into()))?;
            index.push(BlockRef {
                first: KeyPos { table, key, seq },
                offset,
                len,
            });
        }

        Ok(Self {
            file: Mutex::new(file),
            index,
            bloom,
            max_seq,
            path: path.to_path_buf(),
        })
    }

    /// Read every entry of this run into `out` (compaction input). A later run's
    /// version of a key overwrites an earlier one because `out` is keyed by
    /// `(table, key, Reverse(seq))` and runs have disjoint sequence ranges.
    pub(super) fn load_into(&self, out: &mut BTreeMap<MemKey, Op>) -> Result<()> {
        for block in &self.index {
            let buf = self.read_block(block)?;
            let mut c = Cursor::new(&buf);
            while let Some(e) = decode_entry(&mut c) {
                out.insert((e.table, e.key, std::cmp::Reverse(e.seq)), e.op);
            }
        }
        Ok(())
    }

    fn read_block(&self, block: &BlockRef) -> Result<Vec<u8>> {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.seek(SeekFrom::Start(block.offset))?;
        let mut buf = vec![0u8; block.len as usize];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// The index of the first block that may hold `(table, key)`'s versions:
    /// the last block whose first key is `<= (table, key, seq=MAX)`.
    fn start_block(&self, table: u8, key: &[u8]) -> Option<usize> {
        if self.index.is_empty() {
            return None;
        }
        // partition_point: count of blocks whose first key <= target.
        let n = self
            .index
            .partition_point(|b| b.first.cmp(table, key, u64::MAX) != std::cmp::Ordering::Greater);
        Some(n.saturating_sub(1))
    }

    /// Newest version of `(table, key)` visible at `snapshot` in this SST, as an
    /// `Op` (`None` if this SST holds no version at or below `snapshot`).
    pub(super) fn get(&self, table: u8, key: &[u8], snapshot: u64) -> Result<Option<Op>> {
        // Skip the whole run if its Bloom filter rules the key out.
        if !self.bloom.maybe_contains(table, key) {
            return Ok(None);
        }
        let Some(start) = self.start_block(table, key) else {
            return Ok(None);
        };
        for block in &self.index[start..] {
            // Skip blocks whose whole range is before the key.
            let buf = self.read_block(block)?;
            let mut c = Cursor::new(&buf);
            while let Some(e) = decode_entry(&mut c) {
                match (e.table, e.key.as_slice()).cmp(&(table, key)) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Greater => return Ok(None), // passed the key
                    std::cmp::Ordering::Equal => {
                        if e.seq <= snapshot {
                            return Ok(Some(e.op)); // newest visible version
                        }
                    }
                }
            }
            // Reached block end still at/-before the key; try the next block.
        }
        Ok(None)
    }

    /// Newest-visible `Op` per user key in `[start, end)` of `table`, as
    /// `(key, op)` pairs in key order. Tombstones are included (`Op::Del`) so
    /// the merge can apply deletions over older runs.
    pub(super) fn range(
        &self,
        table: u8,
        start: &[u8],
        end: Option<&[u8]>,
        snapshot: u64,
        out: &mut std::collections::BTreeMap<Vec<u8>, Op>,
    ) -> Result<()> {
        let Some(start_block) = self.start_block(table, start) else {
            return Ok(());
        };
        let mut cur_key: Option<Vec<u8>> = None;
        for block in &self.index[start_block..] {
            let buf = self.read_block(block)?;
            let mut c = Cursor::new(&buf);
            while let Some(e) = decode_entry(&mut c) {
                if e.table != table {
                    if e.table > table {
                        return Ok(());
                    }
                    continue;
                }
                if e.key.as_slice() < start {
                    continue;
                }
                if end.is_some_and(|end| e.key.as_slice() >= end) {
                    return Ok(());
                }
                if cur_key.as_deref() == Some(e.key.as_slice()) {
                    continue; // already took this key's newest-visible
                }
                if e.seq > snapshot {
                    continue; // too new; older versions of this key may follow
                }
                cur_key = Some(e.key.clone());
                out.insert(e.key, e.op);
            }
        }
        Ok(())
    }
}

/// The next unused SST number given the existing files in `dir` (sst-000001…).
pub(super) fn next_number(dir: &Path) -> u64 {
    let mut max = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("sst-"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                max = max.max(n);
            }
        }
    }
    max + 1
}

/// Existing SST paths in `dir`, oldest first (ascending number).
pub(super) fn list(dir: &Path) -> Vec<PathBuf> {
    let mut ssts: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_prefix("sst-"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                ssts.push((n, entry.path()));
            }
        }
    }
    ssts.sort_by_key(|(n, _)| *n);
    ssts.into_iter().map(|(_, p)| p).collect()
}
