//! Full-database snapshot + restore (ROADMAP §6): a consistent, portable,
//! id-faithful bundle of every plane's nodes/edges + index declarations + the
//! built vector/keyword sidecars, taken at one commit sequence.
//!
//! **Format.** A length-prefixed stream of postcard [`Frame`]s —
//! `[u32-le len][Frame bytes]` repeated to EOF. Portable across backends
//! (postcard is the durable codec), so a snapshot restores to native, redb, or
//! memory. A restore rebuilds into a *fresh* database preserving node/plane ids
//! and the commit sequence, so the shipped sidecars load as-is (no rebuild).
//!
//! **Consistency.** The KV dump runs on one pinned read snapshot; the sidecars
//! are the live in-memory registries, held read-locked for the dump. Take a
//! snapshot when writes are quiesced for perfect fidelity — under concurrent
//! writes a sidecar may reflect a slightly different commit than the data.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use super::*;

/// Snapshot bundle format version (bumped on any incompatible frame change).
const SNAPSHOT_FORMAT: u32 = 1;

/// One record in the snapshot stream.
#[derive(Serialize, Deserialize)]
enum Frame {
    /// Always first: format, the pinned commit sequence, and the id counters
    /// (next node/edge/plane/label/edge-type id) to restore id-faithfully.
    Manifest {
        format: u32,
        seq: u64,
        counters: [u64; 5],
    },
    Plane {
        id: u32,
        name: String,
        props: Properties,
    },
    Node {
        plane: u32,
        id: u64,
        key: Option<String>,
        labels: Vec<String>,
        props: Properties,
    },
    Edge {
        plane: u32,
        id: u64,
        src: u64,
        dst: u64,
        ty: String,
        props: Properties,
    },
    VectorIndex {
        plane: u32,
        label: String,
        property: String,
        metric: Metric,
    },
    KeywordIndex {
        plane: u32,
        label: String,
        property: String,
        language: Language,
    },
    /// The built HNSW vector-index sidecar bytes (stamped with `seq`).
    Hnsw(Vec<u8>),
    /// The built BM25 keyword-index sidecar bytes (stamped with `seq`).
    Bm25(Vec<u8>),
}

/// Counts a [`Database::snapshot`] / [`Database::restore`] moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    pub planes: usize,
    pub nodes: usize,
    pub edges: usize,
    /// The commit sequence the snapshot was taken at (and restored to).
    pub seq: u64,
}

fn write_frame(w: &mut impl Write, frame: &Frame) -> Result<()> {
    let bytes = postcard::to_stdvec(frame).map_err(crate::error::backend)?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| Error::InvalidArgument("snapshot frame too large".into()))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    Ok(())
}

/// Read the next frame, or `None` at clean end-of-stream.
fn read_frame(r: &mut impl Read) -> Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes)?;
    Ok(Some(
        postcard::from_bytes(&bytes).map_err(crate::error::backend)?,
    ))
}

impl Database {
    /// Write a consistent, whole-database snapshot (ROADMAP §6) to `out`: every
    /// plane's nodes and edges, the index declarations, and the built
    /// vector/keyword sidecars, at one commit sequence. Restore it with
    /// [`restore`](Self::restore).
    pub fn snapshot(&self, mut out: impl Write) -> Result<SnapshotStats> {
        // Hold the registry read locks across the whole dump so no commit can
        // advance them while we serialize (see the module's consistency note).
        let registry = self.indexes();
        let keywords = self.keywords();

        let mut stats = SnapshotStats::default();
        // A registry that diverged from the KV after a durable commit must not
        // be embedded — a restore would load it verbatim. Rebuild from the KV
        // inside the same read snapshot instead (the KV is the truth).
        let mut rebuilt: Option<VectorRegistry> = None;
        self.engine.with_read(|txn| {
            if self.indexes_diverged() {
                let mut fresh = VectorRegistry::new();
                fresh.rebuild_from(txn)?;
                rebuilt = Some(fresh);
            }
            let seq = graph::read_commit_seq(txn)?;
            stats.seq = seq;
            write_frame(
                &mut out,
                &Frame::Manifest {
                    format: SNAPSHOT_FORMAT,
                    seq,
                    counters: graph::read_id_counters(txn)?,
                },
            )?;

            for (pid, name) in graph::list_planes(txn)? {
                let (_, props) = graph::read_plane(txn, pid)?
                    .ok_or_else(|| Error::NotFound(format!("plane {}", pid.0)))?;
                write_frame(
                    &mut out,
                    &Frame::Plane {
                        id: pid.0,
                        name,
                        props,
                    },
                )?;
                stats.planes += 1;

                for nid in graph::scan_all(txn, pid)? {
                    if let Some(n) = graph::get_node(txn, pid, nid)? {
                        write_frame(
                            &mut out,
                            &Frame::Node {
                                plane: pid.0,
                                id: nid.0,
                                key: n.external_key,
                                labels: n.labels,
                                props: n.properties,
                            },
                        )?;
                        stats.nodes += 1;
                    }
                }
                for eid in graph::scan_edges(txn, pid)? {
                    if let Some(e) = graph::get_edge(txn, pid, eid)? {
                        write_frame(
                            &mut out,
                            &Frame::Edge {
                                plane: pid.0,
                                id: eid.0,
                                src: e.src.0,
                                dst: e.dst.0,
                                ty: e.ty,
                                props: e.properties,
                            },
                        )?;
                        stats.edges += 1;
                    }
                }
            }

            for (pid, label, property, metric) in graph::list_vector_indexes(txn)? {
                write_frame(
                    &mut out,
                    &Frame::VectorIndex {
                        plane: pid.0,
                        label,
                        property,
                        metric,
                    },
                )?;
            }
            for (pid, label, property, language) in graph::list_keyword_indexes(txn)? {
                write_frame(
                    &mut out,
                    &Frame::KeywordIndex {
                        plane: pid.0,
                        label,
                        property,
                        language,
                    },
                )?;
            }
            Ok::<(), Error>(())
        })?;

        // The built sidecars, serialized from the live registries stamped at the
        // dump's sequence — a restore loads them directly (ids are preserved).
        let hnsw = rebuilt.as_ref().unwrap_or(&registry);
        write_frame(&mut out, &Frame::Hnsw(hnsw.to_bytes(stats.seq)?))?;
        write_frame(&mut out, &Frame::Bm25(keywords.to_bytes(stats.seq)?))?;
        out.flush()?;
        Ok(stats)
    }

    /// Restore a snapshot (ROADMAP §6) into THIS database, which must be empty
    /// (a freshly opened one). Rebuilds every plane/node/edge with its original
    /// id, restores the id counters and commit sequence, and loads the shipped
    /// sidecars as-is. Errors if the target already holds data.
    pub fn restore(&self, mut input: impl Read) -> Result<SnapshotStats> {
        // Refuse a non-empty target: any user plane, or any node anywhere.
        self.engine.with_read(|txn| {
            for (pid, name) in graph::list_planes(txn)? {
                if name != graph::DEFAULT_PLANE_NAME {
                    return Err(Error::InvalidArgument(
                        "restore target is not empty (extra planes present)".into(),
                    ));
                }
                if !graph::scan_all(txn, pid)?.is_empty() {
                    return Err(Error::InvalidArgument(
                        "restore target is not empty (nodes present)".into(),
                    ));
                }
            }
            Ok::<(), Error>(())
        })?;

        let mut manifest: Option<(u64, [u64; 5])> = None;
        let mut hnsw: Option<Vec<u8>> = None;
        let mut bm25: Option<Vec<u8>> = None;
        let mut stats = SnapshotStats::default();

        // One transaction, no automatic commit-seq bump — we land the source's
        // exact sequence so the sidecars (stamped with it) stay valid — and
        // the engine's own sequence is lifted to it too: a replica that
        // restores its master's snapshot must stand where the master stood,
        // or the master's next live batch is refused as a replay (arch/01
        // §9). A source sequence at or below this store's own (the master
        // never wrote past its bootstrap) cannot be adopted without moving
        // sequences backwards; the restore then lands at the next free one
        // and a follower converges after one more resync.
        self.engine.with_write_raw(|txn| {
            while let Some(frame) = read_frame(&mut input)? {
                match frame {
                    Frame::Manifest {
                        format,
                        seq,
                        counters,
                    } => {
                        if format != SNAPSHOT_FORMAT {
                            return Err(Error::InvalidArgument(format!(
                                "unsupported snapshot format {format} (expected {SNAPSHOT_FORMAT})"
                            )));
                        }
                        manifest = Some((seq, counters));
                        stats.seq = seq;
                    }
                    Frame::Plane { id, name, props } => {
                        // Startup already exists from init(); write_plane at the
                        // same id simply overwrites it.
                        graph::write_plane(txn, PlaneId(id), &name, &props)?;
                        stats.planes += 1;
                    }
                    Frame::Node {
                        plane,
                        id,
                        key,
                        labels,
                        props,
                    } => {
                        let lbls: Vec<&str> = labels.iter().map(String::as_str).collect();
                        graph::insert_node(
                            txn,
                            PlaneId(plane),
                            NodeId(id),
                            key.as_deref(),
                            &lbls,
                            &props,
                        )?;
                        stats.nodes += 1;
                    }
                    Frame::Edge {
                        plane,
                        id,
                        src,
                        dst,
                        ty,
                        props,
                    } => {
                        graph::insert_edge(
                            txn,
                            PlaneId(plane),
                            EdgeId(id),
                            NodeId(src),
                            NodeId(dst),
                            &ty,
                            &props,
                        )?;
                        stats.edges += 1;
                    }
                    Frame::VectorIndex {
                        plane,
                        label,
                        property,
                        metric,
                    } => {
                        graph::declare_vector_index(
                            txn,
                            PlaneId(plane),
                            &label,
                            &property,
                            metric,
                        )?;
                    }
                    Frame::KeywordIndex {
                        plane,
                        label,
                        property,
                        language,
                    } => {
                        graph::declare_keyword_index(
                            txn,
                            PlaneId(plane),
                            &label,
                            &property,
                            language,
                        )?;
                    }
                    Frame::Hnsw(bytes) => hnsw = Some(bytes),
                    Frame::Bm25(bytes) => bm25 = Some(bytes),
                }
            }
            let (seq, counters) =
                manifest.ok_or_else(|| Error::Corrupt("snapshot has no manifest".into()))?;
            graph::set_id_counters(txn, counters)?;
            graph::set_commit_seq(txn, seq)?;
            // The per-plane summary counters (arch/03 §5) are maintained by
            // `WriteTxn::commit`, which this path does not go through: the
            // frames were written with the raw graph inserts. Recount each
            // plane from what was just written, or the target keeps the
            // zero row its bootstrap wrote and `db.stats` / `drsg stats`
            // report an empty database over a full one.
            for (plane, _name) in graph::list_planes(txn)? {
                let counted = catalog::count(txn, plane)?;
                txn.put(
                    TableId::Meta,
                    &crate::storage::keys::counters_key(plane),
                    &counted.encode(),
                )?;
            }
            Ok::<((), u64), Error>(((), seq))
        })?;

        let seq = stats.seq;
        // The restore landed the source's commit sequence, not a fresh one —
        // and this database may already hold cache entries stamped with that
        // very number, read from its own (empty) state before the restore.
        // Exact-seq matching would serve them as current, so drop everything.
        self.cache.invalidate_all();
        // Load the shipped sidecars into the live registries — ids are
        // preserved, so they match — and persist them where this database
        // keeps its own. The live registries are what every search reads, so
        // they are loaded whether or not a sidecar path exists: an in-memory
        // database has none, and before this it kept its pre-restore (empty)
        // registries and searched nothing. A snapshot with no usable sidecar
        // (a frame missing, or stamped with another seq) falls back to the
        // KV, which is always the source of truth.
        let vectors = hnsw
            .as_deref()
            .and_then(|bytes| VectorRegistry::from_bytes(bytes, seq));
        let vectors = match vectors {
            Some(reg) => {
                if let (Some(bytes), Some(path)) = (&hnsw, self.sidecar.as_deref()) {
                    std::fs::write(path, bytes)?;
                }
                reg
            }
            None => {
                let mut reg = VectorRegistry::new();
                self.engine.with_read(|txn| reg.rebuild_from(txn))?;
                tracing::info!("rebuilt vector indexes from the restored KV (no usable sidecar)");
                reg
            }
        };
        let keywords = bm25
            .as_deref()
            .and_then(|bytes| KeywordRegistry::from_bytes(bytes, seq));
        let keywords = match keywords {
            Some(reg) => {
                if let (Some(bytes), Some(path)) = (&bm25, self.keyword_sidecar.as_deref()) {
                    std::fs::write(path, bytes)?;
                }
                reg
            }
            None => {
                let mut reg = KeywordRegistry::new();
                self.engine.with_read(|txn| reg.rebuild_from(txn))?;
                tracing::info!("rebuilt keyword indexes from the restored KV (no usable sidecar)");
                reg
            }
        };
        *self.indexes_mut() = vectors;
        *self.keywords_mut() = keywords;
        Ok(stats)
    }
}

#[cfg(all(test, feature = "native-backend"))]
mod tests {
    use super::*;
    use crate::types::{PropDesc, PropValue};

    fn doc(v: f32) -> Properties {
        let mut p = Properties::new();
        p.insert(
            "title".into(),
            PropDesc::new(PropValue::Str(format!("t{v}"))),
        );
        p.insert(
            "embedding".into(),
            PropDesc::new(PropValue::Vector(vec![v, 1.0 - v])),
        );
        p
    }

    #[test]
    fn roundtrip_preserves_ids_indexes_and_seq() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = Database::open(src_dir.path().join("db")).unwrap();
        let plane = src.create_plane("p", Properties::new()).unwrap();
        plane
            .ensure_vector_index("Doc", "embedding", Metric::Cosine)
            .unwrap();
        let (a, b) = {
            let mut w = plane.write().unwrap();
            let a = w.create_node_with_key("a", &["Doc"], doc(0.1)).unwrap();
            let b = w.create_node_with_key("b", &["Doc"], doc(0.9)).unwrap();
            w.create_edge(a, b, "LINKS", Properties::new()).unwrap();
            w.commit().unwrap();
            (a, b)
        };
        let src_seq = src.commit_seq().unwrap();

        let mut buf = Vec::new();
        let stats = src.snapshot(&mut buf).unwrap();
        assert_eq!((stats.nodes, stats.edges, stats.seq), (2, 1, src_seq));
        let src_counters = src.counters().unwrap();
        assert_eq!(src_counters.nodes, 2);
        drop(src);

        // Restore into a fresh database.
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = Database::open(dst_dir.path().join("db")).unwrap();
        let rstats = dst.restore(&mut buf.as_slice()).unwrap();
        assert_eq!((rstats.nodes, rstats.edges), (2, 1));
        assert_eq!(dst.commit_seq().unwrap(), src_seq, "commit seq restored");
        // The summary counters are recounted from the restored frames, not
        // left at the target's bootstrap zeros.
        assert_eq!(dst.counters().unwrap(), src_counters);

        let dp = dst.plane("p").unwrap();
        // Ids are preserved byte-for-byte.
        assert_eq!(dp.node_by_key("a").unwrap().unwrap().id, a);
        assert_eq!(dp.node_by_key("b").unwrap().unwrap().id, b);
        assert!(
            dp.node_by_key("a")
                .unwrap()
                .unwrap()
                .labels
                .contains(&"Doc".to_string())
        );

        // The vector index was restored from the shipped sidecar (search works).
        let hits = dp
            .query()
            .vector_top_k(
                Some("Doc"),
                "embedding",
                vec![0.85, 0.15],
                Metric::Cosine,
                5,
            )
            .ids()
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], b, "nearest to (0.85,0.15) is b (0.9,0.1)");

        // Counters were restored: a fresh create allocates past everything.
        let c = {
            let mut w = dp.write().unwrap();
            let c = w
                .create_node_with_key("c", &["Doc"], Properties::new())
                .unwrap();
            w.commit().unwrap();
            c
        };
        assert!(c.0 > a.0 && c.0 > b.0, "new id past restored ids");
    }

    /// A source with one vector index and one keyword index over two Docs,
    /// dumped to a snapshot; returns the snapshot and the ids.
    fn indexed_snapshot() -> (Vec<u8>, NodeId, NodeId) {
        let src = Database::in_memory().unwrap();
        let plane = src.create_plane("p", Properties::new()).unwrap();
        plane
            .ensure_vector_index("Doc", "embedding", Metric::Cosine)
            .unwrap();
        plane
            .ensure_keyword_index("Doc", "title", crate::Language::English)
            .unwrap();
        let (a, b) = {
            let mut w = plane.write().unwrap();
            let mut pa = doc(0.1);
            pa.insert(
                "title".into(),
                PropDesc::new(PropValue::Str("graph databases".into())),
            );
            let mut pb = doc(0.9);
            pb.insert(
                "title".into(),
                PropDesc::new(PropValue::Str("vector search".into())),
            );
            let a = w.create_node_with_key("a", &["Doc"], pa).unwrap();
            let b = w.create_node_with_key("b", &["Doc"], pb).unwrap();
            w.commit().unwrap();
            (a, b)
        };
        let mut buf = Vec::new();
        src.snapshot(&mut buf).unwrap();
        (buf, a, b)
    }

    /// Both index-backed searches answer on `db` after a restore of
    /// [`indexed_snapshot`].
    fn assert_indexes_serve(db: &Database, a: NodeId, b: NodeId) {
        let dp = db.plane("p").unwrap();
        let hits = dp
            .query()
            .vector_top_k(
                Some("Doc"),
                "embedding",
                vec![0.85, 0.15],
                Metric::Cosine,
                5,
            )
            .ids()
            .unwrap();
        assert_eq!(hits, vec![b, a], "vector search after restore");
        let hits = dp.keyword_search("Doc", "title", "graph", 5);
        assert_eq!(
            hits.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![a],
            "keyword search after restore"
        );
    }

    /// An in-memory database has no sidecar files, but the live registries
    /// are what searches read: a restore must still load the shipped indexes.
    #[test]
    fn in_memory_restore_loads_the_shipped_indexes() {
        let (buf, a, b) = indexed_snapshot();
        let dst = Database::in_memory().unwrap();
        dst.restore(&mut buf.as_slice()).unwrap();
        assert_indexes_serve(&dst, a, b);
    }

    /// A snapshot carrying no sidecar frames (or unusable ones) still restores
    /// working indexes: they are rebuilt from the restored KV.
    #[test]
    fn restore_without_sidecars_rebuilds_the_indexes_from_the_kv() {
        let (buf, a, b) = indexed_snapshot();
        // Re-serialize the snapshot minus its index frames.
        let mut stripped = Vec::new();
        let mut input = buf.as_slice();
        while let Some(frame) = read_frame(&mut input).unwrap() {
            if !matches!(frame, Frame::Hnsw(_) | Frame::Bm25(_)) {
                write_frame(&mut stripped, &frame).unwrap();
            }
        }
        assert!(stripped.len() < buf.len());

        let dst = Database::in_memory().unwrap();
        dst.restore(&mut stripped.as_slice()).unwrap();
        assert_indexes_serve(&dst, a, b);

        let dir = tempfile::tempdir().unwrap();
        let on_disk = Database::open(dir.path().join("db")).unwrap();
        on_disk.restore(&mut stripped.as_slice()).unwrap();
        assert_indexes_serve(&on_disk, a, b);
    }

    /// Adjacency of node 1 in the startup plane, read through the query
    /// path's reader (so through the L2 cache).
    fn hops_of_node_1(db: &Database) -> Vec<NodeId> {
        db.plane(graph::DEFAULT_PLANE_NAME)
            .unwrap()
            .with_reader(|r| Ok(r.neighbors(NodeId(1), Dir::Out, None)?.to_vec()))
            .unwrap()
            .into_iter()
            .map(|n| n.node)
            .collect()
    }

    /// A snapshot whose source made exactly one graph write past a fresh open
    /// (node 1 → node 2 in the startup plane), and the seq it reached — one
    /// an empty target reaches with a single graph write of its own.
    fn snapshot_one_write_in() -> (Vec<u8>, u64) {
        let src_dir = tempfile::tempdir().unwrap();
        let src = Database::open(src_dir.path().join("db")).unwrap();
        {
            let mut w = src
                .plane(graph::DEFAULT_PLANE_NAME)
                .unwrap()
                .write()
                .unwrap();
            let a = w.create_node(&["Doc"], Properties::new()).unwrap();
            let b = w.create_node(&["Doc"], Properties::new()).unwrap();
            w.create_edge(a, b, "LINKS", Properties::new()).unwrap();
            w.commit().unwrap();
        }
        let seq = src.commit_seq().unwrap();
        let mut buf = Vec::new();
        src.snapshot(&mut buf).unwrap();
        (buf, seq)
    }

    /// Restore lands the source's commit seq verbatim. The target may already
    /// hold cache entries stamped with that number — read from its own, empty
    /// state — and exact-seq matching would keep serving them over the
    /// restored data unless restore drops the cache.
    #[test]
    fn restore_invalidates_cache_entries_stamped_with_the_restored_seq() {
        let (buf, seq) = snapshot_one_write_in();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = Database::open(dst_dir.path().join("db")).unwrap();
        // One graph write that leaves the target empty, so it reaches the
        // source's seq with nothing in it.
        dst.plane(graph::DEFAULT_PLANE_NAME)
            .unwrap()
            .ensure_vector_index("Doc", "embedding", Metric::Cosine)
            .unwrap();
        assert_eq!(dst.commit_seq().unwrap(), seq);
        assert!(
            hops_of_node_1(&dst).is_empty(),
            "warm the cache: no node 1 yet"
        );

        dst.restore(&mut buf.as_slice()).unwrap();
        assert_eq!(dst.commit_seq().unwrap(), seq);
        assert_eq!(
            hops_of_node_1(&dst),
            vec![NodeId(2)],
            "restored data, not the stale entry"
        );
    }

    /// The same hazard one hop away: a replica applies its master's restore as
    /// a replicated batch, whose content changes without the commit seq moving.
    #[test]
    fn replicated_restore_invalidates_the_replica_cache() {
        use std::sync::Mutex;
        let (buf, seq) = snapshot_one_write_in();

        let master_dir = tempfile::tempdir().unwrap();
        let master = Database::open(master_dir.path().join("db")).unwrap();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sink = batches.clone();
        master
            .on_wal_commit(move |b| sink.lock().unwrap().push(b))
            .unwrap();
        master
            .plane(graph::DEFAULT_PLANE_NAME)
            .unwrap()
            .ensure_vector_index("Doc", "embedding", Metric::Cosine)
            .unwrap();
        master.restore(&mut buf.as_slice()).unwrap();
        let mut captured: Vec<_> = batches.lock().unwrap().drain(..).collect();
        assert_eq!(captured.len(), 2, "the index declaration, then the restore");
        let restore = captured.pop().unwrap();
        let declare = captured.pop().unwrap();

        let replica_dir = tempfile::tempdir().unwrap();
        let replica = Database::open_read_only(replica_dir.path().join("db")).unwrap();
        replica.apply_replicated(declare).unwrap();
        assert_eq!(replica.commit_seq().unwrap(), seq);
        assert!(
            hops_of_node_1(&replica).is_empty(),
            "warm at the source's seq"
        );

        replica.apply_replicated(restore).unwrap();
        assert_eq!(replica.commit_seq().unwrap(), seq, "a restore moves no seq");
        assert_eq!(hops_of_node_1(&replica), vec![NodeId(2)]);
    }

    /// The replica flow end to end (arch/01 §9): a fresh follower opens
    /// read-only (its bootstrap commit takes engine sequence 1), restores the
    /// master's snapshot, then applies the master's live batches. The restore
    /// must leave the follower's store at the master's sequence — not at
    /// "one past its own bootstrap" — or the master's very next batch is at
    /// or below the follower's `committed_seq` and `apply_replicated` refuses
    /// it, which the follow loop answers with a full resync.
    #[cfg(feature = "native-backend")]
    #[test]
    fn a_fresh_replica_adopts_the_snapshot_sequence_and_accepts_the_next_batch() {
        use std::sync::Mutex;
        let master_dir = tempfile::tempdir().unwrap();
        let master = Database::open(master_dir.path().join("db")).unwrap();
        // Young master: bootstrap plus two data writes, so its sequence (3)
        // is above what a fresh replica's own bootstrap + restore would
        // allocate (2) — the case where a restore that allocates its own
        // sequence leaves the replica *behind* the master.
        for key in ["a", "b"] {
            let mut w = master
                .plane(graph::DEFAULT_PLANE_NAME)
                .unwrap()
                .write()
                .unwrap();
            w.create_node_with_key(key, &["Doc"], Properties::new())
                .unwrap();
            w.commit().unwrap();
        }
        let mut buf = Vec::new();
        master.snapshot(&mut buf).unwrap();
        let (_, master_seq) = master.engine.snapshot_window().unwrap();
        assert_eq!(master_seq, 3);

        // Live batches from here on, as `/ws/wal` would ship them.
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sink = batches.clone();
        master
            .on_wal_commit(move |b| sink.lock().unwrap().push(b))
            .unwrap();

        let replica_dir = tempfile::tempdir().unwrap();
        let replica = Database::open_read_only(replica_dir.path().join("db")).unwrap();
        let (_, before) = replica.engine.snapshot_window().unwrap();
        assert_eq!(before, 1, "a fresh replica's bootstrap took sequence 1");
        let stats = replica.restore(&mut buf.as_slice()).unwrap();
        assert_eq!(stats.seq, master_seq);
        let (_, after) = replica.engine.snapshot_window().unwrap();
        assert_eq!(
            after, master_seq,
            "the restore landed at the snapshot's sequence, not at the replica's next free one"
        );

        // The master's next commit is the follower's first live batch.
        {
            let mut w = master
                .plane(graph::DEFAULT_PLANE_NAME)
                .unwrap()
                .write()
                .unwrap();
            w.create_node_with_key("c", &["Doc"], Properties::new())
                .unwrap();
            w.commit().unwrap();
        }
        let batch = batches.lock().unwrap().pop().expect("one live batch");
        assert_eq!(batch.seq, master_seq + 1);
        replica
            .apply_replicated(batch)
            .expect("the first live batch after a bootstrap is accepted");
        assert_eq!(replica.commit_seq().unwrap(), master.commit_seq().unwrap());
        assert_eq!(
            replica
                .plane(graph::DEFAULT_PLANE_NAME)
                .unwrap()
                .catalog()
                .unwrap()
                .node_count,
            3
        );
    }

    /// The one sequence a restore cannot adopt: a master that never wrote
    /// past its bootstrap has sequence 1, the same as the fresh replica's
    /// own bootstrap, so the restore lands at 2 and the master's first batch
    /// (2) is refused. The follow loop then resyncs from scratch — and the
    /// resync converges, because the snapshot it pulls now includes that
    /// batch and stands at 2, which the replica adopts. This pins that the
    /// refusal is a single extra round trip and not a loop.
    #[cfg(feature = "native-backend")]
    #[test]
    fn a_never_written_master_costs_a_fresh_replica_one_resync_not_a_loop() {
        use std::sync::Mutex;
        let master_dir = tempfile::tempdir().unwrap();
        let master = Database::open(master_dir.path().join("db")).unwrap();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let sink = batches.clone();
        master
            .on_wal_commit(move |b| sink.lock().unwrap().push(b))
            .unwrap();
        let write = |key: &str| {
            let mut w = master
                .plane(graph::DEFAULT_PLANE_NAME)
                .unwrap()
                .write()
                .unwrap();
            w.create_node_with_key(key, &["Doc"], Properties::new())
                .unwrap();
            w.commit().unwrap();
        };
        let fresh_replica = |buf: &[u8]| {
            let dir = tempfile::tempdir().unwrap();
            let replica = Database::open_read_only(dir.path().join("db")).unwrap();
            replica.restore(&mut &*buf).unwrap();
            (dir, replica)
        };

        // First attempt: snapshot at sequence 1, restore lands at 2, the
        // master's first write (2) is refused.
        let mut buf = Vec::new();
        master.snapshot(&mut buf).unwrap();
        let (_dir, replica) = fresh_replica(&buf);
        write("a");
        let batch = batches.lock().unwrap().pop().unwrap();
        assert_eq!(batch.seq, 2);
        assert!(matches!(
            replica.apply_replicated(batch),
            Err(Error::Conflict(_))
        ));

        // The resync: a new snapshot (sequence 2, the refused write inside),
        // a fresh replica that adopts it, and the next batch is accepted.
        let mut buf = Vec::new();
        master.snapshot(&mut buf).unwrap();
        let (_dir, replica) = fresh_replica(&buf);
        write("b");
        let batch = batches.lock().unwrap().pop().unwrap();
        assert_eq!(batch.seq, 3);
        replica.apply_replicated(batch).unwrap();
        assert_eq!(replica.commit_seq().unwrap(), master.commit_seq().unwrap());
    }

    #[test]
    fn restore_refuses_a_non_empty_target() {
        let src_dir = tempfile::tempdir().unwrap();
        let src = Database::open(src_dir.path().join("db")).unwrap();
        let mut buf = Vec::new();
        src.snapshot(&mut buf).unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let dst = Database::open(dst_dir.path().join("db")).unwrap();
        {
            let mut w = dst.plane("startup").unwrap().write().unwrap();
            w.create_node(&["X"], Properties::new()).unwrap();
            w.commit().unwrap();
        }
        assert!(
            dst.restore(&mut buf.as_slice()).is_err(),
            "restore must refuse a non-empty target"
        );
    }
}
