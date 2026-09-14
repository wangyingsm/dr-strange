//! Sorted String Table (SST) — an immutable, sorted, on-disk run of the LSM
//! engine (arch/01 v2, Phase 2). A flush writes the whole memtable to one SST;
//! reads then merge the live memtable over the SSTs, newest run first.
//!
//! **Layout** (all integers little-endian):
//! ```text
//!   [data block]*     entries, sorted by (table, key, seq DESC)   + crc32
//!   [index block]     one entry per data block: its first key + offset/len + crc32
//!   [bloom block]     Bloom filter over the run's (table, key) pairs  + crc32
//!   [footer]          index+bloom offsets/lens, entry_count, max_seq, magic, crc32
//! ```
//! An **entry** is `table:u8, key_len:u32, key, seq:u64, flag:u8 (0=del,1=put),
//! val_len:u32, val`. Keys are compared structurally as `(table, key, seq DESC)`
//! — the same order the memtable's `BTreeMap` uses — so a block's first key in
//! the index is enough to binary-search to the run of a user key's versions.
//!
//! **Format versions.** The footer's magic names the version. `DRSS` (v1) has no
//! block checksums; `DRS2` (v2, current) ends every block — data, index, bloom —
//! with a CRC-32 of its bytes, and the index/footer lengths include that
//! trailer. Readers accept both; the writer emits v2 only. A block whose CRC
//! does not match, or whose bytes do not decode to whole entries, is
//! `Error::Corrupt` — never a silent "absent", which is what a v1 reader used to
//! report when bit-rot broke an entry mid-block.
//!
//! Reads are positional (`pread`/`seek_read`) a block at a time, so concurrent
//! readers of one run do not serialize on a file cursor; verified blocks are
//! kept in the engine's shared block cache.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::storage::native::{BlockCache, MemKey, Op};

/// v1 footer magic ("DRSS"): blocks carry no checksum. Read-only.
const MAGIC_V1: u32 = 0x5353_5244;
/// v2 footer magic ("DRS2"): every block ends with a CRC-32 trailer.
const MAGIC_V2: u32 = 0x3253_5244;
/// Bytes of the CRC-32 trailer that ends every v2 block.
const BLOCK_CRC_LEN: usize = 4;
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

/// On-disk format version, decided by the footer magic at open.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Version {
    /// No block checksums.
    V1,
    /// CRC-32 trailer on every block.
    V2,
}

pub(super) struct Sst {
    /// Read positionally, never seeked: a shared cursor would serialize readers.
    file: File,
    version: Version,
    index: Vec<BlockRef>,
    bloom: Bloom,
    pub(super) max_seq: u64,
    /// Entries in the file (footer count) — sizes the Bloom filter of a run
    /// this one is merged into, without a counting pass.
    pub(super) count: u64,
    /// This run's file, so compaction can delete it once merged away.
    pub(super) path: PathBuf,
    /// Unique id (keys this run's blocks in the shared cache) + the cache.
    id: u64,
    cache: Arc<BlockCache>,
}

// ---- writing ---------------------------------------------------------------

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// The `table · key-len · key · seq` prefix shared by data-block entries and
/// index entries — one writer, so the two layouts cannot drift apart (the
/// index's first-key must compare consistently with entry order; see
/// [`KeyPos::cmp`]). Borrowed fields, so neither caller clones to encode.
fn encode_key_parts(buf: &mut Vec<u8>, table: u8, key: &[u8], seq: u64) {
    buf.push(table);
    put_u32(buf, key.len() as u32);
    buf.extend_from_slice(key);
    put_u64(buf, seq);
}

fn encode_entry(buf: &mut Vec<u8>, table: u8, key: &[u8], seq: u64, op: &Op) {
    encode_key_parts(buf, table, key, seq);
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
    encode_key_parts(buf, k.table, &k.key, k.seq);
}

/// Append a block's CRC-32 trailer and return its on-disk length (payload +
/// trailer), which is what the index and footer record. A block over `u32::MAX`
/// bytes is refused rather than having its length wrap in the index — a single
/// value that large is not something the engine writes today, but a wrapped
/// length would read back as a checksum failure on a healthy file.
fn seal_block(block: &mut Vec<u8>) -> Result<u32> {
    let crc = super::crc32(block);
    put_u32(block, crc);
    u32::try_from(block.len())
        .map_err(|_| Error::InvalidArgument("SST block exceeds u32::MAX bytes".into()))
}

/// Write a memtable (already sorted, as a `BTreeMap` is) to an SST at `path`,
/// stamped with `max_seq`. The flush path; see [`write_sorted`].
pub(super) fn write(
    path: &Path,
    entries: &std::collections::BTreeMap<MemKey, Op>,
    max_seq: u64,
) -> Result<()> {
    write_sorted(path, entries.iter().map(Ok), entries.len(), max_seq)
}

/// One entry as the writer consumes it. Implemented for a memtable's borrowed
/// `(&MemKey, &Op)` and a merge's owned `(MemKey, Op)`, so neither a flush nor
/// a streamed compaction copies anything to encode.
pub(super) trait SstEntry {
    fn parts(&self) -> (u8, &[u8], u64, &Op);
}

impl SstEntry for (&MemKey, &Op) {
    fn parts(&self) -> (u8, &[u8], u64, &Op) {
        let ((t, k, std::cmp::Reverse(s)), op) = self;
        (*t, k.as_slice(), *s, op)
    }
}

impl SstEntry for (MemKey, Op) {
    fn parts(&self) -> (u8, &[u8], u64, &Op) {
        let ((t, k, std::cmp::Reverse(s)), op) = self;
        (*t, k.as_slice(), *s, op)
    }
}

/// Write `entries` — already in `(table, key, seq DESC)` order, an error
/// aborting the file — to an SST at `path`, stamped with `max_seq`. The
/// entries are consumed as they come, one data block resident at a time, so a
/// compaction can stream a merge of arbitrarily large runs through here;
/// `count_hint` sizes the Bloom filter (an over-estimate only costs bits).
/// Writes to a temp file then renames, so a crash mid-write can't leave a
/// half-written SST under the real name.
pub(super) fn write_sorted<E: SstEntry>(
    path: &Path,
    entries: impl Iterator<Item = Result<E>>,
    count_hint: usize,
    max_seq: u64,
) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = File::create(&tmp)?;

    let mut index: Vec<(KeyPos, u64, u32)> = Vec::new();
    // The in-progress data block: its first key (fixed at creation) bundled
    // with the encoded bytes, so a block-without-first-key is unrepresentable
    // — flushing needs no unwrap and no error path for an unreachable state.
    let mut cur: Option<(KeyPos, Vec<u8>)> = None;
    let mut offset = 0u64;
    let mut count = 0u64;
    let mut bloom = Bloom::new(count_hint);

    let flush_block = |cur: &mut Option<(KeyPos, Vec<u8>)>,
                       offset: &mut u64,
                       file: &mut File,
                       index: &mut Vec<(KeyPos, u64, u32)>|
     -> Result<()> {
        if let Some((first, mut block)) = cur.take() {
            let len = seal_block(&mut block)?;
            file.write_all(&block)?;
            index.push((first, *offset, len));
            *offset += len as u64;
        }
        Ok(())
    };

    for entry in entries {
        let entry = entry?;
        let (table, key, seq, op) = entry.parts();
        let (_, block) = cur.get_or_insert_with(|| {
            let first = KeyPos {
                table,
                key: key.to_vec(),
                seq,
            };
            (first, Vec::with_capacity(BLOCK_TARGET + BLOCK_TARGET / 4))
        });
        encode_entry(block, table, key, seq, op);
        bloom.add(table, key);
        count += 1;
        let full = block.len() >= BLOCK_TARGET;
        if full {
            flush_block(&mut cur, &mut offset, &mut file, &mut index)?;
        }
    }
    flush_block(&mut cur, &mut offset, &mut file, &mut index)?;

    // Index block.
    let index_offset = offset;
    let mut index_bytes = Vec::new();
    for (first, off, len) in &index {
        encode_index_key(&mut index_bytes, first);
        put_u64(&mut index_bytes, *off);
        put_u32(&mut index_bytes, *len);
    }
    seal_block(&mut index_bytes)?;
    file.write_all(&index_bytes)?;

    // Bloom block (right after the index).
    let bloom_offset = index_offset + index_bytes.len() as u64;
    let mut bloom_bytes = bloom.encode();
    seal_block(&mut bloom_bytes)?;
    file.write_all(&bloom_bytes)?;

    // Footer.
    let mut footer = Vec::with_capacity(FOOTER_LEN as usize);
    put_u64(&mut footer, index_offset);
    put_u64(&mut footer, index_bytes.len() as u64);
    put_u64(&mut footer, bloom_offset);
    put_u64(&mut footer, bloom_bytes.len() as u64);
    put_u64(&mut footer, count);
    put_u64(&mut footer, max_seq);
    put_u32(&mut footer, MAGIC_V2);
    let crc = super::crc32(&footer);
    put_u32(&mut footer, crc);
    file.write_all(&footer)?;

    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    // The rename is a directory-entry change; without this the file's bytes
    // are durable but its name may not be, and a WAL truncated on the strength
    // of this SST would then lose the records on a crash.
    if let Some(dir) = path.parent() {
        super::sync_dir(dir)?;
    }
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

/// A decoded data-block entry, borrowing from its block. Scans decode entries
/// by the dozen and keep almost none (wrong table, out of range, shadowed,
/// too-new seq), so decoding is allocation-free; the caller materializes via
/// [`EntryRef::op`] only for the entry it keeps.
struct EntryRef<'a> {
    table: u8,
    key: &'a [u8],
    seq: u64,
    /// `Some(value)` for a put, `None` for a tombstone.
    put: Option<&'a [u8]>,
}

impl EntryRef<'_> {
    fn op(&self) -> Op {
        match self.put {
            Some(v) => Op::Put(v.to_vec()),
            None => Op::Del,
        }
    }
}

/// The next entry of a data block, `Ok(None)` exactly at the block's end. A
/// block that ends mid-entry is `Error::Corrupt`: v2 blocks are CRC-checked
/// before decoding, so this can only mean the writer or the file is broken,
/// and a v1 block has nothing else to catch bit-rot with. Silently stopping
/// here (the old behaviour) turned corruption into "key absent".
fn next_entry<'a>(c: &mut Cursor<'a>) -> Result<Option<EntryRef<'a>>> {
    if c.pos >= c.buf.len() {
        return Ok(None);
    }
    decode_entry(c)
        .map(Some)
        .ok_or_else(|| Error::Corrupt("SST data block ends mid-entry".into()))
}

fn decode_entry<'a>(c: &mut Cursor<'a>) -> Option<EntryRef<'a>> {
    let table = c.u8()?;
    let key_len = c.u32()? as usize;
    let key = c.bytes(key_len)?;
    let seq = c.u64()?;
    let flag = c.u8()?;
    let val_len = c.u32()? as usize;
    let val = c.bytes(val_len)?;
    Some(EntryRef {
        table,
        key,
        seq,
        put: (flag == 1).then_some(val),
    })
}

/// Fill `buf` from `file` at `offset` without touching the file's cursor, so
/// readers on different threads never serialize on a seek. `pread` on unix;
/// `seek_read` (an overlapped read, likewise cursor-free) on Windows.
fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("Sst::read_at needs a positional-read primitive for this platform");
    }
}

/// Read the `len` on-disk bytes of a block at `offset` and hand back its
/// payload: for v2 the trailing CRC-32 is checked and stripped, for v1 the
/// bytes are returned as-is (nothing to check against). A short file is
/// `Corrupt` rather than an io error — the footer promised bytes that are not
/// there.
fn read_block_at(
    file: &File,
    version: Version,
    what: &str,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    read_at(file, offset, &mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::Corrupt(format!("SST {what} block runs past end of file"))
        } else {
            Error::from(e)
        }
    })?;
    match version {
        Version::V1 => Ok(buf),
        Version::V2 => {
            let Some(payload_len) = buf.len().checked_sub(BLOCK_CRC_LEN) else {
                return Err(Error::Corrupt(format!(
                    "SST {what} block shorter than its crc"
                )));
            };
            let stored = u32::from_le_bytes(
                buf[payload_len..]
                    .try_into()
                    .map_err(|_| Error::Corrupt(format!("SST {what} block crc unreadable")))?,
            );
            buf.truncate(payload_len);
            if super::crc32(&buf) != stored {
                return Err(Error::Corrupt(format!("SST {what} block crc mismatch")));
            }
            Ok(buf)
        }
    }
}

/// Bound a footer-declared length before allocating for it: a garbled length
/// cannot exceed the file, so anything larger is corruption, not a request.
fn checked_len(what: &str, offset: u64, len: u64, file_len: u64) -> Result<usize> {
    let fits = offset.checked_add(len).is_some_and(|end| end <= file_len);
    if !fits {
        return Err(Error::Corrupt(format!(
            "SST {what} block lies outside the file"
        )));
    }
    usize::try_from(len).map_err(|_| Error::Corrupt(format!("SST {what} block too large")))
}

impl Sst {
    pub(super) fn open(path: &Path, id: u64, cache: Arc<BlockCache>) -> Result<Self> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < FOOTER_LEN {
            return Err(Error::Corrupt("SST too short".into()));
        }
        // Footer.
        let mut footer = vec![0u8; FOOTER_LEN as usize];
        read_at(&file, file_len - FOOTER_LEN, &mut footer)?;
        let mut c = Cursor::new(&footer);
        let bad = || Error::Corrupt("bad SST footer".into());
        let index_offset = c.u64().ok_or_else(bad)?;
        let index_len = c.u64().ok_or_else(bad)?;
        let bloom_offset = c.u64().ok_or_else(bad)?;
        let bloom_len = c.u64().ok_or_else(bad)?;
        let count = c.u64().ok_or_else(bad)?;
        let max_seq = c.u64().ok_or_else(bad)?;
        let magic = c.u32().ok_or_else(bad)?;
        let crc = c.u32().ok_or_else(bad)?;
        let version = match magic {
            MAGIC_V1 => Version::V1,
            MAGIC_V2 => Version::V2,
            _ => return Err(Error::Corrupt("SST footer magic/crc mismatch".into())),
        };
        if super::crc32(&footer[..FOOTER_LEN as usize - 4]) != crc {
            return Err(Error::Corrupt("SST footer magic/crc mismatch".into()));
        }
        let data_end = file_len - FOOTER_LEN;

        // Bloom block.
        let bloom_len = checked_len("bloom", bloom_offset, bloom_len, data_end)?;
        let bbuf = read_block_at(&file, version, "bloom", bloom_offset, bloom_len)?;
        let bloom = Bloom::decode(&bbuf).ok_or_else(|| Error::Corrupt("bad SST bloom".into()))?;

        // Index block.
        let index_len = checked_len("index", index_offset, index_len, data_end)?;
        let ibuf = read_block_at(&file, version, "index", index_offset, index_len)?;
        let mut ic = Cursor::new(&ibuf);
        let mut index = Vec::new();
        let bad_index = || Error::Corrupt("bad SST index".into());
        while ic.pos < ibuf.len() {
            let table = ic.u8().ok_or_else(bad_index)?;
            let key_len = ic.u32().ok_or_else(bad_index)? as usize;
            let key = ic.bytes(key_len).ok_or_else(bad_index)?.to_vec();
            let seq = ic.u64().ok_or_else(bad_index)?;
            let offset = ic.u64().ok_or_else(bad_index)?;
            let len = ic.u32().ok_or_else(bad_index)?;
            // A data block must lie in the data region; a garbled reference is
            // caught here rather than as a mystery read later.
            checked_len("data", offset, len as u64, index_offset)?;
            index.push(BlockRef {
                first: KeyPos { table, key, seq },
                offset,
                len,
            });
        }

        Ok(Self {
            file,
            version,
            index,
            bloom,
            max_seq,
            count,
            path: path.to_path_buf(),
            id,
            cache,
        })
    }

    /// Every entry of this run in file (= memtable) order, one block resident
    /// at a time — a compaction's input. Blocks are read past the shared cache:
    /// a full sweep of every run would otherwise evict the blocks live readers
    /// are using for the sake of bytes that are read exactly once.
    pub(super) fn entries(&self) -> Entries<'_> {
        Entries {
            sst: self,
            next_block: 0,
            block: Vec::new(),
            pos: 0,
        }
    }

    fn read_block(&self, block: &BlockRef) -> Result<Arc<Vec<u8>>> {
        let key = (self.id, block.offset);
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached);
        }
        // Only verified payloads enter the cache, so a hit needs no re-check.
        let buf = read_block_at(
            &self.file,
            self.version,
            "data",
            block.offset,
            block.len as usize,
        )?;
        let arc = Arc::new(buf);
        self.cache.insert(key, arc.clone());
        Ok(arc)
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
            while let Some(e) = next_entry(&mut c)? {
                match (e.table, e.key).cmp(&(table, key)) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Greater => return Ok(None), // passed the key
                    std::cmp::Ordering::Equal => {
                        if e.seq <= snapshot {
                            return Ok(Some(e.op())); // newest visible version
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
            while let Some(e) = next_entry(&mut c)? {
                if e.table != table {
                    if e.table > table {
                        return Ok(());
                    }
                    continue;
                }
                if e.key < start {
                    continue;
                }
                if end.is_some_and(|end| e.key >= end) {
                    return Ok(());
                }
                if cur_key.as_deref() == Some(e.key) {
                    continue; // already took this key's newest-visible
                }
                if e.seq > snapshot {
                    continue; // too new; older versions of this key may follow
                }
                cur_key = Some(e.key.to_vec());
                out.insert(e.key.to_vec(), e.op());
            }
        }
        Ok(())
    }
}

/// See [`Sst::entries`]. Yields owned `(MemKey, Op)` pairs; a block that fails
/// its checksum or ends mid-entry surfaces as the error and ends the sweep.
pub(super) struct Entries<'a> {
    sst: &'a Sst,
    next_block: usize,
    block: Vec<u8>,
    pos: usize,
}

impl Iterator for Entries<'_> {
    type Item = Result<(MemKey, Op)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos < self.block.len() {
                let mut c = Cursor {
                    buf: &self.block,
                    pos: self.pos,
                };
                let item = match next_entry(&mut c) {
                    Ok(Some(e)) => {
                        Ok(((e.table, e.key.to_vec(), std::cmp::Reverse(e.seq)), e.op()))
                    }
                    // `pos < len` so `next_entry` never says `None` here; a
                    // mid-entry end is the Corrupt it already reports.
                    Ok(None) => Err(Error::Corrupt("SST data block ends mid-entry".into())),
                    Err(e) => Err(e),
                };
                if item.is_err() {
                    // Do not retry the same bytes forever: the sweep is over.
                    self.next_block = self.sst.index.len();
                    self.block.clear();
                    self.pos = 0;
                } else {
                    self.pos = c.pos;
                }
                return Some(item);
            }
            let block = self.sst.index.get(self.next_block)?;
            self.next_block += 1;
            self.pos = 0;
            self.block = match read_block_at(
                &self.sst.file,
                self.sst.version,
                "data",
                block.offset,
                block.len as usize,
            ) {
                Ok(buf) => buf,
                Err(e) => {
                    self.next_block = self.sst.index.len();
                    self.block.clear();
                    return Some(Err(e));
                }
            };
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Reverse;
    use std::collections::BTreeMap;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("drsg-sst-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn sst(&self) -> PathBuf {
            self.0.join("sst-000001")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `n` puts on table 1 with 100-byte values (enough to span several
    /// 16 KiB blocks at n=1000) plus one tombstone, all at seq 1..=n+1.
    fn entries(n: u64) -> BTreeMap<MemKey, Op> {
        let mut m = BTreeMap::new();
        for i in 0..n {
            let key = format!("key-{i:06}").into_bytes();
            m.insert((1u8, key, Reverse(i + 1)), Op::Put(vec![i as u8; 100]));
        }
        m.insert((1u8, b"key-000000".to_vec(), Reverse(n + 1)), Op::Del);
        m
    }

    fn open(path: &Path) -> Result<Sst> {
        Sst::open(path, 1, super::super::new_block_cache())
    }

    fn assert_corrupt<T>(r: Result<T>, what: &str) {
        match r {
            Err(Error::Corrupt(_)) => {}
            Err(e) => panic!("{what}: expected Corrupt, got {e:?}"),
            Ok(_) => panic!("{what}: expected Corrupt, got Ok"),
        }
    }

    fn patch(path: &Path, f: impl FnOnce(&mut Vec<u8>)) {
        let mut bytes = std::fs::read(path).unwrap();
        f(&mut bytes);
        std::fs::write(path, bytes).unwrap();
    }

    /// The layout a pre-v2 writer produced: identical entries and footer, but
    /// no CRC trailers and the `DRSS` magic. Kept here (not in the writer) so
    /// the production path can never emit it by accident.
    fn write_v1(path: &Path, entries: &BTreeMap<MemKey, Op>, truncate_last_block_by: usize) {
        let mut out = Vec::new();
        let mut index: Vec<(KeyPos, u64, u32)> = Vec::new();
        let mut cur: Option<(KeyPos, Vec<u8>)> = None;
        let mut bloom = Bloom::new(entries.len());
        let mut blocks: Vec<(KeyPos, Vec<u8>)> = Vec::new();
        for ((table, key, Reverse(seq)), op) in entries {
            let (_, block) = cur.get_or_insert_with(|| {
                (
                    KeyPos {
                        table: *table,
                        key: key.clone(),
                        seq: *seq,
                    },
                    Vec::new(),
                )
            });
            encode_entry(block, *table, key, *seq, op);
            bloom.add(*table, key);
            if block.len() >= BLOCK_TARGET {
                blocks.push(cur.take().unwrap());
            }
        }
        if let Some(b) = cur.take() {
            blocks.push(b);
        }
        if let Some((_, last)) = blocks.last_mut() {
            let keep = last.len() - truncate_last_block_by;
            last.truncate(keep);
        }
        for (first, block) in blocks {
            index.push((first, out.len() as u64, block.len() as u32));
            out.extend_from_slice(&block);
        }
        let index_offset = out.len() as u64;
        let mut index_bytes = Vec::new();
        for (first, off, len) in &index {
            encode_index_key(&mut index_bytes, first);
            put_u64(&mut index_bytes, *off);
            put_u32(&mut index_bytes, *len);
        }
        out.extend_from_slice(&index_bytes);
        let bloom_offset = out.len() as u64;
        let bloom_bytes = bloom.encode();
        out.extend_from_slice(&bloom_bytes);
        let mut footer = Vec::new();
        put_u64(&mut footer, index_offset);
        put_u64(&mut footer, index_bytes.len() as u64);
        put_u64(&mut footer, bloom_offset);
        put_u64(&mut footer, bloom_bytes.len() as u64);
        put_u64(&mut footer, entries.len() as u64);
        put_u64(&mut footer, 7);
        put_u32(&mut footer, MAGIC_V1);
        let crc = super::super::crc32(&footer);
        put_u32(&mut footer, crc);
        out.extend_from_slice(&footer);
        std::fs::write(path, out).unwrap();
    }

    fn check_reads(sst: &Sst, n: u64) {
        // Point reads: tombstone shadows key 0 at the top, older version visible below.
        assert_eq!(sst.get(1, b"key-000000", u64::MAX).unwrap(), Some(Op::Del));
        assert_eq!(
            sst.get(1, b"key-000000", 1).unwrap(),
            Some(Op::Put(vec![0u8; 100]))
        );
        let mid = format!("key-{:06}", n / 2).into_bytes();
        assert_eq!(
            sst.get(1, &mid, u64::MAX).unwrap(),
            Some(Op::Put(vec![(n / 2) as u8; 100]))
        );
        assert_eq!(sst.get(1, b"key-zzz", u64::MAX).unwrap(), None);
        assert_eq!(sst.get(2, b"key-000001", u64::MAX).unwrap(), None);
        // Range and full load agree on cardinality.
        let mut out = BTreeMap::new();
        sst.range(1, b"", None, u64::MAX, &mut out).unwrap();
        assert_eq!(out.len() as u64, n);
        let all: BTreeMap<MemKey, Op> = sst.entries().collect::<Result<_>>().unwrap();
        assert_eq!(all.len() as u64, n + 1);
        assert_eq!(sst.count, n + 1);
        // The sweep yields file order, which is memtable order.
        let swept: Vec<MemKey> = sst.entries().map(|e| e.unwrap().0).collect();
        assert!(swept.windows(2).all(|w| w[0] < w[1]), "entries not sorted");
    }

    /// Drain a sweep, failing the test unless it ends in `Corrupt`.
    fn sweep_is_corrupt(sst: &Sst, what: &str) {
        let last = sst
            .entries()
            .last()
            .unwrap_or_else(|| panic!("{what}: empty sweep"));
        assert_corrupt(last, what);
        // And a broken sweep stops instead of re-yielding the same error.
        assert!(
            sst.entries().filter(|e| e.is_err()).count() == 1,
            "{what}: sweep must end at the first bad block"
        );
    }

    #[test]
    fn v2_round_trip_spans_blocks() {
        let d = Dir::new("v2-roundtrip");
        write(&d.sst(), &entries(1000), 1001).unwrap();
        let sst = open(&d.sst()).unwrap();
        assert_eq!(sst.version, Version::V2);
        assert!(sst.index.len() > 1, "test must span several blocks");
        assert_eq!(sst.max_seq, 1001);
        check_reads(&sst, 1000);
    }

    #[test]
    fn v1_file_still_opens_and_reads() {
        let d = Dir::new("v1-compat");
        write_v1(&d.sst(), &entries(1000), 0);
        let sst = open(&d.sst()).unwrap();
        assert_eq!(sst.version, Version::V1);
        assert!(sst.index.len() > 1);
        assert_eq!(sst.max_seq, 7);
        check_reads(&sst, 1000);
    }

    #[test]
    fn v1_block_ending_mid_entry_is_corrupt_not_absent() {
        let d = Dir::new("v1-torn");
        // Cut 3 bytes off the last block: its final entry decodes short.
        write_v1(&d.sst(), &entries(1000), 3);
        let sst = open(&d.sst()).unwrap();
        // Earlier blocks are intact and still serve.
        assert_eq!(
            sst.get(1, b"key-000001", u64::MAX).unwrap(),
            Some(Op::Put(vec![1u8; 100]))
        );
        assert_corrupt(sst.get(1, b"key-000999", u64::MAX), "get in torn block");
        assert_corrupt(
            sst.range(1, b"", None, u64::MAX, &mut BTreeMap::new()),
            "range across torn block",
        );
        sweep_is_corrupt(&sst, "sweep torn block");
    }

    #[test]
    fn v2_flipped_byte_in_data_block_is_corrupt() {
        let d = Dir::new("v2-bitrot");
        write(&d.sst(), &entries(1000), 1001).unwrap();
        let (first_off, last_off) = {
            let sst = open(&d.sst()).unwrap();
            let last = sst.index.last().unwrap();
            (sst.index[0].offset, last.offset)
        };
        // Flip one bit inside a value of the last block: the entry still
        // decodes, so only the checksum can tell.
        patch(&d.sst(), |b| b[last_off as usize + 40] ^= 0x01);
        let sst = open(&d.sst()).unwrap();
        assert_eq!(
            sst.get(1, b"key-000001", u64::MAX).unwrap(),
            Some(Op::Put(vec![1u8; 100])),
            "untouched first block still reads"
        );
        assert_corrupt(sst.get(1, b"key-000999", u64::MAX), "get in rotted block");
        sweep_is_corrupt(&sst, "sweep rotted block");
        // Garble the first block's key length (a structural break) as well:
        // the CRC catches it before decoding is even attempted.
        patch(&d.sst(), |b| b[first_off as usize + 1] = 0xff);
        let sst = open(&d.sst()).unwrap();
        assert_corrupt(sst.get(1, b"key-000000", u64::MAX), "get in garbled block");
        assert_corrupt(
            sst.range(1, b"", None, u64::MAX, &mut BTreeMap::new()),
            "range from garbled block",
        );
    }

    #[test]
    fn v2_index_and_bloom_are_checksummed() {
        let d = Dir::new("v2-meta");
        write(&d.sst(), &entries(50), 51).unwrap();
        let bytes = std::fs::read(d.sst()).unwrap();
        let footer = &bytes[bytes.len() - FOOTER_LEN as usize..];
        let mut c = Cursor::new(footer);
        let index_offset = c.u64().unwrap() as usize;
        let _index_len = c.u64().unwrap();
        let bloom_offset = c.u64().unwrap() as usize;
        // Index: a flipped byte in an offset field makes the whole file unopenable.
        let p = d.sst();
        patch(&p, |b| b[index_offset + 20] ^= 0x80);
        assert_corrupt(open(&p), "garbled index");
        // Bloom: restore, then flip a filter bit (would only cause false
        // negatives — silently missing keys — without the checksum).
        std::fs::write(&p, &bytes).unwrap();
        patch(&p, |b| b[bloom_offset + 12] ^= 0x01);
        assert_corrupt(open(&p), "garbled bloom");
    }

    #[test]
    fn unknown_magic_and_out_of_range_blocks_are_corrupt() {
        let d = Dir::new("v2-footer");
        write(&d.sst(), &entries(10), 11).unwrap();
        let bytes = std::fs::read(d.sst()).unwrap();
        let n = bytes.len();
        let p = d.sst();
        // Unknown magic (crc recomputed so only the version is wrong).
        patch(&p, |b| {
            let f = n - FOOTER_LEN as usize;
            b[f + 48..f + 52].copy_from_slice(&0x3353_5244u32.to_le_bytes());
            let crc = super::super::crc32(&b[f..n - 4]);
            b[n - 4..].copy_from_slice(&crc.to_le_bytes());
        });
        assert_corrupt(open(&p), "unknown magic");
        // Truncated file: the footer now points past the end.
        std::fs::write(&p, &bytes[..n - 1]).unwrap();
        assert_corrupt(open(&p), "truncated file");
    }

    #[test]
    fn concurrent_readers_of_one_run_see_correct_values() {
        let d = Dir::new("v2-concurrent");
        write(&d.sst(), &entries(1000), 1001).unwrap();
        // A cache too small to hold anything forces every get to hit the file.
        let cache: Arc<BlockCache> = Arc::new(
            moka::sync::Cache::builder()
                .max_capacity(0)
                .weigher(|_k, v: &Arc<Vec<u8>>| v.len() as u32)
                .build(),
        );
        let sst = Arc::new(Sst::open(&d.sst(), 1, cache).unwrap());
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let sst = sst.clone();
                s.spawn(move || {
                    for i in (t + 1..1000).step_by(4) {
                        let key = format!("key-{i:06}").into_bytes();
                        assert_eq!(
                            sst.get(1, &key, u64::MAX).unwrap(),
                            Some(Op::Put(vec![i as u8; 100]))
                        );
                    }
                });
            }
        });
    }
}
