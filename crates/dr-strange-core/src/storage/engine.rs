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

    /// Single writer at a time is acceptable in v1 (arch/01 §6).
    fn begin_write(&self) -> Result<Self::WriteTxn<'_>>;
}

pub trait ReadTransaction {
    fn get(&self, table: TableId, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Ordered scan of `[start, end)`. Byte order is scan order — keys are
    /// encoded big-endian precisely so prefix ranges are contiguous.
    fn range(
        &self,
        table: TableId,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn Iterator<Item = Result<KvPair>> + '_>>;
}

pub trait WriteTransaction: ReadTransaction {
    fn put(&mut self, table: TableId, key: &[u8], value: &[u8]) -> Result<()>;
    fn delete(&mut self, table: TableId, key: &[u8]) -> Result<()>;

    /// Prefix range-delete; plane drop relies on this being cheap (arch/01 §3).
    fn delete_prefix(&mut self, table: TableId, prefix: &[u8]) -> Result<()>;

    fn commit(self) -> Result<()>;
}
