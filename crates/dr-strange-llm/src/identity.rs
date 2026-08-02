//! Identity resolution — stage 2 of extraction precision (ROADMAP §8).
//!
//! Stage 1 reconciled the *vocabulary*: what kinds of thing exist, and what
//! kinds of relationship. This pass reconciles the *things*. Chunks read
//! independently name the same entity differently, and one digest of a single
//! paper produced `Multi-Head Attention` beside `Multi-head attention`,
//! `Softmax` beside `softmax`, and `K` as a separate node from `Key`.
//!
//! Candidates come from cheap signals, and only what survives them reaches the
//! model:
//!
//! 1. **Folding**, free: keys differing only in case or separators are one key
//!    (`Softmax`/`softmax`, `d_model`/`dmodel`). Same rule stage 1 uses.
//! 2. **Containment**, free to *propose*: one key contained in another is a
//!    candidate — `K` in `Key`, `Transformer` in `Transformer (big)` — but the
//!    two cases end differently, so the model decides each. `K` and `Key` are
//!    one entity written twice; `Transformer (big)` is a *variant of*
//!    `Transformer`, a thing in its own right with its own edges. Getting this
//!    wrong in either direction loses information, which is why no string rule
//!    is allowed to settle it.
//!
//! Merging rewrites edge endpoints onto the survivor and keeps the absorbed
//! key as `_key_as_written` — the same alias mechanism stage 1 established,
//! inherited unchanged (ROADMAP §8, settled).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::provider::Chat;
use crate::reconcile::{Renames, fold_key, resolve_chains};

/// What identity resolution cost and changed.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityReport {
    /// Entities before the pass.
    pub before: usize,
    /// Entities after it.
    pub after: usize,
    /// Keys folded by the deterministic rule, with no model involved.
    pub folded: usize,
    /// Keys the model judged to name the same entity as another.
    pub merged: usize,
    /// Containment pairs put to the model.
    pub adjudicated: usize,
    /// Relations that became the same `(src, dst, type)` once endpoints moved.
    pub merged_relations: usize,
    pub chat_requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One candidate pair, ordered so `inner` is the shorter (possible alias) and
/// `outer` the longer (possible distinct, more specific thing).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pair {
    inner: String,
    outer: String,
}

/// The model's verdict per pair: `{"same": ["<inner>|<outer>", …]}` — only the
/// pairs that name one entity, so silence means "keep them apart".
#[derive(serde::Deserialize, Default)]
struct SameReply {
    #[serde(default)]
    same: Vec<String>,
}

/// Keys that are one key written differently. Survivor: the most frequent
/// spelling, ties lexicographic, so a re-run folds identically.
fn fold_keys(counts: &BTreeMap<String, usize>) -> Renames {
    let mut groups: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for key in counts.keys() {
        groups.entry(fold_key(key)).or_default().push(key);
    }
    let mut renames = Renames::new();
    for (_, mut keys) in groups {
        if keys.len() < 2 {
            continue;
        }
        keys.sort_by_key(|k| (std::cmp::Reverse(counts[*k]), (*k).clone()));
        let survivor = keys[0].clone();
        for other in &keys[1..] {
            renames.insert((*other).clone(), survivor.clone());
        }
    }
    renames
}

/// Pairs where one key is contained in another — an alias, or a more specific
/// thing. Deterministic order, capped per key and in total so no document can
/// explode the prompt.
///
/// Pairs whose entities carry **different labels** are never proposed. Stage 1
/// has just canonicalized the label vocabulary, so by now a disagreement is
/// real, and it is decisive: a live run merged `dropout` into `Dropout: a
/// simple way to prevent neural networks from overfitting` and
/// `machine translation` into `Google's neural machine translation system: …`
/// — a technique and a task swallowed by the *papers about them*. The prompt
/// forbids exactly that and the model did it anyway; the labels (`Technique`
/// vs `Paper`) rule it out for free, before anything is asked.
fn containment_pairs(keys: &[String], labels: &BTreeMap<String, String>) -> Vec<Pair> {
    const MAX_PER_KEY: usize = 4;
    const MAX_PAIRS: usize = 200;
    let comparable = |a: &str, b: &str| match (labels.get(a), labels.get(b)) {
        // An unlabelled entity carries no evidence either way, so it is judged
        // on its name like before.
        (Some(x), Some(y)) if !x.is_empty() && !y.is_empty() => x.eq_ignore_ascii_case(y),
        _ => true,
    };
    let mut pairs = BTreeSet::new();
    for inner in keys {
        let folded_inner = fold_key(inner);
        if folded_inner.is_empty() {
            continue;
        }
        let mut taken = 0;
        for outer in keys {
            if inner == outer || taken >= MAX_PER_KEY || !comparable(inner, outer) {
                continue;
            }
            let folded_outer = fold_key(outer);
            // Contained, and genuinely shorter — equal folds are stage 1's job.
            if folded_outer.len() > folded_inner.len() && folded_outer.contains(&folded_inner) {
                pairs.insert(Pair {
                    inner: inner.clone(),
                    outer: outer.clone(),
                });
                taken += 1;
            }
        }
    }
    // Bounded overall: the set is ordered, so the cut is reproducible.
    pairs.into_iter().take(MAX_PAIRS).collect()
}

/// Ask the model which containment pairs name one entity. Anything it does not
/// name stays two entities — the safe default, since splitting a merged entity
/// afterwards is impossible while merging later is not.
fn adjudicate(
    chat: &dyn Chat,
    pairs: &[Pair],
    descriptions: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
    report: &mut IdentityReport,
) -> Result<Renames> {
    if pairs.is_empty() {
        return Ok(Renames::new());
    }
    let system = "You decide which of these entity-name pairs refer to the SAME entity.\n\
         Each pair comes from one document, read in independent pieces, so the same entity may \
         have been written two ways — but a longer name is just as often a DIFFERENT, more \
         specific thing.\n\
         \n\
         Reply with ONLY {\"same\": [\"<pair>\", …]} listing the pairs, verbatim as given, whose \
         two names denote one entity. Omit every pair that names two things; an empty list is a \
         valid and common answer.\n\
         \n\
         Same entity: an abbreviation and its expansion (`K` / `Key`), or the same name written \
         differently.\n\
         DIFFERENT entities: a thing and a variant, configuration, size or instance of it \
         (`Transformer` / `Transformer (big)`), a whole and its part (`Encoder` / \
         `Encoder-Decoder Structure`), or a thing and a work about it (`Attention` / \
         `Attention Is All You Need`). When unsure, leave the pair out.\n";
    let mut user = String::from("Pairs:\n");
    for p in pairs {
        user.push_str(&format!("{}|{}\n", p.inner, p.outer));
    }
    // A one-line description apiece is what makes an abbreviation decidable.
    let described: Vec<&String> = pairs
        .iter()
        .flat_map(|p| [&p.inner, &p.outer])
        .filter(|k| descriptions.contains_key(*k))
        .collect();
    if !described.is_empty() {
        user.push_str("\nWhat each name was said to be:\n");
        let mut seen = BTreeSet::new();
        for key in described {
            if seen.insert(key) {
                let d = &descriptions[key];
                let label = labels.get(key).map(String::as_str).unwrap_or("");
                user.push_str(&format!(
                    "{key} [{label}]: {}\n",
                    d.chars().take(160).collect::<String>()
                ));
            }
        }
    }

    let reply = chat.complete(system, &user)?;
    report.chat_requests += 1;
    report.input_tokens += reply.input_tokens;
    report.output_tokens += reply.output_tokens;
    report.adjudicated = pairs.len();

    let parsed: SameReply =
        serde_json::from_str(crate::ask::extract_json(&reply.text)).unwrap_or_default();
    let valid: BTreeSet<String> = pairs
        .iter()
        .map(|p| format!("{}|{}", p.inner, p.outer))
        .collect();
    let mut renames = Renames::new();
    for line in parsed.same {
        let line = line.trim();
        if !valid.contains(line) {
            continue; // invented pair — ignore
        }
        if let Some((inner, outer)) = line.split_once('|') {
            // The longer, more explicit name survives: it is the one a reader
            // (or a later question) is most likely to use.
            renames.insert(inner.to_string(), outer.to_string());
        }
    }
    Ok(resolve_chains(renames))
}

/// Resolve entity identity across one run's extraction. Returns the key
/// renames to apply — absorbed key → survivor — and what the pass cost.
pub(crate) fn resolve(
    chat: &dyn Chat,
    counts: &BTreeMap<String, usize>,
    descriptions: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> Result<(Renames, IdentityReport)> {
    let mut report = IdentityReport {
        before: counts.len(),
        after: counts.len(),
        ..Default::default()
    };
    if counts.len() < 2 {
        return Ok((Renames::new(), report));
    }

    let folded = fold_keys(counts);
    report.folded = folded.len();

    // Containment is judged among the survivors of folding only.
    let survivors: Vec<String> = counts
        .keys()
        .filter(|k| !folded.contains_key(*k))
        .cloned()
        .collect();
    let pairs = containment_pairs(&survivors, labels);
    let merged = adjudicate(chat, &pairs, descriptions, labels, &mut report)?;
    report.merged = merged.len();

    let mut renames = folded;
    for (from, into) in &merged {
        renames.insert(from.clone(), into.clone());
    }
    let renames = resolve_chains(renames);
    report.after = counts.len() - renames.len();
    Ok((renames, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;

    fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    #[test]
    fn keys_differing_only_in_spelling_fold_without_a_model() {
        for (a, b) in [
            ("Softmax", "softmax"),
            ("Multi-Head Attention", "Multi-head attention"),
            ("d_model", "dmodel"),
            (
                "Scaled Dot-Product Attention",
                "scaled dot-product attention",
            ),
        ] {
            let r = fold_keys(&counts(&[(a, 3), (b, 1)]));
            assert_eq!(r.get(b), Some(&a.to_string()), "{b} should fold into {a}");
        }
    }

    #[test]
    fn containment_proposes_both_the_alias_and_the_variant() {
        let keys: Vec<String> = ["K", "Key", "Transformer", "Transformer (big)"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pairs = containment_pairs(&keys, &BTreeMap::new());
        assert!(pairs.iter().any(|p| p.inner == "K" && p.outer == "Key"));
        assert!(
            pairs
                .iter()
                .any(|p| p.inner == "Transformer" && p.outer == "Transformer (big)")
        );
    }

    #[test]
    fn the_model_separates_the_alias_from_the_variant() {
        // It names only the abbreviation pair; the variant pair goes unmentioned
        // and so survives as its own entity.
        let chat = MockProvider::new(vec![r#"{"same":["K|Key"]}"#.into()], 4);
        let c = counts(&[
            ("K", 2),
            ("Key", 3),
            ("Transformer", 9),
            ("Transformer (big)", 4),
        ]);
        let (renames, report) = resolve(&chat, &c, &BTreeMap::new(), &BTreeMap::new()).unwrap();
        assert_eq!(renames.get("K"), Some(&"Key".to_string()));
        assert!(
            !renames.contains_key("Transformer"),
            "a variant is not an alias"
        );
        assert_eq!(report.merged, 1);
        assert_eq!(report.after, 3);
    }

    #[test]
    fn silence_keeps_entities_apart() {
        // Merging later is possible; un-merging is not — so no answer means no
        // merge, and a garbled answer likewise.
        for reply in [
            r#"{"same":[]}"#,
            "I'm not sure",
            r#"{"same":["A|NOT_A_PAIR"]}"#,
        ] {
            let chat = MockProvider::new(vec![reply.into()], 4);
            let c = counts(&[("Trans", 1), ("Transformer", 1)]);
            let (renames, _) = resolve(&chat, &c, &BTreeMap::new(), &BTreeMap::new()).unwrap();
            assert!(renames.is_empty(), "reply {reply:?} should merge nothing");
        }
    }

    #[test]
    fn descriptions_are_offered_so_an_abbreviation_is_decidable() {
        let chat = MockProvider::new(vec![r#"{"same":["K|Key"]}"#.into()], 4);
        let d: BTreeMap<String, String> = [
            ("K".to_string(), "The key matrix.".to_string()),
            (
                "Key".to_string(),
                "The key vectors of attention.".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let (renames, report) = resolve(
            &chat,
            &counts(&[("K", 1), ("Key", 1)]),
            &d,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(renames["K"], "Key");
        assert_eq!(report.adjudicated, 1);
    }

    #[test]
    fn nothing_to_compare_costs_no_call() {
        let chat = MockProvider::new(vec!["{}".into()], 4);
        // Two unrelated keys: neither folds nor contains the other.
        let (renames, report) = resolve(
            &chat,
            &counts(&[("alpha", 1), ("beta", 1)]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(renames.is_empty());
        assert_eq!(report.chat_requests, 0, "no candidate pairs, no call");
    }

    #[test]
    fn a_label_disagreement_blocks_the_pair_before_it_is_asked() {
        // The live failure: a technique swallowed by the paper about it. The
        // model was told not to and did it anyway; the labels settle it for
        // free, so the pair is never put.
        let labels: BTreeMap<String, String> = [
            ("dropout".to_string(), "Technique".to_string()),
            (
                "Dropout: a simple way to prevent overfitting".to_string(),
                "Paper".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let keys: Vec<String> = labels.keys().cloned().collect();
        assert!(
            containment_pairs(&keys, &labels).is_empty(),
            "a Technique and a Paper are not two names for one thing"
        );
        // The same names with one label agree ⇒ still asked.
        let agreeing: BTreeMap<String, String> = keys
            .iter()
            .map(|k| (k.clone(), "Technique".to_string()))
            .collect();
        assert_eq!(containment_pairs(&keys, &agreeing).len(), 1);
    }

    #[test]
    fn an_unlabelled_entity_is_still_judged_on_its_name() {
        let labels: BTreeMap<String, String> = [("Key".to_string(), "Matrix".to_string())]
            .into_iter()
            .collect();
        let keys = vec!["K".to_string(), "Key".to_string()];
        assert_eq!(
            containment_pairs(&keys, &labels).len(),
            1,
            "no label on one side is no evidence, not a veto"
        );
    }

    #[test]
    fn the_model_never_sees_an_unbounded_pair_list() {
        // Every key contains "a", so without a cap this would be quadratic.
        let keys: Vec<String> = (0..400).map(|i| format!("a{i}")).collect();
        let pairs = containment_pairs(&keys, &BTreeMap::new());
        assert!(
            pairs.len() <= 200,
            "pair list must stay bounded: {}",
            pairs.len()
        );
        // And the cut is reproducible.
        assert_eq!(pairs, containment_pairs(&keys, &BTreeMap::new()));
    }
}
