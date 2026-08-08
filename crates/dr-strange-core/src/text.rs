//! Text analysis for the BM25 keyword index (ROADMAP §2).
//!
//! One [`Analyzer`] turns a property string into a token stream the inverted
//! index and query both agree on. For the Snowball languages: Unicode-aware
//! lowercasing, split on non-alphanumerics, English stopword removal, then
//! stemming so "databases" and "database" collapse to one term. For
//! [`Language::Chinese`]: jieba word segmentation in search mode (there are
//! no spaces to split on, and `char::is_alphanumeric` is true for Han
//! ideographs — the split-based pipeline would index whole clauses as single
//! terms), a compact stopword list, no stemming. The language is a per-index
//! choice, stored durably with the declaration.
//!
//! The [`Language`] tag byte is stable on disk — append new variants, never
//! renumber — so a declaration written by an older build still decodes. The
//! serde variant order is wire format for the same reason (postcard encodes
//! the variant index).

use ahash::AHashSet;
use std::sync::OnceLock;

use jieba_rs::Jieba;
use rust_stemmers::{Algorithm, Stemmer};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Analysis language for a keyword index. English is the default; the
/// Snowball set mirrors what `rust-stemmers` ships, and Chinese analyzes by
/// jieba segmentation instead of stemming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Language {
    #[default]
    English,
    Arabic,
    Danish,
    Dutch,
    Finnish,
    French,
    German,
    Greek,
    Hungarian,
    Italian,
    Norwegian,
    Portuguese,
    Romanian,
    Russian,
    Spanish,
    Swedish,
    Tamil,
    Turkish,
    // ⚠ Append-only (tag byte + serde variant index are both on-disk).
    Chinese,
}

impl Language {
    /// The Snowball algorithm backing this language — `None` for Chinese,
    /// which segments instead of stemming.
    pub fn algorithm(self) -> Option<Algorithm> {
        Some(match self {
            Language::Arabic => Algorithm::Arabic,
            Language::Danish => Algorithm::Danish,
            Language::Dutch => Algorithm::Dutch,
            Language::English => Algorithm::English,
            Language::Finnish => Algorithm::Finnish,
            Language::French => Algorithm::French,
            Language::German => Algorithm::German,
            Language::Greek => Algorithm::Greek,
            Language::Hungarian => Algorithm::Hungarian,
            Language::Italian => Algorithm::Italian,
            Language::Norwegian => Algorithm::Norwegian,
            Language::Portuguese => Algorithm::Portuguese,
            Language::Romanian => Algorithm::Romanian,
            Language::Russian => Algorithm::Russian,
            Language::Spanish => Algorithm::Spanish,
            Language::Swedish => Algorithm::Swedish,
            Language::Tamil => Algorithm::Tamil,
            Language::Turkish => Algorithm::Turkish,
            Language::Chinese => return None,
        })
    }

    /// Stable on-disk tag. **Append-only** — never renumber existing variants.
    pub fn tag(self) -> u8 {
        match self {
            Language::English => 0,
            Language::Arabic => 1,
            Language::Danish => 2,
            Language::Dutch => 3,
            Language::Finnish => 4,
            Language::French => 5,
            Language::German => 6,
            Language::Greek => 7,
            Language::Hungarian => 8,
            Language::Italian => 9,
            Language::Norwegian => 10,
            Language::Portuguese => 11,
            Language::Romanian => 12,
            Language::Russian => 13,
            Language::Spanish => 14,
            Language::Swedish => 15,
            Language::Tamil => 16,
            Language::Turkish => 17,
            Language::Chinese => 18,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Language> {
        Some(match tag {
            0 => Language::English,
            1 => Language::Arabic,
            2 => Language::Danish,
            3 => Language::Dutch,
            4 => Language::Finnish,
            5 => Language::French,
            6 => Language::German,
            7 => Language::Greek,
            8 => Language::Hungarian,
            9 => Language::Italian,
            10 => Language::Norwegian,
            11 => Language::Portuguese,
            12 => Language::Romanian,
            13 => Language::Russian,
            14 => Language::Spanish,
            15 => Language::Swedish,
            16 => Language::Tamil,
            17 => Language::Turkish,
            18 => Language::Chinese,
            _ => return None,
        })
    }
}

impl std::str::FromStr for Language {
    type Err = Error;

    fn from_str(s: &str) -> Result<Language> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "english" | "en" => Language::English,
            "arabic" | "ar" => Language::Arabic,
            "danish" | "da" => Language::Danish,
            "dutch" | "nl" => Language::Dutch,
            "finnish" | "fi" => Language::Finnish,
            "french" | "fr" => Language::French,
            "german" | "de" => Language::German,
            "greek" | "el" => Language::Greek,
            "hungarian" | "hu" => Language::Hungarian,
            "italian" | "it" => Language::Italian,
            "norwegian" | "no" => Language::Norwegian,
            "portuguese" | "pt" => Language::Portuguese,
            "romanian" | "ro" => Language::Romanian,
            "russian" | "ru" => Language::Russian,
            "spanish" | "es" => Language::Spanish,
            "swedish" | "sv" => Language::Swedish,
            "tamil" | "ta" => Language::Tamil,
            "turkish" | "tr" => Language::Turkish,
            "chinese" | "zh" | "zh-cn" | "zh-tw" => Language::Chinese,
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unknown language '{other}'"
                )));
            }
        })
    }
}

/// The process-wide jieba segmenter. Built on first Chinese analysis — the
/// embedded dictionary takes real time and memory to load, so it is shared,
/// never per-analyzer.
fn jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

/// Turns text into index/query terms for one language. Cheap to build; hold one
/// per (build or search) call rather than per token.
pub struct Analyzer {
    language: Language,
    /// `None` for Chinese, which segments instead of stemming.
    stemmer: Option<Stemmer>,
}

impl Analyzer {
    pub fn new(language: Language) -> Self {
        Self {
            language,
            stemmer: language.algorithm().map(Stemmer::create),
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Returns the ordered term list (duplicates kept, so callers can count
    /// term frequencies). Snowball languages: lowercase → split on
    /// non-alphanumerics → drop stopwords (English only) → stem. Chinese:
    /// jieba search-mode segmentation → lowercase (for embedded Latin) →
    /// drop punctuation/whitespace tokens and Chinese stopwords, unstemmed.
    pub fn analyze(&self, text: &str) -> Vec<String> {
        match &self.stemmer {
            Some(stemmer) => self.analyze_stemmed(text, stemmer),
            None => analyze_chinese(text),
        }
    }

    fn analyze_stemmed(&self, text: &str, stemmer: &Stemmer) -> Vec<String> {
        let mut out = Vec::new();
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }
            let lower = raw.to_lowercase();
            if self.is_stopword(&lower) {
                continue;
            }
            let term = stemmer.stem(&lower);
            if !term.is_empty() {
                out.push(term.into_owned());
            }
        }
        out
    }

    fn is_stopword(&self, token: &str) -> bool {
        matches!(self.language, Language::English) && english_stopwords().contains(token)
    }
}

/// Chinese analysis: jieba's search mode cuts words AND their sub-words
/// (`数据库` also yields `数据`), the same granularity trade query engines
/// index with — a query for the sub-word still hits the document. Mixed-in
/// Latin/digit runs survive as lowercased whole tokens, unstemmed.
fn analyze_chinese(text: &str) -> Vec<String> {
    jieba()
        .cut_for_search(text, true)
        .into_iter()
        .filter(|tok| tok.word.chars().any(char::is_alphanumeric))
        .map(|tok| tok.word.to_lowercase())
        .filter(|tok| !chinese_stopwords().contains(tok.as_str()))
        .collect()
}

/// Common English stopwords, built once. BM25's IDF already downweights
/// ubiquitous terms; this just keeps them out of the postings entirely.
fn english_stopwords() -> &'static AHashSet<&'static str> {
    static SET: OnceLock<AHashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "a",
            "about",
            "above",
            "after",
            "again",
            "against",
            "all",
            "am",
            "an",
            "and",
            "any",
            "are",
            "as",
            "at",
            "be",
            "because",
            "been",
            "before",
            "being",
            "below",
            "between",
            "both",
            "but",
            "by",
            "can",
            "did",
            "do",
            "does",
            "doing",
            "down",
            "during",
            "each",
            "few",
            "for",
            "from",
            "further",
            "had",
            "has",
            "have",
            "having",
            "he",
            "her",
            "here",
            "hers",
            "herself",
            "him",
            "himself",
            "his",
            "how",
            "i",
            "if",
            "in",
            "into",
            "is",
            "it",
            "its",
            "itself",
            "just",
            "me",
            "more",
            "most",
            "my",
            "myself",
            "no",
            "nor",
            "not",
            "now",
            "of",
            "off",
            "on",
            "once",
            "only",
            "or",
            "other",
            "our",
            "ours",
            "ourselves",
            "out",
            "over",
            "own",
            "same",
            "she",
            "should",
            "so",
            "some",
            "such",
            "than",
            "that",
            "the",
            "their",
            "theirs",
            "them",
            "themselves",
            "then",
            "there",
            "these",
            "they",
            "this",
            "those",
            "through",
            "to",
            "too",
            "under",
            "until",
            "up",
            "very",
            "was",
            "we",
            "were",
            "what",
            "when",
            "where",
            "which",
            "while",
            "who",
            "whom",
            "why",
            "will",
            "with",
            "you",
            "your",
            "yours",
            "yourself",
            "yourselves",
        ]
        .into_iter()
        .collect()
    })
}

/// Compact Chinese stopword list (function words and particles). As with the
/// English list, BM25's IDF already downweights ubiquitous terms; this keeps
/// the most common ones out of the postings entirely.
fn chinese_stopwords() -> &'static AHashSet<&'static str> {
    static SET: OnceLock<AHashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            "的", "了", "着", "是", "在", "和", "与", "或", "及", "就", "都", "而", "也", "又",
            "但", "并", "被", "把", "让", "向", "从", "对", "为", "以", "于", "之", "这", "那",
            "些", "个", "我", "你", "他", "她", "它", "我们", "你们", "他们", "什么", "怎么",
            "这样", "那样", "一个", "没有", "不", "很", "会", "能", "要", "去", "来", "到", "吗",
            "呢", "吧", "啊", "嘛",
        ]
        .into_iter()
        .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_stems_and_drops_stopwords() {
        let a = Analyzer::new(Language::English);
        // "the"/"of"/"are" are stopwords; "Databases"/"running" stem.
        let terms = a.analyze("The Study of Graph Databases (are running)!");
        assert_eq!(terms, vec!["studi", "graph", "databas", "run"]);
    }

    #[test]
    fn query_and_doc_share_stems() {
        let a = Analyzer::new(Language::English);
        // A query term stems to the same token its plural does in a document.
        assert_eq!(a.analyze("databases"), a.analyze("database"));
    }

    #[test]
    fn non_english_keeps_stopwordy_tokens_but_still_stems() {
        // German has no stopword list here, so short words survive; stemming
        // still applies (this just checks it runs + lowercases Unicode).
        let a = Analyzer::new(Language::German);
        let terms = a.analyze("Die Häuser");
        assert!(terms.contains(&"die".to_string()));
        assert!(
            terms
                .iter()
                .any(|t| t.starts_with("haus") || t.starts_with("häus"))
        );
    }

    #[test]
    fn language_tag_roundtrips() {
        for lang in [
            Language::English,
            Language::French,
            Language::Turkish,
            Language::Russian,
            Language::Chinese,
        ] {
            assert_eq!(Language::from_tag(lang.tag()), Some(lang));
        }
        assert_eq!(Language::from_tag(200), None);
    }

    #[test]
    fn language_parses_names_and_codes() {
        assert_eq!("en".parse::<Language>().unwrap(), Language::English);
        assert_eq!("French".parse::<Language>().unwrap(), Language::French);
        assert_eq!("zh".parse::<Language>().unwrap(), Language::Chinese);
        assert_eq!("Chinese".parse::<Language>().unwrap(), Language::Chinese);
        assert!("klingon".parse::<Language>().is_err());
    }

    /// Chinese is appended, never inserted: its tag byte AND its postcard
    /// variant index are both on-disk formats. If either changes, existing
    /// declarations and sidecars misdecode.
    #[test]
    fn chinese_wire_positions_are_pinned() {
        assert_eq!(Language::Chinese.tag(), 18);
        assert_eq!(
            postcard::to_stdvec(&Language::Chinese).unwrap(),
            vec![18],
            "postcard variant index moved — Language variants were reordered"
        );
        assert_eq!(postcard::to_stdvec(&Language::Turkish).unwrap(), vec![17]);
    }

    #[test]
    fn chinese_segments_words_not_clauses() {
        let a = Analyzer::new(Language::Chinese);
        // Without segmentation the whole clause is one is_alphanumeric run —
        // a single useless token. jieba must cut real words.
        let terms = a.analyze("我爱北京天安门");
        assert!(terms.contains(&"北京".to_string()), "{terms:?}");
        assert!(terms.contains(&"天安门".to_string()), "{terms:?}");
        assert!(!terms.contains(&"我爱北京天安门".to_string()));
        // "我" is a stopword; punctuation never becomes a token.
        assert!(!terms.contains(&"我".to_string()));

        // Mixed Latin survives lowercased, unstemmed.
        let mixed = a.analyze("用 Rust 写的图数据库");
        assert!(mixed.contains(&"rust".to_string()), "{mixed:?}");
        assert!(mixed.contains(&"数据库".to_string()), "{mixed:?}");
    }

    /// Query and document must agree on terms — the invariant BM25 relies on.
    #[test]
    fn chinese_query_and_doc_share_terms() {
        let a = Analyzer::new(Language::Chinese);
        let doc = a.analyze("向量检索与关键词检索的混合排序");
        for q in ["向量", "检索", "关键词"] {
            let query = a.analyze(q);
            assert!(
                query.iter().any(|t| doc.contains(t)),
                "query {q:?} → {query:?} shares no term with doc {doc:?}"
            );
        }
    }
}
