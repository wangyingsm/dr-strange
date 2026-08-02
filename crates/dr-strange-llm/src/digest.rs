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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use dr_strange_core::json;
use dr_strange_core::{
    BulkEdge, BulkNode, BulkStats, Metric, PropDesc, PropValue, Properties, WriteTxn,
};
use serde::Deserialize;
use serde_json::Value;

use crate::identity::{self, IdentityReport};
use crate::provider::{Chat, Embedder, OutputTruncated};
use crate::reconcile::{self, ReconcileReport};
use crate::refine::{self, RefineReport};

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

    /// Which of `keys` the target graph already holds, looked up **exactly**.
    ///
    /// Similarity search cannot answer this: it depends on the plane's nodes
    /// carrying usable embeddings, and a plane digested without an embedder
    /// carries none — every vector empty, every search empty, every entity
    /// proposed as new, and a second digest writing a *second* node under a key
    /// the plane already holds (ROADMAP §8). An exact lookup has no such
    /// dependency, so identity survives whatever the embeddings are doing.
    ///
    /// Defaults to "none", for sources that cannot look keys up.
    fn existing_keys(&self, _keys: &[String]) -> Result<Vec<ExistingEntity>> {
        Ok(Vec::new())
    }
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

    fn existing_keys(&self, keys: &[String]) -> Result<Vec<ExistingEntity>> {
        let mut found = Vec::new();
        for key in keys {
            let node = self
                .plane
                .node_by_key(key)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Some(n) = node {
                found.push(ExistingEntity {
                    key: key.clone(),
                    label: n.labels.into_iter().next().unwrap_or_default(),
                    description: match n.properties.get("description").map(|p| &p.value) {
                        Some(PropValue::Str(s)) => s.clone(),
                        _ => String::new(),
                    },
                });
            }
        }
        Ok(found)
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
    /// Relations that became the same `(src, dst, type)` once edge types were
    /// reconciled, and so collapsed into one.
    pub merged_relations: usize,
    /// What reconciling the entity-label vocabulary cost and changed (ROADMAP §8).
    pub labels: ReconcileReport,
    /// The same, for the edge-type vocabulary.
    pub edge_types: ReconcileReport,
    /// What identity resolution cost and changed (ROADMAP §8 stage 2).
    pub identity: IdentityReport,
    /// What per-entity refinement cost and changed (ROADMAP §8 stage 3).
    pub refined: RefineReport,
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
    /// How many per-chunk extraction chat calls to run concurrently. `1` is
    /// fully sequential; higher values overlap the (slow, network-bound) LLM
    /// requests. Clamped to `[1, chunk count]`.
    pub concurrency: usize,
    /// Reconcile the label and edge-type vocabularies after extraction
    /// (ROADMAP §8 stage 1). Costs O(1) chat calls in document size and folds
    /// the spelling variants independent chunks inevitably produce.
    pub reconcile: bool,
    /// Merge extracted entities that name the same thing (ROADMAP §8 stage 2),
    /// and check every remaining key against the graph exactly so a re-digest
    /// links rather than duplicating.
    pub resolve_identity: bool,
    /// Re-read each entity against every passage mentioning it (ROADMAP §8
    /// stage 3), repairing properties that one-round extraction fixed from
    /// whichever chunk mentioned the entity first. Costs one call per entity
    /// that has something new to read; the two caps below bound it.
    pub refine: bool,
    /// Cap on entities refined. `None` — the default — refines every eligible
    /// one, with the cost visible in the report.
    pub refine_max_entities: Option<usize>,
    /// Cap on passages shown per entity. `None` refines against every mention;
    /// this is the budget that matters, since a hub can otherwise carry most of
    /// the document into one call.
    pub refine_max_context: Option<usize>,
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
    chat: &(dyn Chat + Sync),
    embedder: &dyn Embedder,
    candidates: Option<&dyn CandidateSource>,
    opts: &DigestOptions,
) -> Result<DigestResult> {
    let chunks = chunk(document, opts.chunk_chars);
    // A span so the per-chunk chat calls (and any provider warnings emitted in
    // dr-strange-llm::openai) nest under one identifiable digest run.
    let _span = tracing::info_span!(
        "digest",
        run_id = %opts.run_id,
        chunks = chunks.len(),
        embed = opts.embed,
    )
    .entered();
    let system = system_prompt(candidates.is_some());

    // Existing graph entities surfaced across all chunks (key → entity). Used
    // to (a) skip re-creating them as nodes and (b) treat them as valid
    // relation endpoints so new→existing edges survive.
    let mut existing: HashMap<String, ExistingEntity> = HashMap::new();
    let mut report = DigestReport {
        chunks: chunks.len(),
        ..Default::default()
    };

    // Phase A (sequential): entity-link each chunk into a reuse-context block.
    // Kept sequential so the candidate graph reads are never concurrent.
    let mut blocks: Vec<Option<String>> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let block = match candidates {
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
                block
            }
            None => None,
        };
        blocks.push(block);
    }

    // Phase B (concurrent): the per-chunk extraction chat calls — the slow,
    // network-bound part. Each recovers from a truncated reply by re-splitting.
    let extracts = extract_all(chat, &system, &chunks, &blocks, opts.concurrency)?;

    // Phase C (sequential, in chunk order): merge entities and relations. The
    // order is deterministic (chunk order, then sub-chunk order) despite the
    // parallel extraction above.
    let mut entities: BTreeMap<String, DigestNode> = BTreeMap::new();
    let mut edges: Vec<DigestEdge> = Vec::new();
    let mut seen_rel: HashSet<(String, String, String)> = HashSet::new();
    // Which chunk(s) produced each entity — stage 3 needs it to tell an entity
    // that has more to say elsewhere in the document from one that does not
    // (ROADMAP §8), and nothing else records it.
    let mut origins: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (chunk_index, extraction) in extracts.into_iter().enumerate() {
        report.chat_requests += extraction.chat_requests;
        report.input_tokens += extraction.input_tokens;
        report.output_tokens += extraction.output_tokens;
        for e in extraction.entities {
            origins
                .entry(e.key.clone())
                .or_default()
                .insert(chunk_index);
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

    // Stage 1 of extraction precision (ROADMAP §8): the chunks were read
    // independently, so the label and edge-type vocabularies never converged.
    // Reconcile each *set* — O(1) calls in document size — before anything
    // downstream sees them, keeping the document's own wording as an alias.
    if opts.reconcile {
        let label_counts = reconcile::tally(
            entities
                .values()
                .map(|n| n.label.as_str())
                .filter(|l| !l.is_empty()),
        );
        let (renames, r) = reconcile::reconcile(chat, "entity label", "", &label_counts)?;
        report.labels = r;
        for node in entities.values_mut() {
            if let Some(canonical) = renames.get(&node.label) {
                reconcile::note_original(
                    &mut node.props,
                    "_label_as_written",
                    "label as the document wrote it, before reconciliation",
                    &node.label,
                );
                node.label = canonical.clone();
            }
        }

        let type_counts = reconcile::tally(edges.iter().map(|e| e.ty.as_str()));
        let (renames, r) =
            reconcile::reconcile(chat, "edge type", reconcile::EDGE_RULE, &type_counts)?;
        report.edge_types = r;
        for edge in &mut edges {
            if let Some(canonical) = renames.get(&edge.ty) {
                reconcile::note_original(
                    &mut edge.props,
                    "_type_as_written",
                    "edge type as the document wrote it, before reconciliation",
                    &edge.ty,
                );
                edge.ty = canonical.clone();
            }
        }
        // Renaming can make two edges the same `(src, dst, ty)` triple; keep the
        // first in the existing deterministic order.
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        let before = edges.len();
        edges.retain(|e| seen.insert((e.src.clone(), e.dst.clone(), e.ty.clone())));
        report.merged_relations = before - edges.len();

        report.chat_requests += report.labels.chat_requests + report.edge_types.chat_requests;
        report.input_tokens += report.labels.input_tokens + report.edge_types.input_tokens;
        report.output_tokens += report.labels.output_tokens + report.edge_types.output_tokens;
        tracing::info!(
            labels = format!("{}→{}", report.labels.before, report.labels.after),
            edge_types = format!("{}→{}", report.edge_types.before, report.edge_types.after),
            folded = report.labels.folded + report.edge_types.folded,
            merged = report.labels.merged + report.edge_types.merged,
            collapsed_relations = report.merged_relations,
            "vocabulary reconciled",
        );
    }

    // Stage 2 of extraction precision (ROADMAP §8): the vocabulary is settled,
    // now the entities themselves. Independent chunks name one thing several
    // ways; merge those, keeping every absorbed key as an alias.
    if opts.resolve_identity {
        let key_counts = reconcile::tally(entities.keys().map(String::as_str));
        let descriptions: BTreeMap<String, String> = entities
            .iter()
            .filter_map(
                |(k, n)| match n.props.get("description").map(|p| &p.value) {
                    Some(PropValue::Str(d)) => Some((k.clone(), d.clone())),
                    _ => None,
                },
            )
            .collect();
        let labels: BTreeMap<String, String> = entities
            .iter()
            .map(|(k, n)| (k.clone(), n.label.clone()))
            .collect();
        let (renames, r) = identity::resolve(chat, &key_counts, &descriptions, &labels)?;
        report.identity = r;

        if !renames.is_empty() {
            // Fold each absorbed entity into its survivor: properties fill gaps
            // rather than overwrite (the survivor was named more often, so its
            // account is the better-attested one), and the absorbed key is kept.
            for (from, into) in &renames {
                let Some(absorbed) = entities.remove(from) else {
                    continue;
                };
                // The absorbed name's chunks are the survivor's chunks now.
                let moved = origins.remove(from).unwrap_or_default();
                origins.entry(into.clone()).or_default().extend(moved);
                let survivor = entities.entry(into.clone()).or_insert_with(|| DigestNode {
                    key: into.clone(),
                    label: absorbed.label.clone(),
                    props: Properties::new(),
                });
                if survivor.label.is_empty() {
                    survivor.label = absorbed.label;
                }
                for (k, v) in absorbed.props {
                    survivor.props.entry(k).or_insert(v);
                }
                reconcile::note_original(
                    &mut survivor.props,
                    "_key_as_written",
                    "another name this entity was written under, before identity resolution",
                    from,
                );
            }
            // Move every edge onto the surviving endpoints, then collapse the
            // duplicates that creates — and the self-loops, which are what two
            // names for one entity related to each other become.
            for edge in &mut edges {
                if let Some(into) = renames.get(&edge.src) {
                    edge.src = into.clone();
                }
                if let Some(into) = renames.get(&edge.dst) {
                    edge.dst = into.clone();
                }
            }
            let mut seen: HashSet<(String, String, String)> = HashSet::new();
            let before = edges.len();
            edges.retain(|e| {
                e.src != e.dst && seen.insert((e.src.clone(), e.dst.clone(), e.ty.clone()))
            });
            report.identity.merged_relations = before - edges.len();
        }

        report.chat_requests += report.identity.chat_requests;
        report.input_tokens += report.identity.input_tokens;
        report.output_tokens += report.identity.output_tokens;
        tracing::info!(
            entities = format!("{}→{}", report.identity.before, report.identity.after),
            folded = report.identity.folded,
            merged = report.identity.merged,
            adjudicated = report.identity.adjudicated,
            collapsed_relations = report.identity.merged_relations,
            "identity resolved",
        );
    }

    // Stage 3 (ROADMAP §8): the graph now holds the right things under the
    // right names, but each still says only what its first chunk said. Re-read
    // every entity that is mentioned somewhere its own chunks did not cover.
    if opts.refine {
        let degrees = {
            let mut d: BTreeMap<String, usize> = BTreeMap::new();
            for e in &edges {
                *d.entry(e.src.clone()).or_default() += 1;
                *d.entry(e.dst.clone()).or_default() += 1;
            }
            d
        };
        let prop_counts: BTreeMap<String, usize> = entities
            .iter()
            .map(|(k, n)| {
                let visible = n.props.keys().filter(|p| !p.starts_with('_')).count();
                (k.clone(), visible)
            })
            .collect();
        let mut candidates = refine::candidates(
            &chunks,
            &origins,
            &degrees,
            &prop_counts,
            opts.refine_max_context,
        );
        report.refined.eligible = candidates.len();
        report.refined.skipped_nothing_new = entities.len().saturating_sub(candidates.len());
        if let Some(cap) = opts.refine_max_entities {
            candidates.truncate(cap);
        }

        // One call per entity, run concurrently like the extraction round and
        // collected in candidate order, so a re-run applies the same answers in
        // the same sequence however the requests interleave.
        /// One entity's question: what to ask about, and everything the ask
        /// needs that lives in the entity map.
        type Ask<'a> = (&'a refine::Candidate, String, Properties, Vec<String>);
        let asks: Vec<Ask<'_>> = candidates
            .iter()
            .filter_map(|c| {
                let node = entities.get(&c.key)?;
                // How the entity sits in the graph: the relations are what make
                // a passage's mention interpretable.
                let relations: Vec<String> = edges
                    .iter()
                    .filter(|e| e.src == c.key || e.dst == c.key)
                    .map(|e| format!("{} --{}--> {}", e.src, e.ty, e.dst))
                    .collect();
                Some((c, node.label.clone(), node.props.clone(), relations))
            })
            .collect();

        let n = asks.len();
        type Answer = Result<(Option<refine::Refined>, RefineReport)>;
        let slots: Vec<Mutex<Option<Answer>>> = (0..n).map(|_| Mutex::new(None)).collect();
        let cursor = AtomicUsize::new(0);
        let workers = opts.concurrency.clamp(1, n.max(1));
        let (slots_ref, cursor_ref, asks_ref, chunks_ref) = (&slots, &cursor, &asks, &chunks);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(move || {
                    loop {
                        let i = cursor_ref.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        let (c, label, props, relations) = &asks_ref[i];
                        // Each worker tallies its own cost; the totals are
                        // summed in order below, so they never race.
                        let mut local = RefineReport::default();
                        let out = refine::refine_one(
                            chat, chunks_ref, c, label, props, relations, &mut local,
                        )
                        .map(|r| (r, local));
                        *slots_ref[i].lock().unwrap() = Some(out);
                    }
                });
            }
        });

        for (slot, (candidate, ..)) in slots.into_iter().zip(&asks) {
            // A refinement that failed leaves its entity exactly as extraction
            // left it. Aborting instead would throw away every chunk's
            // extraction and both earlier passes over one timed-out request —
            // and with one call per entity, some request failing is ordinary.
            let (refined, cost) = match slot.into_inner().unwrap().expect("every ask was run") {
                Ok(out) => out,
                Err(e) => {
                    report.refined.failed += 1;
                    tracing::warn!(entity = %candidate.key, error = %e, "entity left unrefined");
                    continue;
                }
            };
            report.refined.chat_requests += cost.chat_requests;
            report.refined.input_tokens += cost.input_tokens;
            report.refined.output_tokens += cost.output_tokens;
            if let (Some(r), Some(node)) = (refined, entities.get_mut(&candidate.key)) {
                refine::apply(&mut node.props, &r, &mut report.refined);
                report.refined.refined += 1;
            }
        }

        report.chat_requests += report.refined.chat_requests;
        report.input_tokens += report.refined.input_tokens;
        report.output_tokens += report.refined.output_tokens;
        tracing::info!(
            eligible = report.refined.eligible,
            refined = report.refined.refined,
            skipped = report.refined.skipped_nothing_new,
            props_added = report.refined.props_added,
            props_revised = report.refined.props_revised,
            failed = report.refined.failed,
            "entities refined",
        );
    }

    // Whatever keys remain, check them against the graph *exactly* — the one
    // duplicate-prevention path that does not depend on the plane's embeddings
    // being present and usable (ROADMAP §8). Without it a re-digest of a plane
    // with empty vectors writes a second node under a key it already holds.
    if let Some(src) = candidates {
        let keys: Vec<String> = entities.keys().cloned().collect();
        for e in src.existing_keys(&keys)? {
            existing.entry(e.key.clone()).or_insert(e);
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
    if report.dropped_relations > 0 {
        tracing::warn!(
            dropped = report.dropped_relations,
            "digest dropped relations whose endpoints were neither extracted nor existing nodes",
        );
    }
    tracing::info!(
        entities = report.entities,
        relations = report.relations,
        linked = report.linked,
        dropped = report.dropped_relations,
        input_tokens = report.input_tokens,
        output_tokens = report.output_tokens,
        embed_tokens = report.embed_tokens,
        "digest complete",
    );
    Ok(DigestResult {
        nodes,
        edges,
        report,
    })
}

// ---- helpers -------------------------------------------------------------

pub(crate) fn prop(value: PropValue) -> PropDesc {
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
/// One chunk's extracted entities and relations plus its token/request tally.
/// Produced in parallel (Phase B) and merged in chunk order (Phase C).
#[derive(Default)]
struct ChunkExtract {
    entities: Vec<ExEntity>,
    relations: Vec<ExRelation>,
    input_tokens: u64,
    output_tokens: u64,
    chat_requests: usize,
}

/// Runs every chunk's extraction chat call, up to `concurrency` at once, and
/// returns the results in chunk order. A bounded scoped-thread pool over an
/// atomic cursor; the chat provider is `Sync` and only immutable data is shared,
/// so no locks are held across a request. The first chunk to error aborts.
fn extract_all(
    chat: &(dyn Chat + Sync),
    system: &str,
    chunks: &[String],
    blocks: &[Option<String>],
    concurrency: usize,
) -> Result<Vec<ChunkExtract>> {
    let n = chunks.len();
    let slots: Vec<Mutex<Option<Result<ChunkExtract>>>> =
        (0..n).map(|_| Mutex::new(None)).collect();
    let cursor = AtomicUsize::new(0);
    let workers = concurrency.clamp(1, n.max(1));
    let slots_ref = &slots;
    let cursor_ref = &cursor;
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(move || {
                loop {
                    let i = cursor_ref.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let out = extract_chunk(chat, system, blocks[i].as_deref(), &chunks[i]);
                    *slots_ref[i].lock().unwrap() = Some(out);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().expect("every chunk was processed"))
        .collect()
}

/// Extracts one chunk. On a truncated reply (the chunk is too dense to fit the
/// model's output-token cap), splits the chunk and extracts each piece,
/// recursing until the pieces are small enough — or the chunk can no longer be
/// divided, in which case the truncation error is surfaced.
fn extract_chunk(
    chat: &(dyn Chat + Sync),
    system: &str,
    block: Option<&str>,
    text: &str,
) -> Result<ChunkExtract> {
    let user = match block {
        Some(b) => format!("{b}\n---\n{text}"),
        None => text.to_string(),
    };
    match chat.complete(system, &user) {
        Ok(reply) => {
            let extraction = parse_extraction(&reply.text)?;
            Ok(ChunkExtract {
                entities: extraction.entities,
                relations: extraction.relations,
                input_tokens: reply.input_tokens,
                output_tokens: reply.output_tokens,
                chat_requests: 1,
            })
        }
        Err(e) if e.downcast_ref::<OutputTruncated>().is_some() => {
            let pieces = chunk(text, text.chars().count() / 2);
            if pieces.len() < 2 {
                return Err(e); // indivisible — surface the truncation
            }
            let mut acc = ChunkExtract::default();
            for piece in &pieces {
                let sub = extract_chunk(chat, system, block, piece)?;
                acc.entities.extend(sub.entities);
                acc.relations.extend(sub.relations);
                acc.input_tokens += sub.input_tokens;
                acc.output_tokens += sub.output_tokens;
                acc.chat_requests += sub.chat_requests;
            }
            Ok(acc)
        }
        Err(e) => Err(e),
    }
}

fn chunk(doc: &str, size: usize) -> Vec<String> {
    let size = size.max(200);
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in doc.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        // A paragraph longer than `size` must itself be split — otherwise a doc
        // with few blank lines becomes one giant chunk whose extraction JSON
        // overruns the model's output limit and comes back truncated.
        for piece in split_paragraph(para, size) {
            if !cur.is_empty() && cur.len() + piece.len() + 2 > size {
                chunks.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(&piece);
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Break a paragraph into pieces of at most `size` chars, cutting at whitespace
/// where possible (char-safe: never splits a UTF-8 codepoint). Returns the
/// paragraph unchanged when it already fits.
fn split_paragraph(para: &str, size: usize) -> Vec<String> {
    if para.chars().count() <= size {
        return vec![para.to_string()];
    }
    let mut pieces = Vec::new();
    let mut cur = String::new();
    for word in para.split_whitespace() {
        // A single word longer than `size` gets hard-split by chars.
        if word.chars().count() > size {
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
            }
            for ch in word.chars() {
                if cur.chars().count() >= size {
                    pieces.push(std::mem::take(&mut cur));
                }
                cur.push(ch);
            }
            continue;
        }
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > size {
            pieces.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
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
         Relations must reference entity keys you also emit.\n\
         Write every `description` in the SAME language as the key it describes — a Chinese key \
         like \"数据库\" gets a Chinese description, an English key an English one; never translate \
         to English. Describe each relation in the language of its endpoint keys.",
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
        // Descriptions follow the key's language, not always English.
        assert!(p.contains("SAME language as the key"));
        assert!(p.contains("never translate"));
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

    #[test]
    fn chunk_bounds_even_a_single_giant_paragraph() {
        // One paragraph, no blank lines, far bigger than `size` — must still be
        // split so no chunk overruns the model's output limit.
        let para = "word ".repeat(500); // ~2500 chars, no "\n\n"
        let chunks = chunk(&para, 400);
        assert!(chunks.len() > 1, "oversized paragraph was not split");
        assert!(
            chunks.iter().all(|c| c.chars().count() <= 400),
            "a chunk exceeded the size bound"
        );
        // Reassembled words are preserved (whitespace-normalised).
        let joined: Vec<&str> = chunks.iter().flat_map(|c| c.split_whitespace()).collect();
        assert_eq!(joined.len(), 500);
        assert!(joined.iter().all(|w| *w == "word"));
    }

    #[test]
    fn split_paragraph_hard_splits_a_giant_word() {
        // A single token longer than `size` is cut by chars, never mid-codepoint.
        let word = "é".repeat(300); // multi-byte chars
        let pieces = split_paragraph(&word, 100);
        assert!(pieces.len() >= 3);
        assert!(pieces.iter().all(|p| p.chars().count() <= 100));
        assert_eq!(pieces.concat().chars().count(), 300);
    }
}
