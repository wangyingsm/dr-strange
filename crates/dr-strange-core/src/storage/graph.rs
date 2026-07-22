//! Graph operations over the KV encoding (arch/01 §2–§3): meta/init,
//! dictionaries, ID allocation, planes, nodes, edges, adjacency.
//!
//! Everything here takes `&dyn ReadTransaction` / `&mut dyn WriteTransaction`
//! so it is written once for every backend. The API layer (arch/04) wraps
//! these in `Database` / `PlaneHandle` / `WriteTxn` handles.
//!
//! Convention: every raw integer stored in the KV — keys AND standalone
//! values (counters, dictionary entries, id pointers) — is big-endian.
//! Record bodies are the codec's business (postcard varint).

use crate::error::{Error, Result};
use crate::storage::engine::{ReadTransaction, TableId, WriteTransaction, prefix_successor};
use crate::storage::{codec, keys};
use crate::types::{Dir, EdgeId, Neighbor, NodeId, NodeRecord, PlaneId, Properties};

pub const FORMAT_VERSION: u32 = 1;
pub const DEFAULT_PLANE_NAME: &str = "startup";

// ---- meta / init ----------------------------------------------------------

/// First-open initialization; verifies magic/version on an existing database.
pub fn init(txn: &mut dyn WriteTransaction) -> Result<()> {
    match txn.get(TableId::Meta, keys::META_MAGIC)? {
        Some(magic) if magic == keys::MAGIC => {
            let version = get_u32(txn, keys::META_FORMAT_VERSION)?
                .ok_or_else(|| Error::Corrupt("missing format version".into()))?;
            if version != FORMAT_VERSION {
                return Err(Error::Corrupt(format!(
                    "format version {version} not supported (expected {FORMAT_VERSION})"
                )));
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
            write_plane(
                txn,
                PlaneId::STARTUP,
                DEFAULT_PLANE_NAME,
                &Properties::new(),
            )
        }
    }
}

fn decode_u32(bytes: &[u8], what: &str) -> Result<u32> {
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

fn get_u64(txn: &dyn ReadTransaction, key: &[u8]) -> Result<Option<u64>> {
    txn.get(TableId::Meta, key)?
        .map(|v| {
            v.as_slice()
                .try_into()
                .map(u64::from_be_bytes)
                .map_err(|_| Error::Corrupt("bad u64 in meta".into()))
        })
        .transpose()
}

fn put_u64(txn: &mut dyn WriteTransaction, key: &[u8], v: u64) -> Result<()> {
    txn.put(TableId::Meta, key, &v.to_be_bytes())
}

/// Allocates the next id from a meta counter. TODO(M1): batch counter bumps
/// so bulk ingest doesn't pay a meta write per allocation.
fn next_id(txn: &mut dyn WriteTransaction, counter: &[u8]) -> Result<u64> {
    let id = get_u64(txn, counter)?.ok_or_else(|| Error::Corrupt("missing id counter".into()))?;
    put_u64(txn, counter, id + 1)?;
    Ok(id)
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

pub fn resolve_label(txn: &dyn ReadTransaction, id: u32) -> Result<String> {
    let bytes = txn
        .get(TableId::Meta, &keys::dict_label_rev_key(id))?
        .ok_or_else(|| Error::Corrupt(format!("dangling label id {id}")))?;
    String::from_utf8(bytes).map_err(|_| Error::Corrupt("bad label name".into()))
}

// ---- planes ---------------------------------------------------------------

fn write_plane(
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

// ---- nodes ----------------------------------------------------------------

pub fn create_node(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    labels: &[&str],
    props: &Properties,
) -> Result<NodeId> {
    let id = NodeId(next_id(txn, keys::META_NEXT_NODE_ID)?);
    let mut label_ids = Vec::with_capacity(labels.len());
    for l in labels {
        label_ids.push(intern_label(txn, l)?);
    }
    let record = codec::encode_node_record(&label_ids, props);
    txn.put(TableId::Nodes, &keys::node_key(plane, id), &record)?;
    txn.put(
        TableId::NodePlane,
        &keys::node_plane_key(id),
        &plane.0.to_be_bytes(),
    )?;
    for lid in label_ids {
        txn.put(TableId::LabelIdx, &keys::label_idx_key(plane, lid, id), b"")?;
    }
    Ok(id)
}

pub fn get_node(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    id: NodeId,
) -> Result<Option<NodeRecord>> {
    let Some(buf) = txn.get(TableId::Nodes, &keys::node_key(plane, id))? else {
        return Ok(None);
    };
    let (label_ids, properties) = codec::decode_node_record(&buf)?;
    let mut labels = Vec::with_capacity(label_ids.len());
    for lid in label_ids {
        labels.push(resolve_label(txn, lid)?);
    }
    Ok(Some(NodeRecord {
        id,
        plane,
        labels,
        properties,
    }))
}

fn node_exists(txn: &dyn ReadTransaction, plane: PlaneId, id: NodeId) -> Result<bool> {
    Ok(txn
        .get(TableId::Nodes, &keys::node_key(plane, id))?
        .is_some())
}

// ---- edges & adjacency ----------------------------------------------------

pub fn create_edge(
    txn: &mut dyn WriteTransaction,
    plane: PlaneId,
    src: NodeId,
    dst: NodeId,
    ty: &str,
    props: &Properties,
) -> Result<EdgeId> {
    // Both endpoints must exist in this plane — cross-plane edges are
    // rejected here, at the storage layer (arch/09 §1).
    for (which, node) in [("src", src), ("dst", dst)] {
        if !node_exists(txn, plane, node)? {
            return Err(Error::PlaneMismatch(format!(
                "{which} node {} does not exist in plane {}",
                node.0, plane.0
            )));
        }
    }
    let id = EdgeId(next_id(txn, keys::META_NEXT_EDGE_ID)?);
    let ty_id = intern_edge_type(txn, ty)?;
    let record = codec::encode_edge_record(src, dst, ty_id, props);
    txn.put(TableId::Edges, &keys::edge_key(plane, id), &record)?;
    txn.put(
        TableId::AdjFwd,
        &keys::adj_key(plane, src, ty_id, dst, id),
        b"",
    )?;
    txn.put(
        TableId::AdjRev,
        &keys::adj_key(plane, dst, ty_id, src, id),
        b"",
    )?;
    Ok(id)
}

/// 1-hop expansion via prefix scan on the adjacency tables (arch/01 §3).
pub fn neighbors(
    txn: &dyn ReadTransaction,
    plane: PlaneId,
    node: NodeId,
    dir: Dir,
    ty: Option<&str>,
) -> Result<Vec<Neighbor>> {
    // Unknown edge type ⇒ no edges of that type anywhere ⇒ empty result.
    let ty_id = match ty {
        None => None,
        Some(name) => match lookup_edge_type(txn, name)? {
            Some(id) => Some(id),
            None => return Ok(Vec::new()),
        },
    };

    let tables: &[TableId] = match dir {
        Dir::Out => &[TableId::AdjFwd],
        Dir::In => &[TableId::AdjRev],
        Dir::Both => &[TableId::AdjFwd, TableId::AdjRev],
    };

    let prefix: Vec<u8> = match ty_id {
        Some(t) => keys::adj_prefix_typed(plane, node, t).to_vec(),
        None => keys::adj_prefix(plane, node).to_vec(),
    };
    let end = prefix_successor(&prefix);

    let mut out = Vec::new();
    for table in tables {
        for item in txn.range(*table, &prefix, end.as_deref())? {
            let (key, _) = item?;
            let entry = keys::parse_adj_key(&key)?;
            out.push(Neighbor {
                node: entry.to,
                edge: entry.edge,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::engine::StorageEngine;
    use crate::storage::memory::MemoryEngine;

    /// Runs `f` in an initialized write transaction and commits.
    fn with_db<T>(f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T>) -> T {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let out = f(&mut txn).unwrap();
        txn.commit().unwrap();
        out
    }

    #[test]
    fn init_is_idempotent() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        init(&mut txn).unwrap(); // second init on same data: verify, not clobber
        // the startup plane exists exactly once
        assert_eq!(
            plane_id_by_name(&txn, DEFAULT_PLANE_NAME).unwrap(),
            Some(PlaneId::STARTUP)
        );
    }

    #[test]
    fn init_rejects_bad_magic() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        txn.put(TableId::Meta, keys::META_MAGIC, b"NOPE").unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    /// A corrupted database must surface `Corrupt` errors, never panic.
    #[test]
    fn corrupted_meta_errors_cleanly() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();

        // garbage node-id counter (wrong width)
        txn.put(TableId::Meta, keys::META_NEXT_NODE_ID, b"xx")
            .unwrap();
        assert!(matches!(
            create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()),
            Err(Error::Corrupt(_))
        ));
        put_u64(&mut txn, keys::META_NEXT_NODE_ID, 1).unwrap();

        // missing counter
        txn.delete(TableId::Meta, keys::META_NEXT_EDGE_TYPE_ID)
            .unwrap();
        assert!(matches!(
            intern_edge_type(&mut txn, "T"),
            Err(Error::Corrupt(_))
        ));

        // garbage dictionary entry (wrong width)
        txn.put(TableId::Meta, &keys::dict_label_key("Bad"), b"toolong")
            .unwrap();
        assert!(matches!(
            intern_label(&mut txn, "Bad"),
            Err(Error::Corrupt(_))
        ));

        // reverse dictionary entry with invalid utf-8
        let id = intern_label(&mut txn, "Ok").unwrap();
        txn.put(TableId::Meta, &keys::dict_label_rev_key(id), &[0xFF, 0xFE])
            .unwrap();
        assert!(matches!(resolve_label(&txn, id), Err(Error::Corrupt(_))));

        // garbage plane-name entry (wrong width)
        txn.put(TableId::PlaneNames, &keys::plane_name_key("bad"), b"12345")
            .unwrap();
        assert!(matches!(
            plane_id_by_name(&txn, "bad"),
            Err(Error::Corrupt(_))
        ));

        // garbage node record body
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.put(
            TableId::Nodes,
            &keys::node_key(PlaneId::STARTUP, n),
            &[0xFF; 3],
        )
        .unwrap();
        assert!(matches!(
            get_node(&txn, PlaneId::STARTUP, n),
            Err(Error::Corrupt(_))
        ));

        // node referencing a label id with no dictionary entry
        let m = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.put(
            TableId::Nodes,
            &keys::node_key(PlaneId::STARTUP, m),
            &codec::encode_node_record(&[4040], &Properties::new()),
        )
        .unwrap();
        assert!(matches!(
            get_node(&txn, PlaneId::STARTUP, m),
            Err(Error::Corrupt(_))
        ));

        // malformed adjacency key (wrong length)
        txn.put(TableId::AdjFwd, b"short", b"").unwrap();
        let mut prefix_hit = keys::adj_prefix(PlaneId::STARTUP, NodeId(0)).to_vec();
        prefix_hit.clear(); // scan whole table via empty prefix
        let _ = prefix_hit;
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        // craft a bad entry under a's own prefix so neighbors() parses it
        let mut bad_key = keys::adj_prefix(PlaneId::STARTUP, a).to_vec();
        bad_key.push(0xAB);
        txn.put(TableId::AdjFwd, &bad_key, b"").unwrap();
        assert!(matches!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn init_rejects_future_format_version() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.put(
            TableId::Meta,
            keys::META_FORMAT_VERSION,
            &(FORMAT_VERSION + 1).to_be_bytes(),
        )
        .unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    #[test]
    fn init_rejects_missing_version() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.delete(TableId::Meta, keys::META_FORMAT_VERSION)
            .unwrap();
        assert!(matches!(init(&mut txn), Err(Error::Corrupt(_))));
    }

    #[test]
    fn interning_is_stable_and_distinct() {
        with_db(|txn| {
            let a1 = intern_label(txn, "Person")?;
            let a2 = intern_label(txn, "Person")?;
            let b = intern_label(txn, "Paper")?;
            assert_eq!(a1, a2, "same name → same id");
            assert_ne!(a1, b, "different names → different ids");
            assert_eq!(resolve_label(txn, a1)?, "Person");
            assert_eq!(resolve_label(txn, b)?, "Paper");

            // labels and edge types are separate dictionaries
            let e = intern_edge_type(txn, "Person")?;
            assert_eq!(lookup_edge_type(txn, "Person")?, Some(e));
            assert_eq!(lookup_edge_type(txn, "KNOWS")?, None);
            Ok(())
        });
    }

    #[test]
    fn resolving_a_dangling_label_id_is_corrupt() {
        with_db(|txn| {
            assert!(matches!(resolve_label(txn, 999), Err(Error::Corrupt(_))));
            Ok(())
        });
    }

    #[test]
    fn ids_are_sequential_within_and_across_transactions() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n1 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let n2 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        assert_eq!(n2.0, n1.0 + 1);
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let n3 = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        assert_eq!(n3.0, n2.0 + 1);
        txn.commit().unwrap();
    }

    #[test]
    fn aborted_transaction_ids_may_be_reused() {
        // Counter bumps roll back with the transaction: an id handed out by
        // an aborted txn was never committed, so reuse is safe. This test
        // documents that semantic.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        txn.commit().unwrap();

        let mut txn = eng.begin_write().unwrap();
        let ghost = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        drop(txn); // abort

        let mut txn = eng.begin_write().unwrap();
        let real = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        txn.commit().unwrap();
        assert_eq!(ghost, real);
    }

    #[test]
    fn node_with_no_labels_and_no_props() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert!(rec.labels.is_empty());
        assert!(rec.properties.is_empty());
    }

    #[test]
    fn duplicate_labels_are_preserved_as_given() {
        // Soft schema: storage does not deduplicate; documents behavior.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &["A", "A"], &Properties::new()).unwrap();
        let rec = get_node(&txn, PlaneId::STARTUP, n).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["A".to_string(), "A".to_string()]);
    }

    #[test]
    fn unicode_names_survive() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let plane = create_plane(&mut txn, "研究-λ", &Properties::new()).unwrap();
        let n = create_node(&mut txn, plane, &["实体", "Ünïcodé"], &Properties::new()).unwrap();
        assert_eq!(plane_id_by_name(&txn, "研究-λ").unwrap(), Some(plane));
        let rec = get_node(&txn, plane, n).unwrap().unwrap();
        assert_eq!(rec.labels, vec!["实体".to_string(), "Ünïcodé".to_string()]);
    }

    #[test]
    fn parallel_edges_coexist() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let e1 = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "CITES",
            &Properties::new(),
        )
        .unwrap();
        let e2 = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "CITES",
            &Properties::new(),
        )
        .unwrap();
        assert_ne!(e1, e2);
        let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("CITES")).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|n| n.node == b));
    }

    #[test]
    fn typed_neighbors_filter_by_edge_type() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let c = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            b,
            "KNOWS",
            &Properties::new(),
        )
        .unwrap();
        create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            c,
            "CITES",
            &Properties::new(),
        )
        .unwrap();

        let knows = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, Some("KNOWS")).unwrap();
        assert_eq!(knows.len(), 1);
        assert_eq!(knows[0].node, b);
        let all = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn self_loop_appears_in_both_directions() {
        // A self-loop writes one adj_fwd and one adj_rev entry, so Dir::Both
        // reports it twice (once per direction). Documents behavior.
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, PlaneId::STARTUP, a, a, "SELF", &Properties::new()).unwrap();
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::In, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            neighbors(&txn, PlaneId::STARTUP, a, Dir::Both, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn neighbors_of_unknown_node_is_empty() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let out = neighbors(&txn, PlaneId::STARTUP, NodeId(999), Dir::Both, None).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn edge_with_missing_endpoint_reports_which_side() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();

        let err = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            a,
            NodeId(999),
            "X",
            &Properties::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::PlaneMismatch(m) if m.contains("dst")),
            "got: {err}"
        );

        let err = create_edge(
            &mut txn,
            PlaneId::STARTUP,
            NodeId(999),
            a,
            "X",
            &Properties::new(),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::PlaneMismatch(m) if m.contains("src")),
            "got: {err}"
        );
    }

    #[test]
    fn adjacency_is_isolated_per_node_and_per_plane() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();

        let a = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        let b = create_node(&mut txn, PlaneId::STARTUP, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, PlaneId::STARTUP, a, b, "T", &Properties::new()).unwrap();

        let x = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
        let y = create_node(&mut txn, p2, &[], &Properties::new()).unwrap();
        create_edge(&mut txn, p2, x, y, "T", &Properties::new()).unwrap();

        // b has no out-edges; a's expansion does not leak plane 2's edges
        assert!(
            neighbors(&txn, PlaneId::STARTUP, b, Dir::Out, None)
                .unwrap()
                .is_empty()
        );
        let out = neighbors(&txn, PlaneId::STARTUP, a, Dir::Out, None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].node, b);
        // and node ids are globally unique across planes
        assert_ne!(a, x);
        assert_ne!(b, y);
    }

    #[test]
    fn plane_ids_are_distinct_and_names_unique() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p1 = create_plane(&mut txn, "p1", &Properties::new()).unwrap();
        let p2 = create_plane(&mut txn, "p2", &Properties::new()).unwrap();
        assert_ne!(p1, p2);
        assert_ne!(p1, PlaneId::STARTUP);
        assert!(matches!(
            create_plane(&mut txn, "p1", &Properties::new()),
            Err(Error::PlaneExists(_))
        ));
        assert_eq!(plane_id_by_name(&txn, "p1").unwrap(), Some(p1));
        assert_eq!(plane_id_by_name(&txn, "absent").unwrap(), None);
    }

    #[test]
    fn get_node_in_wrong_plane_is_none() {
        let eng = MemoryEngine::new();
        let mut txn = eng.begin_write().unwrap();
        init(&mut txn).unwrap();
        let p2 = create_plane(&mut txn, "other", &Properties::new()).unwrap();
        let n = create_node(&mut txn, PlaneId::STARTUP, &["L"], &Properties::new()).unwrap();
        assert!(get_node(&txn, p2, n).unwrap().is_none());
        assert!(get_node(&txn, PlaneId::STARTUP, n).unwrap().is_some());
    }
}
