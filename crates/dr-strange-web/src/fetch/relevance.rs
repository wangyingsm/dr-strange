//! Relevance for the URL fetcher (ROADMAP §9).
//!
//! §9 settled that relevance is decided **twice**, because the two decisions
//! answer different questions. Before fetching, all that exists is anchor text,
//! a `title` attribute and the words in a URL — [`Target::coverage`] scores
//! those, and only the best candidates cost a request. After fetching, the page
//! itself is in hand and [`Target::score`] re-judges it, so a link that promised
//! "Transformer architecture" and delivered a login page is dropped having cost
//! exactly one request.
//!
//! Scoring is BM25's term-saturation with the target weight standing in for
//! IDF. Real IDF needs corpus statistics, and a handful of pages is not a
//! corpus — the frequencies would be noise dressed as information. What we do
//! have is a statement of what the fetch is looking for, which is the thing IDF
//! is a proxy for anyway.
//!
//! Both use `dr_strange_core::Analyzer`, the same tokenizer, stemmer and
//! stopword set the BM25 index uses, so "transformers" in a link matches
//! "Transformer" in a heading and the whole system keeps one notion of what a
//! word is.

use std::collections::BTreeMap;

use ahash::AHashMap;

use dr_strange_core::Analyzer;

/// How many of the root page's own terms describe it. Enough to characterize a
/// paper, few enough that a long page's tail of incidental words does not
/// drown the terms that matter.
const ROOT_TERMS: usize = 40;
/// A term the user typed counts for more than one the root page merely used
/// often — it *sharpens* rather than replaces, so a topic narrows the target
/// without discarding what the page is plainly about.
const TOPIC_WEIGHT: f32 = 3.0;
/// BM25 saturation and length-normalization constants, at their usual values.
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// What a fetch is looking for: stemmed terms, each with a weight.
///
/// The terms are held **sorted** rather than in a hash map, because scoring
/// sums a float per term and float addition is not associative: iterating a
/// `HashMap` would produce sums that differ in their last bits between two
/// identically-built targets. That is invisible almost always and decides a
/// keep-or-drop the one time a page sits exactly on the relevance floor.
#[derive(Debug, Default, Clone)]
pub struct Target {
    terms: Vec<(String, f32)>,
    total: f32,
}

impl Target {
    /// Build from the root page's own text, sharpened by an optional topic.
    ///
    /// Both, as §9 settled: root-only makes a broad landing page useless as a
    /// seed, and topic-only makes the common case — paste a URL, press go —
    /// require homework.
    pub fn new(analyzer: &Analyzer, root_text: &str, topic: Option<&str>) -> Self {
        let mut counts: AHashMap<String, usize> = AHashMap::new();
        for t in analyzer.analyze(root_text) {
            *counts.entry(t).or_default() += 1;
        }
        let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
        // Sort by frequency, then by term, so the same page always yields the
        // same target however the hash map happened to order itself.
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(ROOT_TERMS);
        let top = ranked.first().map_or(1, |(_, n)| *n).max(1) as f32;

        let mut terms: BTreeMap<String, f32> = BTreeMap::new();
        for (term, n) in ranked {
            terms.insert(term, n as f32 / top);
        }
        for t in topic.map(|t| analyzer.analyze(t)).unwrap_or_default() {
            *terms.entry(t).or_default() += TOPIC_WEIGHT;
        }
        let terms: Vec<(String, f32)> = terms.into_iter().collect();
        let total = terms.iter().map(|(_, w)| w).sum();
        Self { terms, total }
    }

    /// True when nothing was learned about the target — an empty root page and
    /// no topic. The caller then keeps everything it fetched rather than
    /// ranking against noise.
    pub fn is_empty(&self) -> bool {
        self.total <= 0.0
    }

    /// Score a full document, 0..1, with BM25 length normalization so a long
    /// page cannot win on bulk alone.
    pub fn score(&self, analyzer: &Analyzer, text: &str, avg_len: f32) -> f32 {
        let tokens = analyzer.analyze(text);
        let len = tokens.len() as f32;
        let mut tf: AHashMap<&str, f32> = AHashMap::new();
        for t in &tokens {
            *tf.entry(t.as_str()).or_default() += 1.0;
        }
        let norm = if avg_len > 0.0 { len / avg_len } else { 1.0 };
        let denom_len = K1 * (1.0 - B + B * norm);
        self.sum(|term| {
            tf.get(term)
                .map(|f| f * (K1 + 1.0) / (f + denom_len))
                .unwrap_or(0.0)
        })
    }

    /// Score a short string — anchor text, a `title`, the words in a URL —
    /// where length normalization would be meaningless. This is coverage: how
    /// much of the target's weight the string mentions at all.
    pub fn coverage(&self, analyzer: &Analyzer, text: &str) -> f32 {
        let tokens = analyzer.analyze(text);
        self.sum(|term| {
            if tokens.iter().any(|t| t == term) {
                1.0
            } else {
                0.0
            }
        })
    }

    fn sum(&self, mut factor: impl FnMut(&str) -> f32) -> f32 {
        if self.total <= 0.0 {
            return 0.0;
        }
        let raw: f32 = self.terms.iter().map(|(t, w)| w * factor(t)).sum();
        // `factor` peaks at K1 + 1 for `score` and at 1 for `coverage`; divide
        // by the larger so both land in 0..1 and can share one floor.
        (raw / (self.total * (K1 + 1.0))).clamp(0.0, 1.0)
    }
}

/// The readable words in a URL's **path**, split on separators and case
/// boundaries, so `/docs/attention-is-all-you-need` contributes terms to match
/// against.
///
/// The path only. A query string usually parameterizes a *view* of a document
/// rather than naming one, and when a site's own tooling echoes the current
/// page into it the query actively lies about relevance: crawling a Wikipedia
/// article, `/w/index.php?title=Transformer_(deep_learning)&action=edit` scored
/// above the article on attention, because the query repeated every word the
/// target was looking for. Sites that do put the subject in the query still
/// have their anchor text read, which is the stronger signal anyway.
pub fn url_words(url: &url::Url) -> String {
    let mut out = String::new();
    let mut push = |s: &str| {
        for part in s.split(|c: char| !c.is_alphanumeric()) {
            if part.is_empty() {
                continue;
            }
            // Split camelCase into words too.
            let mut word = String::new();
            for (i, ch) in part.char_indices() {
                if i > 0 && ch.is_uppercase() && !word.ends_with(char::is_uppercase) {
                    out.push_str(&word);
                    out.push(' ');
                    word.clear();
                }
                word.push(ch);
            }
            out.push_str(&word);
            out.push(' ');
        }
    };
    push(url.path());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dr_strange_core::Language;

    fn analyzer() -> Analyzer {
        Analyzer::new(Language::English)
    }

    const ROOT: &str = "The Transformer is a model architecture based on attention. \
        Attention lets the model relate positions of a sequence. The Transformer \
        replaces recurrence with attention entirely.";

    #[test]
    fn a_page_about_the_target_outscores_one_that_is_not() {
        let a = analyzer();
        let t = Target::new(&a, ROOT, None);
        let on = t.score(
            &a,
            "Attention and the Transformer architecture in sequence models",
            12.0,
        );
        let off = t.score(
            &a,
            "Our cookie policy explains how this website stores preferences",
            12.0,
        );
        assert!(on > off, "on-topic {on} should beat off-topic {off}");
        assert!(on > 0.0 && off < 0.2, "on={on} off={off}");
    }

    #[test]
    fn a_typed_topic_sharpens_without_erasing_the_page() {
        let a = analyzer();
        let plain = Target::new(&a, ROOT, None);
        let sharp = Target::new(&a, ROOT, Some("positional encoding"));
        let text = "Positional encoding injects order information into the model.";
        assert!(
            sharp.score(&a, text, 10.0) > plain.score(&a, text, 10.0),
            "the topic must lift a page that matches it"
        );
        // …and the root's own subject still counts for something.
        let other = "The Transformer relies on attention.";
        assert!(
            sharp.score(&a, other, 10.0) > 0.0,
            "root terms survive a topic"
        );
    }

    #[test]
    fn a_topic_alone_works_when_the_root_says_nothing() {
        let a = analyzer();
        let t = Target::new(&a, "", Some("graph databases"));
        assert!(!t.is_empty());
        assert!(t.coverage(&a, "an introduction to graph databases") > 0.0);
    }

    #[test]
    fn an_empty_target_is_reported_rather_than_scoring_everything_zero() {
        let a = analyzer();
        let t = Target::new(&a, "   ", None);
        assert!(t.is_empty(), "nothing to rank against");
        assert_eq!(t.score(&a, "anything at all", 5.0), 0.0);
    }

    #[test]
    fn length_normalization_stops_a_long_page_winning_on_bulk() {
        let a = analyzer();
        let t = Target::new(&a, ROOT, None);
        let tight = "Transformer attention architecture";
        let padded = format!("{tight} {}", "unrelated filler wording ".repeat(80));
        assert!(
            t.score(&a, tight, 20.0) > t.score(&a, &padded, 20.0),
            "padding must not raise the score"
        );
    }

    #[test]
    fn coverage_reads_short_strings_where_length_would_be_noise() {
        let a = analyzer();
        let t = Target::new(&a, ROOT, None);
        let good = t.coverage(&a, "Attention in Transformer models");
        let bad = t.coverage(&a, "Privacy policy");
        assert!(good > bad, "good={good} bad={bad}");
        assert_eq!(bad, 0.0);
    }

    #[test]
    fn a_urls_path_is_evidence_and_its_query_is_not() {
        let u = url::Url::parse("https://x.test/docs/attentionIsAll-you-need?tab=cookiePolicy")
            .unwrap();
        let words = url_words(&u);
        assert!(words.contains("attention"), "{words}");
        assert!(words.contains("need"), "{words}");
        assert!(
            words.contains("attention Is All"),
            "camelCase splits: {words}"
        );
        assert!(!words.contains("cookie"), "the query is not read: {words}");
    }

    #[test]
    fn a_sites_own_machinery_cannot_win_by_echoing_the_page_into_a_query() {
        let a = analyzer();
        let t = Target::new(&a, ROOT, None);
        // Both links live on a page about the Transformer. One is the article
        // the reader wants; the other is that page's own edit form, whose query
        // repeats every word the target is looking for.
        let article = url::Url::parse("https://x.test/wiki/Attention_(machine_learning)").unwrap();
        let edit =
            url::Url::parse("https://x.test/w/index.php?title=Transformer_model&action=edit")
                .unwrap();
        assert!(
            t.coverage(&a, &url_words(&article)) > t.coverage(&a, &url_words(&edit)),
            "the edit form must not outrank the article"
        );
        assert_eq!(t.coverage(&a, &url_words(&edit)), 0.0);
    }

    #[test]
    fn the_same_page_always_builds_the_same_target() {
        let a = analyzer();
        let one = Target::new(&a, ROOT, Some("encoding"));
        let two = Target::new(&a, ROOT, Some("encoding"));
        let text = "attention and encoding";
        assert_eq!(one.score(&a, text, 8.0), two.score(&a, text, 8.0));
    }
}
