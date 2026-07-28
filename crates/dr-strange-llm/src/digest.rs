//! Document → graph digestion (arch/07 §1–2, the deferred pipeline). The
//! boundary: a document + models in, a plane's worth of nodes/edges/vectors +
//! a report out. Nothing is written by this module — [`digest`] returns a
//! [`DigestResult`] the caller inspects (dry-run is the default posture,
//! arch/07 §2) and then [`DigestResult::apply`]s through the bulk API.
//!
//! Pipeline: chunk the document → for each chunk, ask the [`Chat`] model to
//! extract typed entities + relations as strict JSON (labels chosen purely from
//! the document — no preset schema, no catalog grounding) → merge entities
//! across chunks by key → embed each entity's text → attach provenance (source,
//! model, run id) as self-describing [`PropDesc`]. Extraction is
//! schema-constrained; the soft schema still absorbs whatever labels/types the
//! model returns.
//!
//! Optional entity linking (arch/07 §1 v1.5): given a [`CandidateSource`], each
//! chunk is embedded and its most-similar existing graph entities are offered
//! to the model as reuse candidates. Entities whose key the model reuses are
//! linked (not re-created) and relations to them are kept — the bulk loader
//! resolves those keys against the plane.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use dr_strange_core::json;
use dr_strange_core::{
    BulkEdge, BulkNode, BulkStats, Metric, PropDesc, PropValue, Properties, WriteTxn,
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Chat, Embedder};

/// How many existing entities to retrieve per chunk as reuse candidates.
const LINK_K: usize = 8;

// ---- entity linking (arch/07 §1 v1.5) ------------------------------------

/// An entity already present in the target graph, surfaced to the model so it
/// can reuse its identity (its `key`) instead of minting a duplicate.
pub struct ExistingEntity {
    pub key: String,
    pub label: String,
    pub description: String,
}

/// Supplies the existing entities most similar to a chunk's embedding, so the
/// digest can offer them as reuse candidates. Implemented by the caller over
/// the target plane's vector search — [`digest`] stays decoupled from storage.
pub trait CandidateSource {
    /// Existing entities similar to `query`, most-similar first (may be empty).
    /// Best-effort: an empty result simply means "propose everything as new".
    fn similar(&self, query: &[f32], k: usize) -> Result<Vec<ExistingEntity>>;
}

/// The default [`CandidateSource`]: cosine top-k over a plane's `embedding`
/// property — where [`digest`] stores entity vectors — so a re-digest links
/// against what previous digests wrote. Nodes without an embedding (or with a
/// mismatched dimension) are simply invisible to the search.
pub struct PlaneCandidates<'a> {
    plane: &'a dr_strange_core::PlaneHandle<'a>,
}

impl<'a> PlaneCandidates<'a> {
    pub fn new(plane: &'a dr_strange_core::PlaneHandle<'a>) -> Self {
        Self { plane }
    }
}

impl CandidateSource for PlaneCandidates<'_> {
    fn similar(&self, query: &[f32], k: usize) -> Result<Vec<ExistingEntity>> {
        let hits = self
            .plane
            .query()
            .vector_top_k(None, "embedding", query.to_vec(), Metric::Cosine, k as u64)
            .scored_nodes()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(hits
            .into_iter()
            .filter_map(|(n, _)| {
                let key = n.external_key?; // only keyed nodes are linkable
                let label = n.labels.into_iter().next().unwrap_or_default();
                let description = match n.properties.get("description").map(|p| &p.value) {
                    Some(PropValue::Str(s)) => s.clone(),
                    _ => String::new(),
                };
                Some(ExistingEntity {
                    key,
                    label,
                    description,
                })
            })
            .collect())
    }
}

// ---- what the model returns ----------------------------------------------

#[derive(Deserialize, Default)]
struct Extraction {
    #[serde(default)]
    entities: Vec<ExEntity>,
    #[serde(default)]
    relations: Vec<ExRelation>,
}

#[derive(Deserialize)]
struct ExEntity {
    key: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    properties: serde_json::Map<String, Value>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct ExRelation {
    src: String,
    dst: String,
    #[serde(rename = "type", default)]
    ty: String,
    #[serde(default)]
    properties: serde_json::Map<String, Value>,
    #[serde(default)]
    description: Option<String>,
}

// ---- the result ----------------------------------------------------------

pub struct DigestNode {
    pub key: String,
    pub label: String,
    pub props: Properties,
}

pub struct DigestEdge {
    pub src: String,
    pub dst: String,
    pub ty: String,
    pub props: Properties,
}

#[derive(Default)]
pub struct DigestReport {
    pub chunks: usize,
    pub entities: usize,
    pub relations: usize,
    /// Extracted entities that matched an existing graph node (by reused key)
    /// and were linked to rather than re-created.
    pub linked: usize,
    pub dropped_relations: usize,
    pub chat_requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub embed_tokens: u64,
}

pub struct DigestResult {
    pub nodes: Vec<DigestNode>,
    pub edges: Vec<DigestEdge>,
    pub report: DigestReport,
}

pub struct DigestOptions {
    /// Provenance: the source document's name/path.
    pub source: String,
    /// Provenance: the chat model id.
    pub model: String,
    /// Provenance: this run's id (caller-supplied so digest stays deterministic).
    pub run_id: String,
    /// Target chunk size in characters (paragraph-aware).
    pub chunk_chars: usize,
    /// Whether to embed entities into `embedding` vectors.
    pub embed: bool,
}

impl DigestResult {
    /// Writes the digested nodes + edges into `txn` through the bulk-load fast
    /// path (arch/07 §2: writing is the separate, explicit step). Entity keys
    /// are the node external keys, so relations resolve within the batch.
    pub fn apply(&self, txn: &mut WriteTxn) -> Result<BulkStats> {
        let label_slots: Vec<[&str; 1]> = self.nodes.iter().map(|n| [n.label.as_str()]).collect();
        let nodes: Vec<BulkNode> = self
            .nodes
            .iter()
            .zip(&label_slots)
            .map(|(n, ls)| BulkNode {
                external_key: Some(&n.key),
                labels: ls,
                props: n.props.clone(),
            })
            .collect();
        let edges: Vec<BulkEdge> = self
            .edges
            .iter()
            .map(|e| BulkEdge {
                src_key: &e.src,
                dst_key: &e.dst,
                ty: &e.ty,
                props: e.props.clone(),
            })
            .collect();
        txn.bulk_load(nodes, edges).map_err(Into::into)
    }
}

// ---- pipeline ------------------------------------------------------------

pub fn digest(
    document: &str,
    chat: &dyn Chat,
    embedder: &dyn Embedder,
    candidates: Option<&dyn CandidateSource>,
    opts: &DigestOptions,
) -> Result<DigestResult> {
    let chunks = chunk(document, opts.chunk_chars);
    let system = system_prompt(candidates.is_some());

    let mut entities: BTreeMap<String, DigestNode> = BTreeMap::new();
    let mut edges: Vec<DigestEdge> = Vec::new();
    let mut seen_rel: HashSet<(String, String, String)> = HashSet::new();
    // Existing graph entities surfaced across all chunks (key → entity). Used
    // to (a) skip re-creating them as nodes and (b) treat them as valid
    // relation endpoints so new→existing edges survive.
    let mut existing: HashMap<String, ExistingEntity> = HashMap::new();
    let mut report = DigestReport {
        chunks: chunks.len(),
        ..Default::default()
    };

    for chunk in &chunks {
        // Entity linking: retrieve existing entities similar to this chunk and
        // prepend them as reuse context (arch/07 §1 v1.5).
        let user = match candidates {
            Some(src) => {
                let emb = embedder.embed(std::slice::from_ref(chunk))?;
                report.embed_tokens += emb.tokens;
                let cands = match emb.vectors.first() {
                    Some(qv) => src.similar(qv, LINK_K)?,
                    None => Vec::new(),
                };
                let block = existing_block(&cands);
                for e in cands {
                    existing.entry(e.key.clone()).or_insert(e);
                }
                block.map(|b| format!("{b}\n---\n{chunk}"))
            }
            None => None,
        };
        let user = user.as_deref().unwrap_or(chunk);

        let reply = chat.complete(&system, user)?;
        report.chat_requests += 1;
        report.input_tokens += reply.input_tokens;
        report.output_tokens += reply.output_tokens;

        let extraction = parse_extraction(&reply.text)?;
        for e in extraction.entities {
            let node = entities.entry(e.key.clone()).or_insert_with(|| DigestNode {
                key: e.key.clone(),
                label: String::new(),
                props: Properties::new(),
            });
            if node.label.is_empty() && !e.label.is_empty() {
                node.label = e.label;
            }
            merge_props(&mut node.props, &e.properties);
            if let Some(d) = e.description {
                node.props
                    .entry("description".into())
                    .or_insert_with(|| desc_prop(d));
            }
        }
        for r in extraction.relations {
            if r.ty.is_empty() {
                continue;
            }
            if seen_rel.insert((r.src.clone(), r.dst.clone(), r.ty.clone())) {
                let mut props = Properties::new();
                merge_props(&mut props, &r.properties);
                if let Some(d) = r.description {
                    props.insert("description".into(), desc_prop(d));
                }
                edges.push(DigestEdge {
                    src: r.src,
                    dst: r.dst,
                    ty: r.ty,
                    props,
                });
            }
        }
    }

    // Entities the model reused an existing key for are linked, not re-created:
    // drop them from the new-node set (bulk load would otherwise write a second
    // node under a key the plane already holds and corrupt the key index).
    report.linked = entities
        .keys()
        .filter(|k| existing.contains_key(*k))
        .count();
    let mut nodes: Vec<DigestNode> = entities
        .into_values()
        .filter(|n| !existing.contains_key(&n.key))
        .collect();
    for n in &mut nodes {
        if n.label.is_empty() {
            n.label = "Entity".into();
        }
    }

    // Drop relations whose endpoints are neither a freshly-extracted entity nor
    // an existing graph node (model hallucination) — otherwise the bulk load
    // would reject the batch. Edges to existing nodes are kept: the bulk
    // loader resolves those keys against the plane.
    let mut valid: HashSet<&str> = nodes.iter().map(|n| n.key.as_str()).collect();
    valid.extend(existing.keys().map(String::as_str));
    let before = edges.len();
    edges.retain(|e| valid.contains(e.src.as_str()) && valid.contains(e.dst.as_str()));
    report.dropped_relations = before - edges.len();

    if opts.embed && !nodes.is_empty() {
        let texts: Vec<String> = nodes.iter().map(embed_text).collect();
        let (unique, index) = dedup(&texts); // run-scoped: embed each text once
        let reply = embedder.embed(&unique)?;
        report.embed_tokens += reply.tokens;
        for (i, node) in nodes.iter_mut().enumerate() {
            let v = reply.vectors[index[i]].clone();
            node.props
                .insert("embedding".into(), prop(PropValue::Vector(v)));
        }
    }

    for n in &mut nodes {
        add_provenance(&mut n.props, opts);
    }
    for e in &mut edges {
        add_provenance(&mut e.props, opts);
    }

    report.entities = nodes.len();
    report.relations = edges.len();
    Ok(DigestResult {
        nodes,
        edges,
        report,
    })
}

// ---- helpers -------------------------------------------------------------

fn prop(value: PropValue) -> PropDesc {
    PropDesc {
        description: None,
        value,
    }
}

fn desc_prop(text: String) -> PropDesc {
    PropDesc {
        description: Some("LLM-extracted description".into()),
        value: PropValue::Str(text),
    }
}

/// Merge JSON properties into a property map, skipping ones that don't convert
/// and never clobbering an existing key (first chunk wins).
fn merge_props(into: &mut Properties, from: &serde_json::Map<String, Value>) {
    for (k, v) in from {
        if into.contains_key(k) {
            continue;
        }
        if let Ok(value) = json::json_to_value(v) {
            into.insert(k.clone(), prop(value));
        }
    }
}

/// Provenance stamped on everything written (arch/07 §2), as self-describing
/// `PropDesc`. Underscore-prefixed to sit apart from extracted content.
fn add_provenance(props: &mut Properties, opts: &DigestOptions) {
    let stamp = |props: &mut Properties, key: &str, what: &str, value: &str| {
        props.insert(
            key.into(),
            PropDesc {
                description: Some(what.into()),
                value: PropValue::Str(value.into()),
            },
        );
    };
    stamp(
        props,
        "_source",
        "source document this was digested from",
        &opts.source,
    );
    stamp(props, "_model", "model that extracted this", &opts.model);
    stamp(props, "_run", "digest run id", &opts.run_id);
}

/// The text embedded for an entity: its key + label, then every string
/// property (`key: value` per line, in the map's stable order). Provenance and
/// the embedding itself aren't attached yet at this point in the pipeline, so
/// only extracted content feeds the vector. Property order is deterministic
/// (`Properties` is a `BTreeMap`), keeping the run-scoped dedup reproducible.
fn embed_text(n: &DigestNode) -> String {
    let mut s = format!("{} ({})", n.key, n.label);
    for (k, pd) in &n.props {
        if let PropValue::Str(v) = &pd.value {
            let v = v.trim();
            if !v.is_empty() {
                s.push_str(&format!("\n{k}: {v}"));
            }
        }
    }
    s
}

fn dedup(texts: &[String]) -> (Vec<String>, Vec<usize>) {
    let mut unique = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut index = Vec::with_capacity(texts.len());
    for t in texts {
        let i = *seen.entry(t.as_str()).or_insert_with(|| {
            unique.push(t.clone());
            unique.len() - 1
        });
        index.push(i);
    }
    (unique, index)
}

/// Paragraph-aware chunking: accumulate paragraphs until the next would exceed
/// `size`. A single oversized paragraph becomes its own chunk.
fn chunk(doc: &str, size: usize) -> Vec<String> {
    let size = size.max(200);
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in doc.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if !cur.is_empty() && cur.len() + para.len() + 2 > size {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(para);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

fn system_prompt(link: bool) -> String {
    // No preset labels: the document alone dictates the schema. The plane's
    // existing catalog is deliberately never shown to the model — labels can
    // always be edited after the fact (arch/07 §2).
    let mut p = String::from(
        "You extract a knowledge graph from the user's text. Let the DOCUMENT decide the schema: \
         give each entity the most specific, accurate label for what it actually is here \
         (e.g. Protein, City, Statute, Album, Algorithm — not a generic default), and each \
         relation a precise UPPER_SNAKE type drawn from the text. Do not fall back on a fixed set \
         of labels.\n\
         Reply with ONLY strict JSON, no prose, no markdown fences, in exactly this shape:\n\
         {\"entities\":[{\"key\":\"stable canonical name\",\"label\":\"SpecificType\",\
         \"properties\":{\"...\":\"...\"},\"description\":\"one concise sentence\"}],\
         \"relations\":[{\"src\":\"entity key\",\"dst\":\"entity key\",\"type\":\"REL_TYPE\",\
         \"description\":\"one concise sentence\"}]}\n\
         Use the SAME key for an entity every time it appears so mentions collapse to one node. \
         Relations must reference entity keys you also emit.",
    );
    if link {
        p.push_str(
            "\nSome messages start with an \"EXISTING ENTITIES\" block listing nodes already in \
             the graph as `key (Label): description`. That block is CONTEXT, not text to extract from. \
             If the document refers to one of those entities, reuse its EXACT key so your output \
             attaches to that existing node — do not invent a variant key and do not restate its \
             properties. You may emit relations that reference those keys (to link new entities to \
             existing ones, or two existing ones together). Still label genuinely new entities from \
             the document.",
        );
    }
    p
}

/// The reuse-candidate context block prepended to a chunk, or `None` when there
/// are no candidates (so the chunk is sent unchanged).
fn existing_block(cands: &[ExistingEntity]) -> Option<String> {
    if cands.is_empty() {
        return None;
    }
    let mut s = String::from(
        "EXISTING ENTITIES (already in the graph — reuse a key if the text refers to it; do NOT \
         re-extract these):\n",
    );
    for e in cands {
        let description: String = e.description.chars().take(160).collect();
        s.push_str(&format!(
            "- {} ({}): {}\n",
            e.key,
            e.label,
            description.trim()
        ));
    }
    Some(s)
}

/// Pull the JSON object out of a model reply — tolerate ```json fences and
/// leading/trailing prose.
fn parse_extraction(raw: &str) -> Result<Extraction> {
    let t = raw.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.trim().trim_end_matches("```").trim();
    let body = match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &t[a..=b],
        _ => t,
    };
    serde_json::from_str(body).with_context(|| {
        format!(
            "model reply was not valid extraction JSON: {}…",
            &body[..body.len().min(160)]
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_document_driven_with_no_preset_labels() {
        let p = system_prompt(false);
        // Tells the model to pick the most specific label from the document…
        assert!(p.contains("most specific, accurate label"));
        assert!(p.contains("Let the DOCUMENT decide the schema"));
        assert!(p.contains("Do not fall back on a fixed set"));
        // …and never leaks a plane's existing schema as a preset to reuse.
        assert!(!p.contains("already uses these labels"));
        assert!(!p.contains("Existing relation types"));
        // No linking ⇒ no reuse-candidate instructions.
        assert!(!p.contains("EXISTING ENTITIES"));
    }

    #[test]
    fn linking_adds_reuse_instructions_and_context_block() {
        // The linked prompt keeps the document-driven base and adds the
        // reuse-by-exact-key convention.
        let p = system_prompt(true);
        assert!(p.contains("most specific, accurate label"));
        assert!(p.contains("EXISTING ENTITIES"));
        assert!(p.contains("reuse its EXACT key"));

        // A candidate list renders as a labelled context block; empty ⇒ none.
        assert!(existing_block(&[]).is_none());
        let block = existing_block(&[ExistingEntity {
            key: "alice".into(),
            label: "Person".into(),
            description: "an engineer at Acme".into(),
        }])
        .unwrap();
        assert!(block.contains("EXISTING ENTITIES"));
        assert!(block.contains("alice (Person): an engineer at Acme"));
    }
}
