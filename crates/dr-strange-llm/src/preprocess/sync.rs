//! Folding one commit into a plane (ROADMAP §11's watch mode).
//!
//! A digest reads a tree once; a watched repository changes in commits. This
//! module reconciles the files one commit touched against what the plane
//! already holds — **facts only, no model call ever**: the changed files run
//! through the same plugin router a digest uses.
//!
//! The shape is delete-then-reload, on the bulk fast path:
//!
//! 1. every node the plane attributes to an affected file is **deleted**
//!    (incident edges cascade), after snapshotting the edges that point *into*
//!    those nodes from files the commit did not touch — those are the
//!    untouched files' assertions, and this commit has no authority to drop
//!    them;
//! 2. the fresh facts are **bulk-loaded**: new and re-parsed nodes created in
//!    one batch, fact edges resolved within the batch first and against the
//!    plane for everything else. A fact key that still exists outside the
//!    affected set — a stand-in like `stdio.h` some other file already put
//!    there — is skipped rather than re-created, because `bulk_load` writes
//!    the key index unconditionally and would shadow the original;
//! 3. the snapshot is **re-attached by key**: an incoming edge whose target
//!    key was re-created lands on the new node; one whose target vanished has
//!    nothing to land on and is dropped — which is exactly what a call to a
//!    deleted function ought to do.
//!
//! One carve-over survives the reload: `_`-reserved and vector-valued
//! properties of a re-created key (provenance, embeddings — the digest
//! pipeline's, not the parser's) are copied onto the new node, so a watch
//! update does not strip the embedding that makes a node searchable.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use dr_strange_core::{BulkEdge, BulkNode, Database, Dir, PropValue, Properties};

use super::{Host, Plugins, route_paths, stamp_run};

/// What one [`sync_paths`] call did — the summary a watch loop logs.
#[derive(Debug, Default)]
pub struct SyncStats {
    pub nodes_deleted: usize,
    pub nodes_loaded: usize,
    /// Fact keys skipped because the plane already holds them outside the
    /// affected set (stand-ins another file's facts created).
    pub nodes_skipped: usize,
    pub edges_written: usize,
    /// Fact edges dropped because an endpoint key resolved to nothing.
    pub edges_dropped: usize,
    /// Incoming edges from untouched files re-attached after the reload.
    pub edges_reattached: usize,
    /// Prose the router produced and this sync deliberately did not spend a
    /// model call on — counted so a thin update is explainable.
    pub prose_chars: usize,
    pub notes: Vec<String>,
}

/// An incoming edge preserved across the delete: the untouched source node by
/// id, the affected target by key.
struct SavedEdge {
    src: dr_strange_core::NodeId,
    dst_key: String,
    ty: String,
    props: Properties,
}

/// The files one commit touched, repo-root-relative — matching what the
/// host serves and what plugins write into `file` props.
#[derive(Debug, Default, Clone)]
pub struct CommitDelta {
    /// Paths whose current content should be believed (added, modified,
    /// rename targets).
    pub changed: Vec<String>,
    /// Paths that no longer exist (including rename sources).
    pub deleted: Vec<String>,
}

/// Reconcile one commit's files into `plane_name`.
pub fn sync_paths(
    db: &Database,
    plane_name: &str,
    host: &dyn Host,
    delta: &CommitDelta,
    plugins: &Plugins,
    source: &str,
    run_id: &str,
) -> Result<SyncStats> {
    let mut stats = SyncStats::default();
    let (changed, deleted) = (&delta.changed, &delta.deleted);

    let mut facts =
        route_paths(host, changed.to_vec(), None, plugins).context("routing the commit's files")?;
    stamp_run(&mut facts, source, run_id);
    stats.prose_chars = facts.prose.chars().count();
    stats.notes = std::mem::take(&mut facts.report.notes);

    let plane = db.plane(plane_name)?;

    // Which existing nodes the commit speaks for: keyed by an affected path
    // itself (File/Module/Page nodes) or carrying it in `file` — the family
    // convention every plugin follows.
    let affected: BTreeSet<&str> = changed
        .iter()
        .chain(deleted.iter())
        .map(String::as_str)
        .collect();
    let mut old: BTreeMap<String, dr_strange_core::NodeRecord> = BTreeMap::new();
    for node in plane.query().scan_all().nodes()? {
        let Some(key) = node.external_key.clone() else {
            continue; // keyless nodes are a model's, never a parser's
        };
        let by_key = affected.contains(key.as_str());
        // `file` is the family convention; `path` is what a file-level node
        // (rust's module, whose key is the module path) carries instead.
        let by_file = ["file", "path"].iter().any(|p| {
            matches!(
                node.properties.get(*p).map(|d| &d.value),
                Some(PropValue::Str(f)) if affected.contains(f.as_str())
            )
        });
        if by_key || by_file {
            old.insert(key, node);
        }
    }

    // Snapshot the untouched files' assertions before the cascade takes them.
    let old_ids: BTreeSet<dr_strange_core::NodeId> = old.values().map(|n| n.id).collect();
    let mut saved: Vec<SavedEdge> = Vec::new();
    for (key, node) in &old {
        for n in plane.neighbors(node.id, Dir::In, None)? {
            if old_ids.contains(&n.node) {
                continue; // between affected nodes: the facts re-assert it
            }
            if let Some(edge) = plane.edge(n.edge)? {
                saved.push(SavedEdge {
                    src: edge.src,
                    dst_key: key.clone(),
                    ty: edge.ty,
                    props: edge.properties,
                });
            }
        }
    }

    // Delete + bulk-load in one transaction: no reader ever sees the affected
    // files half-gone.
    let mut txn = plane.write()?;
    for node in old.values() {
        txn.delete_node(node.id)?; // incident edges cascade
    }
    stats.nodes_deleted = old.len();

    let mut batch_keys: BTreeSet<&str> = BTreeSet::new();
    let mut loadable: Vec<&crate::digest::DigestNode> = Vec::new();
    for fact in &facts.nodes {
        // A key the plane holds *outside* the affected set is someone else's
        // node (a stand-in); re-loading it would shadow the original.
        if !old.contains_key(&fact.key) && plane.node_by_key(&fact.key)?.is_some() {
            stats.nodes_skipped += 1;
            continue;
        }
        if !batch_keys.insert(&fact.key) {
            continue; // intra-batch duplicate would reject the whole load
        }
        loadable.push(fact);
    }
    let label_slots: Vec<Vec<&str>> = loadable
        .iter()
        .map(|fact| {
            std::iter::once(fact.label.as_str())
                .chain(fact.extra_labels.iter().map(String::as_str))
                .collect()
        })
        .collect();
    let nodes: Vec<BulkNode> = loadable
        .iter()
        .zip(&label_slots)
        .map(|(fact, labels)| BulkNode {
            external_key: Some(&fact.key),
            labels,
            props: fact.props.clone(),
        })
        .collect();

    // An edge endpoint must resolve in the batch or in the plane — minus what
    // this transaction just deleted, which outside reads still show.
    let resolves = |key: &str, txn_deleted: &BTreeMap<String, dr_strange_core::NodeRecord>| {
        if batch_keys.contains(key) {
            return Ok::<bool, anyhow::Error>(true);
        }
        if txn_deleted.contains_key(key) {
            return Ok(false);
        }
        Ok(plane.node_by_key(key)?.is_some())
    };
    // A fact edge between two nodes the fold did NOT re-create — an unchanged
    // manifest's CONTAINS onto its re-parsed module, or any node the plane
    // holds without file attribution — still has its old copy standing, since
    // only affected nodes' edges cascaded. Re-asserting it would stack a
    // duplicate per fold, so it is skipped when the same (src, dst, type)
    // already exists.
    let already = |src: &str, dst: &str, ty: &str| -> Result<bool> {
        if batch_keys.contains(src) || batch_keys.contains(dst) {
            return Ok(false); // at least one endpoint is fresh: no old copy
        }
        let (Some(s), Some(d)) = (plane.node_by_key(src)?, plane.node_by_key(dst)?) else {
            return Ok(false);
        };
        Ok(plane
            .neighbors(s.id, Dir::Out, Some(ty))?
            .iter()
            .any(|n| n.node == d.id))
    };
    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let mut edges: Vec<BulkEdge> = Vec::new();
    for edge in &facts.edges {
        if !seen.insert((&edge.src, &edge.dst, &edge.ty)) {
            continue; // the same assertion twice in one batch is one fact
        }
        if already(&edge.src, &edge.dst, &edge.ty)? {
            continue;
        }
        if resolves(&edge.src, &old)? && resolves(&edge.dst, &old)? {
            edges.push(BulkEdge {
                src_key: &edge.src,
                dst_key: &edge.dst,
                ty: &edge.ty,
                props: edge.props.clone(),
            });
        } else {
            stats.edges_dropped += 1;
        }
    }

    stats.nodes_loaded = nodes.len();
    stats.edges_written = edges.len();
    txn.bulk_load(nodes, edges)?;
    txn.commit()?;

    // Re-attach and carry over in a second transaction: the re-created nodes
    // are only visible to reads once the load committed.
    let mut txn = plane.write()?;
    for edge in &saved {
        let Some(dst) = plane.node_by_key(&edge.dst_key)? else {
            continue; // the symbol vanished; the dangling assertion goes too
        };
        // The load may already have re-asserted this very edge (a fact edge
        // whose source lives outside the batch); reads here see the committed
        // load, so the copy is visible and the snapshot yields to it.
        if plane
            .neighbors(edge.src, Dir::Out, Some(&edge.ty))?
            .iter()
            .any(|n| n.node == dst.id)
        {
            continue;
        }
        txn.create_edge(edge.src, dst.id, &edge.ty, edge.props.clone())?;
        stats.edges_reattached += 1;
    }
    for (key, node) in &old {
        if !batch_keys.contains(key.as_str()) {
            continue;
        }
        let Some(fresh) = plane.node_by_key(key)? else {
            continue;
        };
        for (prop_key, prop) in &node.properties {
            let keep = (prop_key.starts_with('_') || matches!(prop.value, PropValue::Vector(_)))
                && !fresh.properties.contains_key(prop_key);
            if keep {
                txn.set_prop(fresh.id, prop_key, prop.clone())?;
            }
        }
    }
    txn.commit()?;

    Ok(stats)
}

/// Rebuild `plane_name` from the whole tree: drop it, re-create it, and fold
/// every file the host lists as one delta — `serve watch --force`.
///
/// One delta means one assemble over the full set, so cross-file resolution
/// matches a fresh digest's facts rather than a per-commit fold's. Facts
/// only, like every sync: embeddings and model prose are a digest's business
/// and return on the next one.
pub fn resync(
    db: &Database,
    plane_name: &str,
    host: &dyn Host,
    plugins: &Plugins,
    source: &str,
    run_id: &str,
) -> Result<SyncStats> {
    if let Ok(plane) = db.plane(plane_name) {
        let id = plane.id();
        db.drop_plane(id)?;
    }
    db.create_plane(plane_name, Properties::new())?;
    let delta = CommitDelta {
        changed: host.list("")?,
        deleted: Vec::new(),
    };
    sync_paths(db, plane_name, host, &delta, plugins, source, run_id)
}
