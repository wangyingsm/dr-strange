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

/// Nodes embedded per provider call and per write transaction. A plane is
/// walked once, but its texts are not all held at once: a repository plane
/// has hundreds of thousands of nodes, and a few KiB of text each is more
/// than a small machine wants resident before the first vector is written.
/// Each batch is committed on its own, so a pass that dies halfway leaves
/// what it embedded, and `_embedded_from` lets the next pass resume.
pub const EMBED_BATCH: usize = 512;

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
            if work.len() >= EMBED_BATCH {
                embed_batch(&plane, embedder, &mut work, &mut stats)?;
            }
        }
    }
    embed_batch(&plane, embedder, &mut work, &mut stats)?;

    // Ensured even when nothing embedded: embeddings can be current while a
    // label gained since the last pass has no index yet.
    for label in &labels {
        plane.ensure_vector_index(label, "embedding", metric)?;
    }
    stats.labels = labels.into_iter().collect();
    Ok(stats)
}

/// Embed one batch of `work` and write the vectors in one transaction,
/// draining the batch. Nothing to do when it is empty.
fn embed_batch(
    plane: &dr_strange_core::PlaneHandle<'_>,
    embedder: &dyn Embedder,
    work: &mut Vec<(NodeId, String, String)>,
    stats: &mut VectorizeStats,
) -> Result<()> {
    if work.is_empty() {
        return Ok(());
    }
    // Identical texts embed once — external stand-ins and boilerplate
    // repeat. Within the batch: a repeat across batches costs one more
    // call's worth, which is the price of not holding the plane.
    let mut unique: Vec<String> = Vec::new();
    let mut index: Vec<usize> = Vec::with_capacity(work.len());
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (_, text, _) in work.iter() {
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
    if reply.vectors.len() != unique.len() {
        anyhow::bail!(
            "the embedding provider answered {} vectors for {} texts",
            reply.vectors.len(),
            unique.len()
        );
    }
    stats.embedded += work.len();
    stats.unique += unique.len();
    stats.tokens += reply.tokens;

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
    work.clear();
    Ok(())
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

/// `search` behind the agent surface: embed the query text, run the plane's
/// cosine top-k, render compactly. Core holds no provider, so the text→vector
/// step lives here and every surface (CLI, MCP) shares it.
pub fn semantic_search(
    db: &Database,
    plane_name: &str,
    query: &str,
    embedder: &dyn Embedder,
    k: u64,
) -> Result<String> {
    let reply = embedder
        .embed(std::slice::from_ref(&query.to_string()))
        .context("embedding the query")?;
    let vector = reply
        .vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("the embedder returned no vector"))?;
    let plane = db.plane(plane_name)?;
    Ok(dr_strange_core::compact::search(&plane, &vector, k)?)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dr_strange_core::{PropDesc, Properties};

    use super::*;
    use crate::provider::{EmbedReply, MockProvider};

    fn plane_with(db: &Database, keys: &[&str]) {
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        for key in keys {
            txn.create_node_with_key(key, &["Person"], Properties::new())
                .unwrap();
        }
        txn.commit().unwrap();
    }

    fn set(db: &Database, key: &str, prop: &str, value: &str) {
        let plane = db.plane("startup").unwrap();
        let node = plane.node_by_key(key).unwrap().unwrap();
        let mut txn = plane.write().unwrap();
        txn.set_prop(
            node.id,
            prop,
            PropDesc::new(PropValue::Str(value.to_string())),
        )
        .unwrap();
        txn.commit().unwrap();
    }

    /// An embedder that remembers how many texts each call carried.
    struct Counting {
        inner: MockProvider,
        calls: Mutex<Vec<usize>>,
    }

    impl Embedder for Counting {
        fn embed(&self, texts: &[String]) -> Result<EmbedReply> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(texts.len());
            self.inner.embed(texts)
        }
    }

    /// A pass embeds what has no vector, writes the vector and the hash it
    /// was built from, and a second pass over the same plane pays for
    /// nothing; a node whose text changed is the one node the third pays for.
    #[test]
    fn a_pass_is_incremental_by_text() {
        let db = Database::in_memory().unwrap();
        plane_with(&db, &["alice", "bob"]);
        let mock = MockProvider::new(vec![], 8);

        let first = vectorize_plane(&db, "startup", &mock, Metric::Cosine).unwrap();
        assert_eq!((first.embedded, first.unique, first.current), (2, 2, 0));
        assert_eq!(first.labels, vec!["Person".to_string()]);
        let plane = db.plane("startup").unwrap();
        let alice = plane.node_by_key("alice").unwrap().unwrap();
        assert!(matches!(
            alice.properties.get("embedding").map(|d| &d.value),
            Some(PropValue::Vector(v)) if v.len() == 8
        ));
        assert!(matches!(
            alice.properties.get("_embedded_from").map(|d| &d.value),
            Some(PropValue::Str(h)) if h == &text_hash("alice (Person)")
        ));

        let second = vectorize_plane(&db, "startup", &mock, Metric::Cosine).unwrap();
        assert_eq!((second.embedded, second.current), (0, 2));

        set(&db, "bob", "role", "engineer");
        let third = vectorize_plane(&db, "startup", &mock, Metric::Cosine).unwrap();
        assert_eq!((third.embedded, third.current), (1, 1));
    }

    /// Identical texts embed once per call, and a node with nothing to say
    /// is counted rather than sent.
    #[test]
    fn identical_texts_embed_once_and_empty_nodes_are_counted() {
        let db = Database::in_memory().unwrap();
        plane_with(&db, &["alice", "bob"]);
        set(&db, "alice", "note", "same");
        set(&db, "bob", "note", "same");
        {
            // No key, no label, no text.
            let plane = db.plane("startup").unwrap();
            let mut txn = plane.write().unwrap();
            txn.create_node(&[], Properties::new()).unwrap();
            txn.commit().unwrap();
        }
        // Different keys give different texts, so make them agree on one.
        let plane = db.plane("startup").unwrap();
        let alice = plane.node_by_key("alice").unwrap().unwrap();
        let bob = plane.node_by_key("bob").unwrap().unwrap();
        let unique_before = {
            let a = crate::embeddable_text("alice", &alice.labels, &alice.properties);
            let b = crate::embeddable_text("bob", &bob.labels, &bob.properties);
            if a == b { 1 } else { 2 }
        };
        let mock = MockProvider::new(vec![], 4);
        let stats = vectorize_plane(&db, "startup", &mock, Metric::Cosine).unwrap();
        assert_eq!(stats.empty, 1);
        assert_eq!(stats.embedded, 2);
        assert_eq!(stats.unique, unique_before);
    }

    /// A large plane goes to the provider a batch at a time, each batch
    /// committed on its own, and every node still ends up with a vector.
    #[test]
    fn a_large_plane_is_embedded_in_batches() {
        let db = Database::in_memory().unwrap();
        let keys: Vec<String> = (0..EMBED_BATCH + 3).map(|i| format!("n{i}")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        plane_with(&db, &refs);
        let counting = Counting {
            inner: MockProvider::new(vec![], 4),
            calls: Mutex::new(Vec::new()),
        };
        let stats = vectorize_plane(&db, "startup", &counting, Metric::Cosine).unwrap();
        assert_eq!(stats.embedded, EMBED_BATCH + 3);
        let calls = counting.calls.into_inner().unwrap();
        assert_eq!(
            calls,
            vec![EMBED_BATCH, 3],
            "one call per batch, none held back"
        );
        let plane = db.plane("startup").unwrap();
        let last = plane.node_by_key("n514").unwrap().unwrap();
        assert!(last.properties.contains_key("embedding"));
    }

    /// A provider that answers the wrong number of vectors is an error, not
    /// an index out of range — and nothing of that batch is written.
    #[test]
    fn a_short_reply_is_an_error_not_a_panic() {
        struct Short;
        impl Embedder for Short {
            fn embed(&self, _texts: &[String]) -> Result<EmbedReply> {
                Ok(EmbedReply {
                    vectors: vec![],
                    tokens: 0,
                })
            }
        }
        let db = Database::in_memory().unwrap();
        plane_with(&db, &["alice"]);
        let err = vectorize_plane(&db, "startup", &Short, Metric::Cosine).unwrap_err();
        assert!(
            format!("{err:#}").contains("0 vectors for 1 texts"),
            "{err:#}"
        );
        let plane = db.plane("startup").unwrap();
        let alice = plane.node_by_key("alice").unwrap().unwrap();
        assert!(!alice.properties.contains_key("embedding"));
    }
}
