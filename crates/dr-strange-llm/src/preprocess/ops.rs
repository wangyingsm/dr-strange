//! Where an ops reader's facts land: a plane of their own, `<name>_ops`.
//!
//! The Compose, Kubernetes and Terraform readers are routed by the ordinary
//! digest — a compose file by its name, a `.tf` by its extension — so their
//! facts arrive mixed in with the code's. They do not belong there. A workload
//! and a function answer different questions, and a chart changes on a
//! different cadence from the code it deploys, so re-reading one must not
//! invalidate the other (`arch/10-ops.md` §2).
//!
//! Nothing has to ask for this. An ops file that is in the tree is read
//! because it is there, and the split is made on the one thing every fact
//! already carries: the plugin that produced it.

use std::collections::BTreeSet;

use dr_strange_core::{PlaneHandle, PropValue, Properties, Result};

use super::Preprocessed;
use crate::digest::DigestResult;

/// The readers whose facts are the ops graph (`arch/10-ops.md` §1).
///
/// Plugin names rather than labels: a plugin owns its vocabulary, and
/// `Namespace` means one thing from the Kubernetes reader and another from a
/// language parser that borrows the word.
pub const OPS_PLUGINS: &[&str] = &["compose", "k8s", "terraform", "prometheus"];

/// The suffix an ops plane takes, beside the code plane it belongs to.
///
/// Core's, not a second copy — the same reason the history suffix lives there:
/// the surfaces that *read* an ops plane need the string that writes it.
pub use dr_strange_core::compact::{OPS_SUFFIX as PLANE_SUFFIX, ops_plane_name as plane_name};

/// Whether a fact came from one of the ops readers.
///
/// `_generated_by` is `name@version`, stamped onto every fact a handler
/// produced, so this asks the handler and never guesses from a label.
pub fn from_ops_reader(props: &Properties) -> bool {
    matches!(
        props.get("_generated_by").map(|d| &d.value),
        Some(PropValue::Str(stamp))
            if stamp.split('@').next().is_some_and(|name| OPS_PLUGINS.contains(&name))
    )
}

/// How many of a digest's facts belong to the ops plane — what a dry run says
/// before anything is written.
pub fn ops_facts(result: &DigestResult) -> (usize, usize) {
    (
        result
            .nodes
            .iter()
            .filter(|n| from_ops_reader(&n.props))
            .count(),
        result
            .edges
            .iter()
            .filter(|e| from_ops_reader(&e.props))
            .count(),
    )
}

/// Move the ops readers' facts out of `result` into a result of their own.
///
/// Edges go by their own stamp rather than by their endpoints: an ops reader's
/// edges join its own nodes, and a model-proposed edge onto one of them would
/// be a guess about a topology nobody asked it to read. Such an edge stays
/// behind and is dropped as dangling, which is what the code plane should do
/// with it.
pub fn take_ops(result: &mut DigestResult) -> DigestResult {
    let (nodes, code): (Vec<_>, Vec<_>) = std::mem::take(&mut result.nodes)
        .into_iter()
        .partition(|n| from_ops_reader(&n.props));
    result.nodes = code;
    let (edges, code): (Vec<_>, Vec<_>) = std::mem::take(&mut result.edges)
        .into_iter()
        .partition(|e| from_ops_reader(&e.props));
    result.edges = code;
    DigestResult {
        nodes,
        edges,
        ..Default::default()
    }
}

/// Drop the ops edges whose endpoints this plane will not hold, and say how
/// many.
///
/// A fold already does this (`sync::loadable_edges`): an edge naming a key
/// that resolved to nothing is counted, not written. The digest path instead
/// hands every edge to `bulk_load`, which refuses an unknown endpoint and
/// takes the whole ingest down with it — so one ops reader naming another's
/// key would make a digest fail on a tree that is merely incomplete. That is
/// the ordinary case here, not an error: a scrape target names the service it
/// scrapes, and whether this tree declares that service is exactly what the
/// edge was asking.
pub fn drop_dangling(ops: &mut DigestResult, plane: &PlaneHandle<'_>) -> Result<usize> {
    let minted: BTreeSet<&str> = ops.nodes.iter().map(|n| n.key.as_str()).collect();
    let holds = |key: &str| -> Result<bool> {
        Ok(minted.contains(key) || plane.node_by_key(key)?.is_some())
    };
    let before = ops.edges.len();
    let mut keep = Vec::with_capacity(before);
    for edge in std::mem::take(&mut ops.edges) {
        if holds(&edge.src)? && holds(&edge.dst)? {
            keep.push(edge);
        }
    }
    ops.edges = keep;
    Ok(before - ops.edges.len())
}

/// The same split over a fold's routed facts, which carry prose and a report
/// the ops side has no use for.
///
/// Returned as a whole `Preprocessed` so that the reconciliation an ops plane
/// gets is the one a code plane gets — the same writer, the same deletes when
/// a manifest leaves the tree.
pub fn take_ops_facts(facts: &mut Preprocessed) -> Preprocessed {
    let (nodes, code): (Vec<_>, Vec<_>) = std::mem::take(&mut facts.nodes)
        .into_iter()
        .partition(|n| from_ops_reader(&n.props));
    facts.nodes = code;
    let (edges, code): (Vec<_>, Vec<_>) = std::mem::take(&mut facts.edges)
        .into_iter()
        .partition(|e| from_ops_reader(&e.props));
    facts.edges = code;
    Preprocessed {
        nodes,
        edges,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use dr_strange_core::PropDesc;

    use super::*;
    use crate::digest::{DigestEdge, DigestNode};

    fn stamped(by: &str) -> Properties {
        let mut props = Properties::new();
        props.insert(
            "_generated_by".into(),
            PropDesc::new(PropValue::Str(by.to_string())),
        );
        props
    }

    fn node(key: &str, by: &str) -> DigestNode {
        DigestNode {
            key: key.to_string(),
            label: "Thing".into(),
            extra_labels: Vec::new(),
            props: stamped(by),
        }
    }

    fn edge(src: &str, dst: &str, by: &str) -> DigestEdge {
        DigestEdge {
            src: src.to_string(),
            dst: dst.to_string(),
            ty: "USES_IMAGE".into(),
            props: stamped(by),
        }
    }

    fn mixed() -> DigestResult {
        DigestResult {
            nodes: vec![
                node("prod/Deployment/api", "k8s@1+abc"),
                node("crate::main", "rust@3+def"),
                node("aws_instance.web", "terraform@1+ghi"),
                node("svc", "compose@1+jkl"),
            ],
            edges: vec![
                edge("prod/Deployment/api", "api:latest", "k8s@1+abc"),
                edge("crate::main", "crate::run", "rust@3+def"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn every_ops_reader_is_taken_and_nothing_else_is() {
        let mut result = mixed();
        let (nodes, edges) = ops_facts(&result);
        assert_eq!((nodes, edges), (3, 1));

        let ops = take_ops(&mut result);
        assert_eq!(
            ops.nodes.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(),
            ["prod/Deployment/api", "aws_instance.web", "svc"]
        );
        assert_eq!(ops.edges.len(), 1);
        assert_eq!(
            result
                .nodes
                .iter()
                .map(|n| n.key.as_str())
                .collect::<Vec<_>>(),
            ["crate::main"]
        );
        assert_eq!(result.edges.len(), 1);
    }

    /// A fact with no stamp is a model's, and a model reads prose rather than
    /// manifests: it stays with the code.
    #[test]
    fn an_unstamped_fact_is_not_an_ops_fact() {
        assert!(!from_ops_reader(&Properties::new()));
        assert!(!from_ops_reader(&stamped("k8s-ish@1+abc")));
        assert!(from_ops_reader(&stamped("k8s@1+abc")));
    }

    #[test]
    fn the_plane_sits_beside_the_code_plane() {
        assert_eq!(plane_name("myrepo"), "myrepo_ops");
        assert_eq!(PLANE_SUFFIX, "_ops");
    }

    /// One reader naming another's key is the ordinary case, and a tree that
    /// declares only half of it must still digest — the end-to-end run that
    /// did not: `bulk edge endpoint 'prod/Service/api' not found`.
    #[test]
    fn an_edge_onto_a_key_the_tree_lacks_is_dropped_not_fatal() {
        use dr_strange_core::Database;

        let db = Database::in_memory().unwrap();
        db.create_plane("ops", Properties::new()).unwrap();
        let plane = db.plane("ops").unwrap();
        {
            let mut txn = plane.write().unwrap();
            txn.create_node_with_key("already/here", &["Service"], Properties::new())
                .unwrap();
            txn.commit().unwrap();
        }

        let mut ops = DigestResult {
            nodes: vec![node("cfg/job/api", "prometheus@1+abc")],
            edges: vec![
                // Both ends in hand.
                edge("cfg/job/api", "cfg/job/api", "prometheus@1+abc"),
                // One end already in the plane.
                edge("cfg/job/api", "already/here", "prometheus@1+abc"),
                // And one end nowhere at all.
                edge("cfg/job/api", "prod/Service/api", "prometheus@1+abc"),
            ],
            ..Default::default()
        };

        assert_eq!(drop_dangling(&mut ops, &plane).unwrap(), 1);
        assert_eq!(ops.edges.len(), 2);
        assert!(ops.edges.iter().all(|e| e.dst != "prod/Service/api"));
    }
}
