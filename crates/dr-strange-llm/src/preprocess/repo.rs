//! Digesting a repository's *history*, beside its code (ROADMAP §11).
//!
//! A checkout carries two sources of truth. The tree says what the code is
//! now; the repository says how it got there — who changed what, in what
//! order, on which branch, and which of those commits were later rewritten.
//! The file router reads the first. This module reads the second.
//!
//! ## Why it is not routed like everything else
//!
//! Every other handler is chosen by a file's extension, because that is what
//! its input is. History is not a file, so there is no extension to claim and
//! nothing for [`route_tree`](super::route_tree) to dispatch on. It is chosen
//! by the **shape of the source** instead: a digested directory with a git
//! directory in it is a repository, which is a fact the host can see rather
//! than a guess, and the plugin named [`REPO_PLUGIN`] is what reads one.
//!
//! That name is the declaration — the same kind of statement `--handler git`
//! makes, and reserved in the same way the official catalog reserves the
//! others. Nothing is guessed: with no such plugin installed, a digest simply
//! does not read history and says so.
//!
//! ## What it is allowed to see
//!
//! A [`Host`] rooted at the **git directory** and nothing else. That is a
//! *tighter* grant than the working tree every code plugin gets: it cannot
//! read a single source file, and the tree's plugins cannot read a single
//! object — `.git` is excluded from the ordinary walk, and always was.
//!
//! ## And why it lands in its own plane
//!
//! `<plane>_git`, beside the code plane, because the two answer different
//! questions and have different lifetimes. A code plane is a picture of the
//! tree *now* and is rewritten whenever a file changes; history only ever
//! grows. Folding one into the other would mean re-reading a thousand commits
//! every time a function moved, and every `MATCH (n)` over the code would walk
//! a graph mostly made of commits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dr_strange_core::{BulkEdge, BulkNode, Database, Dir, PropValue, Properties};

use super::{IgnorePolicy, Input, LocalFiles, Plugins, Preprocessed, stamp_provenance};
use crate::digest::{DigestEdge, DigestNode};

/// The plugin that reads a repository's history. Reserved: installing a plugin
/// under this name is what tells the host one is available.
pub const REPO_PLUGIN: &str = "git";

/// The suffix a history plane takes, beside the code plane it belongs to.
///
/// Core's, not a second copy: the surfaces that *read* a history plane (the
/// MCP tool, the CLI verb) need the same string as the one that writes it,
/// and two constants that must agree are one constant with a bug waiting.
pub use dr_strange_core::compact::{
    HISTORY_SUFFIX as PLANE_SUFFIX, history_plane_name as plane_name,
};

/// What `dir/.git` turned out to be.
pub enum GitDir {
    /// An ordinary repository: the directory to hand the plugin.
    Here(PathBuf),
    /// A `.git` *file* — a linked worktree or a submodule, whose real git
    /// directory is elsewhere on the filesystem. Named rather than ignored:
    /// history is genuinely unreadable from here, and silence would read as
    /// "this repository has none".
    Elsewhere(String),
    /// Not a repository, or not one at this directory.
    None,
}

/// Look for a repository *at* `dir` — not above it.
///
/// Deliberately no upward search. The digested directory is the whole of what
/// the operator pointed at and the whole of what the host will answer for;
/// finding a `.git` three levels up and reading it would quietly widen both.
pub fn git_dir(dir: &Path) -> GitDir {
    let dot = dir.join(".git");
    match std::fs::metadata(&dot) {
        Ok(m) if m.is_dir() => GitDir::Here(dot),
        Ok(_) => GitDir::Elsewhere(format!(
            "{} is a linked worktree or a submodule — its git directory is \
             outside the directory being digested, which is the whole of what \
             this host will read, so its history is not in this graph",
            dir.display()
        )),
        Err(_) => GitDir::None,
    }
}

/// The single path handed to the plugin as its input.
///
/// A repository is one input rather than a tree of them — one object store,
/// one set of refs, one reflog, each part of the answer depending on the rest
/// — so there is exactly one chunk, and everything else the plugin needs it
/// pulls through the host. `HEAD` is the entry point every git directory has.
const ENTRY: &str = "HEAD";

/// Read `dir`'s history through the installed [`REPO_PLUGIN`].
///
/// `Ok(None)` means there was nothing to read and nothing went wrong: not a
/// repository, or no such plugin installed. Both are ordinary, and neither is
/// worth failing a digest over — the caller says which one it was.
pub fn route_repository(dir: &Path, plugins: &Plugins) -> Result<Option<Preprocessed>> {
    let GitDir::Here(git) = git_dir(dir) else {
        return Ok(None);
    };
    let Some(plugin) = plugins
        .handlers
        .iter()
        .find(|p| p.manifest().name == REPO_PLUGIN)
    else {
        return Ok(None);
    };

    // Rooted at the git directory, and hiding nothing inside it: every default
    // this policy carries — skip dotfiles, honour `.gitignore`, skip `.git` —
    // is about reading a *working tree*, and every one of them would hide the
    // objects from the one plugin whose input they are.
    let host = LocalFiles::with_policy(
        &git,
        IgnorePolicy {
            gitignore: false,
            dockerignore: false,
            hidden: false,
            builtin_dirs: false,
            extra: Vec::new(),
        },
    )
    .with_context(|| format!("opening {}", git.display()))?;

    let mark = format!("{}@{}", plugin.manifest().name, plugin.manifest().version);
    let paths = [ENTRY.to_string()];
    let mut out = plugin
        .preprocess(&Input::Files { paths: &paths }, &host)
        .with_context(|| format!("reading the history of {}", dir.display()))?;
    stamp_provenance(&mut out, &mark);
    if out.report.handlers.is_empty() {
        let facts = out.nodes.len() + out.edges.len();
        out.report.handlers.push((mark, facts));
    }
    Ok(Some(out))
}

// ---- writing -------------------------------------------------------------

/// What [`write_history`] did.
#[derive(Debug, Default)]
pub struct WriteStats {
    pub nodes_created: usize,
    /// Nodes this read found again saying something different — a branch whose
    /// tip moved — updated in place.
    pub nodes_patched: usize,
    /// Keys the plane holds on a node this plugin does not own. Left alone:
    /// stealing a key would be worse than declining to write.
    pub nodes_skipped: usize,
    pub edges_created: usize,
    /// Stale edges removed — the `TIP` a branch no longer has.
    pub edges_deleted: usize,
    /// Edges whose endpoint resolved to nothing.
    pub edges_dropped: usize,
}

/// Write history facts into `plane_name`, which must already exist.
///
/// History is **append-mostly**, and this is shaped by which parts of it can
/// change at all:
///
/// * A commit is immutable — its sha *is* its content — so one already in the
///   plane is left exactly as it is and its `PARENT` edges are never rewritten.
///   That is what makes re-digesting cheap: only commits made since the last
///   run are new.
/// * A branch, a tag or a rebase is a moving pointer. Those are patched in
///   place and their outgoing edges re-asserted, so a branch whose tip advanced
///   ends with one `TIP` edge rather than one per digest.
/// * A key held by a node this plugin does not own is skipped, not overwritten.
pub fn write_history(db: &Database, plane_name: &str, facts: &Preprocessed) -> Result<WriteStats> {
    let mut stats = WriteStats::default();
    let plane = db
        .plane(plane_name)
        .with_context(|| format!("no such plane '{plane_name}'"))?;

    let mut stored: BTreeMap<String, dr_strange_core::NodeRecord> = BTreeMap::new();
    for node in plane.query().scan_all().nodes()? {
        if let Some(key) = node.external_key.clone() {
            stored.insert(key, node);
        }
    }
    let ours = |node: &dr_strange_core::NodeRecord| {
        matches!(
            node.properties.get("_generated_by").map(|d| &d.value),
            Some(PropValue::Str(g)) if g.split('@').next() == Some(REPO_PLUGIN)
        )
    };

    let mut creates: Vec<&DigestNode> = Vec::new();
    let mut patches: Vec<(dr_strange_core::NodeId, &DigestNode)> = Vec::new();
    // The sources whose outgoing edges this write re-asserts: everything that
    // can move. A commit is not among them.
    let mut moving: BTreeMap<&str, dr_strange_core::NodeId> = BTreeMap::new();
    let mut skipped: BTreeSet<&str> = BTreeSet::new();

    for fact in &facts.nodes {
        match stored.get(&fact.key) {
            None => creates.push(fact),
            Some(node) if !ours(node) => {
                stats.nodes_skipped += 1;
                skipped.insert(fact.key.as_str());
            }
            Some(node) => {
                if fact.label != COMMIT {
                    moving.insert(fact.key.as_str(), node.id);
                }
                if content_differs(&fact.props, &node.properties) {
                    patches.push((node.id, fact));
                }
            }
        }
    }

    let created: BTreeSet<&str> = creates.iter().map(|n| n.key.as_str()).collect();
    let resolves = |key: &str| created.contains(key) || stored.contains_key(key);

    // What the moving pointers assert today, so an edge that did not change is
    // left exactly where it is. Rewriting every one of them each digest would
    // work and would churn the graph for nothing — and would make "3 edges
    // replaced" stop meaning anything.
    let by_id: BTreeMap<dr_strange_core::NodeId, &str> = stored
        .iter()
        .map(|(key, node)| (node.id, key.as_str()))
        .collect();
    let mut standing: BTreeMap<(&str, String, &str), (dr_strange_core::EdgeId, Properties)> =
        BTreeMap::new();
    for (key, id) in &moving {
        for n in plane.neighbors(*id, Dir::Out, None)? {
            let Some(dst) = by_id.get(&n.node) else {
                continue;
            };
            if let Some(edge) = plane.edge(n.edge)? {
                standing.insert((key, edge.ty, dst), (n.edge, edge.properties));
            }
        }
    }

    let mut wanted: Vec<&DigestEdge> = Vec::new();
    let mut edge_deletes: Vec<dr_strange_core::EdgeId> = Vec::new();
    let mut seen: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    for edge in &facts.edges {
        // Only edges out of something this write is creating or re-asserting:
        // a commit already in the plane keeps the parents it was written with,
        // because they cannot have changed.
        let mine = created.contains(edge.src.as_str()) || moving.contains_key(edge.src.as_str());
        if !mine || skipped.contains(edge.src.as_str()) {
            continue;
        }
        if !seen.insert((&edge.src, &edge.ty, &edge.dst)) {
            continue;
        }
        if !(resolves(&edge.src) && resolves(&edge.dst)) {
            stats.edges_dropped += 1;
            continue;
        }
        match standing.remove(&(edge.src.as_str(), edge.ty.clone(), edge.dst.as_str())) {
            // Asserted and standing, saying the same thing: nothing to do.
            Some((_, props)) if !content_differs(&edge.props, &props) => {}
            Some((id, _)) => {
                edge_deletes.push(id);
                wanted.push(edge);
            }
            None => wanted.push(edge),
        }
    }
    // Whatever a moving pointer still has that no fact asserts — the `TIP` a
    // branch moved off — goes.
    for (id, _) in standing.into_values() {
        edge_deletes.push(id);
    }

    let mut txn = plane.write()?;
    for id in edge_deletes {
        if plane.edge(id)?.is_some() {
            txn.delete_edge(id)?;
            stats.edges_deleted += 1;
        }
    }
    for (id, fact) in &patches {
        let old = &stored[fact.key.as_str()];
        for (key, _) in &old.properties {
            if !key.starts_with('_') && !fact.props.contains_key(key) {
                txn.remove_prop(*id, key)?;
            }
        }
        for (key, desc) in &fact.props {
            if old.properties.get(key).map(|o| &o.value) != Some(&desc.value) {
                txn.set_prop(*id, key, desc.clone())?;
            }
        }
    }
    stats.nodes_patched = patches.len();

    let label_slots: Vec<Vec<&str>> = creates
        .iter()
        .map(|n| {
            std::iter::once(n.label.as_str())
                .chain(n.extra_labels.iter().map(String::as_str))
                .collect()
        })
        .collect();
    let nodes: Vec<BulkNode> = creates
        .iter()
        .zip(&label_slots)
        .map(|(n, labels)| BulkNode {
            external_key: Some(&n.key),
            labels: labels.as_slice(),
            props: n.props.clone(),
        })
        .collect();
    let edges: Vec<BulkEdge> = wanted
        .iter()
        .map(|e| BulkEdge {
            src_key: &e.src,
            dst_key: &e.dst,
            ty: &e.ty,
            props: e.props.clone(),
        })
        .collect();
    stats.nodes_created = nodes.len();
    stats.edges_created = edges.len();
    txn.bulk_load(nodes, edges)?;
    txn.commit()?;
    Ok(stats)
}

/// The label an immutable fact carries — the one kind of node whose edges are
/// never rewritten. Named here rather than imported: the vocabulary belongs to
/// the plugin, and the host knows exactly this much of it.
const COMMIT: &str = "Commit";

/// Whether a fact says something different from what the plane holds, ignoring
/// the `_`-reserved provenance the pipeline restamps on every run.
fn content_differs(fact: &Properties, stored: &Properties) -> bool {
    fn content(p: &Properties) -> BTreeMap<&str, &PropValue> {
        p.iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, d)| (k.as_str(), &d.value))
            .collect()
    }
    content(fact) != content(stored)
}
