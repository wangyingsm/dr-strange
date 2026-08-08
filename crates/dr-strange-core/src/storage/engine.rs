//! The backend swap seam (arch/01 §1): the graph layer is written against
//! these traits; redb is the v1 implementation, an in-memory one serves
//! tests, and a custom engine can replace both in v2.

use crate::error::Result;

/// Logical tables of the graph encoding (arch/01 §3). Backends map these to
/// their own namespaces (redb tables, prefixes, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableId {
    Meta,
    Planes,
    PlaneNames,
    Nodes,
    Edges,
    AdjFwd,
    AdjRev,
    LabelIdx,
    ExtKeys,
    PropIdx,
    NodePlane,
}

impl TableId {
    pub const ALL: [TableId; 11] = [
        TableId::Meta,
        TableId::Planes,
        TableId::PlaneNames,
        TableId::Nodes,
        TableId::Edges,
        TableId::AdjFwd,
        TableId::AdjRev,
        TableId::LabelIdx,
        TableId::ExtKeys,
        TableId::PropIdx,
        TableId::NodePlane,
    ];

    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|t| *t == self)
            .expect("all variants listed")
    }
}

pub type KvPair = (Vec<u8>, Vec<u8>);

pub trait StorageEngine: Send + Sync + 'static {
    type ReadTxn<'a>: ReadTransaction
    where
        Self: 'a;
    type WriteTxn<'a>: WriteTransaction
    where
        Self: 'a;

    /// Stable MVCC snapshot; concurrent with other readers and the writer.
    fn begin_read(&self) -> Result<Self::ReadTxn<'_>>;

    /// Single writer at a time now since we don't need distributed storage yet.
    /// If we do in the future, maybe try raft + TiKV.
    fn begin_write(&self) -> Result<Self::WriteTxn<'_>>;
}

/// Reference receivers only, so the trait stays dyn-compatible: graph
/// operations are written once against `&dyn ReadTransaction` /
/// `&mut dyn WriteTransaction` instead of being generic per engine.
pub trait ReadTransaction {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Ordered scan of `[start, end)`; `end = None` scans to the end of the
    /// table. Byte order is scan order — keys are encoded big-endian
    /// precisely so prefix ranges are contiguous.
    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>>;
}

pub trait WriteTransaction: ReadTransaction {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()>;

    /// Inserts many `(key, value)` pairs into one table. The bulk-load fast
    /// path (arch/01 §2) calls this so a backend can amortize per-op overhead
    /// across the whole batch — redb opens the table once instead of per key.
    /// Callers that want the sorted-insert win (redb B-tree) pre-sort `items`.
    /// The default just loops [`put`](Self::put); override where it pays.
    fn put_batch(&mut self, table: TableId, items: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        for (k, v) in items {
            self.put(table, k, v)?;
        }
        Ok(())
    }

    /// Prefix range-delete; plane drop relies on this being cheap (arch/01 §3).
    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()>;

    /// Consumes the transaction: write-after-commit and double-commit are
    /// compile errors. By-value receiver ⇒ excluded from the vtable
    /// (`Self: Sized`), so it is called on the concrete type — the API
    /// layer's `WriteTxn`, which owns the engine transaction, is the one
    /// place that commits.
    fn commit(self) -> Result<()>
    where
        Self: Sized;
}

/// Smallest byte string strictly greater than every key starting with
/// `prefix`, or `None` if the prefix is all `0xFF` (then scan to table end).
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        match last.checked_add(1) {
            Some(bumped) => {
                *last = bumped;
                return Some(end);
            }
            None => {
                end.pop();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::prefix_successor;

    #[test]
    fn successor_increments_last_byte() {
        assert_eq!(prefix_successor(&[1, 2, 3]), Some(vec![1, 2, 4]));
    }

    #[test]
    fn successor_carries_past_ff() {
        assert_eq!(prefix_successor(&[1, 0xFF, 0xFF]), Some(vec![2]));
    }

    #[test]
    fn successor_of_all_ff_is_none() {
        assert_eq!(prefix_successor(&[0xFF, 0xFF]), None);
        assert_eq!(prefix_successor(&[]), None);
    }
}
