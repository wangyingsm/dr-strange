//! Whole-plane embedding, incremental by meaning — the engine behind
//! `drsg vectorize` and the dashboard's per-plane button.
//!
//! Each node's text comes from [`crate::embeddable_text`]: parser facts get
//! the stable projection (no positional properties), document-extracted
//! nodes their full content. `_embedded_from` records a hash of the text
//! each vector was built from, so a re-run pays only for nodes whose
//! *meaning* changed. The pass ends by ensuring a vector index per label
//! that carries embeddings — the plane answers similarity queries the
//! moment it returns.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use dr_strange_core::{Database, Metric, NodeId, PropDesc, PropValue};

use crate::provider::Embedder;

/// Inputs have provider token ceilings; a pathological `value` should
/// truncate, not fail the batch.
const TEXT_CAP: usize = 6000;

/// What one [`vectorize_plane`] pass did.
#[derive(Debug, Default, serde::Serialize)]
pub struct VectorizeStats {
    /// Nodes whose vector was (re)built this pass.
    pub embedded: usize,
    /// Distinct texts sent to the provider (identical texts embed once).
    pub unique: usize,
    pub tokens: u64,
    /// Nodes already carrying a vector built from their current text.
    pub current: usize,
    /// Nodes with nothing to embed.
    pub empty: usize,
    /// Labels whose vector index was ensured.
    pub labels: Vec<String>,
}

/// Embed every node in `plane_name` that needs it, then ensure a vector
/// index on `embedding` for every label that carries one.
pub fn vectorize_plane(
    db: &Database,
    plane_name: &str,
    embedder: &dyn Embedder,
    metric: Metric,
) -> Result<VectorizeStats> {
    let mut stats = VectorizeStats::default();
    let plane = db.plane(plane_name)?;

    let mut work: Vec<(NodeId, String, String)> = Vec::new(); // id, text, hash
    let mut labels: BTreeSet<String> = BTreeSet::new();
    for node in plane.query().scan_all().nodes()? {
        let key = node.external_key.as_deref().unwrap_or("");
        let mut text = crate::embeddable_text(key, &node.labels, &node.properties);
        if text.trim().is_empty() {
            stats.empty += 1;
            continue;
        }
        if let Some(primary) = node.labels.first() {
            labels.insert(primary.clone());
        }
        if text.len() > TEXT_CAP {
            let mut end = TEXT_CAP;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let hash = text_hash(&text);
        let up_to_date = matches!(
            node.properties.get("_embedded_from").map(|d| &d.value),
            Some(PropValue::Str(h)) if *h == hash
        ) && matches!(
            node.properties.get("embedding").map(|d| &d.value),
            Some(PropValue::Vector(_))
        );
        if up_to_date {
            stats.current += 1;
        } else {
            work.push((node.id, text, hash));
        }
    }

    if !work.is_empty() {
        // Identical texts embed once — external stand-ins and boilerplate
        // repeat.
        let mut unique: Vec<String> = Vec::new();
        let mut index: Vec<usize> = Vec::with_capacity(work.len());
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (_, text, _) in &work {
            match seen.get(text.as_str()) {
                Some(&i) => index.push(i),
                None => {
                    seen.insert(text.as_str(), unique.len());
                    index.push(unique.len());
                    unique.push(text.clone());
                }
            }
        }
        let reply = embedder.embed(&unique).context("embedding the plane")?;
        stats.embedded = work.len();
        stats.unique = unique.len();
        stats.tokens = reply.tokens;

        let mut txn = plane.write()?;
        for (i, (id, _, hash)) in work.iter().enumerate() {
            txn.set_prop(
                *id,
                "embedding",
                PropDesc::described(
                    "embedding of this node's text",
                    PropValue::Vector(reply.vectors[index[i]].clone()),
                ),
            )?;
            txn.set_prop(
                *id,
                "_embedded_from",
                PropDesc::described(
                    "hash of the text the embedding was built from",
                    PropValue::Str(hash.clone()),
                ),
            )?;
        }
        txn.commit()?;
    }

    // Ensured even when nothing embedded: embeddings can be current while a
    // label gained since the last pass has no index yet.
    for label in &labels {
        plane.ensure_vector_index(label, "embedding", metric)?;
    }
    stats.labels = labels.into_iter().collect();
    Ok(stats)
}

/// A stable fingerprint of an embedded text — what `_embedded_from` stores
/// so a re-run can tell "unchanged" without asking the provider. sha256
/// rather than a std hasher because the value persists in the plane: a
/// hasher whose algorithm may change across toolchains would quietly
/// invalidate every skip on the next binary.
fn text_hash(text: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let d = Sha256::digest(text.as_bytes());
    // 16 hex chars: 64 bits is plenty when a collision merely re-embeds one
    // node.
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}
