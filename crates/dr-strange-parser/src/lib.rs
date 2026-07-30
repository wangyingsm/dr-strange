//! dr-strange's query language: an openCypher subset that parses into core's
//! `LogicalPlan` (arch/00 §5 — "a Cypher/GQL-subset parser lands in v2 on top
//! of the same logical plan layer"). The `nom` grammar and the plan compiler
//! live here so the parser-combinator dependency never reaches the lean core;
//! the crate depends on core only for the plan types it targets.
//!
//! ```text
//! MATCH (p:Paper)-[:CITES]->(q:Paper)
//! WHERE p.year >= 2020
//! RETURN q
//! ORDER BY q.year DESC
//! LIMIT 10
//! ```
//!
//! # Supported (this cut)
//! - `MATCH` one linear path: `(a:Label)`, `-[:TYPE]->` / `<-[:TYPE]-` / `-[:T]-`
//!   (in/out/both), bare `-->`/`--`, and bounded variable-length `-[:T*1..3]->`.
//! - **`SEARCH (v:Label) ON prop NEAR "text"|[..] [METRIC m] [TOPK k]`** — an
//!   indexed vector seed (`Source::VectorTopK`). `"text"` is embedded server-side
//!   via [`parse_with_embedder`]; `[..]` is a literal escape hatch.
//! - `WHERE` over `=,<>,!=,<,<=,>,>=`, `AND`/`OR`/`NOT`, `+ - * /`, `IS [NOT] NULL`,
//!   property access `a.key`, the label predicate `a:Label`, and the scoring
//!   terms `score()`, `hops()`, `similarity(a.prop, "text"|[..][, metric])`,
//!   `distance(...)` (usable in `ORDER BY` too — a brute-force rank).
//! - `RETURN [DISTINCT] <var|*>`, `ORDER BY expr [ASC|DESC], …`, `SKIP n`, `LIMIT n`.
//!
//! # Not yet (each is a clear error, never a silent mis-compile)
//! - cross-variable predicates (`p.year < q.year`);
//! - returning / ordering by a non-terminal variable, projections, aggregation;
//! - unbounded variable-length (`*`, `*n..`);
//! - similarity-guided beam traversal (`ExpandBeam`), writes (`CREATE`/`SET`/`DELETE`),
//!   and named `$params`.

mod ast;
mod compile;
mod parse;

use dr_strange_core::LogicalPlan;

/// Resolves the text in a `SEARCH … NEAR "text"` (or `similarity(p, "text")`)
/// into a query vector. The pure parser owns no embedding machinery — a
/// surface (web/MCP/CLI) supplies this, backed by the server's configured
/// provider (API key from the environment, never the client). Keeping it a
/// one-method trait keeps `dr-strange-parser` dependency-free.
pub trait Embedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
}

/// Why a query didn't turn into a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The text didn't match the grammar.
    Syntax(String),
    /// The text parsed, but the pattern/clauses can't map onto a `LogicalPlan`
    /// (e.g. a cross-variable predicate, an unbounded `*`, or a text `NEAR`
    /// with no embedder configured).
    Compile(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Syntax(m) => write!(f, "syntax error: {m}"),
            ParseError::Compile(m) => write!(f, "unsupported query: {m}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a query into a [`LogicalPlan`]. A text `SEARCH … NEAR "…"` errors
/// (there's no embedder); use [`parse_with_embedder`] for the semantic path, or
/// a literal vector `NEAR [..]`.
pub fn parse(input: &str) -> Result<LogicalPlan, ParseError> {
    parse_inner(input, None)
}

/// Like [`parse`], but resolves text `NEAR "…"` terms into query vectors via
/// `embedder` — the semantic-search entry point the surfaces use.
pub fn parse_with_embedder(
    input: &str,
    embedder: &dyn Embedder,
) -> Result<LogicalPlan, ParseError> {
    parse_inner(input, Some(embedder))
}

fn parse_inner(input: &str, embedder: Option<&dyn Embedder>) -> Result<LogicalPlan, ParseError> {
    let (rest, query) = parse::query(input).map_err(|e| ParseError::Syntax(describe(e)))?;
    // Everything must be consumed — trailing tokens mean a mistyped clause.
    let rest = rest.trim();
    if !rest.is_empty() {
        return Err(ParseError::Syntax(format!(
            "unexpected trailing input near `{}`",
            snippet(rest)
        )));
    }
    compile::compile(query, embedder).map_err(ParseError::Compile)
}

fn describe(e: nom::Err<nom::error::Error<&str>>) -> String {
    match e {
        nom::Err::Error(er) | nom::Err::Failure(er) => {
            let at = er.input.trim();
            if at.is_empty() {
                "unexpected end of query".to_string()
            } else {
                format!("near `{}`", snippet(at))
            }
        }
        nom::Err::Incomplete(_) => "incomplete query".to_string(),
    }
}

fn snippet(s: &str) -> &str {
    let end = s.char_indices().nth(40).map_or(s.len(), |(i, _)| i);
    &s[..end]
}
