//! The join a scrape target cannot make for itself.
//!
//! The Prometheus reader sees one configuration file. It writes a
//! `ScrapeTarget` for every address a job declares, and draws the `SCRAPES`
//! edge only where the address *is* a key: `api.prod.svc.cluster.local` is
//! `prod/Service/api` and nothing else. A bare `api:9090` — a Compose service,
//! a short-name Kubernetes target — names no namespace, so the reader draws
//! nothing rather than guess one.
//!
//! Here the whole plane is visible, which is what that parser lacked. The host
//! name is matched against what the other ops readers minted, and what is
//! found is marked for what it is: one match is an edge carrying
//! `matched_by: name` and `confidence: low`; several are candidates recorded
//! on the target with no edge, because choosing between them would be the
//! guess the reader refused to make.

use std::collections::{BTreeMap, BTreeSet};

use dr_strange_core::{PlaneHandle, PropDesc, PropValue, Properties, Result};

use crate::digest::{DigestEdge, DigestNode};

/// The label the Prometheus reader mints a declared address under.
const SCRAPE_TARGET: &str = "ScrapeTarget";

/// The edge from an address to whatever answers at it.
const SCRAPES: &str = "SCRAPES";

/// What a target's host may name: a stable address, or the thing that runs
/// behind one (`arch/10-ops.md` §3).
const ADDRESSABLE: &[&str] = &["Service", "Workload"];

/// What the join found, for the note a fold leaves behind.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Joined {
    /// Targets joined to exactly one node, by name.
    pub matched: usize,
    /// Targets whose host several nodes answer to. Left unjoined, with the
    /// candidates recorded.
    pub ambiguous: usize,
    /// Targets nothing in the plane answers to — an external host, an IP, or a
    /// service this tree does not declare.
    pub unmatched: usize,
}

impl Joined {
    /// Whether anything happened worth saying.
    pub fn any(&self) -> bool {
        self.matched + self.ambiguous + self.unmatched > 0
    }
}

/// Join the scrape targets in `nodes` to what the facts and the plane hold.
///
/// `plane` is the ops plane as it stands — `None` before it exists. It is read
/// rather than indexed, for the reason `deployed` scans images: an ops plane
/// holds hundreds of services, not millions.
pub fn join(
    nodes: &mut [DigestNode],
    edges: &mut Vec<DigestEdge>,
    plane: Option<&PlaneHandle<'_>>,
) -> Result<Joined> {
    let targets: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| is_a(n, SCRAPE_TARGET))
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return Ok(Joined::default());
    }
    // Already drawn by the reader, from an address that derived a key.
    let joined: BTreeSet<&str> = edges
        .iter()
        .filter(|e| e.ty == SCRAPES)
        .map(|e| e.src.as_str())
        .collect();
    let unjoined: Vec<usize> = targets
        .into_iter()
        .filter(|i| !joined.contains(nodes[*i].key.as_str()))
        .collect();
    if unjoined.is_empty() {
        return Ok(Joined::default());
    }

    let by_name = addressable(nodes, plane)?;
    // Owned: the ambiguous arm below writes back into `nodes`.
    let keys: BTreeSet<String> = nodes.iter().map(|n| n.key.clone()).collect();
    let mut found = Joined::default();
    let mut edged = Vec::new();
    for i in unjoined {
        // An address that names a key answers for itself, if this tree holds
        // what it names. Nothing is guessed either way: an address naming a
        // service no manifest here declares is simply unmatched.
        if let Some(named) = prop_str(&nodes[i].props, "names").map(str::to_string) {
            if keys.contains(named.as_str()) || holds(plane, &named)? {
                found.matched += 1;
                edged.push((nodes[i].key.clone(), named, by_address_props()));
            } else {
                found.unmatched += 1;
            }
            continue;
        }
        // Owned, because the ambiguous arm writes back to the same node.
        let Some(host) = prop_str(&nodes[i].props, "host").map(str::to_string) else {
            continue;
        };
        match by_name
            .get(&host)
            .map(|keys| keys.iter().collect::<Vec<_>>())
        {
            None => found.unmatched += 1,
            Some(keys) if keys.len() == 1 => {
                found.matched += 1;
                edged.push((nodes[i].key.clone(), keys[0].clone(), by_name_props()));
            }
            Some(keys) => {
                found.ambiguous += 1;
                let candidates = keys.iter().map(|k| PropValue::Str((*k).clone())).collect();
                nodes[i].props.insert(
                    "candidates".into(),
                    PropDesc::described(
                        "the ops nodes this host names, where more than one does — \
                         no edge is drawn, because choosing between them would be a guess",
                        PropValue::List(candidates),
                    ),
                );
            }
        }
    }
    for (src, dst, props) in edged {
        edges.push(DigestEdge {
            src,
            dst,
            ty: SCRAPES.into(),
            props,
        });
    }
    Ok(found)
}

/// Whether the plane already holds this key.
fn holds(plane: Option<&PlaneHandle<'_>>, key: &str) -> Result<bool> {
    match plane {
        Some(plane) => Ok(plane.node_by_key(key)?.is_some()),
        None => Ok(false),
    }
}

/// What an edge drawn from an address says about itself: the address named the
/// key, and the tree declares it.
fn by_address_props() -> Properties {
    let mut props = Properties::new();
    props.insert(
        "matched_by".into(),
        PropDesc::described(
            "how the target was resolved: `dns` is a Kubernetes service address, which names \
             one service and admits no other reading",
            PropValue::Str("dns".into()),
        ),
    );
    props.insert(
        "confidence".into(),
        PropDesc::new(PropValue::Str("exact".into())),
    );
    props
}

/// Every addressable ops node, by the name it answers to — the facts in hand
/// first, then the plane they are being written into.
///
/// Shared with the trace fold, which asks the same question of the same two
/// labels: a name a service answers to is one lookup, not one per reader.
pub(super) fn addressable(
    nodes: &[DigestNode],
    plane: Option<&PlaneHandle<'_>>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut by_name: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in nodes {
        if let Some(name) = ADDRESSABLE
            .iter()
            .any(|l| is_a(node, l))
            .then(|| prop_str(&node.props, "name"))
            .flatten()
        {
            by_name
                .entry(name.to_string())
                .or_default()
                .insert(node.key.clone());
        }
    }
    let Some(plane) = plane else {
        return Ok(by_name);
    };
    for label in ADDRESSABLE {
        for record in plane.query().scan_label(*label).nodes()? {
            let (Some(key), Some(name)) = (
                record.external_key.as_deref(),
                prop_str(&record.properties, "name"),
            ) else {
                continue;
            };
            by_name
                .entry(name.to_string())
                .or_default()
                .insert(key.to_string());
        }
    }
    Ok(by_name)
}

/// What a name-matched edge says about itself. Never the same as the edge the
/// reader draws from an address that derives a key: this one is a weaker
/// claim, and says so where it is read.
fn by_name_props() -> Properties {
    let mut props = Properties::new();
    props.insert(
        "matched_by".into(),
        PropDesc::described(
            "how the target was resolved: `name` is the host matched against the name of an \
             ops node in this plane, which is weaker than an address that names a key",
            PropValue::Str("name".into()),
        ),
    );
    props.insert(
        "confidence".into(),
        PropDesc::described(
            "`low` for a name match: a name is not an address, and two namespaces may \
             hold one name",
            PropValue::Str("low".into()),
        ),
    );
    props
}

/// Whether a node carries this label, chosen or asserted.
fn is_a(node: &DigestNode, label: &str) -> bool {
    node.label == label || node.extra_labels.iter().any(|l| l == label)
}

/// One string property, where it is one.
fn prop_str<'a>(props: &'a Properties, name: &str) -> Option<&'a str> {
    match props.get(name).map(|d| &d.value) {
        Some(PropValue::Str(s)) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
