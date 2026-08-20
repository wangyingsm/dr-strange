//! Folding one commit into a plane (ROADMAP §11's watch mode).
//!
//! A digest reads a tree once; a watched repository changes in commits. This
//! module reconciles the plane against the tree — **facts only, no model call
//! ever**: files run through the same plugin router a digest uses.
//!
//! **Every fold routes the whole tree, and applies only the diff.** Cross-file
//! resolution is global by nature — a call in `a.rs` binds to a declaration
//! in `b.rs` — so assembling just the files one commit touched produces facts
//! a whole-tree digest would not (stand-ins where declarations exist, missed
//! rebinds where they moved). Routing everything makes a fold-built plane
//! converge on the digest-built one *by construction*; the commit's delta
//! only tells the caller what to log. The price is parse time proportional to
//! the tree, paid off-thread by the watch loop; a partial-parse cache behind
//! the plugin contract can cut it later without changing what lands.
//!
//! What lands is a diff against the plane's **parser-owned** nodes — those
//! stamped `_generated_by` by one of the routed plugins. Model-extracted
//! entities, document digests and their links are never touched:
//!
//! 1. a parser node whose facts vanished is **deleted** (incident edges
//!    cascade — a call to a deleted function ought to dangle and drop);
//! 2. a node whose facts changed is **patched in place**: content properties
//!    written through, provenance restamped, `_`-reserved and vector-valued
//!    properties (embeddings, pipeline bookkeeping) left standing — and its
//!    incident edges with them;
//! 3. a node whose labels changed is **replaced** (delete + re-create), with
//!    incoming edges from unowned sources snapshotted and re-attached by key,
//!    and `_`/vector properties carried over;
//! 4. new facts are **bulk-loaded**; a fact key the plane holds on an unowned
//!    node (a document's, a model's) is skipped rather than shadowed;
//! 5. fact edges are diffed the same way, between parser-owned endpoints:
//!    missing ones created, no-longer-asserted ones deleted, changed ones
//!    replaced. An unchanged node's unchanged edges are never rewritten.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use dr_strange_core::{BulkEdge, BulkNode, Database, Dir, EdgeId, PropDesc, PropValue, Properties};

use super::{Host, Plugins, route_paths, stamp_run};

/// What one [`sync_paths`] call did — the summary a watch loop logs.
#[derive(Debug, Default)]
pub struct SyncStats {
    pub nodes_deleted: usize,
    pub nodes_loaded: usize,
    /// Nodes whose facts changed and were updated in place, embeddings and
    /// incident edges left standing.
    pub nodes_patched: usize,
    /// Fact keys skipped because the plane holds them on a node no parser
    /// owns (a document's or a model's — shadowing it would steal the key).
    pub nodes_skipped: usize,
    pub edges_written: usize,
    /// Fact edges dropped because an endpoint key resolved to nothing.
    pub edges_dropped: usize,
    /// Stale parser edges removed because no fact asserts them any more.
    pub edges_deleted: usize,
    /// Incoming edges from unowned sources re-attached after a replace.
    pub edges_reattached: usize,
    /// Prose the router produced and this sync deliberately did not spend a
    /// model call on — counted so a thin update is explainable.
    pub prose_chars: usize,
    pub notes: Vec<String>,
}

/// An incoming edge preserved across a replace: the unowned source node by
/// id, the replaced target by key.
struct SavedEdge {
    src: dr_strange_core::NodeId,
    dst_key: String,
    ty: String,
    props: Properties,
}

/// The files one commit touched, repo-root-relative — what the caller logs
/// and records its sync point against. The fold itself reconciles the whole
/// tree (see the module docs for why).
#[derive(Debug, Default, Clone)]
pub struct CommitDelta {
    /// Paths whose current content should be believed (added, modified,
    /// rename targets).
    pub changed: Vec<String>,
    /// Paths that no longer exist (including rename sources).
    pub deleted: Vec<String>,
}

/// A property map reduced to what the parser actually said: `_`-reserved
/// entries are pipeline provenance (`_run` changes every fold) and vectors
/// are the embedder's — neither makes two facts different.
fn content_of(props: &Properties) -> BTreeMap<&str, &PropValue> {
    props
        .iter()
        .filter(|(k, d)| !k.starts_with('_') && !matches!(d.value, PropValue::Vector(_)))
        .map(|(k, d)| (k.as_str(), &d.value))
        .collect()
}

/// Reconcile the tree behind `host` into `plane_name`.
pub fn sync_paths(
    db: &Database,
    plane_name: &str,
    host: &dyn Host,
    _delta: &CommitDelta,
    plugins: &Plugins,
    source: &str,
    run_id: &str,
) -> Result<SyncStats> {
    let mut stats = SyncStats::default();

    let all = host.list("").context("listing the tree")?;
    let mut facts = route_paths(host, all, None, plugins).context("routing the tree")?;
    stamp_run(&mut facts, source, run_id);
    stats.prose_chars = facts.prose.chars().count();
    stats.notes = std::mem::take(&mut facts.report.notes);

    let plane = db.plane(plane_name)?;

    // The plane's parser-owned nodes: keyed, and stamped by a plugin this
    // router carries (matched by name, so a version bump still owns its
    // older nodes). Everything else — model entities, document pages — is
    // outside this fold's authority.
    let manifests = plugins.manifests();
    let owners: BTreeSet<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
    let mut stored: BTreeMap<String, dr_strange_core::NodeRecord> = BTreeMap::new();
    for node in plane.query().scan_all().nodes()? {
        let Some(key) = node.external_key.clone() else {
            continue; // keyless nodes are a model's, never a parser's
        };
        let owned = matches!(
            node.properties.get("_generated_by").map(|d| &d.value),
            Some(PropValue::Str(g)) if owners.contains(g.split('@').next().unwrap_or(g))
        );
        if owned {
            stored.insert(key, node);
        }
    }

    // The tree's facts, one node per key.
    let mut fresh: BTreeMap<&str, &crate::digest::DigestNode> = BTreeMap::new();
    for fact in &facts.nodes {
        fresh.entry(fact.key.as_str()).or_insert(fact);
    }

    // ---- classify nodes ---------------------------------------------------
    let mut creates: Vec<&crate::digest::DigestNode> = Vec::new(); // incl. replaces
    let mut patches: Vec<(dr_strange_core::NodeId, &crate::digest::DigestNode)> = Vec::new();
    let mut replaced: BTreeSet<&str> = BTreeSet::new();
    let mut deleted: BTreeSet<&str> = BTreeSet::new();

    for (key, fact) in &fresh {
        match stored.get(*key) {
            None => {
                if plane.node_by_key(key)?.is_some() {
                    stats.nodes_skipped += 1; // an unowned node holds the key
                } else {
                    creates.push(fact);
                }
            }
            Some(node) => {
                let mut want: Vec<&str> = std::iter::once(fact.label.as_str())
                    .chain(fact.extra_labels.iter().map(String::as_str))
                    .collect();
                want.sort_unstable();
                let mut have: Vec<&str> = node.labels.iter().map(String::as_str).collect();
                have.sort_unstable();
                if want != have {
                    replaced.insert(key);
                    creates.push(fact);
                } else if content_of(&fact.props) != content_of(&node.properties) {
                    patches.push((node.id, fact));
                }
            }
        }
    }
    for key in stored.keys() {
        if !fresh.contains_key(key.as_str()) {
            deleted.insert(key);
        }
    }

    // ---- diff the parser edges -------------------------------------------
    // The stored universe: edges between parser-owned endpoints. A model's
    // link *into* a parser node has an unowned source and is never here.
    let ids: BTreeMap<dr_strange_core::NodeId, &str> =
        stored.iter().map(|(k, n)| (n.id, k.as_str())).collect();
    let mut standing: BTreeMap<(String, String, String), (EdgeId, Properties)> = BTreeMap::new();
    for (key, node) in &stored {
        for n in plane.neighbors(node.id, Dir::Out, None)? {
            let Some(dst_key) = ids.get(&n.node) else {
                continue;
            };
            if let Some(edge) = plane.edge(n.edge)? {
                standing.insert(
                    (key.clone(), edge.ty, dst_key.to_string()),
                    (n.edge, edge.properties),
                );
            }
        }
    }

    let batch_keys: BTreeSet<&str> = creates.iter().map(|f| f.key.as_str()).collect();
    // An endpoint resolves in the batch, or on a plane node this fold is not
    // about to delete.
    let resolves = |key: &str| -> Result<bool> {
        if batch_keys.contains(key) {
            return Ok(true);
        }
        if deleted.contains(key) {
            return Ok(false);
        }
        Ok(plane.node_by_key(key)?.is_some())
    };

    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    let mut edge_creates: Vec<&crate::digest::DigestEdge> = Vec::new();
    let mut edge_deletes: Vec<EdgeId> = Vec::new();
    for edge in &facts.edges {
        if !seen.insert((&edge.src, &edge.dst, &edge.ty)) {
            continue; // the same assertion twice in one batch is one fact
        }
        match standing.remove(&(edge.src.clone(), edge.ty.clone(), edge.dst.clone())) {
            // Asserted and standing with the same content: nothing to do.
            Some((_, props)) if content_of(&props) == content_of(&edge.props) => {}
            // Standing but different (a call site moved lines, a resolution
            // strategy changed): replace it.
            Some((id, _)) => {
                edge_deletes.push(id);
                edge_creates.push(edge);
            }
            None => edge_creates.push(edge),
        }
    }
    // Whatever still stands was asserted by no fact — unless its endpoint is
    // being deleted or replaced, in which case the node cascade owns it.
    for ((src, _, dst), (id, _)) in &standing {
        let cascades = [src, dst]
            .into_iter()
            .any(|k| deleted.contains(k.as_str()) || replaced.contains(k.as_str()));
        if !cascades {
            edge_deletes.push(*id);
            stats.edges_deleted += 1;
        }
    }

    // Snapshot unowned incoming edges of replaced nodes before the cascade.
    let mut saved: Vec<SavedEdge> = Vec::new();
    for key in &replaced {
        let node = &stored[*key];
        for n in plane.neighbors(node.id, Dir::In, None)? {
            if ids.contains_key(&n.node) {
                continue; // parser-owned source: the edge diff owns it
            }
            if let Some(edge) = plane.edge(n.edge)? {
                saved.push(SavedEdge {
                    src: edge.src,
                    dst_key: key.to_string(),
                    ty: edge.ty,
                    props: edge.properties,
                });
            }
        }
    }

    // ---- apply, atomically ------------------------------------------------
    let mut txn = plane.write()?;
    for key in deleted.iter().chain(replaced.iter()) {
        txn.delete_node(stored[*key].id)?; // incident edges cascade
    }
    stats.nodes_deleted = deleted.len() + replaced.len();

    for (id, fact) in &patches {
        // Content written through — the parser's word replaces the old —
        // then provenance restamped. `_`-reserved extras the pipeline added
        // (embedding bookkeeping) and vectors are left standing.
        let old = &stored[fact.key.as_str()];
        for (k, d) in &old.properties {
            let stale = !k.starts_with('_')
                && !matches!(d.value, PropValue::Vector(_))
                && !fact.props.contains_key(k);
            if stale {
                txn.remove_prop(*id, k)?;
            }
        }
        for (k, d) in &fact.props {
            if old.properties.get(k).map(|o| &o.value) != Some(&d.value) {
                txn.set_prop(*id, k, d.clone())?;
            }
        }
    }
    stats.nodes_patched = patches.len();

    for id in edge_deletes {
        if plane.edge(id)?.is_some() {
            txn.delete_edge(id)?;
        }
    }

    let label_slots: Vec<Vec<&str>> = creates
        .iter()
        .map(|fact| {
            std::iter::once(fact.label.as_str())
                .chain(fact.extra_labels.iter().map(String::as_str))
                .collect()
        })
        .collect();
    let nodes: Vec<BulkNode> = creates
        .iter()
        .zip(&label_slots)
        .map(|(fact, labels)| BulkNode {
            external_key: Some(&fact.key),
            labels,
            props: fact.props.clone(),
        })
        .collect();
    let mut edges: Vec<BulkEdge> = Vec::new();
    for edge in edge_creates {
        if resolves(&edge.src)? && resolves(&edge.dst)? {
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
    for key in &replaced {
        let node = &stored[*key];
        let Some(new) = plane.node_by_key(key)? else {
            continue;
        };
        for (prop_key, prop) in &node.properties {
            let keep = (prop_key.starts_with('_') || matches!(prop.value, PropValue::Vector(_)))
                && !new.properties.contains_key(prop_key);
            if keep {
                txn.set_prop(new.id, prop_key, prop.clone())?;
            }
        }
    }
    txn.commit()?;

    Ok(stats)
}

/// Rebuild `plane_name` from scratch: drop it, re-create it, and reconcile
/// the whole tree into the empty plane — `serve watch --force`.
///
/// [`sync_paths`] already routes the whole tree, so this differs only in
/// starting clean: embeddings, model prose and anything else a digest added
/// are dropped with the plane and return on the next digest/vectorize.
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
    // Created already carrying the marker, in the same call: a plane that is
    // empty *and* silent about why is the window this closes, so there must
    // not be an instant where one exists without the other. Cleared below only
    // on success — a rebuild that dies halfway leaves a plane that really is
    // incomplete, and the marker is then exactly the right thing to find.
    let mut props = Properties::new();
    props.insert(
        dr_strange_core::compact::REBUILDING_PROP.into(),
        PropDesc::described(
            "unix time this plane's rebuild started; absent once it finished",
            PropValue::Int(now_unix()),
        ),
    );
    db.create_plane(plane_name, props)?;
    let stats = sync_paths(
        db,
        plane_name,
        host,
        &CommitDelta::default(),
        plugins,
        source,
        run_id,
    )?;
    let plane = db.plane(plane_name)?;
    let mut props = plane.properties()?;
    props.remove(dr_strange_core::compact::REBUILDING_PROP);
    plane.set_properties(props)?;
    Ok(stats)
}

/// Seconds since the epoch, or 0 on a clock before it — the marker is a
/// human-readable "how long has this been going", not an ordering key.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
