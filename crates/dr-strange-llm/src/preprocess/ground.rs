//! Joining a preprocessor's facts to a model's reading of the same input.
//!
//! Two problems, one for each direction:
//!
//! - **Before the digest**, the model must be told what the parser already
//!   found, or it will mint `Function:parse` beside the `parse` the AST just
//!   established. That is [`FactsAndPlane`].
//! - **After it**, the two results have to become one, and where they disagree
//!   something has to win. That is [`fold`].

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use dr_strange_core::{PropDesc, PropValue};

use super::Preprocessed;
use crate::digest::{CandidateSource, DigestResult, ExistingEntity};

/// Existing entities, plus the facts this run just parsed.
///
/// [`digest`](crate::digest::digest) checks every key the model proposes
/// against the graph exactly, so a re-digest links rather than duplicating
/// (ROADMAP §8 stage 2). A fact from *this* run is not in the graph yet — it is
/// written afterwards — so without this the model would be told `parse` is new,
/// and would happily propose a second one.
///
/// `similar()` is delegated untouched. Facts carry no embedding at this point,
/// and inventing one to make them findable by similarity would be guessing at a
/// vector rather than answering the question asked.
pub struct FactsAndPlane<'a> {
    inner: Option<&'a dyn CandidateSource>,
    facts: BTreeMap<String, (String, String)>,
}

impl<'a> FactsAndPlane<'a> {
    pub fn new(facts: &Preprocessed, inner: Option<&'a dyn CandidateSource>) -> Self {
        Self {
            inner,
            facts: facts
                .nodes
                .iter()
                .map(|n| {
                    let description = match n.props.get("doc_comment").map(|d| &d.value) {
                        Some(dr_strange_core::PropValue::Str(s)) => s.clone(),
                        _ => String::new(),
                    };
                    (n.key.clone(), (n.label.clone(), description))
                })
                .collect(),
        }
    }
}

impl CandidateSource for FactsAndPlane<'_> {
    fn similar(&self, query: &[f32], k: usize) -> Result<Vec<ExistingEntity>> {
        match self.inner {
            Some(inner) => inner.similar(query, k),
            None => Ok(Vec::new()),
        }
    }

    fn wants_similar(&self) -> bool {
        // Facts alone answer `similar` with nothing; only a real plane behind
        // them makes a chunk vector worth producing.
        self.inner.is_some_and(|inner| inner.wants_similar())
    }

    fn existing_keys(&self, keys: &[String]) -> Result<Vec<ExistingEntity>> {
        let mut found = match self.inner {
            Some(inner) => inner.existing_keys(keys)?,
            None => Vec::new(),
        };
        let already: BTreeSet<&str> = found.iter().map(|e| e.key.as_str()).collect();
        let fresh: Vec<ExistingEntity> = keys
            .iter()
            .filter(|k| !already.contains(k.as_str()))
            .filter_map(|k| {
                let (label, description) = self.facts.get(k)?;
                Some(ExistingEntity {
                    key: k.clone(),
                    label: label.clone(),
                    description: description.clone(),
                })
            })
            .collect();
        found.extend(fresh);
        Ok(found)
    }
}

/// Stamp `_source` and `_run` onto facts, matching what the digest writes.
///
/// Deliberately no `_model`: nothing about these came from one, and an empty
/// or invented value there would be worse than the property's absence, which
/// says exactly what happened. `_generated_by` names the parser instead.
pub fn stamp_run(facts: &mut Preprocessed, source: &str, run_id: &str) {
    facts
        .nodes
        .iter_mut()
        .map(|n| &mut n.props)
        .chain(facts.edges.iter_mut().map(|e| &mut e.props))
        .for_each(|props| {
            props.insert(
                "_source".into(),
                PropDesc::described(
                    "source this was preprocessed from",
                    PropValue::Str(source.to_string()),
                ),
            );
            props.insert(
                "_run".into(),
                PropDesc::described("digest run id", PropValue::Str(run_id.to_string())),
            );
        });
}

/// Fold parsed facts and a model's extraction into one result — **facts win**.
///
/// A parser knows; a model infers. Where both produced a node under one key,
/// the parsed one is kept and the model's is dropped: its label is a guess at a
/// vocabulary the plugin states as a constant, and its properties were read out
/// of prose the plugin read out of a syntax tree.
///
/// Deliberately not routed through §8's stage 2, which spends a model call
/// deciding whether two entities are the same thing. Here that question is
/// already answered — the keys are equal, and one side is an AST.
///
/// Edges are *not* deduplicated against the model's. An edge carries no
/// identity of its own, two `CALLS` between the same pair are not obviously one
/// fact, and dropping a relation the model found because a parser found
/// something between the same nodes would lose real information.
pub fn fold(facts: Preprocessed, model: DigestResult) -> DigestResult {
    let parsed: BTreeSet<String> = facts.nodes.iter().map(|n| n.key.clone()).collect();

    let mut nodes = facts.nodes;
    let mut dropped = 0usize;
    for n in model.nodes {
        if parsed.contains(&n.key) {
            dropped += 1;
            continue;
        }
        nodes.push(n);
    }

    let mut edges = facts.edges;
    edges.extend(model.edges);

    let mut report = model.report;
    report.notes.extend(facts.report.notes);
    for (handler, count) in &facts.report.handlers {
        report
            .notes
            .push(format!("{handler} contributed {count} fact(s)"));
    }
    if facts.report.skipped > 0 {
        report
            .notes
            .push(format!("{} file(s) skipped", facts.report.skipped));
    }
    for c in &facts.report.collisions {
        report.notes.push(format!("key claimed twice: {c}"));
    }
    if dropped > 0 {
        report.notes.push(format!(
            "{dropped} model entity(ies) dropped for a parsed fact under the \
             same key — a parser knows where a model infers"
        ));
    }

    DigestResult {
        nodes,
        edges,
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::{DigestEdge, DigestNode};
    use dr_strange_core::Properties;

    fn node(key: &str, label: &str) -> DigestNode {
        DigestNode {
            key: key.into(),
            label: label.into(),
            extra_labels: Vec::new(),
            props: Properties::new(),
        }
    }

    fn edge(src: &str, dst: &str) -> DigestEdge {
        DigestEdge {
            src: src.into(),
            dst: dst.into(),
            ty: "CALLS".into(),
            props: Properties::new(),
        }
    }

    fn facts(nodes: Vec<DigestNode>) -> Preprocessed {
        Preprocessed {
            nodes,
            ..Default::default()
        }
    }

    /// A key the parser just produced is not in the graph yet, so an exact
    /// lookup against the plane alone says "new" — and the model mints a
    /// duplicate of the function the AST just established.
    #[test]
    fn a_fact_from_this_run_counts_as_existing() {
        let f = facts(vec![node("k::parse", "Function")]);
        let src = FactsAndPlane::new(&f, None);

        let found = src
            .existing_keys(&["k::parse".into(), "k::nothing".into()])
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "k::parse");
        assert_eq!(found[0].label, "Function");
    }

    /// A doc comment is the one description the parser has, and it is better
    /// than none for the model deciding whether to reuse the key.
    #[test]
    fn a_doc_comment_becomes_the_candidate_description() {
        let mut n = node("k::parse", "Function");
        n.props.insert(
            "doc_comment".into(),
            PropDesc::new(PropValue::Str("Reads a token stream.".into())),
        );
        let f = facts(vec![n]);
        let src = FactsAndPlane::new(&f, None);

        let found = src.existing_keys(&["k::parse".into()]).unwrap();
        assert_eq!(found[0].description, "Reads a token stream.");
    }

    /// The plane's own answer is authoritative for a key it holds; the facts
    /// only add what it did not have.
    #[test]
    fn the_plane_is_not_shadowed_by_a_fact_under_the_same_key() {
        struct Plane;
        impl CandidateSource for Plane {
            fn similar(&self, _q: &[f32], _k: usize) -> Result<Vec<ExistingEntity>> {
                Ok(Vec::new())
            }
            fn existing_keys(&self, _keys: &[String]) -> Result<Vec<ExistingEntity>> {
                Ok(vec![ExistingEntity {
                    key: "k::parse".into(),
                    label: "FromThePlane".into(),
                    description: String::new(),
                }])
            }
        }
        let f = facts(vec![node("k::parse", "Function")]);
        let src = FactsAndPlane::new(&f, Some(&Plane));

        let found = src.existing_keys(&["k::parse".into()]).unwrap();
        assert_eq!(found.len(), 1, "one answer per key, not two");
        assert_eq!(found[0].label, "FromThePlane");
    }

    #[test]
    fn a_parsed_fact_beats_a_model_entity_under_the_same_key() {
        let f = facts(vec![node("k::parse", "Function")]);
        let model = DigestResult {
            nodes: vec![node("k::parse", "Procedure"), node("Idea", "Concept")],
            edges: vec![edge("Idea", "k::parse")],
            report: Default::default(),
        };

        let out = fold(f, model);
        let parse: Vec<_> = out.nodes.iter().filter(|n| n.key == "k::parse").collect();
        assert_eq!(parse.len(), 1, "one node per key");
        assert_eq!(
            parse[0].label, "Function",
            "the parser's label, not a guess"
        );
        // What the model found *beyond* the facts is kept.
        assert!(out.nodes.iter().any(|n| n.key == "Idea"));
        assert_eq!(out.edges.len(), 1);
        assert!(
            out.report.notes.iter().any(|n| n.contains("dropped")),
            "{:?}",
            out.report.notes
        );
    }

    /// An edge has no identity of its own, so the model's relations survive
    /// alongside the parser's rather than being deduplicated against them.
    #[test]
    fn edges_from_both_sides_are_kept() {
        let f = Preprocessed {
            nodes: vec![node("a", "Function"), node("b", "Function")],
            edges: vec![edge("a", "b")],
            ..Default::default()
        };
        let model = DigestResult {
            nodes: Vec::new(),
            edges: vec![edge("a", "b")],
            report: Default::default(),
        };
        assert_eq!(fold(f, model).edges.len(), 2);
    }

    /// A thin graph should be explained by the report rather than investigated.
    #[test]
    fn the_reports_are_joined() {
        let f = Preprocessed {
            report: super::super::PreprocessReport {
                handlers: vec![("rust@1".into(), 40)],
                skipped: 3,
                notes: vec!["12 call(s) named nothing defined here".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let out = fold(
            f,
            DigestResult {
                nodes: Vec::new(),
                edges: Vec::new(),
                report: Default::default(),
            },
        );
        let joined = out.report.notes.join("\n");
        assert!(joined.contains("rust@1 contributed 40"), "{joined}");
        assert!(joined.contains("3 file(s) skipped"), "{joined}");
        assert!(joined.contains("named nothing defined here"), "{joined}");
    }
}
