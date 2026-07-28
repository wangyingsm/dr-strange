//! Document → graph digestion (arch/07 §1–2, the deferred pipeline). The
//! boundary: a document + models in, a plane's worth of nodes/edges/vectors +
//! a report out. Nothing is written by this module — [`digest`] returns a
//! [`DigestResult`] the caller inspects (dry-run is the default posture,
//! arch/07 §2) and then [`DigestResult::apply`]s through the bulk API.
//!
//! Pipeline: chunk the document → for each chunk, ask the [`Chat`] model to
//! extract typed entities + relations as strict JSON (grounded on the plane's
//! catalog when there is one) → merge entities across chunks by key → embed
//! each entity's text → attach provenance (source, model, run id) as
//! self-describing [`PropDesc`]. Extraction is schema-constrained; the soft
//! schema still absorbs whatever labels/types the model returns.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use dr_strange_core::json;
use dr_strange_core::{
    BulkEdge, BulkNode, BulkStats, CatalogSnapshot, PropDesc, PropValue, Properties, WriteTxn,
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Chat, Embedder};

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
    catalog: Option<&CatalogSnapshot>,
    opts: &DigestOptions,
) -> Result<DigestResult> {
    let chunks = chunk(document, opts.chunk_chars);
    let system = system_prompt(catalog);

    let mut entities: BTreeMap<String, DigestNode> = BTreeMap::new();
    let mut edges: Vec<DigestEdge> = Vec::new();
    let mut seen_rel: HashSet<(String, String, String)> = HashSet::new();
    let mut report = DigestReport {
        chunks: chunks.len(),
        ..Default::default()
    };

    for chunk in &chunks {
        let reply = chat.complete(&system, chunk)?;
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
                    .entry("summary".into())
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
                    props.insert("summary".into(), desc_prop(d));
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

    let mut nodes: Vec<DigestNode> = entities.into_values().collect();
    for n in &mut nodes {
        if n.label.is_empty() {
            n.label = "Entity".into();
        }
    }

    // Drop relations whose endpoints weren't extracted as entities (model
    // hallucination) — otherwise the bulk load would reject the batch.
    let keys: HashSet<&str> = nodes.iter().map(|n| n.key.as_str()).collect();
    let before = edges.len();
    edges.retain(|e| keys.contains(e.src.as_str()) && keys.contains(e.dst.as_str()));
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
        description: Some("LLM-extracted summary".into()),
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

fn embed_text(n: &DigestNode) -> String {
    let summary = match n.props.get("summary").map(|p| &p.value) {
        Some(PropValue::Str(s)) => s.as_str(),
        _ => "",
    };
    format!("{} ({}): {}", n.key, n.label, summary)
        .trim()
        .to_string()
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

fn system_prompt(catalog: Option<&CatalogSnapshot>) -> String {
    let mut p = String::from(
        "You extract a knowledge graph from the user's text. Reply with ONLY strict JSON, no prose, \
         no markdown fences, in exactly this shape:\n\
         {\"entities\":[{\"key\":\"stable canonical name\",\"label\":\"EntityType\",\
         \"properties\":{\"...\":\"...\"},\"description\":\"one concise sentence\"}],\
         \"relations\":[{\"src\":\"entity key\",\"dst\":\"entity key\",\"type\":\"REL_TYPE\",\
         \"description\":\"one concise sentence\"}]}\n\
         Use the SAME key for an entity every time it appears so mentions collapse to one node. \
         Relations must reference entity keys you also emit.",
    );
    if let Some(cat) = catalog
        && (!cat.labels.is_empty() || !cat.edge_types.is_empty())
    {
        if !cat.labels.is_empty() {
            let labels: Vec<&str> = cat.labels.keys().map(String::as_str).collect();
            p.push_str(&format!(
                "\nPrefer these existing labels where they fit: {}.",
                labels.join(", ")
            ));
        }
        if !cat.edge_types.is_empty() {
            let types: Vec<&str> = cat.edge_types.keys().map(String::as_str).collect();
            p.push_str(&format!(
                "\nPrefer these existing relation types where they fit: {}.",
                types.join(", ")
            ));
        }
    }
    p
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
