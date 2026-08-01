//! Text analysis for the BM25 keyword index (ROADMAP §2).
//!
//! One [`Analyzer`] turns a property string into a token stream the inverted
//! index and query both agree on: Unicode-aware lowercasing, split on
//! non-alphanumerics, English stopword removal, then Snowball stemming so
//! "databases" and "database" collapse to one term. The stemmer language is a
//! per-index choice ([`Language`]), stored durably with the declaration.
//!
//! The [`Language`] tag byte is stable on disk — append new variants, never
//! renumber — so a declaration written by an older build still decodes.

use std::collections::HashSet;
use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Snowball stemmer language for a keyword index. English is the default; the
/// set mirrors what `rust-stemmers` ships.
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
}

impl Language {
    /// The Snowball algorithm backing this language.
    pub fn algorithm(self) -> Algorithm {
        match self {
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
        }
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
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unknown language '{other}'"
                )));
            }
        })
    }
}

/// Turns text into index/query terms for one language. Cheap to build; hold one
/// per (build or search) call rather than per token.
pub struct Analyzer {
    language: Language,
    stemmer: Stemmer,
}

impl Analyzer {
    pub fn new(language: Language) -> Self {
        Self {
            language,
            stemmer: Stemmer::create(language.algorithm()),
        }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Lowercase → split on non-alphanumerics → drop stopwords (English only)
    /// → stem. Returns the ordered term list (duplicates kept, so callers can
    /// count term frequencies).
    pub fn analyze(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in text.split(|c: char| !c.is_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }
            let lower = raw.to_lowercase();
            if self.is_stopword(&lower) {
                continue;
            }
            let term = self.stemmer.stem(&lower);
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

/// Common English stopwords, built once. BM25's IDF already downweights
/// ubiquitous terms; this just keeps them out of the postings entirely.
fn english_stopwords() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
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
        ] {
            assert_eq!(Language::from_tag(lang.tag()), Some(lang));
        }
        assert_eq!(Language::from_tag(200), None);
    }

    #[test]
    fn language_parses_names_and_codes() {
        assert_eq!("en".parse::<Language>().unwrap(), Language::English);
        assert_eq!("French".parse::<Language>().unwrap(), Language::French);
        assert!("klingon".parse::<Language>().is_err());
    }
}
