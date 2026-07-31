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
        self.engine.with_read(|txn| {
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
        write_frame(&mut out, &Frame::Hnsw(registry.to_bytes(stats.seq)?))?;
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
        // exact sequence so the sidecars (stamped with it) stay valid.
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
            Ok::<(), Error>(())
        })?;

        let seq = stats.seq;
        // Persist + load the sidecars. Ids are preserved, so they match; loading
        // them into the live registries means this database (and its drop-time
        // save) stays coherent without a rebuild-from-KV.
        if let (Some(bytes), Some(path)) = (&hnsw, self.sidecar.as_deref()) {
            std::fs::write(path, bytes)?;
            if let Some(reg) = VectorRegistry::from_bytes(bytes, seq) {
                *self.indexes_mut() = reg;
            }
        }
        if let (Some(bytes), Some(path)) = (&bm25, self.keyword_sidecar.as_deref()) {
            std::fs::write(path, bytes)?;
            if let Some(reg) = KeywordRegistry::from_bytes(bytes, seq) {
                *self.keywords_mut() = reg;
            }
        }
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
        drop(src);

        // Restore into a fresh database.
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = Database::open(dst_dir.path().join("db")).unwrap();
        let rstats = dst.restore(&mut buf.as_slice()).unwrap();
        assert_eq!((rstats.nodes, rstats.edges), (2, 1));
        assert_eq!(dst.commit_seq().unwrap(), src_seq, "commit seq restored");

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
