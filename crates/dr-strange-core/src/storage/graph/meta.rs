//! Database metadata & containers: format/init, id allocation, the label and
//! edge-type dictionaries, plane lifecycle, and vector-index declarations
//! (arch/01 §2, arch/09). The base layer of the graph encoding — `node` and
//! `edge` build on it, never the reverse.

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction, prefix_successor};
use crate::storage::vector::Metric;
use crate::storage::{codec, keys};
use crate::text::Language;
use crate::types::{EdgeId, NodeId, PlaneId, Properties, QueryRecord};

/// v2 (M1): node records gained an inline `external_key` field
/// (arch/01 §2 — `codec::NodeRecordRaw`).
pub const FORMAT_VERSION: u32 = 2;

/// Oldest on-disk format this build can open — versions in
/// `MIN_SUPPORTED_VERSION..FORMAT_VERSION` are upgraded in place by the
/// migration ladder in [`init`]. v1 never appeared in a public release
/// (M0-internal only), so it has no upgrade path.
pub const MIN_SUPPORTED_VERSION: u32 = 2;

pub const DEFAULT_PLANE_NAME: &str = "startup";

// ---- meta / init ----------------------------------------------------------

/// First-open initialization; verifies magic/version on an existing database.
///
/// Version policy: a database *newer* than this build is refused (only a
/// newer drsg knows that format); one older than [`MIN_SUPPORTED_VERSION`] is
/// refused (no upgrade path); anything in between is migrated forward one
/// version step at a time and re-stamped — atomically, since `init` runs
/// inside the caller's write transaction, so a crash mid-migration leaves the
/// old version intact.
pub fn init(txn: &mut dyn WriteTransaction) -> Result<()> {
    match txn.get(TableId::Meta, keys::META_MAGIC)? {
        Some(magic) if magic == keys::MAGIC => {
            let version = get_u32(txn, keys::META_FORMAT_VERSION)?
                .ok_or_else(|| Error::Corrupt("missing format version".into()))?;
            if version > FORMAT_VERSION {
                return Err(Error::Corrupt(format!(
                    "database format v{version} is newer than this build supports \
                     (v{FORMAT_VERSION}) — upgrade drsg to open it"
                )));
            }
            if version < MIN_SUPPORTED_VERSION {
                return Err(Error::Corrupt(format!(
                    "database format v{version} predates the oldest supported \
                     (v{MIN_SUPPORTED_VERSION}) and has no upgrade path"
                )));
            }
            // The migration ladder: one step per historical version bump.
            for from in version..FORMAT_VERSION {
                migrate_step(txn, from)?;
            }
            if version != FORMAT_VERSION {
                put_u32(txn, keys::META_FORMAT_VERSION, FORMAT_VERSION)?;
            }
            Ok(())
        }
        Some(_) => Err(Error::Corrupt(
            "not a dr-strange database (bad magic)".into(),
        )),
        None => {
            txn.put(TableId::Meta, keys::META_MAGIC, keys::MAGIC)?;
            put_u32(txn, keys::META_FORMAT_VERSION, FORMAT_VERSION)?;
            // Counters start at 1; 0 is never a valid allocated id, and
            // PlaneId(0) is pre-assigned to the startup plane below.
            put_u64(txn, keys::META_NEXT_NODE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_EDGE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_PLANE_ID, 1)?;
            put_u64(txn, keys::META_NEXT_LABEL_ID, 1)?;
            put_u64(txn, keys::META_NEXT_EDGE_TYPE_ID, 1)?;
            put_u64(txn, keys::META_COMMIT_SEQ, 0)?;
            write_plane(
                txn,
                PlaneId::STARTUP,
                DEFAULT_PLANE_NAME,
                &Properties::new(),
            )
        }
    }
}

/// One rung of the migration ladder: bring the on-disk data from format
/// `from` up to `from + 1`, inside the caller's write transaction.
///
/// No migrations exist yet (`MIN_SUPPORTED_VERSION == FORMAT_VERSION`); when
/// `FORMAT_VERSION` bumps, turn the body into a `match from` with one arm per
/// historical step, keeping this error as the wildcard for ladder gaps.
fn migrate_step(_txn: &mut dyn WriteTransaction, from: u32) -> Result<()> {
    Err(Error::Corrupt(format!(
        "no migration step from format v{from} — gap in the migration ladder (bug)"
    )))
}

pub(super) fn decode_u32(bytes: &[u8], what: &str) -> Result<u32> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| Error::Corrupt(format!("bad u32 in {what}")))
}

fn get_u32(txn: &dyn ReadTransaction, key: &[u8]) -> Result<Option<u32>> {
    txn.get(TableId::Meta, key)?
        .map(|v| decode_u32(&v, "meta"))
        .transpose()
}

fn put_u32(txn: &mut dyn WriteTransaction, key: &[u8], v: u32) -> Result<()> {
    txn.put(TableId::Meta, key, &v.to_be_bytes())
}

/// Reads a `u64` from the `meta` table (id counters). `pub(super)` so tests
/// can inspect the id counters.
pub(super) fn get_u64(txn: &dyn ReadTransaction, key: &[u8]) -> Result<Option<u64>> {
    txn.get(TableId::Meta, key)?
        .map(|v| {
            v.as_slice()
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| Error::Corrupt("bad u64 in meta".into()))
        })
        .transpose()
}

pub(super) fn put_u64(txn: &mut dyn WriteTransaction, key: &[u8], v: u64) -> Result<()> {
    txn.put(TableId::Meta, key, &v.to_be_bytes())
}

/// The current commit sequence, read from `txn`'s snapshot (arch/02 §3). Absent
/// ⇒ 0 (a pre-commit-seq database). Because it lives in the KV, a reader's seq
/// is always consistent with the storage snapshot it sees — the cache's
/// version stamp with no separate coordination.
pub fn read_commit_seq(txn: &dyn ReadTransaction) -> Result<u64> {
    Ok(get_u64(txn, keys::META_COMMIT_SEQ)?.unwrap_or(0))
}

/// Advances the commit sequence by one, within the caller's write txn (so it
/// commits atomically with the data). Returns the new value. Called once per
/// committed write, so any change bumps the stamp the cache versions against.
pub fn bump_commit_seq(txn: &mut dyn WriteTransaction) -> Result<u64> {
    let next = read_commit_seq(txn)? + 1;
    put_u64(txn, keys::META_COMMIT_SEQ, next)?;
    Ok(next)
}

/// The five id-allocation counters (next node/edge/plane/label/edge-type id),
/// captured for a snapshot manifest and restored id-faithfully (ROADMAP §6).
pub fn read_id_counters(txn: &dyn ReadTransaction) -> Result<[u64; 5]> {
    let one = |k: &[u8]| -> Result<u64> { Ok(get_u64(txn, k)?.unwrap_or(1)) };
    Ok([
        one(keys::META_NEXT_NODE_ID)?,
        one(keys::META_NEXT_EDGE_ID)?,
        one(keys::META_NEXT_PLANE_ID)?,
        one(keys::META_NEXT_LABEL_ID)?,
        one(keys::META_NEXT_EDGE_TYPE_ID)?,
    ])
}

/// Restore the id counters + commit sequence from a snapshot manifest, so a
/// restored database allocates fresh ids past everything it just loaded and
/// carries the source's commit sequence (ROADMAP §6).
pub fn set_id_counters(txn: &mut dyn WriteTransaction, counters: [u64; 5]) -> Result<()> {
    put_u64(txn, keys::META_NEXT_NODE_ID, counters[0])?;
    put_u64(txn, keys::META_NEXT_EDGE_ID, counters[1])?;
    put_u64(txn, keys::META_NEXT_PLANE_ID, counters[2])?;
    put_u64(txn, keys::META_NEXT_LABEL_ID, counters[3])?;
    put_u64(txn, keys::META_NEXT_EDGE_TYPE_ID, counters[4])
}

/// Set the commit sequence outright (a snapshot restore, ROADMAP §6), rather
/// than bumping it — so the restored database resumes at the source's sequence.
pub fn set_commit_seq(txn: &mut dyn WriteTransaction, seq: u64) -> Result<()> {
    put_u64(txn, keys::META_COMMIT_SEQ, seq)
}

/// Wall-clock time (unix-epoch millis) of the latest commit visible at `txn`'s
/// snapshot — the time index for time-addressed time-travel (ROADMAP §4).
/// Absent (a pre-time-index database, or before the first stamped commit) ⇒
/// `None`.
pub fn read_commit_time(txn: &dyn ReadTransaction) -> Result<Option<i64>> {
    txn.get(TableId::Meta, keys::META_COMMIT_TIME)?
        .map(|v| {
            v.as_slice()
                .try_into()
                .map(i64::from_be_bytes)
                .map_err(|_| Error::Corrupt("bad i64 commit_time in meta".into()))
        })
        .transpose()
}

/// Stamps the current commit's wall-clock time (unix-epoch millis), within the
/// caller's write txn so it commits atomically with the data. Called once per
/// committed write, right after [`bump_commit_seq`].
pub fn write_commit_time(txn: &mut dyn WriteTransaction, millis: i64) -> Result<()> {
    txn.put(TableId::Meta, keys::META_COMMIT_TIME, &millis.to_be_bytes())
}

/// Allocates the next id from a meta counter, one meta write per call. Used
/// for planes/labels/edge-types, which are created rarely — no need for
/// [`IdAllocator`]'s batching there. `pub(super)` for node/edge creation.
pub(super) fn next_id(txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
    let id = get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, id + 1)?;
    Ok(id)
}

/// Reserves `count` contiguous ids from `counter` in one meta write, and
/// returns the batch's starting id — `[start, start + count)` all now
/// belong to the caller. The building block under [`IdAllocator`].
pub(super) fn reserve_id_batch(
    txn: &mut dyn WriteTransaction,
    counter: &[u8],
    count: u64,
) -> Result<u64> {
    let start =
        get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, start + count)?;
    Ok(start)
}

/// Number of ids reserved per meta write by [`IdAllocator`]. Deliberately
/// small: it bounds how many ids a transaction can waste by reserving a
/// batch and then committing without using all of it (§ below).
pub(crate) const ID_BATCH_SIZE: u64 = 64;

#[derive(Default)]
struct IdBatch {
    next: u64,
    remaining: u64,
}

impl IdBatch {
    fn take(&mut self, txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
        if self.remaining == 0 {
            self.next = reserve_id_batch(txn, counter, ID_BATCH_SIZE)?;
            self.remaining = ID_BATCH_SIZE;
        }
        let id = self.next;
        self.next += 1;
        self.remaining -= 1;
        Ok(id)
    }
}

/// A batch ID allocator to reserve default `ID_BATCH_SIZE` IDs in one IO operation.
#[derive(Default)]
pub(crate) struct IdAllocator {
    node: IdBatch,
    edge: IdBatch,
}

impl IdAllocator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn next_node_id(&mut self, txn: &mut dyn WriteTransaction) -> Result<NodeId> {
        Ok(NodeId(self.node.take(txn, keys::META_NEXT_NODE_ID)?))
    }

    pub(crate) fn next_edge_id(&mut self, txn: &mut dyn WriteTransaction) -> Result<EdgeId> {
        Ok(EdgeId(self.edge.take(txn, keys::META_NEXT_EDGE_ID)?))
    }
}

// ---- dictionaries ---------------------------------------------------------

fn intern(
    txn: &mut dyn WriteTransaction,
    fwd_key: Vec<u8>,
    rev_key: impl FnOnce(u32) -> Vec<u8>,
    counter: &'static [u8],
    name: &str,
) -> Result<u32> {
    if let Some(v) = txn.get(TableId::Meta, &fwd_key)? {
        return decode_u32(&v, "dictionary");
    }
    let id = u32::try_from(next_id(txn, counter)?)
        .map_err(|_| Error::InvalidArgument("dictionary exhausted (u32)".into()))?;
    txn.put(TableId::Meta, &fwd_key, &id.to_be_bytes())?;
    txn.put(TableId::Meta, &rev_key(id), name.as_bytes())?;
    Ok(id)
}

pub fn intern_label(txn: &mut dyn WriteTransaction, name: &str) -> Result<u32> {
    intern(
        txn,
        keys::dict_label_key(name),
        keys::dict_label_rev_key,
        keys::META_NEXT_LABEL_ID,
        name,
    )
}

pub fn intern_edge_type(txn: &mut dyn WriteTransaction, name: &str) -> Result<u32> {
    intern(
        txn,
        keys::dict_edge_type_key(name),
        keys::dict_edge_type_rev_key,
        keys::META_NEXT_EDGE_TYPE_ID,
        name,
    )
}

pub fn lookup_edge_type(txn: &dyn ReadTransaction, name: &str) -> Result<Option<u32>> {
    txn.get(TableId::Meta, &keys::dict_edge_type_key(name))?
        .map(|v| decode_u32(&v, "dictionary"))
        .transpose()
}

/// Read-only label id lookup (mirror of [`lookup_edge_type`]); `None` if the
/// label name was never interned — used by `scan_label`, which then yields no
/// nodes rather than erroring.
pub fn lookup_label(txn: &dyn ReadTransaction, name: &str) -> Result<Option<u32>> {
    txn.get(TableId::Meta, &keys::dict_label_key(name))?
        .map(|v| decode_u32(&v, "dictionary"))
        .transpose()
}

pub fn resolve_label(txn: &dyn ReadTransaction, id: u32) -> Result<String> {
    let bytes = txn
        .get(TableId::Meta, &keys::dict_label_rev_key(id))?
        .ok_or_else(|| Error::Corrupt(format!("dangling label id {id}")))?;
    String::from_utf8(bytes).map_err(|_| Error::Corrupt("bad label name".into()))
}

// ---- planes ---------------------------------------------------------------

/// Write a plane record at an explicit id — the id-faithful primitive a
/// snapshot restore (ROADMAP §6) uses to rebuild planes with their original ids
/// (so the vector/keyword sidecars, keyed by plane id, stay valid). Normal
/// creation goes through [`create_plane`], which allocates the id.
pub(crate) fn write_plane(
    txn: &mut dyn WriteTransaction,
    id: PlaneId,
    name: &str,
    props: &Properties,
) -> Result<()> {
    // plane record: u32-BE name length · name bytes · props (codec)
    let name_len = u32::try_from(name.len())
        .map_err(|_| Error::InvalidArgument("plane name too long".into()))?;
    let mut record = name_len.to_be_bytes().to_vec();
    record.extend_from_slice(name.as_bytes());
    record.extend_from_slice(&codec::encode_props(props));
    txn.put(TableId::Planes, &keys::plane_key(id), &record)?;
    txn.put(
        TableId::PlaneNames,
        &keys::plane_name_key(name),
        &id.0.to_be_bytes(),
    )
}

pub fn plane_id_by_name(txn: &dyn ReadTransaction, name: &str) -> Result<Option<PlaneId>> {
    txn.get(TableId::PlaneNames, &keys::plane_name_key(name))?
        .map(|v| decode_u32(&v, "plane_names").map(PlaneId))
        .transpose()
}

pub fn create_plane(
    txn: &mut dyn WriteTransaction,
    name: &str,
    props: &Properties,
) -> Result<PlaneId> {
    if plane_id_by_name(txn, name)?.is_some() {
        return Err(Error::PlaneExists(name.to_string()));
    }
    let id = u32::try_from(next_id(txn, keys::META_NEXT_PLANE_ID)?)
        .map_err(|_| Error::InvalidArgument("plane ids exhausted (u32)".into()))?;
    let id = PlaneId(id);
    write_plane(txn, id, name, props)?;
    Ok(id)
}

/// Reads a plane's `(name, properties)`; `None` if the plane doesn't exist.
pub fn read_plane(txn: &dyn ReadTransaction, id: PlaneId) -> Result<Option<(String, Properties)>> {
    let Some(buf) = txn.get(TableId::Planes, &keys::plane_key(id))? else {
        return Ok(None);
    };
    let len_bytes: [u8; 4] = buf
        .get(..4)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?
        .try_into()
        .expect("checked length");
    let name_len = u32::from_be_bytes(len_bytes) as usize;
    let name_end = 4 + name_len;
    let name_bytes = buf
        .get(4..name_end)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?;
    let name = String::from_utf8(name_bytes.to_vec())
        .map_err(|_| Error::Corrupt("bad plane name".into()))?;
    let props_bytes = buf
        .get(name_end..)
        .ok_or_else(|| Error::Corrupt("truncated plane record".into()))?;
    let properties = codec::decode_props(props_bytes)?;
    Ok(Some((name, properties)))
}

/// Replaces a plane's property map (arch/09 §3), keeping its name. Errors
/// `NotFound` if the plane doesn't exist.
pub fn set_plane_properties(
    txn: &mut dyn WriteTransaction,
    id: PlaneId,
    props: &Properties,
) -> Result<()> {
    let (name, _) =
        read_plane(txn, id)?.ok_or_else(|| Error::NotFound(format!("plane {}", id.0)))?;
    write_plane(txn, id, &name, props)
}

/// Renames a plane (arch/09 §3), keeping its id and properties. Errors
/// `PlaneExists` if the new name is taken, `NotFound` if the plane is
/// absent, and `InvalidArgument` for the startup plane (whose name is an
/// invariant). No-op if `new_name` equals the current name.
pub fn rename_plane(txn: &mut dyn WriteTransaction, id: PlaneId, new_name: &str) -> Result<()> {
    if id == PlaneId::STARTUP {
        return Err(Error::InvalidArgument(
            "the startup plane cannot be renamed".into(),
        ));
    }
    let (old_name, props) =
        read_plane(txn, id)?.ok_or_else(|| Error::NotFound(format!("plane {}", id.0)))?;
    if old_name == new_name {
        return Ok(());
    }
    if plane_id_by_name(txn, new_name)?.is_some() {
        return Err(Error::PlaneExists(new_name.to_string()));
    }
    txn.delete(TableId::PlaneNames, &keys::plane_name_key(&old_name))?;
    write_plane(txn, id, new_name, &props)
}

/// Deletes a plane and everything on it: every plane-scoped table is
/// prefix-range-deleted (arch/09 §1, §3). Idempotent for an absent plane.
/// The `"startup"` plane always exists and cannot be dropped.
pub fn drop_plane(txn: &mut dyn WriteTransaction, id: PlaneId) -> Result<()> {
    if id == PlaneId::STARTUP {
        return Err(Error::InvalidArgument(
            "the startup plane always exists and cannot be dropped".into(),
        ));
    }
    let Some((name, _)) = read_plane(txn, id)? else {
        return Ok(());
    };

    // `node_plane` is keyed by bare node id (no plane prefix — arch/01 §10
    // open question 7), so its entries can't be prefix-deleted. Collect the
    // plane's node ids from the (still-intact) Nodes table first.
    let prefix = keys::plane_key(id).to_vec();
    let end = prefix_successor(&prefix);
    let mut node_ids = Vec::new();
    for item in txn.range(TableId::Nodes, &prefix, end.as_deref())? {
        let (key, _) = item?;
        let (_, node) = keys::parse_node_key(&key)?;
        node_ids.push(node);
    }
    for node in node_ids {
        txn.delete(TableId::NodePlane, &keys::node_plane_key(node))?;
    }

    for table in [
        TableId::Nodes,
        TableId::Edges,
        TableId::AdjFwd,
        TableId::AdjRev,
        TableId::LabelIdx,
        TableId::ExtKeys,
        TableId::PropIdx,
    ] {
        txn.delete_prefix(table, &prefix)?;
    }

    txn.delete(TableId::Planes, &keys::plane_key(id))?;
    txn.delete(TableId::PlaneNames, &keys::plane_name_key(&name))?;
    Ok(())
}

/// All planes as `(id, name)`, ascending by id (a `plane_names` scan). Used
/// for the cross-plane catalog roll-up.
pub fn list_planes(txn: &dyn ReadTransaction) -> Result<Vec<(PlaneId, String)>> {
    let mut out = Vec::new();
    for item in txn.range(TableId::PlaneNames, b"", None)? {
        let (key, value) = item?;
        let name = String::from_utf8(key).map_err(|_| Error::Corrupt("bad plane name".into()))?;
        let id = value
            .as_slice()
            .try_into()
            .map(|b| PlaneId(u32::from_be_bytes(b)))
            .map_err(|_| Error::Corrupt("bad plane id".into()))?;
        out.push((id, name));
    }
    out.sort_by_key(|(p, _)| p.0);
    Ok(out)
}

// ---- vector index declarations (arch/01 §5) -------------------------------
// Only the *declaration* (which (plane,label,property) is indexed, and its
// metric) is durable, in `meta`. The index structure itself is rebuilt from
// the KV — the KV is the source of truth (see `crate::index`).

/// Records that `(plane, label, property)` is vector-indexed with `metric`.
/// Returns whether this was a new declaration. Errors if it already exists
/// with a different metric.
pub fn declare_vector_index(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    label: &str,
    property: &str,
    metric: Metric,
) -> Result<bool> {
    let key = keys::vindex_decl_key(plane, label, property);
    if let Some(existing) = txn.get(TableId::Meta, &key)? {
        let current = existing
            .first()
            .and_then(|&t| Metric::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad vindex metric tag".into()))?;
        if current != metric {
            return Err(Error::InvalidArgument(format!(
                "vector index on {label}.{property} already exists with a different metric"
            )));
        }
        return Ok(false);
    }
    txn.put(TableId::Meta, &key, &[metric.tag()])?;
    Ok(true)
}

/// All declared vector indexes, `(plane, label, property, metric)`.
pub fn list_vector_indexes(
    txn: &dyn ReadTransaction,
) -> Result<Vec<(PlaneId, String, String, Metric)>> {
    let prefix = keys::VINDEX_PREFIX;
    let end = prefix_successor(prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Meta, prefix, end.as_deref())? {
        let (key, value) = item?;
        let (plane, label, property) = keys::parse_vindex_decl_key(&key)?;
        let metric = value
            .first()
            .and_then(|&t| Metric::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad vindex metric tag".into()))?;
        out.push((plane, label, property, metric));
    }
    Ok(out)
}

// ---- query history --------------------------------------------------------
//
// A list of what has been run, so it can be run again — the dashboard's
// clickable list and the CLI's `history`. It lives in `meta` beside the
// counters and the index declarations, keyed by a monotonic id.

/// What is kept per entry; the id is the key it is filed under.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredQuery {
    at: i64,
    plane: String,
    query: String,
}

fn history_entries(txn: &dyn ReadTransaction) -> Result<Vec<(u64, StoredQuery)>> {
    let prefix = keys::HISTORY_PREFIX;
    let end = prefix_successor(prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Meta, prefix, end.as_deref())? {
        let (key, value) = item?;
        let Some(id) = keys::parse_history_key(&key) else {
            continue;
        };
        let stored: StoredQuery = postcard::from_bytes(&value)
            .map_err(|e| Error::Corrupt(format!("bad history entry: {e}")))?;
        out.push((id, stored));
    }
    Ok(out)
}

/// Files `query` under the next id, and purges the oldest beyond `keep`.
///
/// Running the same query twice in a row does not fill the list with it: the
/// newest entry is stamped with the new time instead, which is what a shell's
/// history does and what a list you pick from wants. An older identical entry
/// is left where it is — it is a different moment, and the list is a record of
/// moments.
///
/// Returns the id it was filed under.
pub fn record_query(
    txn: &mut dyn WriteTransaction,
    plane: &str,
    query: &str,
    at: i64,
    keep: usize,
) -> Result<u64> {
    let entries = history_entries(&*txn)?;
    if let Some((id, last)) = entries.last()
        && last.plane == plane
        && last.query == query
    {
        let id = *id;
        write_history(txn, id, at, plane, query)?;
        return Ok(id);
    }
    let id = entries.last().map_or(1, |(id, _)| id + 1);
    write_history(txn, id, at, plane, query)?;
    // Oldest first, so the head of the range is what a full history drops.
    let keep = keep.max(1);
    let over = (entries.len() + 1).saturating_sub(keep);
    for (old, _) in entries.iter().take(over) {
        txn.delete(TableId::Meta, &keys::history_key(*old))?;
    }
    Ok(id)
}

fn write_history(
    txn: &mut dyn WriteTransaction,
    id: u64,
    at: i64,
    plane: &str,
    query: &str,
) -> Result<()> {
    let value = postcard::to_stdvec(&StoredQuery {
        at,
        plane: plane.to_string(),
        query: query.to_string(),
    })
    .map_err(|e| Error::Corrupt(format!("encoding a history entry: {e}")))?;
    txn.put(TableId::Meta, &keys::history_key(id), &value)
}

/// The recorded queries, **newest first**, at most `limit` of them.
pub fn list_history(txn: &dyn ReadTransaction, limit: usize) -> Result<Vec<QueryRecord>> {
    let mut all = history_entries(txn)?;
    all.reverse();
    all.truncate(limit);
    Ok(all
        .into_iter()
        .map(|(id, s)| QueryRecord {
            id,
            at: s.at,
            plane: s.plane,
            query: s.query,
        })
        .collect())
}

/// One recorded query, by the id it was filed under.
pub fn get_history(txn: &dyn ReadTransaction, id: u64) -> Result<Option<QueryRecord>> {
    let Some(value) = txn.get(TableId::Meta, &keys::history_key(id))? else {
        return Ok(None);
    };
    let s: StoredQuery = postcard::from_bytes(&value)
        .map_err(|e| Error::Corrupt(format!("bad history entry: {e}")))?;
    Ok(Some(QueryRecord {
        id,
        at: s.at,
        plane: s.plane,
        query: s.query,
    }))
}

// ---- keyword index declarations (ROADMAP §2) ------------------------------
// Same durable-declaration model as vector indexes: only which
// (plane,label,property) is keyword-indexed, and its analyzer language, lives
// in `meta`; the BM25 index itself is rebuilt from the KV (see crate::keyword).

/// Records that `(plane, label, property)` is keyword-indexed with `language`.
/// Returns whether this was a new declaration. Errors if it already exists with
/// a different language (re-declaring with the same language is idempotent).
pub fn declare_keyword_index(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    label: &str,
    property: &str,
    language: Language,
) -> Result<bool> {
    let key = keys::kindex_decl_key(plane, label, property);
    if let Some(existing) = txn.get(TableId::Meta, &key)? {
        let current = existing
            .first()
            .and_then(|&t| Language::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad kindex language tag".into()))?;
        if current != language {
            return Err(Error::InvalidArgument(format!(
                "keyword index on {label}.{property} already exists with a different language"
            )));
        }
        return Ok(false);
    }
    txn.put(TableId::Meta, &key, &[language.tag()])?;
    Ok(true)
}

/// All declared keyword indexes, `(plane, label, property, language)`.
pub fn list_keyword_indexes(
    txn: &dyn ReadTransaction,
) -> Result<Vec<(PlaneId, String, String, Language)>> {
    let prefix = keys::KINDEX_PREFIX;
    let end = prefix_successor(prefix);
    let mut out = Vec::new();
    for item in txn.range(TableId::Meta, prefix, end.as_deref())? {
        let (key, value) = item?;
        let (plane, label, property) = keys::parse_kindex_decl_key(&key)?;
        let language = value
            .first()
            .and_then(|&t| Language::from_tag(t))
            .ok_or_else(|| Error::Corrupt("bad kindex language tag".into()))?;
        out.push((plane, label, property, language));
    }
    Ok(out)
}
