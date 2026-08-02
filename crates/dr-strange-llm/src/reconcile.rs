//! Vocabulary reconciliation — stage 1 of extraction precision (ROADMAP §8).
//!
//! One-round extraction merges chunks positionally, and no chunk ever sees what
//! another extracted, so the vocabulary never converges: one digest of a single
//! paper produced 43 labels for 108 nodes (`Attention Mechanism` beside
//! `AttentionMechanism`) and 48 edge types for 113 edges (`COMPARED_TO`,
//! `COMPARED_WITH`, `COMPARED_IN` and `CONTRASTS_WITH` all at once, only the
//! last holding any data).
//!
//! This pass canonicalizes those two *sets* — not the nodes, not the edges, just
//! the vocabulary — so its cost is O(1) in document size: a few hundred tokens
//! however long the document was.
//!
//! Two steps, cheapest first:
//!
//! 1. **Deterministic folding**, free and model-free: names that differ only in
//!    case, spacing, underscores or hyphens are the same name. This catches
//!    `AttentionMechanism`/`Attention Mechanism` and `Softmax`/`softmax` with no
//!    call and no judgement.
//! 2. **Model adjudication** for what is left — genuine synonyms that no string
//!    rule can equate (`CONTRASTS_WITH` vs `COMPARED_WITH`).
//!
//! Whatever is renamed keeps the wording the document used, recorded beside the
//! canonical form as `_label_as_written` / `_type_as_written` (ROADMAP §8,
//! settled): provenance properties are hidden from the schema the model reads,
//! so the alias costs the read paths nothing and the original stays recoverable.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use dr_strange_core::{PropDesc, PropValue, Properties};
use serde::Deserialize;

use crate::provider::Chat;

/// A canonical-name mapping: every name that should be renamed, to what. Names
/// that keep their own spelling are absent, so an empty map means nothing moved.
pub(crate) type Renames = BTreeMap<String, String>;

/// What one reconciliation pass cost and changed.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Distinct names before the pass.
    pub before: usize,
    /// Distinct names after it.
    pub after: usize,
    /// Names folded by the deterministic rule, with no model involved.
    pub folded: usize,
    /// Names the model merged on meaning.
    pub merged: usize,
    pub chat_requests: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The model's reply: `{"groups": [["<survivor>", "<alias>", …], …]}`.
///
/// Asking for *groups* rather than a from→into map is what made this pass
/// reliable. The map framing was answered wildly differently run to run on the
/// same input — one live run merged 78 names, the next merged 15 — because
/// "emit the renames" invites the model to stop whenever it feels done.
/// Grouping is a bounded, checkable task over a list it can walk once.
#[derive(Deserialize, Default)]
struct GroupReply {
    #[serde(default)]
    groups: Vec<Vec<String>>,
}

/// Normalized form used to decide that two names are the same name written
/// differently: case, and every separator, ignored.
pub(crate) fn fold_key(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Fold names that differ only in case or separators, model-free. The survivor
/// of each group is the most frequently extracted spelling, ties broken
/// lexicographically so a re-run of the same input folds the same way.
fn fold(counts: &BTreeMap<String, usize>) -> Renames {
    let mut groups: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for name in counts.keys() {
        groups.entry(fold_key(name)).or_default().push(name);
    }
    let mut renames = Renames::new();
    for (_, mut names) in groups {
        if names.len() < 2 {
            continue;
        }
        // Most-used spelling wins; ties by name, so the choice is reproducible.
        names.sort_by_key(|n| (std::cmp::Reverse(counts[*n]), (*n).clone()));
        let canonical = names[0].clone();
        for other in &names[1..] {
            renames.insert((*other).clone(), canonical.clone());
        }
    }
    renames
}

/// Ask the model to merge the names that mean the same thing. `kind` names what
/// is being reconciled, for the prompt; `extra` carries any rule specific to it.
fn adjudicate(
    chat: &dyn Chat,
    kind: &str,
    extra: &str,
    names: &[String],
    counts: &BTreeMap<String, usize>,
    report: &mut ReconcileReport,
) -> Result<Renames> {
    // Nothing to merge into: one name is already canonical by definition.
    if names.len() < 2 {
        return Ok(Renames::new());
    }
    let system = format!(
        "You reconcile the {kind} vocabulary extracted from one document into a canonical set. \
         Different chunks of the document were read independently, so the same {kind} routinely \
         appears under several names, and your job is to find EVERY such family.\n\
         \n\
         Work through the whole list once, in order. Reply with ONLY a JSON object \
         {{\"groups\": [[\"<canonical>\", \"<other name>\", …], …]}} — one array per family of \
         names that denote the SAME thing, the canonical name first. Use each name at most once, \
         verbatim as given. Names with no synonym in the list belong to no group; do not list \
         them alone.\n\
         \n\
         Group two names ONLY when they denote the same thing. Keep them apart when they differ \
         in meaning, however similar they read.\n\
         {extra}\
         Put the more frequent, more explicit name first — it becomes the name the graph keeps. \
         The count after each name is how often it was extracted.\n"
    );
    let listed: Vec<String> = names
        .iter()
        .map(|n| match counts.get(n) {
            Some(c) => format!("{n}  ({c})"),
            None => n.clone(),
        })
        .collect();
    let user = format!("The {kind}s extracted:\n{}", listed.join("\n"));
    let reply = chat.complete(&system, &user)?;
    report.chat_requests += 1;
    report.input_tokens += reply.input_tokens;
    report.output_tokens += reply.output_tokens;

    let parsed: GroupReply =
        serde_json::from_str(crate::ask::extract_json(&reply.text)).unwrap_or_default();
    let valid: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    let mut renames = Renames::new();
    let mut claimed: BTreeSet<String> = BTreeSet::new();
    for group in parsed.groups {
        // Keep only real names, and only the first group to claim one — a name
        // in two families would otherwise make the rename order decide the
        // outcome.
        let members: Vec<String> = group
            .into_iter()
            .filter(|n| valid.contains(n.as_str()) && !claimed.contains(n))
            .collect();
        let Some((survivor, others)) = members.split_first() else {
            continue;
        };
        if others.is_empty() {
            continue; // a family of one renames nothing
        }
        claimed.insert(survivor.clone());
        for other in others {
            claimed.insert(other.clone());
            renames.insert(other.clone(), survivor.clone());
        }
    }
    Ok(resolve_chains(renames))
}

/// Collapse `A→B, B→C` into `A→C` so applying the map once is enough, and break
/// any cycle the model may have produced by dropping the offending entry.
pub(crate) fn resolve_chains(renames: Renames) -> Renames {
    let mut out = Renames::new();
    for from in renames.keys() {
        let mut target = &renames[from];
        let mut seen: BTreeSet<&String> = BTreeSet::from([from]);
        while let Some(next) = renames.get(target) {
            if !seen.insert(target) {
                break; // a cycle — stop where we are rather than loop
            }
            target = next;
        }
        if target != from {
            out.insert(from.clone(), target.clone());
        }
    }
    out
}

/// Reconcile one vocabulary: fold the spellings, then ask the model about what
/// remains. Returns the rename map to apply and what it cost.
pub(crate) fn reconcile(
    chat: &dyn Chat,
    kind: &str,
    extra: &str,
    counts: &BTreeMap<String, usize>,
) -> Result<(Renames, ReconcileReport)> {
    let mut report = ReconcileReport {
        before: counts.len(),
        after: counts.len(),
        ..Default::default()
    };
    if counts.len() < 2 {
        return Ok((Renames::new(), report));
    }

    let folded = fold(counts);
    report.folded = folded.len();

    // The model only sees what survived folding — fewer names, and no pairs it
    // could "merge" that were already the same name.
    let survivors: Vec<String> = counts
        .keys()
        .filter(|n| !folded.contains_key(*n))
        .cloned()
        .collect();
    let merged = adjudicate(chat, kind, extra, &survivors, counts, &mut report)?;
    report.merged = merged.len();

    // Fold first, then the model's merges — so a folded name follows its
    // survivor wherever the model sent it.
    let mut renames = folded;
    for (from, into) in &merged {
        renames.insert(from.clone(), into.clone());
    }
    let renames = resolve_chains(renames);
    report.after = counts.len() - renames.len();
    Ok((renames, report))
}

/// Record the name the document used, beside the canonical one. Written only
/// when the two differ, so untouched entities gain nothing.
pub(crate) fn note_original(props: &mut Properties, key: &str, what: &str, original: &str) {
    props.insert(
        key.into(),
        PropDesc {
            description: Some(what.into()),
            value: PropValue::Str(original.into()),
        },
    );
}

/// Count how often each name occurs, for the frequency tie-break.
pub(crate) fn tally<'a>(names: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for n in names {
        *counts.entry(n).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// The rule the edge-type prompt adds: direction is part of an edge type's
/// meaning, so two types that read alike but point opposite ways must not be
/// merged — doing so would silently invert relationships.
pub(crate) const EDGE_RULE: &str = "An edge type's direction is part of its meaning: `USES` (source uses target) and `USED_IN` \
     (source used in target) are OPPOSITE and must NEVER be merged, however alike they read. \
     Merge only types that relate their endpoints the same way round.\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;

    fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(n, c)| (n.to_string(), *c)).collect()
    }

    #[test]
    fn folding_needs_no_model_and_keeps_the_common_spelling() {
        let c = counts(&[
            ("Attention Mechanism", 5),
            ("AttentionMechanism", 1),
            ("Model", 3),
        ]);
        let r = fold(&c);
        assert_eq!(
            r.get("AttentionMechanism"),
            Some(&"Attention Mechanism".into())
        );
        assert_eq!(r.len(), 1, "Model is untouched");
    }

    #[test]
    fn folding_covers_case_spacing_hyphens_and_underscores() {
        for (a, b) in [
            ("Softmax", "softmax"),
            ("d_model", "dmodel"),
            ("Multi-Head Attention", "Multi head attention"),
            ("Self-Attention", "self attention"),
        ] {
            let r = fold(&counts(&[(a, 2), (b, 1)]));
            assert_eq!(r.get(b), Some(&a.to_string()), "{b} should fold into {a}");
        }
    }

    #[test]
    fn folding_ties_break_lexicographically_so_reruns_agree() {
        let a = fold(&counts(&[("Beta", 1), ("BETA", 1)]));
        let b = fold(&counts(&[("BETA", 1), ("Beta", 1)]));
        assert_eq!(a, b);
    }

    #[test]
    fn the_model_only_sees_what_folding_left() {
        // One call, and the two spellings of one name are already gone from it.
        let chat = MockProvider::new(
            vec![r#"{"groups":[["COMPARED_WITH","CONTRASTS_WITH"]]}"#.into()],
            4,
        );
        let c = counts(&[
            ("COMPARED_WITH", 3),
            ("CONTRASTS_WITH", 2),
            ("compared_with", 1),
        ]);
        let (renames, report) = reconcile(&chat, "edge type", EDGE_RULE, &c).unwrap();
        assert_eq!(report.folded, 1);
        assert_eq!(report.merged, 1);
        assert_eq!(report.before, 3);
        assert_eq!(report.after, 1);
        assert_eq!(renames["compared_with"], "COMPARED_WITH");
        assert_eq!(renames["CONTRASTS_WITH"], "COMPARED_WITH");
    }

    #[test]
    fn invented_and_self_referential_merges_are_dropped() {
        let chat = MockProvider::new(
            vec![r#"{"groups":[["A","NOT_IN_LIST","B"],["A"]]}"#.into()],
            4,
        );
        let (renames, _) = reconcile(&chat, "label", "", &counts(&[("A", 1), ("B", 1)])).unwrap();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames["B"], "A");
    }

    #[test]
    fn a_name_belongs_to_the_first_family_that_claims_it() {
        // `B` appears in two groups. Taking the first keeps the outcome
        // independent of the order the renames happen to be applied in — the
        // alternative is a chain whose result depends on iteration order.
        let chat = MockProvider::new(vec![r#"{"groups":[["B","A"],["C","B"]]}"#.into()], 4);
        let (renames, _) =
            reconcile(&chat, "label", "", &counts(&[("A", 1), ("B", 1), ("C", 1)])).unwrap();
        assert_eq!(renames["A"], "B");
        assert!(!renames.contains_key("B"), "B was already spoken for");
        assert!(!renames.contains_key("C"));
    }

    #[test]
    fn a_cyclic_rename_map_terminates() {
        let cyclic: Renames = [("A".to_string(), "B".to_string()), ("B".into(), "A".into())]
            .into_iter()
            .collect();
        let _ = resolve_chains(cyclic); // must not loop
    }

    #[test]
    fn the_prompt_carries_how_often_each_name_was_seen() {
        // Frequency is what tells the model which spelling the graph should
        // keep, so it has to reach the prompt.
        let chat = MockProvider::new(vec![r#"{"groups":[]}"#.into()], 4);
        let c = counts(&[("COMPARED_WITH", 7), ("CONTRASTS_WITH", 2)]);
        let (_, report) = reconcile(&chat, "edge type", EDGE_RULE, &c).unwrap();
        assert_eq!(report.chat_requests, 1);
    }

    #[test]
    fn a_single_name_never_calls_the_model() {
        let chat = MockProvider::new(vec!["{}".into()], 4);
        let (renames, report) = reconcile(&chat, "label", "", &counts(&[("Only", 1)])).unwrap();
        assert!(renames.is_empty());
        assert_eq!(report.chat_requests, 0);
    }

    #[test]
    fn a_garbled_reply_leaves_the_vocabulary_alone() {
        let chat = MockProvider::new(vec!["sorry, I can't do that".into()], 4);
        let (renames, report) =
            reconcile(&chat, "label", "", &counts(&[("A", 1), ("B", 1)])).unwrap();
        assert!(renames.is_empty());
        assert_eq!(report.after, 2, "nothing merged, nothing lost");
    }
}
