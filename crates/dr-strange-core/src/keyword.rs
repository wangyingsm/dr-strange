//! In-memory BM25 keyword-index registry (ROADMAP §2).
//!
//! The keyword counterpart to [`VectorRegistry`](crate::index::VectorRegistry),
//! built the same way: one inverted index per declared `(plane, label,
//! property)`, only the *declaration* durable (in `meta`), the index itself
//! rebuilt from the KV on open ([`KeywordRegistry::rebuild_from`]) and cached
//! to a `.bm25` sidecar. The owning [`Database`](crate::Database) wraps it in an
//! `RwLock`; write commits mirror node changes in via [`upsert`]/[`remove_node`].
//!
//! Scoring is Okapi BM25 (`k1 = 1.2`, `b = 0.75`): each declared index keeps
//! per-term postings `(node, term-frequency)`, per-doc token lengths, and the
//! total length for the average-document-length normalization.
//!
//! [`upsert`]: KeywordRegistry::upsert
//! [`remove_node`]: KeywordRegistry::remove_node

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};
use crate::storage::engine::ReadTransaction;
use crate::storage::graph;
use crate::text::{Analyzer, Language};
use crate::types::{NodeId, PlaneId};

/// BM25 term-frequency saturation.
const K1: f32 = 1.2;
/// BM25 length-normalization strength.
const B: f32 = 0.75;

/// Magic prefixing a keyword sidecar (cf. the HNSW `DRSH`).
const SIDECAR_MAGIC: &[u8; 4] = b"DRSK";
/// On-disk sidecar layout version; bump on any wire-shape change.
const SIDECAR_VERSION: u32 = 1;

type IndexKey = (PlaneId, String, String);

/// Per-document bookkeeping: token count (for BM25 length norm) and the unique
/// terms it contributed (so a delete can be surgically removed from postings).
#[derive(Serialize, Deserialize)]
struct DocEntry {
    len: u32,
    terms: Vec<String>,
}

/// One inverted index over a declared `(plane, label, property)`.
#[derive(Serialize, Deserialize)]
struct Entry {
    language: Language,
    /// term → `[(node, term-frequency), …]`.
    postings: HashMap<String, Vec<(u64, u32)>>,
    /// node → its length + terms.
    docs: HashMap<u64, DocEntry>,
    /// Σ of every doc's token length (for `avgdl`).
    total_len: u64,
}

impl Entry {
    fn new(language: Language) -> Self {
        Self {
            language,
            postings: HashMap::new(),
            docs: HashMap::new(),
            total_len: 0,
        }
    }

    /// Index (or re-index) one document's text. Empty documents (no terms after
    /// analysis) are not stored, so they don't perturb `avgdl`.
    fn add_doc(&mut self, node: u64, analyzer: &Analyzer, text: &str) {
        let tokens = analyzer.analyze(text);
        if tokens.is_empty() {
            return;
        }
        let len = tokens.len() as u32;
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in tokens {
            *tf.entry(t).or_insert(0) += 1;
        }
        let terms: Vec<String> = tf.keys().cloned().collect();
        for (term, count) in &tf {
            self.postings
                .entry(term.clone())
                .or_default()
                .push((node, *count));
        }
        self.total_len += u64::from(len);
        self.docs.insert(node, DocEntry { len, terms });
    }

    /// Remove a document from the index (no-op if absent).
    fn remove_doc(&mut self, node: u64) {
        if let Some(doc) = self.docs.remove(&node) {
            self.total_len -= u64::from(doc.len);
            for term in doc.terms {
                if let Some(list) = self.postings.get_mut(&term) {
                    list.retain(|&(n, _)| n != node);
                    if list.is_empty() {
                        self.postings.remove(&term);
                    }
                }
            }
        }
    }

    /// BM25 top-`k` for `query`, highest score first (ties by ascending id).
    fn search(&self, query: &str, k: usize) -> Vec<(NodeId, f32)> {
        let n = self.docs.len();
        if n == 0 || k == 0 {
            return Vec::new();
        }
        let analyzer = Analyzer::new(self.language);
        let mut qterms = analyzer.analyze(query);
        qterms.sort();
        qterms.dedup();

        let avgdl = self.total_len as f32 / n as f32;
        let mut scores: HashMap<u64, f32> = HashMap::new();
        for term in &qterms {
            let Some(list) = self.postings.get(term) else {
                continue;
            };
            let df = list.len() as f32;
            // Okapi IDF with the +1 that keeps it non-negative even for a term
            // in more than half the docs.
            let idf = ((n as f32 - df + 0.5) / (df + 0.5) + 1.0).ln();
            for &(node, tf) in list {
                let dl = self.docs.get(&node).map_or(0, |d| d.len) as f32;
                let tf = tf as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                *scores.entry(node).or_insert(0.0) += idf * (tf * (K1 + 1.0)) / denom;
            }
        }

        let mut hits: Vec<(NodeId, f32)> =
            scores.into_iter().map(|(n, s)| (NodeId(n), s)).collect();
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(Ordering::Equal)
                .then(a.0.0.cmp(&b.0.0))
        });
        hits.truncate(k);
        hits
    }
}

/// Live set of keyword indexes. Not internally locked — the owning
/// [`Database`](crate::Database) wraps it in an `RwLock`.
#[derive(Default)]
pub struct KeywordRegistry {
    entries: HashMap<IndexKey, Entry>,
}

impl KeywordRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discards all indexes and rebuilds them from the declarations in `txn`
    /// (called on database open).
    pub fn rebuild_from(&mut self, txn: &dyn ReadTransaction) -> Result<()> {
        self.entries.clear();
        for (plane, label, property, language) in graph::list_keyword_indexes(txn)? {
            self.build_entry(txn, plane, &label, &property, language)?;
        }
        Ok(())
    }

    /// Build (or replace) one index by scanning its label and analyzing every
    /// node's `property` string.
    pub fn build_entry(
        &mut self,
        txn: &dyn ReadTransaction,
        plane: PlaneId,
        label: &str,
        property: &str,
        language: Language,
    ) -> Result<()> {
        let analyzer = Analyzer::new(language);
        let mut entry = Entry::new(language);
        for id in graph::scan_label(txn, plane, label)? {
            if let Some(text) = graph::node_text(txn, plane, id, property)? {
                entry.add_doc(id.0, &analyzer, &text);
            }
        }
        self.entries
            .insert((plane, label.to_string(), property.to_string()), entry);
        Ok(())
    }

    /// Declared indexes on `plane`, as `(label, property, language)` — the
    /// snapshot a write transaction takes to know which mutations to mirror.
    pub fn declared(&self, plane: PlaneId) -> Vec<(String, String, Language)> {
        self.entries
            .iter()
            .filter(|((p, _, _), _)| *p == plane)
            .map(|((_, l, prop), e)| (l.clone(), prop.clone(), e.language))
            .collect()
    }

    /// BM25 search, or `None` if no such index exists (caller falls back to a
    /// linear scan).
    pub fn search(
        &self,
        plane: PlaneId,
        label: &str,
        property: &str,
        query: &str,
        k: usize,
    ) -> Option<Vec<(NodeId, f32)>> {
        let entry = self
            .entries
            .get(&(plane, label.to_string(), property.to_string()))?;
        Some(entry.search(query, k))
    }

    /// Insert/replace a document's text in the matching index (no-op if that
    /// `(plane, label, property)` isn't indexed).
    pub fn upsert(
        &mut self,
        plane: PlaneId,
        label: &str,
        property: &str,
        node: NodeId,
        text: &str,
    ) {
        if let Some(entry) = self
            .entries
            .get_mut(&(plane, label.to_string(), property.to_string()))
        {
            let analyzer = Analyzer::new(entry.language);
            entry.remove_doc(node.0);
            entry.add_doc(node.0, &analyzer, text);
        }
    }

    /// Remove a node from one specific index (its property became absent or
    /// non-string).
    pub fn remove_one(&mut self, plane: PlaneId, label: &str, property: &str, node: NodeId) {
        if let Some(entry) = self
            .entries
            .get_mut(&(plane, label.to_string(), property.to_string()))
        {
            entry.remove_doc(node.0);
        }
    }

    /// Remove a node from every index (node deletion). Ids are globally unique,
    /// so removing from indexes it was never in is a harmless no-op.
    pub fn remove_node(&mut self, node: NodeId) {
        for entry in self.entries.values_mut() {
            entry.remove_doc(node.0);
        }
    }

    /// Serialize the whole registry to `path`, stamped with `seq`. Best-effort:
    /// a failure only costs a rebuild-from-KV on the next open.
    pub fn save_sidecar(&self, path: &Path, seq: u64) -> Result<()> {
        let bytes = self.to_bytes(seq)?;
        let tmp = path.with_extension("bm25.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The registry serialized to the sidecar byte form (magic + postcard),
    /// stamped with `seq` — the in-memory counterpart of [`save_sidecar`], used
    /// to embed the built index in a database snapshot (ROADMAP §6).
    pub fn to_bytes(&self, seq: u64) -> Result<Vec<u8>> {
        let sidecar = Sidecar {
            version: SIDECAR_VERSION,
            seq,
            entries: self
                .entries
                .iter()
                .map(|((plane, label, property), e)| (*plane, label.as_str(), property.as_str(), e))
                .collect(),
        };
        let mut bytes = Vec::from(*SIDECAR_MAGIC);
        bytes.extend_from_slice(&postcard::to_stdvec(&sidecar).map_err(backend)?);
        Ok(bytes)
    }

    /// Load a registry from `path`, but only if fresh: its stamped sequence must
    /// equal `expected_seq` and its version must match. Returns `None` (→ caller
    /// rebuilds from KV) on absence, staleness, or any decode error.
    pub fn load_sidecar(path: &Path, expected_seq: u64) -> Option<Self> {
        Self::from_bytes(&std::fs::read(path).ok()?, expected_seq)
    }

    /// Parse a registry from the sidecar byte form (the in-memory counterpart of
    /// [`load_sidecar`]); `None` on version/`seq` mismatch or decode error.
    pub fn from_bytes(bytes: &[u8], expected_seq: u64) -> Option<Self> {
        let payload = bytes.strip_prefix(SIDECAR_MAGIC)?;
        let sidecar: SidecarOwned = postcard::from_bytes(payload).ok()?;
        if sidecar.version != SIDECAR_VERSION || sidecar.seq != expected_seq {
            return None;
        }
        let entries = sidecar
            .entries
            .into_iter()
            .map(|(plane, label, property, entry)| ((plane, label, property), entry))
            .collect();
        Some(Self { entries })
    }
}

// ---- Sidecar wire form ----------------------------------------------------
// Borrowing (save) vs owning (load), agreeing on field order + wire types, so
// `save` serializes the live entries without cloning them.

#[derive(Serialize)]
struct Sidecar<'a> {
    version: u32,
    seq: u64,
    entries: Vec<(PlaneId, &'a str, &'a str, &'a Entry)>,
}

#[derive(Deserialize)]
struct SidecarOwned {
    version: u32,
    seq: u64,
    entries: Vec<(PlaneId, String, String, Entry)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::engine::{StorageEngine, WriteTransaction};
    use crate::storage::memory::MemoryEngine;
    use crate::types::{PropDesc, PropValue, Properties};

    fn text_props(body: &str) -> Properties {
        [(
            "body".to_string(),
            PropDesc::new(PropValue::Str(body.into())),
        )]
        .into_iter()
        .collect()
    }

    /// A startup plane with three "Doc" nodes carrying `body` text, and a
    /// keyword index built over them.
    fn setup() -> (MemoryEngine, KeywordRegistry, [NodeId; 3]) {
        let eng = MemoryEngine::new();
        let ids;
        {
            let mut txn = eng.begin_write().unwrap();
            graph::init(&mut txn).unwrap();
            let a = graph::create_node(
                &mut txn,
                PlaneId::STARTUP,
                &["Doc"],
                &text_props("graph databases store nodes and edges"),
            )
            .unwrap();
            let b = graph::create_node(
                &mut txn,
                PlaneId::STARTUP,
                &["Doc"],
                &text_props("vector search finds similar embeddings"),
            )
            .unwrap();
            let c = graph::create_node(
                &mut txn,
                PlaneId::STARTUP,
                &["Doc"],
                &text_props("a graph database indexes graph structure for graph queries"),
            )
            .unwrap();
            ids = [a, b, c];
            txn.commit().unwrap();
        }
        let mut reg = KeywordRegistry::new();
        {
            let txn = eng.begin_read().unwrap();
            reg.build_entry(&txn, PlaneId::STARTUP, "Doc", "body", Language::English)
                .unwrap();
        }
        (eng, reg, ids)
    }

    #[test]
    fn ranks_by_bm25_relevance() {
        let (_eng, reg, [a, _b, c]) = setup();
        let hits = reg
            .search(PlaneId::STARTUP, "Doc", "body", "graph database", 3)
            .unwrap();
        // c mentions "graph" three times + "database" → outranks a.
        assert_eq!(hits[0].0, c);
        assert_eq!(hits[1].0, a);
        assert!(hits.iter().all(|(_, s)| *s > 0.0));
    }

    #[test]
    fn query_stems_match_document_plurals() {
        let (_eng, reg, _) = setup();
        // "databases" (plural) stems to the same term as "database" in the docs.
        let hits = reg
            .search(PlaneId::STARTUP, "Doc", "body", "databases", 3)
            .unwrap();
        assert_eq!(hits.len(), 2); // docs a and c mention database(s)
    }

    #[test]
    fn missing_index_is_none() {
        let (_eng, reg, _) = setup();
        assert!(
            reg.search(PlaneId::STARTUP, "Ghost", "body", "x", 1)
                .is_none()
        );
        assert!(
            reg.search(PlaneId::STARTUP, "Doc", "missing", "x", 1)
                .is_none()
        );
    }

    #[test]
    fn upsert_and_remove_keep_scores_coherent() {
        let (_eng, mut reg, [a, b, _c]) = setup();
        // b has no "graph"; give it a graph-heavy body and it should now match.
        reg.upsert(PlaneId::STARTUP, "Doc", "body", b, "graph graph graph");
        let hits = reg
            .search(PlaneId::STARTUP, "Doc", "body", "graph", 3)
            .unwrap();
        assert_eq!(
            hits[0].0, b,
            "re-indexed b is now the strongest graph match"
        );

        // Remove a: it drops out of results entirely.
        reg.remove_node(a);
        let hits = reg
            .search(PlaneId::STARTUP, "Doc", "body", "graph", 3)
            .unwrap();
        assert!(!hits.iter().any(|(id, _)| *id == a));
    }

    #[test]
    fn sidecar_roundtrips_and_honors_seq() {
        let (_eng, reg, [_a, _b, c]) = setup();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.drsg.bm25");

        reg.save_sidecar(&path, 9).unwrap();
        assert!(
            KeywordRegistry::load_sidecar(&path, 8).is_none(),
            "stale seq is rejected"
        );
        let loaded = KeywordRegistry::load_sidecar(&path, 9).expect("fresh sidecar loads");
        let hits = loaded
            .search(PlaneId::STARTUP, "Doc", "body", "graph database", 3)
            .unwrap();
        assert_eq!(hits[0].0, c);
    }
}
