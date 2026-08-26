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
//! # Supported
//! Every source below binds one node pattern and may continue with a
//! relationship tail, so a typed hop chains off a retrieval seed exactly as it
//! does off a `MATCH` node (ROADMAP §7).
//! - `MATCH` one linear path: `(a:Label)`, `-[:TYPE]->` / `<-[:TYPE]-` / `-[:T]-`
//!   (in/out/both), bare `-->`/`--`, and bounded variable-length `-[:T*1..3]->`.
//! - **`SEARCH (v:Label) [ON prop] NEAR "text"|[..] [METRIC m] [TOPK k]`** — an
//!   indexed vector seed (`Source::VectorTopK`). `"text"` is embedded server-side
//!   via [`parse_with_embedder`]; `[..]` is a literal escape hatch. `ON` may be
//!   omitted: every `NEAR` defaults to the `embedding` property, which is what
//!   the digest pipeline writes.
//! - **`SEARCH (v:Label) ON prop MATCHING "text" [TOPK k]`** — a BM25 keyword
//!   seed (`Source::KeywordTopK`) over a declared keyword index. Same verb as
//!   the vector seed, different operator: `NEAR` compares meaning, `MATCHING`
//!   compares words.
//! - **`HYBRID (v:Label) [VECTOR [ON p] NEAR q [METRIC m] [WEIGHT w]]
//!   [KEYWORD ON p MATCHING "text" [WEIGHT w]]
//!   [GRAPH HOPS h [DECAY d] [SEEDS n] [WEIGHT w]] [CANDIDATES n] [TOPK k]`** —
//!   fused retrieval (`Source::Hybrid`); channels in any order, at least one of
//!   VECTOR/KEYWORD.
//! - **`CALL <pagerank|components|shortest_path|louvain>(arg: v, …) ON (v[:Label])`**
//!   — a graph algorithm as a source (`Source::Algo`). `ON` both scopes the
//!   algorithm and binds the variable; the per-node result rides `score()`.
//! - **`BEAM (result[:Label]) <OUT|IN|BOTH> [:TYPE] ON prop NEAR "text"|[..]
//!   [METRIC m] WIDTH w DEPTH d`** — similarity-guided beam traversal
//!   (`Step::ExpandBeam`) from the current frontier; chains after any source.
//! - `WHERE` over `=,<>,!=,<,<=,>,>=`, `AND`/`OR`/`NOT`, `+ - * /`, `IS [NOT] NULL`,
//!   `x IN [a, b]`, property access `a.key`, the label predicate `a:Label`, the
//!   external key `key(a)`, and the scoring terms `score()`, `hops()`,
//!   `similarity(a.prop, "text"|[..][, metric])`, `distance(...)` (usable in
//!   `ORDER BY` too — a brute-force rank).
//! - `RETURN [DISTINCT] <var|*>`, `ORDER BY expr [ASC|DESC], …`, `SKIP n`, `LIMIT n`.
//! - **`AS OF <seq|"RFC-3339"|TIME <ms>>`** — last clause; reads a past
//!   snapshot (native backend). Not a plan node: it rides on [`ReadQuery`] for
//!   the surface to apply with `PlaneHandle::as_of`.
//!
//! `key(n) = "…"` (or `key(n) IN […]`) on a scanned source compiles to a
//! `SeekKeys` seek rather than a scan-and-filter — an index lookup, so an LLM
//! can anchor on an entity it knows by key.
//!
//! # Writes (via [`parse_statement`], applied with [`WriteStatement::apply`])
//! - `CREATE (n:L {k: v, …}), (a)-[:T {…}]->(b), …` — a string `key:` sets the
//!   external key; edges are directed (`->`/`<-`).
//! - `MERGE (n:L {key: "…", …}) [ON CREATE SET …] [ON MATCH SET …]` — upsert a
//!   node by its external key; a path `MERGE (a {key})-[:T]->(b {key})` upserts
//!   each keyed node and ensures the edge (idempotent, element-wise).
//! - `MATCH pattern [WHERE …] SET n.p = v, n:Label, n += {…}` /
//!   `REMOVE n.p, n:Label` / `[DETACH] DELETE n` — find-then-mutate on the
//!   pattern's terminal variable (plain `DELETE` refuses a connected node).
//! - `MATCH pattern [WHERE …] CREATE (n)-[:T]->(x {…})` /
//!   `MERGE (n)-[:T]->(x {key})` — once per matched row, with the terminal
//!   variable pre-bound so `(n)` anchors to the matched node.
//!
//! # Parameters
//! `$name` placeholders in value positions (WHERE/ORDER literals, SET/CREATE/
//! MERGE props) are resolved from a caller-supplied [`Params`] map via
//! [`parse_statement_full`] — the SDK-safe way to pass values (no string
//! interpolation).
//!
//! # Not yet (each is a clear error, never a silent mis-compile)
//! - cross-variable predicates (`p.year < q.year`);
//! - returning / ordering by a non-terminal variable, projections, aggregation;
//! - unbounded variable-length (`*`, `*n..`).

mod ast;
mod compile;
mod parse;
mod write;

use ahash::AHashMap;

use dr_strange_core::{LogicalPlan, PropValue};

pub use ast::AsOfSpec;
pub use write::{WriteStatement, WriteSummary};

/// Values for `$name` placeholders in a query, supplied by the caller and
/// resolved at parse time (so no string interpolation — the SDK-safe way to
/// pass values). Keyed by the bare name (no `$`).
pub type Params = AHashMap<String, PropValue>;

/// Resolve a `$name` placeholder against the caller's params.
pub(crate) fn resolve_param(params: &Params, name: &str) -> Result<PropValue, String> {
    params
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unbound parameter `${name}`"))
}

/// A parsed statement: a read (a runnable plan) or a write (applied with
/// [`WriteStatement::apply`]). Surfaces branch on this to run a query.
#[derive(Debug)]
pub enum Statement {
    Read(ReadQuery),
    Write(WriteStatement),
}

/// A compiled read: the plan to run, and the snapshot to run it against.
///
/// `AS OF` is not a plan node — it addresses the *plane handle* — so it rides
/// alongside. A surface applies it with `PlaneHandle::as_of` before running
/// the plan; `None` reads the latest commit.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadQuery {
    pub plan: LogicalPlan,
    pub as_of: Option<AsOfSpec>,
}

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

/// Parse a *read* query into a runnable [`ReadQuery`]. A write statement
/// (`CREATE`, …) is an error here — use [`parse_statement`]. A text
/// `SEARCH … NEAR "…"` also errors (no embedder); use [`parse_with_embedder`]
/// or a literal vector.
pub fn parse(input: &str) -> Result<ReadQuery, ParseError> {
    read_only(parse_statement_inner(input, None, &Params::new())?)
}

/// Like [`parse`], but resolves text `NEAR "…"` terms into query vectors via
/// `embedder` — the semantic-search entry point the surfaces use for reads.
pub fn parse_with_embedder(input: &str, embedder: &dyn Embedder) -> Result<ReadQuery, ParseError> {
    read_only(parse_statement_inner(
        input,
        Some(embedder),
        &Params::new(),
    )?)
}

/// Parse a statement — either a read query or a write (`CREATE`, …). This is the
/// entry point a surface uses when it wants to run *either*.
pub fn parse_statement(input: &str) -> Result<Statement, ParseError> {
    parse_statement_inner(input, None, &Params::new())
}

/// Like [`parse_statement`], but resolves a text `SEARCH … NEAR "…"` via `embedder`.
pub fn parse_statement_with_embedder(
    input: &str,
    embedder: &dyn Embedder,
) -> Result<Statement, ParseError> {
    parse_statement_inner(input, Some(embedder), &Params::new())
}

/// The general entry point: parse a statement, resolving text `SEARCH` via an
/// optional `embedder` and `$name` placeholders via `params`. The surfaces that
/// accept parameters (CLI/MCP/plane.cypher) use this.
pub fn parse_statement_full(
    input: &str,
    embedder: Option<&dyn Embedder>,
    params: &Params,
) -> Result<Statement, ParseError> {
    parse_statement_inner(input, embedder, params)
}

fn read_only(stmt: Statement) -> Result<ReadQuery, ParseError> {
    match stmt {
        Statement::Read(read) => Ok(read),
        Statement::Write(_) => Err(ParseError::Compile(
            "this is a write statement; run it with parse_statement, not a read query".to_string(),
        )),
    }
}

fn parse_statement_inner(
    input: &str,
    embedder: Option<&dyn Embedder>,
    params: &Params,
) -> Result<Statement, ParseError> {
    let (rest, stmt) = parse::statement(input).map_err(|e| ParseError::Syntax(describe(e)))?;
    // Everything must be consumed — trailing tokens mean a mistyped clause.
    let rest = rest.trim_start();
    if !rest.trim_end().is_empty() {
        return Err(match unsupported_return(input, rest) {
            Some(why) => ParseError::Compile(why),
            None => ParseError::Syntax(format!(
                "unexpected trailing input near `{}`",
                snippet(rest.trim_end())
            )),
        });
    }
    Ok(match stmt {
        ast::StmtAst::Read(query) => {
            let as_of = query.as_of;
            let plan = compile::compile(*query, embedder, params).map_err(ParseError::Compile)?;
            Statement::Read(ReadQuery { plan, as_of })
        }
        ast::StmtAst::Write(w) => {
            Statement::Write(write::compile(w, params.clone()).map_err(ParseError::Compile)?)
        }
    })
}

/// Read a RETURN this subset does not have, and say so in those terms.
///
/// `RETURN` takes one variable or `*` (appendix B), so full Cypher's
/// projections stop the parse mid-clause and surface as "unexpected trailing
/// input near `.file, …`" — a position, not an answer, and appendix B promises
/// these are "a clear error, never a silent mis-compile". Whoever wrote
/// `RETURN f.file, f.line` needs to hear that whole records come back anyway,
/// not to go hunting for a typo that isn't there.
///
/// `rest` is what the parse left, so it is a suffix of `input`: the text
/// before it is what parsed, and the RETURN clause is recognised there rather
/// than guessed at, which keeps a `WHERE n.name = 'return'` from being read as
/// one.
fn unsupported_return(input: &str, rest: &str) -> Option<String> {
    let var = returned_variable(&input[..input.len() - rest.len()])?;
    let what = match rest.as_bytes().first()? {
        b'.' => format!(
            "RETURN takes one variable or `*`, so the projection `{var}.…` is not \
             supported — `RETURN {var}` hands back whole records, properties and \
             all, and ORDER BY still takes `{var}.…`"
        ),
        b',' => format!(
            "RETURN takes one variable or `*`, so a column list is not supported — \
             `RETURN {var}` hands back whole records, properties and all"
        ),
        b'(' => format!(
            "RETURN takes one variable or `*`, so calling `{var}(…)` in it is not \
             supported — there is no aggregation, and `key()` and `score()` belong \
             in WHERE and ORDER BY; a returned record already carries the key"
        ),
        _ => {
            let after_as = keyword_tail(rest, "as")?;
            // `AS OF …` is a real clause and parses; reaching here with `OF`
            // next means its argument is the problem, not an alias.
            if keyword_tail(after_as, "of").is_some() {
                return None;
            }
            format!(
                "RETURN takes one variable or `*`, so aliasing `{var}` with AS is not supported"
            )
        }
    };
    Some(format!("{what} (near `{}`)", snippet(rest.trim_end())))
}

/// The variable a RETURN clause got as far as naming, if `parsed` ends in one:
/// the last `RETURN` keyword, an optional `DISTINCT`, and a bare identifier —
/// exactly what the grammar accepts, and nothing else in between.
fn returned_variable(parsed: &str) -> Option<&str> {
    let at = parsed.to_ascii_lowercase().rfind("return")?;
    let after = keyword_tail(&parsed[at..], "return")?;
    let after = keyword_tail(after, "distinct").unwrap_or(after).trim();
    let ident = !after.is_empty()
        && after.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !after.starts_with(|c: char| c.is_ascii_digit());
    ident.then_some(after)
}

/// What follows `word` in `s`, when `s` opens with that keyword as a whole
/// word — `None` when it does not, so `ordered` never reads as `order`.
fn keyword_tail<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let (head, tail) = s.split_at_checked(word.len())?;
    if !head.eq_ignore_ascii_case(word) {
        return None;
    }
    match tail.chars().next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => None,
        _ => Some(tail),
    }
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
