//! In-memory `StorageEngine` backend (arch/01 §1, §7): fast tests and the
//! oracle for property-based testing against the redb backend.
//!
//! Semantics mirror the redb backend: readers get a stable snapshot taken at
//! `begin_read`; one writer at a time (a second `begin_write` blocks until
//! the first transaction commits or drops); writes become visible atomically
//! at commit. Snapshots are full clones — O(data), fine for a test backend.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::Result;
use crate::storage::engine::{KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction};

type Table = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone)]
struct Tables([Table; TableId::ALL.len()]);

impl Tables {
    fn new() -> Self {
        Tables(std::array::from_fn(|_| BTreeMap::new()))
    }

    fn table(&self, id: TableId) -> &Table {
        &self.0[id.index()]
    }

    fn table_mut(&mut self, id: TableId) -> &mut Table {
        &mut self.0[id.index()]
    }

    fn get(&self, table: TableId, key: &[u8]) -> Option<Vec<u8>> {
        self.table(table).get(key).cloned()
    }

    fn range<'s>(
        &'s self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Box<dyn Iterator<Item = Result<KvPair>> + 's> {
        let lower = Bound::Included(start.to_vec());
        let upper = match end {
            Some(e) => Bound::Excluded(e.to_vec()),
            None => Bound::Unbounded,
        };
        Box::new(
            self.table(table)
                .range((lower, upper))
                .map(|(k, v)| Ok((k.clone(), v.clone()))),
        )
    }
}

#[derive(Debug)]
pub struct MemoryEngine {
    /// Committed data behind an `Arc` so a read snapshot is an O(1) pointer
    /// clone, not an O(data) deep copy. A write deep-copies once (into its
    /// staged buffer) and publishes a fresh `Arc` at commit; readers keep the
    /// `Arc` they opened with — copy-on-write MVCC.
    data: Mutex<Arc<Tables>>,
    /// Held for the lifetime of a write transaction — single-writer.
    writer: Mutex<()>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(Arc::new(Tables::new())),
            writer: Mutex::new(()),
        }
    }

    /// Poisoning only means a panic elsewhere while a lock was held; data is
    /// only ever replaced atomically at commit, so it is always consistent.
    fn data(&self) -> MutexGuard<'_, Arc<Tables>> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for MemoryEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageEngine for MemoryEngine {
    type ReadTxn<'a>
        = MemoryReadTxn
    where
        Self: 'a;
    type WriteTxn<'a>
        = MemoryWriteTxn<'a>
    where
        Self: 'a;

    fn begin_read(&self) -> Result<MemoryReadTxn> {
        Ok(MemoryReadTxn {
            snapshot: Arc::clone(&self.data()),
        })
    }

    fn begin_write(&self) -> Result<MemoryWriteTxn<'_>> {
        let guard = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        // Deep-copy the committed tables into a mutable staging buffer.
        let staged = (**self.data()).clone();
        Ok(MemoryWriteTxn {
            engine: self,
            staged,
            _writer: guard,
        })
    }
}

pub struct MemoryReadTxn {
    snapshot: Arc<Tables>,
}

impl ReadTransaction for MemoryReadTxn {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.snapshot.get(table, key))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        Ok(self.snapshot.range(table, start, end))
    }
}

pub struct MemoryWriteTxn<'a> {
    engine: &'a MemoryEngine,
    staged: Tables,
    _writer: MutexGuard<'a, ()>,
}

/// A write transaction is also a reader — reading from `staged`, so it sees
/// its own uncommitted writes (required by graph ops, e.g. create_edge
/// validating nodes created earlier in the same transaction).
impl ReadTransaction for MemoryWriteTxn<'_> {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.staged.get(table, key))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        Ok(self.staged.range(table, start, end))
    }
}

impl WriteTransaction for MemoryWriteTxn<'_> {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()> {
        self.staged
            .table_mut(table)
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()> {
        self.staged.table_mut(table).remove(key);
        Ok(())
    }

    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()> {
        let doomed: Vec<Vec<u8>> = self
            .staged
            .table(table)
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect();
        let t = self.staged.table_mut(table);
        for key in doomed {
            t.remove(&key);
        }
        Ok(())
    }

    fn commit(self) -> Result<()> {
        // Publish the staged tables as the new committed snapshot; readers
        // holding the previous Arc are unaffected.
        *self.engine.data() = Arc::new(self.staged);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip_after_commit() {
        let eng = MemoryEngine::new();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"k1", b"v1").unwrap();
        w.commit().unwrap();

        let r = eng.begin_read().unwrap();
        assert_eq!(r.get(TableId::Nodes, b"k1").unwrap(), Some(b"v1".to_vec()));
        assert_eq!(r.get(TableId::Nodes, b"nope").unwrap(), None);
        // Tables are namespaces: same key elsewhere is absent.
        assert_eq!(r.get(TableId::Edges, b"k1").unwrap(), None);
    }

    #[test]
    fn uncommitted_writes_are_invisible_and_dropped() {
        let eng = MemoryEngine::new();
        {
            let mut w = eng.begin_write().unwrap();
            w.put(TableId::Nodes, b"k1", b"v1").unwrap();
            // dropped without commit
        }
        let r = eng.begin_read().unwrap();
        assert_eq!(r.get(TableId::Nodes, b"k1").unwrap(), None);
    }

    #[test]
    fn readers_keep_their_snapshot() {
        let eng = MemoryEngine::new();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"k", b"old").unwrap();
        w.commit().unwrap();

        let before = eng.begin_read().unwrap();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"k", b"new").unwrap();
        w.commit().unwrap();

        assert_eq!(
            before.get(TableId::Nodes, b"k").unwrap(),
            Some(b"old".to_vec())
        );
        let after = eng.begin_read().unwrap();
        assert_eq!(
            after.get(TableId::Nodes, b"k").unwrap(),
            Some(b"new".to_vec())
        );
    }

    #[test]
    fn write_txn_reads_its_own_writes() {
        let eng = MemoryEngine::new();
        let mut w = eng.begin_write().unwrap();
        w.put(TableId::Nodes, b"k", b"v").unwrap();
        assert_eq!(w.get(TableId::Nodes, b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn range_is_ordered_and_end_exclusive() {
        let eng = MemoryEngine::new();
        let mut w = eng.begin_write().unwrap();
        for k in [&b"a"[..], b"b", b"c", b"d"] {
            w.put(TableId::AdjFwd, k, b"").unwrap();
        }
        w.commit().unwrap();

        let r = eng.begin_read().unwrap();
        let keys: Vec<Vec<u8>> = r
            .range(TableId::AdjFwd, b"b", Some(b"d"))
            .unwrap()
            .map(|kv| kv.unwrap().0)
            .collect();
        assert_eq!(keys, vec![b"b".to_vec(), b"c".to_vec()]);

        let keys: Vec<Vec<u8>> = r
            .range(TableId::AdjFwd, b"b", None)
            .unwrap()
            .map(|kv| kv.unwrap().0)
            .collect();
        assert_eq!(keys, vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn delete_prefix_removes_only_the_prefix() {
        let eng = MemoryEngine::new();
        let mut w = eng.begin_write().unwrap();
        for k in [&b"aa1"[..], b"aa2", b"ab1", b"b"] {
            w.put(TableId::Nodes, k, b"").unwrap();
        }
        w.delete_prefix(TableId::Nodes, b"aa").unwrap();
        w.commit().unwrap();

        let r = eng.begin_read().unwrap();
        let keys: Vec<Vec<u8>> = r
            .range(TableId::Nodes, b"", None)
            .unwrap()
            .map(|kv| kv.unwrap().0)
            .collect();
        assert_eq!(keys, vec![b"ab1".to_vec(), b"b".to_vec()]);
    }
}
