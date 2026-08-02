//! Per-entity refinement — stage 3 of extraction precision (ROADMAP §8).
//!
//! Stages 1 and 2 settled *what* the graph contains. This settles what it
//! *says*. One-round extraction merges chunks positionally and `merge_props` is
//! explicit about the cost — "never clobbering an existing key (first chunk
//! wins)" — so an entity's properties and description come from whichever chunk
//! happened to mention it first, and every later, better mention is discarded.
//! This pass puts every mention in front of the model at once and asks again.
//!
//! **Gate on possibility, rank on value** (ROADMAP §8, settled). Refinement can
//! only add something when an entity is mentioned *outside* the chunks that
//! produced it, and that is computable for free — so entities with nothing new
//! to read cost nothing to skip. What survives is ranked by degree and property
//! sparsity, because the measured graph shows importance and thinness coincide:
//! the hubs are the thinnest.
//!
//! Two budgets, both unlimited by default, because the larger cost is *input*
//! rather than calls — a hub mentioned throughout a document would otherwise
//! carry nearly the whole text as context.
//!
//! Entities are refined one per call. Batching would cut the call count and let
//! entities contaminate each other's answer; per-entity isolation is what makes
//! this pass trustworthy.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use dr_strange_core::{Analyzer, Language, Properties};
use serde::Deserialize;

use crate::provider::Chat;

/// What refinement cost and changed.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineReport {
    /// Entities that had mentions beyond their own chunks, so were worth asking
    /// about.
    pub eligible: usize,
    /// Entities actually refined (`eligible`, less whatever the budget cut).
    pub refined: usize,
    /// Entities skipped because nothing outside their own chunks mentions them.
    pub skipped_nothing_new: usize,
    /// Properties added that extraction had missed.
    pub props_added: usize,
    /// Properties whose value the fuller reading changed.
    pub props_revised: usize,
    pub chat_requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One entity's case for refinement: where it is mentioned, and how much it
/// stands to gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub key: String,
    /// Chunks mentioning it that did not produce it — the new reading.
    pub fresh: Vec<usize>,
    /// Every chunk to show the model, producing chunks included, in order.
    pub context: Vec<usize>,
    /// Ranking signal: incident edges, then how few properties it carries.
    pub degree: usize,
    pub props: usize,
}

/// The model's reply: the entity's properties as it now reads them.
#[derive(Deserialize, Default)]
pub(crate) struct Refined {
    #[serde(default)]
    properties: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
}

/// Find every chunk mentioning each entity, and keep the entities that have
/// something to gain. Model-free: this is the gate, and it must cost nothing.
///
/// Matching is BM25's own tokenizer over the chunk text, so an entity is found
/// however the document spaced or cased it — the same analyzer the keyword
/// index uses, rather than a second notion of what a word is.
pub(crate) fn candidates(
    chunks: &[String],
    origins: &BTreeMap<String, BTreeSet<usize>>,
    degrees: &BTreeMap<String, usize>,
    prop_counts: &BTreeMap<String, usize>,
    max_context: Option<usize>,
) -> Vec<Candidate> {
    let analyzer = Analyzer::new(Language::English);
    // Each chunk's token multiset, once — every entity is looked up against it.
    let tokens: Vec<BTreeSet<String>> = chunks
        .iter()
        .map(|c| analyzer.analyze(c).into_iter().collect())
        .collect();

    let mut out = Vec::new();
    for (key, produced_in) in origins {
        let wanted: Vec<String> = analyzer.analyze(key);
        if wanted.is_empty() {
            continue; // nothing to match on — punctuation or a stopword alone
        }
        // A chunk mentions the entity when it carries every one of its tokens.
        let mentions: Vec<usize> = (0..chunks.len())
            .filter(|i| wanted.iter().all(|w| tokens[*i].contains(w)))
            .collect();
        let fresh: Vec<usize> = mentions
            .iter()
            .copied()
            .filter(|i| !produced_in.contains(i))
            .collect();
        if fresh.is_empty() {
            continue; // the producing chunks already said everything there is
        }
        // Producing chunks first — they are what the current values came from —
        // then the new reading, in document order.
        let mut context: Vec<usize> = produced_in.iter().copied().collect();
        context.extend(&fresh);
        context.sort_unstable();
        context.dedup();
        if let Some(cap) = max_context {
            context.truncate(cap.max(1));
        }
        out.push(Candidate {
            key: key.clone(),
            fresh,
            context,
            degree: degrees.get(key).copied().unwrap_or(0),
            props: prop_counts.get(key).copied().unwrap_or(0),
        });
    }
    // Most connected first, then thinnest, then by key so a re-run agrees.
    out.sort_by(|a, b| {
        b.degree
            .cmp(&a.degree)
            .then(a.props.cmp(&b.props))
            .then(a.key.cmp(&b.key))
    });
    out
}

/// Re-read one entity with every mention in front of the model.
pub(crate) fn refine_one(
    chat: &dyn Chat,
    chunks: &[String],
    candidate: &Candidate,
    label: &str,
    current: &Properties,
    relations: &[String],
    report: &mut RefineReport,
) -> Result<Option<Refined>> {
    let system = "You re-read one entity from a document that was first extracted piece by piece, \
         so its properties came from whichever piece happened to mention it first.\n\
         \n\
         Below are ALL the passages mentioning it, what is currently recorded, and how it relates \
         to other entities. Reply with ONLY {\"properties\": {…}, \"description\": \"…\"} — the \
         entity's properties as the whole document supports them.\n\
         \n\
         Every value must be stated or directly implied by the passages. Keep a current value \
         unless the passages contradict or sharpen it. Add what the passages support and the \
         current reading missed. Never invent a property the text does not support, and never \
         answer about a different entity. Prefer a specific value over a vague one, and keep the \
         description to one or two sentences.\n";

    let mut user = format!("Entity: {}\nKind: {label}\n", candidate.key);
    if !relations.is_empty() {
        user.push_str("\nHow it relates to other entities:\n");
        for r in relations {
            user.push_str(&format!("  {r}\n"));
        }
    }
    user.push_str("\nCurrently recorded:\n");
    for (k, v) in current {
        if k.starts_with('_') || k == "embedding" {
            continue; // provenance and vectors are not the model's business
        }
        let shown = dr_strange_core::json::value_to_json(&v.value).to_string();
        user.push_str(&format!(
            "  {k}: {}\n",
            shown.chars().take(200).collect::<String>()
        ));
    }
    user.push_str("\nEvery passage mentioning it:\n");
    for i in &candidate.context {
        if let Some(text) = chunks.get(*i) {
            user.push_str(&format!("--- passage {i} ---\n{text}\n"));
        }
    }

    let reply = chat.complete(system, &user)?;
    report.chat_requests += 1;
    report.input_tokens += reply.input_tokens;
    report.output_tokens += reply.output_tokens;
    Ok(serde_json::from_str::<Refined>(crate::ask::extract_json(&reply.text)).ok())
}

/// Fold a refinement into an entity's properties. Unlike extraction's
/// first-wins merge this one may *replace* a value — that is the point — but
/// only where the fuller reading actually differs, and never for provenance or
/// the vector.
pub(crate) fn apply(props: &mut Properties, refined: &Refined, report: &mut RefineReport) {
    let mut incoming: Vec<(String, serde_json::Value)> = refined
        .properties
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(d) = &refined.description {
        incoming.push((
            "description".to_string(),
            serde_json::Value::String(d.clone()),
        ));
    }
    for (k, v) in incoming {
        if k.starts_with('_') || k == "embedding" {
            continue;
        }
        let Ok(value) = dr_strange_core::json::json_to_value(&v) else {
            continue;
        };
        match props.get(&k) {
            Some(existing) if existing.value == value => {}
            Some(_) => {
                report.props_revised += 1;
                props.insert(k, crate::digest::prop(value));
            }
            None => {
                report.props_added += 1;
                props.insert(k, crate::digest::prop(value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origins(pairs: &[(&str, &[usize])]) -> BTreeMap<String, BTreeSet<usize>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().copied().collect()))
            .collect()
    }

    const CHUNKS: [&str; 3] = [
        "The Transformer uses multi-head attention throughout its encoder.",
        "Softmax normalizes the attention weights.",
        "The Transformer was trained on eight GPUs for twelve hours.",
    ];

    fn chunks() -> Vec<String> {
        CHUNKS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_entity_mentioned_only_where_it_was_found_is_skipped() {
        // Softmax appears in chunk 1 alone, which is also where it came from:
        // re-reading it would show the model exactly what it already saw.
        let c = candidates(
            &chunks(),
            &origins(&[("Softmax", &[1])]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert!(c.is_empty(), "nothing new to read, so nothing to ask");
    }

    #[test]
    fn an_entity_mentioned_elsewhere_is_eligible_and_carries_both_passages() {
        // Transformer came from chunk 0 but is also discussed in chunk 2.
        let c = candidates(
            &chunks(),
            &origins(&[("Transformer", &[0])]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].fresh, vec![2], "chunk 2 is the new reading");
        assert_eq!(
            c[0].context,
            vec![0, 2],
            "and chunk 0 is why it says what it says"
        );
    }

    #[test]
    fn matching_survives_the_documents_own_spacing_and_case() {
        let chunks = vec![
            "We introduce Multi-Head Attention.".to_string(),
            "The multi head attention layer is fast.".to_string(),
        ];
        let c = candidates(
            &chunks,
            &origins(&[("Multi-Head Attention", &[0])]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            None,
        );
        assert_eq!(c.len(), 1, "the analyzer sees one name, however written");
        assert_eq!(c[0].fresh, vec![1]);
    }

    #[test]
    fn the_most_connected_and_thinnest_come_first() {
        let o = origins(&[("Transformer", &[0]), ("Softmax", &[0])]);
        let chunks = vec![
            "Transformer and Softmax.".to_string(),
            "Transformer again. Softmax again.".to_string(),
        ];
        let degrees: BTreeMap<String, usize> =
            [("Transformer".to_string(), 9), ("Softmax".to_string(), 1)]
                .into_iter()
                .collect();
        let props: BTreeMap<String, usize> =
            [("Transformer".to_string(), 1), ("Softmax".to_string(), 1)]
                .into_iter()
                .collect();
        let c = candidates(&chunks, &o, &degrees, &props, None);
        assert_eq!(c[0].key, "Transformer", "the hub is asked about first");
    }

    #[test]
    fn context_is_capped_without_losing_determinism() {
        let chunks: Vec<String> = (0..10).map(|_| "Transformer here.".to_string()).collect();
        let c = candidates(
            &chunks,
            &origins(&[("Transformer", &[0])]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            Some(3),
        );
        assert_eq!(
            c[0].context.len(),
            3,
            "a hub cannot drag in the whole document"
        );
        assert_eq!(
            c,
            candidates(
                &chunks,
                &origins(&[("Transformer", &[0])]),
                &BTreeMap::new(),
                &BTreeMap::new(),
                Some(3)
            )
        );
    }

    #[test]
    fn applying_adds_what_was_missed_and_revises_what_was_wrong() {
        use dr_strange_core::{PropValue, Properties};
        let mut props = Properties::new();
        props.insert("layers".into(), crate::digest::prop(PropValue::Int(4)));
        props.insert(
            "name".into(),
            crate::digest::prop(PropValue::Str("T".into())),
        );

        let refined: Refined = serde_json::from_str(
            r#"{"properties":{"layers":6,"heads":8,"name":"T"},"description":"An architecture."}"#,
        )
        .unwrap();
        let mut report = RefineReport::default();
        apply(&mut props, &refined, &mut report);

        assert_eq!(props["layers"].value, PropValue::Int(6), "corrected");
        assert_eq!(props["heads"].value, PropValue::Int(8), "added");
        assert_eq!(report.props_revised, 1, "only `layers` actually changed");
        assert_eq!(report.props_added, 2, "`heads` and `description`");
    }

    #[test]
    fn provenance_and_vectors_are_never_touched() {
        use dr_strange_core::{PropValue, Properties};
        let mut props = Properties::new();
        props.insert(
            "_run".into(),
            crate::digest::prop(PropValue::Str("r1".into())),
        );
        props.insert(
            "embedding".into(),
            crate::digest::prop(PropValue::Vector(vec![1.0])),
        );
        let refined: Refined =
            serde_json::from_str(r#"{"properties":{"_run":"hacked","embedding":[9.9]}}"#).unwrap();
        let mut report = RefineReport::default();
        apply(&mut props, &refined, &mut report);
        assert_eq!(props["_run"].value, PropValue::Str("r1".into()));
        assert_eq!(props["embedding"].value, PropValue::Vector(vec![1.0]));
        assert_eq!(report.props_revised + report.props_added, 0);
    }
}
