//! redb `StorageEngine` backend — the v1 durable backend (arch/01 §1).
//!
//! redb gives us single-file ACID with MVCC: many concurrent readers on
//! stable snapshots, one writer. Our 11 logical tables map to redb tables
//! by name. All tables are created eagerly at open so read transactions
//! never race table creation.

use std::ops::Bound;
use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::error::{Result, backend};
use crate::storage::engine::{KvPair, ReadTransaction, StorageEngine, TableId, WriteTransaction};

type Def = TableDefinition<'static, &'static [u8], &'static [u8]>;

const DEFS: [Def; TableId::ALL.len()] = [
    TableDefinition::new("meta"),
    TableDefinition::new("planes"),
    TableDefinition::new("plane_names"),
    TableDefinition::new("nodes"),
    TableDefinition::new("edges"),
    TableDefinition::new("adj_fwd"),
    TableDefinition::new("adj_rev"),
    TableDefinition::new("label_idx"),
    TableDefinition::new("ext_keys"),
    TableDefinition::new("prop_idx"),
    TableDefinition::new("node_plane"),
];

fn def(table: TableId) -> Def {
    DEFS[table.index()]
}

pub struct RedbEngine {
    db: redb::Database,
}

impl RedbEngine {
    /// Opens (creating if absent) the database file and ensures all logical
    /// tables exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = redb::Database::create(path).map_err(backend)?;
        let txn = db.begin_write().map_err(backend)?;
        for d in DEFS {
            // open_table on a write transaction creates the table if missing
            txn.open_table(d).map_err(backend)?;
        }
        txn.commit().map_err(backend)?;
        Ok(Self { db })
    }
}

impl StorageEngine for RedbEngine {
    type ReadTxn<'a>
        = RedbReadTxn
    where
        Self: 'a;
    type WriteTxn<'a>
        = RedbWriteTxn
    where
        Self: 'a;

    fn begin_read(&self) -> Result<RedbReadTxn> {
        Ok(RedbReadTxn {
            txn: self.db.begin_read().map_err(backend)?,
        })
    }

    fn begin_write(&self) -> Result<RedbWriteTxn> {
        Ok(RedbWriteTxn {
            txn: self.db.begin_write().map_err(backend)?,
        })
    }
}

fn bounds<'a>(start: &'a [u8], end: Option<&'a [u8]>) -> (Bound<&'a [u8]>, Bound<&'a [u8]>) {
    let upper = match end {
        Some(e) => Bound::Excluded(e),
        None => Bound::Unbounded,
    };
    (Bound::Included(start), upper)
}

pub struct RedbReadTxn {
    txn: redb::ReadTransaction,
}

impl ReadTransaction for RedbReadTxn {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let t = self.txn.open_table(def(table)).map_err(backend)?;
        Ok(t.get(key).map_err(backend)?.map(|g| g.value().to_vec()))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        // ReadOnlyTable::range returns Range<'static>: the iterator owns
        // everything it needs, so this streams without collecting.
        let t = self.txn.open_table(def(table)).map_err(backend)?;
        let iter = t.range::<&[u8]>(bounds(start, end)).map_err(backend)?;
        Ok(Box::new(iter.map(|item| {
            item.map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                .map_err(backend)
        })))
    }
}

pub struct RedbWriteTxn {
    txn: redb::WriteTransaction,
}

/// A write transaction is also a reader over its own staged state
/// (read-your-own-writes), same as the memory backend.
impl ReadTransaction for RedbWriteTxn {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let t = self.txn.open_table(def(table)).map_err(backend)?;
        Ok(t.get(key).map_err(backend)?.map(|g| g.value().to_vec()))
    }

    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>> {
        // Table<'txn>::range borrows the table, which cannot outlive this
        // call — collect instead. Write-side scans in graph ops are small
        // (adjacency checks); revisit if M1 profiling says otherwise.
        let t = self.txn.open_table(def(table)).map_err(backend)?;
        let iter = t.range::<&[u8]>(bounds(start, end)).map_err(backend)?;
        let items: Vec<Result<KvPair>> = iter
            .map(|item| {
                item.map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                    .map_err(backend)
            })
            .collect();
        Ok(Box::new(items.into_iter()))
    }
}

impl WriteTransaction for RedbWriteTxn {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()> {
        let mut t = self.txn.open_table(def(table)).map_err(backend)?;
        t.insert(key, value).map_err(backend)?;
        Ok(())
    }

    fn put_batch(&mut self, table: TableId, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        // Open the table once for the whole batch (vs once per key in the
        // default) — the measured ~1.5x of the bulk-load win. When `items` is
        // pre-sorted by key, redb's B-tree inserts stay near-sequential too.
        let mut t = self.txn.open_table(def(table)).map_err(backend)?;
        for (k, v) in items {
            t.insert(k.as_slice(), v.as_slice()).map_err(backend)?;
        }
        Ok(())
    }

    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()> {
        let mut t = self.txn.open_table(def(table)).map_err(backend)?;
        t.remove(key).map_err(backend)?;
        Ok(())
    }

    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()> {
        use crate::storage::engine::prefix_successor;
        let mut t = self.txn.open_table(def(table)).map_err(backend)?;
        let end = prefix_successor(prefix);
        t.retain_in::<&[u8], _>(bounds(prefix, end.as_deref()), |_, _| false)
            .map_err(backend)?;
        Ok(())
    }

    fn commit(self) -> Result<()> {
        self.txn.commit().map_err(backend)
    }
}
