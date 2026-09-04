//! Completing a half-typed query: what may come next, and what this plane
//! would make of it.
//!
//! An editor's own completion can only offer keywords — it has no idea which
//! labels a plane holds, let alone that a `Function` is far more often the
//! source of a `CALLS` than of an `IMPORTS`. This does, because it is handed
//! the plane's vocabulary ([`Vocab`], the soft-schema catalog) along with the
//! text: after `MATCH ` the best guess is `(n:Function)` because that is the
//! label the plane holds most of, and after `MATCH (n:Function) ` it is
//! `-[:CALLS]->(m:Function)` because that is the edge most often leaving a
//! `Function`, landing where such edges most often land.
//!
//! ## Why not the grammar
//!
//! [`crate::parse`] is a `nom` grammar over *whole* statements: a prefix fails
//! it, and a failure says where it stopped rather than what would have
//! continued. So this is a second, much smaller reading of the same language —
//! a tokenizer and a position machine that only ever asks "given what has been
//! typed, what kind of thing does the caret sit at?". It accepts far more than
//! the grammar does, deliberately: a half-written query is not yet wrong.
//!
//! The two are kept honest by the grammar itself: every suggestion is written
//! the way [`crate::parse`] reads it, and the tests parse what they complete.

/// What a plane holds, as far as completing a query needs to know: the
/// soft-schema catalog, which a server has already computed.
///
/// Counts are what make one suggestion better than another, so they are part
/// of the vocabulary rather than an afterthought. An empty `Vocab` is
/// perfectly usable — completion falls back to the language's own keywords and
/// to `(n)`, which names no label and so cannot be wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vocab {
    pub labels: Vec<LabelInfo>,
    pub edges: Vec<EdgeInfo>,
}

/// One node label: how many nodes carry it, and the properties they hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelInfo {
    pub name: String,
    pub count: u64,
    /// Property names, in the order the catalog gives them.
    pub properties: Vec<String>,
}

/// One edge type: how many the plane holds, and between which labels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeInfo {
    pub name: String,
    pub count: u64,
    pub connections: Vec<Connection>,
}

/// `src -[:type]-> dst`, and how many such edges there are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub src: String,
    pub dst: String,
    pub count: u64,
}

/// What the caret sits at — the question a suggestion answers.
///
/// Carries the context that made the guess possible, so a caller can say *why*
/// a list is what it is ("properties of `n`, a `Function`") instead of showing
/// a bare list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// The keyword a statement opens with.
    Statement,
    /// A whole node pattern — `(n:Label)`. `var` is the name it would bind.
    Node { var: String },
    /// A variable name, just inside `(`.
    NodeVar { var: String },
    /// A label, after `(n:`.
    Label,
    /// The end of a node pattern: `:Label`, or `)`.
    NodeEnd,
    /// A relationship leaving the node just closed — or the clause that
    /// follows it, since a pattern may simply end here. `from` is that node's
    /// label, `var` the name the next node would bind, and `bound` how many
    /// the pattern has bound already.
    Hop {
        from: Option<String>,
        var: String,
        bound: usize,
    },
    /// An edge type — anywhere from just after the arrow to just after the
    /// `:`. `lead` is the punctuation that still has to be written before the
    /// type: `[:` right after the arrow, `:` right after the bracket, and
    /// nothing once the colon is there.
    EdgeType {
        from: Option<String>,
        var: String,
        incoming: bool,
        lead: &'static str,
    },
    /// The end of a relationship: `]`, or the bounds of a `*` range —
    /// `ranged` when a `*` has been written and is still waiting for them.
    RelEnd { incoming: bool, ranged: bool },
    /// The arrow that closes a relationship — `->`, or `-`.
    Arrow { incoming: bool },
    /// A predicate, after `WHERE`.
    Predicate { vars: Vec<String> },
    /// A returned item, after `RETURN`.
    Projection { vars: Vec<String> },
    /// A property of `var`, which is a `label`.
    Property { var: String, label: Option<String> },
    /// The right-hand side of a comparison — a value only the author knows.
    Value,
    /// The key a `ORDER BY` sorts on.
    SortKey { vars: Vec<String> },
    /// A clause that may follow a complete one — `ORDER BY`, `LIMIT`,
    /// `AS OF`. `done` are the ones already written, which cannot come again.
    Clause { done: Vec<String> },
    /// Nothing worth guessing at: inside a string literal, where the words
    /// typed are data rather than syntax.
    Nothing,
}

/// What kind of thing a suggestion is, for a caller that shows them
/// differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Label,
    EdgeType,
    Property,
    Variable,
    /// A whole shape — `(n:Function)`, `-[:CALLS]->(m:Function)`.
    Snippet,
}

/// One thing the caret could become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// What to show in a list — the token or shape itself.
    pub text: String,
    /// What to insert **at the caret**: [`Self::text`] less the partial word
    /// already typed, plus whatever punctuation finishes the position.
    pub insert: String,
    /// What tells this candidate from the next — a count, a destination label.
    pub detail: Option<String>,
    pub kind: Kind,
}

/// What may come next, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The best candidate's [`Suggestion::insert`]: what a ghost after the
    /// caret should read, and what accepting it should type. `None` when there
    /// is nothing to say.
    pub best: Option<String>,
    /// Every candidate, best first.
    pub suggestions: Vec<Suggestion>,
    /// What the caret sits at.
    pub expects: Expect,
    /// The partial word under the caret, which every suggestion completes.
    pub word: String,
}

/// How many candidates a position offers. Enough to choose from, few enough to
/// read: a plane with three hundred labels has no useful three-hundredth
/// suggestion.
const MAX_SUGGESTIONS: usize = 12;

/// What may follow `prefix` — the query text from its start to the caret — in
/// a plane whose vocabulary is `vocab`.
///
/// Never fails and never parses: a prefix is not a statement, and asking
/// whether it is one would answer the wrong question. Reads only what is
/// *before* the caret, which is what an editor can hand over cheaply whenever
/// typing pauses.
pub fn complete(prefix: &str, vocab: &Vocab) -> Completion {
    let Some((tokens, word)) = tokenize(prefix) else {
        // The caret is inside an unterminated string: what is typed there is
        // data, and completing it would be noise.
        return Completion {
            best: None,
            suggestions: Vec::new(),
            expects: Expect::Nothing,
            word: String::new(),
        };
    };
    let expects = scan(&tokens);
    // A relationship's brackets take no spaces — `-[:CALLS ]->` is not a
    // relationship — so a caret that has left one has nothing to add there.
    let loose = word.is_empty() && prefix.ends_with(char::is_whitespace);
    let mut suggestions = suggest(&expects, &word, vocab, loose);
    suggestions.truncate(MAX_SUGGESTIONS);
    Completion {
        best: suggestions.first().map(|s| s.insert.clone()),
        suggestions,
        expects,
        word,
    }
}

// ---- tokens -----------------------------------------------------------------

/// One token of a prefix. Words keep their text because a suggestion may need
/// to read it — a variable's name, a label's spelling.
#[derive(Debug, Clone, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    Punct(&'a str),
    Num,
    Str,
}

/// Punctuation that runs together into one token: the arrows a relationship is
/// written with (`->`, `<-`, `-->`, `--`) and the comparisons (`<=`, `>=`,
/// `<>`, `!=`), which are the only places two of these characters mean one
/// thing.
const ARROWY: &str = "-<>=!";

/// Split a prefix into complete tokens and the partial word under the caret.
///
/// `None` when the caret is inside an unterminated string literal. The
/// language has no escapes, so that scan is exact.
///
/// The partial word is whatever the prefix ends *part-way through*: a prefix
/// ending in a space, or in punctuation, has none — the caret sits at a fresh
/// position rather than inside a name.
fn tokenize(prefix: &str) -> Option<(Vec<Tok<'_>>, String)> {
    let bytes = prefix.as_bytes();
    let mut toks = Vec::new();
    let mut partial = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c == '"' || c == '\'' {
            i += 1;
            while i < bytes.len() && bytes[i] as char != c {
                i += 1;
            }
            if i == bytes.len() {
                return None; // unterminated: the caret is inside it
            }
            i += 1;
            toks.push(Tok::Str);
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            i += 1;
            while i < bytes.len()
                && matches!(bytes[i] as char, c if c.is_alphanumeric() || c == '_')
            {
                i += 1;
            }
            let word = &prefix[start..i];
            if i == bytes.len() {
                partial = Some(word.to_string()); // the caret is inside it
            } else {
                toks.push(Tok::Word(word));
            }
        } else if c.is_ascii_digit() {
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            toks.push(Tok::Num);
        } else {
            // A run of arrow characters is one token, and so is `..`;
            // everything else stands alone.
            if ARROWY.contains(c) || c == '.' {
                let run = if c == '.' { "." } else { ARROWY };
                while i < bytes.len() && run.contains(bytes[i] as char) {
                    i += 1;
                }
            } else {
                i += 1;
            }
            toks.push(Tok::Punct(&prefix[start..i]));
        }
    }
    Some((toks, partial.unwrap_or_default()))
}

// ---- the position machine -----------------------------------------------------

/// Where a scan has got to. Most states become a public [`Expect`]; the few
/// that do not are the places a reader would not call a position — just inside
/// a `(`, part-way through a relationship — and they fold into one when the
/// scan ends.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pos {
    Statement,
    /// After `MATCH` / `CREATE` / a closed relationship: a node pattern.
    NodeOpen,
    /// Just inside `(`: a variable, or `:`.
    NodeVar,
    /// After `(n`: `:`, or `)`.
    NodeColon,
    /// After `(n:`: a label.
    NodeLabel,
    /// After `(n:Label`: `)`.
    NodeClose,
    /// After `(…)`: a relationship, or the clause that follows.
    Hop,
    /// After `-` or `<-`: `[`.
    RelOpen,
    /// Just inside `-[`: `:`, or a relationship variable.
    RelVar,
    /// After `-[r`: `:`.
    RelColon,
    /// After `-[:`: an edge type.
    RelType,
    /// After `-[:TYPE`: `]`, or a `*1..3` range.
    RelClose,
    /// After `-[:TYPE]`: the arrow.
    Arrow,
    /// After `WHERE`, and after each `AND`.
    Predicate,
    /// After `RETURN`, and after each `,`.
    Projection,
    /// After a variable in a clause: `.`, an operator, `AS`, `,`.
    Term,
    /// After `n.`: a property name.
    Property(String),
    /// After a comparison operator.
    Value,
    /// After `ORDER BY`: the key to sort on.
    Sort,
    /// After a complete clause: `ORDER BY`, `SKIP`, `LIMIT`, `AS OF`.
    Clause,
}

/// What a scan learned: where the caret is, and the variables the pattern
/// bound on the way — which is what turns `n.` into a list of properties.
struct Scan {
    pos: Pos,
    /// `(variable, label)` for every node pattern closed so far.
    vars: Vec<(String, Option<String>)>,
    /// The label of the node most recently closed — what a relationship
    /// leaving it is ranked by.
    from: Option<String>,
    /// The node being read: its variable, then its label.
    var: Option<String>,
    label: Option<String>,
    /// The relationship being read is written `<-`.
    incoming: bool,
    /// The punctuation still owed before its type — see [`Expect::EdgeType`].
    /// A lone `<` owes its hyphen as well as the bracket.
    owed: &'static str,
    /// A `*` has been written in it, and whether its bounds followed.
    star: Option<bool>,
    /// The variable a `.` was written after.
    term: Option<String>,
    /// The tail clauses already written — a query has one `LIMIT`.
    done: Vec<String>,
}

impl Scan {
    fn close_node(&mut self) {
        let label = self.label.take();
        if let Some(var) = self.var.take() {
            self.vars.push((var, label.clone()));
        }
        self.from = label;
    }

    fn label_of(&self, var: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(v, _)| v == var)
            .and_then(|(_, l)| l.clone())
    }

    fn names(&self) -> Vec<String> {
        self.vars.iter().map(|(v, _)| v.clone()).collect()
    }

    /// A name for the next node a pattern binds. `n`, then `m`, then `o` — the
    /// letters every Cypher example in the world uses, in that order — and a
    /// numbered one past the end of them, so a long pattern still cannot bind
    /// the same name twice.
    fn next_var(&self) -> String {
        const NAMES: &[&str] = &["n", "m", "o", "p", "q", "r", "s", "t"];
        NAMES
            .iter()
            .find(|name| !self.vars.iter().any(|(v, _)| v == *name))
            .map_or_else(
                || format!("n{}", self.vars.len()),
                |name| (*name).to_string(),
            )
    }
}

/// The keywords that end whatever clause came before them, wherever they
/// appear — which is what lets `MATCH (n) WHERE … CREATE (m)` scan in one
/// pass.
fn clause_keyword(word: &str) -> Option<&'static str> {
    Some(match word.to_ascii_uppercase().as_str() {
        "WHERE" => "WHERE",
        "RETURN" => "RETURN",
        "CREATE" => "CREATE",
        "MERGE" => "MERGE",
        "SET" | "REMOVE" | "DELETE" => "SET",
        "AND" | "OR" | "NOT" => "AND",
        "BY" => "BY",
        "DISTINCT" => "RETURN",
        "SKIP" | "LIMIT" => "COUNT",
        "ORDER" | "DETACH" | "AS" | "OF" => "TAIL",
        _ => return None,
    })
}

/// Walk the tokens, ending where the caret sits.
///
/// Deliberately forgiving: a token that fits no transition leaves the position
/// alone rather than derailing the scan, because half a query is full of
/// tokens the grammar has not seen the rest of yet.
fn scan(tokens: &[Tok<'_>]) -> Expect {
    let mut s = Scan {
        pos: Pos::Statement,
        vars: Vec::new(),
        from: None,
        var: None,
        label: None,
        incoming: false,
        owed: "[:",
        star: None,
        term: None,
        done: Vec::new(),
    };
    for tok in tokens {
        step(&mut s, tok);
    }
    match &s.pos {
        Pos::Statement => Expect::Statement,
        Pos::NodeOpen => Expect::Node { var: s.next_var() },
        Pos::NodeVar => Expect::NodeVar { var: s.next_var() },
        Pos::NodeColon | Pos::NodeClose => Expect::NodeEnd,
        Pos::NodeLabel => Expect::Label,
        Pos::Hop => Expect::Hop {
            from: s.from.clone(),
            var: s.next_var(),
            bound: s.vars.len(),
        },
        Pos::RelOpen | Pos::RelVar | Pos::RelColon | Pos::RelType => Expect::EdgeType {
            from: s.from.clone(),
            var: s.next_var(),
            incoming: s.incoming,
            lead: match s.pos {
                Pos::RelOpen => s.owed,
                Pos::RelVar | Pos::RelColon => ":",
                _ => "",
            },
        },
        Pos::RelClose => Expect::RelEnd {
            incoming: s.incoming,
            ranged: s.star == Some(false),
        },
        Pos::Arrow => Expect::Arrow {
            incoming: s.incoming,
        },
        Pos::Predicate => Expect::Predicate { vars: s.names() },
        Pos::Projection => Expect::Projection { vars: s.names() },
        Pos::Property(var) => Expect::Property {
            label: s.label_of(var),
            var: var.clone(),
        },
        Pos::Value => Expect::Value,
        Pos::Sort => Expect::SortKey { vars: s.names() },
        Pos::Term | Pos::Clause => Expect::Clause {
            done: s.done.clone(),
        },
    }
}

fn step(s: &mut Scan, tok: &Tok<'_>) {
    if let Tok::Word(w) = tok
        && let Some(kw) = clause_keyword(w)
    {
        // A tail clause comes once: a query has one `LIMIT`, and offering a
        // second is offering a syntax error.
        let written = w.to_ascii_uppercase();
        if let Some((tail, _)) = TAIL
            .iter()
            .find(|(t, _)| t.split(' ').next() == Some(written.as_str()))
        {
            s.done.push((*tail).to_string());
        }
        s.pos = match kw {
            "WHERE" | "AND" | "SET" => Pos::Predicate,
            "RETURN" => Pos::Projection,
            "CREATE" | "MERGE" => Pos::NodeOpen,
            "BY" => Pos::Sort,
            // `SKIP 10` — a number nobody but the author knows.
            "COUNT" => Pos::Value,
            _ => Pos::Clause,
        };
        return;
    }
    match (&s.pos, tok) {
        // A statement opens with the keyword that says where its rows come
        // from. `CALL` names an algorithm before its `ON (n:Label)`, so it
        // waits where it is until the node arrives.
        (Pos::Statement, Tok::Word(w)) => {
            if matches!(
                w.to_ascii_uppercase().as_str(),
                "MATCH" | "SEARCH" | "HYBRID" | "BEAM" | "ON"
            ) {
                s.pos = Pos::NodeOpen;
            }
        }

        // ---- node patterns ---------------------------------------------------
        (Pos::NodeOpen, Tok::Punct("(")) => s.pos = Pos::NodeVar,
        (Pos::NodeVar, Tok::Word(w)) => {
            s.var = Some((*w).to_string());
            s.pos = Pos::NodeColon;
        }
        (Pos::NodeVar | Pos::NodeColon, Tok::Punct(":")) => s.pos = Pos::NodeLabel,
        (Pos::NodeLabel, Tok::Word(w)) => {
            s.label = Some((*w).to_string());
            s.pos = Pos::NodeClose;
        }
        (Pos::NodeVar | Pos::NodeColon | Pos::NodeClose, Tok::Punct(")")) => {
            s.close_node();
            s.pos = Pos::Hop;
        }

        // ---- relationships ----------------------------------------------------
        (Pos::Hop, Tok::Punct(p)) if p.starts_with('-') || p.starts_with('<') => {
            s.incoming = p.starts_with('<');
            s.star = None;
            // `<` on its own is half an arrow: the hyphen is owed too.
            s.owed = if *p == "<" { "-[:" } else { "[:" };
            // `-->` and `--` are a whole relationship in one token: nothing
            // bracketed follows them.
            s.pos = if *p == "--" || *p == "-->" || *p == "<--" {
                Pos::NodeOpen
            } else {
                Pos::RelOpen
            };
        }
        (Pos::RelOpen, Tok::Punct("[")) => s.pos = Pos::RelVar,
        (Pos::RelVar, Tok::Word(_)) => s.pos = Pos::RelColon,
        (Pos::RelVar | Pos::RelColon, Tok::Punct(":")) => s.pos = Pos::RelType,
        (Pos::RelType, Tok::Word(_)) => s.pos = Pos::RelClose,
        (Pos::RelVar | Pos::RelColon | Pos::RelType | Pos::RelClose, Tok::Punct("]")) => {
            s.pos = Pos::Arrow;
        }
        // The pieces of a `*1..3` range leave the position where it was, and
        // say how much of the range is written.
        (Pos::RelClose, Tok::Punct("*")) => s.star = Some(false),
        (Pos::RelClose, Tok::Punct("..") | Tok::Num) => s.star = Some(true),
        (Pos::Arrow, Tok::Punct(_)) => s.pos = Pos::NodeOpen,

        // ---- clauses -----------------------------------------------------------
        (Pos::Predicate | Pos::Projection | Pos::Value | Pos::Clause | Pos::Sort, Tok::Word(w)) => {
            s.term = Some((*w).to_string());
            s.pos = Pos::Term;
        }
        (Pos::Term, Tok::Punct(".")) => {
            s.pos = Pos::Property(s.term.clone().unwrap_or_default());
        }
        (Pos::Property(_), Tok::Word(_)) => s.pos = Pos::Term,
        (Pos::Term, Tok::Punct(p)) if is_comparison(p) => s.pos = Pos::Value,
        (Pos::Term, Tok::Punct(",")) => s.pos = Pos::Projection,
        (Pos::Value, Tok::Str | Tok::Num) => s.pos = Pos::Clause,

        _ => {}
    }
}

fn is_comparison(p: &str) -> bool {
    matches!(p, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=")
}

// ---- suggesting -----------------------------------------------------------------

/// The keywords a statement may open with, reads before writes: far more
/// queries read.
const OPENERS: &[(&str, &str)] = &[
    ("MATCH", "walk a pattern"),
    ("SEARCH", "a vector or keyword seed"),
    ("HYBRID", "fused retrieval"),
    ("CALL", "a graph algorithm"),
    ("CREATE", "write nodes and edges"),
    ("MERGE", "upsert by key"),
];

/// What may follow a complete pattern.
const AFTER_PATTERN: &[(&str, &str)] =
    &[("RETURN", "what to return"), ("WHERE", "filter the rows")];

/// What may follow a complete projection.
const TAIL: &[(&str, &str)] = &[
    ("ORDER BY", "sort the rows"),
    ("LIMIT", "cap the rows"),
    ("SKIP", "drop the first n"),
    ("AS", "name the column"),
    ("AS OF", "read a past snapshot"),
];

/// What a predicate is built from, once its variable is written.
const PREDICATE_TERMS: &[(&str, &str)] = &[
    ("NOT", "negate"),
    ("key(", "the external key"),
    ("score()", "the retrieval score"),
];

/// The folds a `RETURN` may project with.
const FOLDS: &[(&str, &str)] = &[
    ("count(*)", "how many rows"),
    ("collect(", "gather into a list"),
    ("sum(", "total"),
    ("avg(", "mean"),
    ("min(", "least"),
    ("max(", "greatest"),
];

fn suggest(expects: &Expect, word: &str, vocab: &Vocab, loose: bool) -> Vec<Suggestion> {
    match expects {
        Expect::Nothing | Expect::Value => Vec::new(),
        Expect::Statement => keywords(OPENERS, word),
        Expect::Node { var } => node_patterns(word, vocab, var),
        // A variable is the author's to name; there is nothing to complete
        // once they have started one.
        Expect::NodeVar { var } => at_boundary(word, var, "a name for this node"),
        Expect::Label => labels(word, vocab, ")"),
        Expect::NodeEnd => node_ends(word, vocab),
        Expect::Hop { from, var, bound } => hops(word, vocab, from.as_deref(), var, *bound),
        Expect::EdgeType {
            from,
            var,
            incoming,
            lead,
        } => {
            // A word typed where the bracket has not been opened yet is not
            // the start of a type — there is nowhere for it to go — so
            // nothing completes it.
            if lead.contains('[') && !word.is_empty() {
                return Vec::new();
            }
            edge_types(word, vocab, from.as_deref(), var, *incoming, lead)
        }
        Expect::RelEnd { .. } | Expect::Arrow { .. } if loose => Vec::new(),
        Expect::RelEnd { incoming, ranged } => {
            let close = if *incoming { "]-" } else { "]->" };
            // A `*` with no bounds is the one variable-length shape the
            // compiler refuses, so what follows one is a bound, not a bracket.
            if *ranged {
                at_boundary(word, &format!("1..3{close}"), "how many hops to walk")
            } else {
                at_boundary(word, close, "close the relationship")
            }
        }
        Expect::Arrow { incoming } => {
            let arrow = if *incoming { "-" } else { "->" };
            at_boundary(word, arrow, "close the relationship")
        }
        Expect::Predicate { vars } => {
            let mut out = variables(vars, word);
            out.extend(keywords(PREDICATE_TERMS, word));
            out
        }
        Expect::Projection { vars } => {
            // The last variable bound, first: a pattern's rows *are* its
            // terminal node, and returning an earlier variable's rows is the
            // one thing the compiler refuses (an earlier one still projects,
            // `RETURN p.name`).
            let terminal: Vec<String> = vars.iter().rev().cloned().collect();
            let mut out = variables(&terminal, word);
            out.extend(keywords(FOLDS, word));
            out
        }
        Expect::Property { label, .. } => properties(word, vocab, label.as_deref()),
        Expect::SortKey { vars } => {
            let mut out = variables(vars, word);
            out.extend(keywords(FOLDS, word));
            out
        }
        Expect::Clause { done } => keywords(TAIL, word)
            .into_iter()
            .filter(|s| !done.iter().any(|d| d.eq_ignore_ascii_case(&s.text)))
            .collect(),
    }
}

/// A fixed piece of syntax, offered only where the caret is at a boundary or
/// part-way through that very text — never appended to a word it does not
/// continue.
fn at_boundary(word: &str, text: &str, detail: &str) -> Vec<Suggestion> {
    if !text.starts_with(word) || text.len() == word.len() {
        return Vec::new();
    }
    vec![Suggestion {
        insert: text[word.len()..].to_string(),
        text: text.to_string(),
        detail: Some(detail.to_string()),
        kind: Kind::Snippet,
    }]
}

/// A keyword list, filtered by what is typed and cased to match it: a query
/// written in lower case stays in lower case, since the compiler folds case
/// and `matCH` would be nobody's idea of a completion.
fn keywords(list: &[(&str, &str)], word: &str) -> Vec<Suggestion> {
    list.iter()
        .filter(|(kw, _)| starts_with(kw, word))
        .map(|(kw, detail)| {
            let text = fit_case(kw, word);
            Suggestion {
                insert: text[word.len()..].to_string(),
                text,
                detail: Some((*detail).to_string()),
                kind: Kind::Keyword,
            }
        })
        .collect()
}

/// A keyword in the case the query is being written in. Only keywords are
/// re-cased: a label or a property is data, and its spelling is not ours to
/// change.
fn fit_case(keyword: &str, word: &str) -> String {
    let lower = !word.is_empty() && word.chars().all(|c| !c.is_alphabetic() || c.is_lowercase());
    if lower {
        keyword.to_ascii_lowercase()
    } else {
        keyword.to_string()
    }
}

/// The variables the pattern bound, which is what a predicate or a projection
/// is written about.
fn variables(vars: &[String], word: &str) -> Vec<Suggestion> {
    vars.iter()
        .filter(|v| starts_with(v, word))
        .map(|v| Suggestion {
            insert: v[word.len()..].to_string(),
            text: v.clone(),
            detail: Some("a bound node".to_string()),
            kind: Kind::Variable,
        })
        .collect()
}

/// Labels, most nodes first. `trailer` finishes the position — a label inside
/// `(n:` is followed by the `)` that closes it.
fn labels(word: &str, vocab: &Vocab, trailer: &str) -> Vec<Suggestion> {
    let mut hits: Vec<&LabelInfo> = vocab
        .labels
        .iter()
        .filter(|l| starts_with(&l.name, word))
        .collect();
    hits.sort_by_key(|l| (std::cmp::Reverse(l.count), l.name.clone()));
    hits.iter()
        .map(|l| Suggestion {
            insert: format!("{}{trailer}", &l.name[word.len()..]),
            text: l.name.clone(),
            detail: Some(nodes(l.count)),
            kind: Kind::Label,
        })
        .collect()
}

/// After `MATCH `: the whole node pattern, one per label the plane holds. With
/// no label to name — an empty plane, or one nobody has digested — the only
/// honest suggestion is `(n)`, which matches everything.
fn node_patterns(word: &str, vocab: &Vocab, var: &str) -> Vec<Suggestion> {
    if !word.is_empty() {
        return Vec::new();
    }
    if vocab.labels.is_empty() {
        return at_boundary(word, &format!("({var})"), "any node");
    }
    ranked_labels(vocab)
        .iter()
        .map(|l| {
            let text = format!("({var}:{})", l.name);
            Suggestion {
                insert: text.clone(),
                text,
                detail: Some(nodes(l.count)),
                kind: Kind::Snippet,
            }
        })
        .collect()
}

/// After `(n`: the label that narrows it, or the `)` that ends it.
fn node_ends(word: &str, vocab: &Vocab) -> Vec<Suggestion> {
    if !word.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Suggestion> = ranked_labels(vocab)
        .iter()
        .map(|l| {
            let text = format!(":{})", l.name);
            Suggestion {
                insert: text.clone(),
                text,
                detail: Some(nodes(l.count)),
                kind: Kind::Snippet,
            }
        })
        .collect();
    out.extend(at_boundary(word, ")", "any node"));
    out
}

/// Edge types leaving (or, `incoming`, entering) a label, most edges first,
/// written as the whole rest of the hop: whatever punctuation is still owed
/// before the type, the type, the bracket that closes it, and the node it
/// lands on — labelled with where such edges most often land.
fn edge_types(
    word: &str,
    vocab: &Vocab,
    from: Option<&str>,
    var: &str,
    incoming: bool,
    lead: &str,
) -> Vec<Suggestion> {
    // With punctuation still owed, the word before the caret is not part of
    // the type's name — it is the relationship's own variable, `-[r` — so the
    // type is written whole after it rather than completed from it.
    let typed = if lead.is_empty() { word } else { "" };
    ranked_edges(vocab, from, incoming)
        .iter()
        .filter(|(e, _, _)| starts_with(&e.name, typed))
        .map(|(e, count, dst)| Suggestion {
            insert: format!(
                "{lead}{}{}",
                &e.name[typed.len()..],
                hop_tail(*dst, var, incoming)
            ),
            text: e.name.clone(),
            detail: Some(edge_detail(*count, *dst)),
            kind: Kind::EdgeType,
        })
        .collect()
}

/// After a closed node: the hop that leaves it, and the clauses that would end
/// the pattern instead.
///
/// Both directions, ranked together, because plenty of labels are only ever
/// arrived at — an `UnresolvedRef` is called and calls nothing — and offering
/// such a node no hop at all is offering it nothing it wants. With no label
/// to go on there is no direction to tell apart either, so an unlabelled node
/// gets the outgoing form alone rather than each type twice.
///
/// The hop comes first while the pattern is one node long, because one more
/// hop is the reason to be writing a pattern at all; past that the reverse is
/// true, since a two-hop pattern is already a question and a third hop is
/// rarely one.
fn hops(word: &str, vocab: &Vocab, from: Option<&str>, var: &str, bound: usize) -> Vec<Suggestion> {
    let mut shapes = Vec::new();
    if word.is_empty() {
        let out = ranked_edges(vocab, from, false)
            .into_iter()
            .map(|(e, count, dst)| (e, count, dst, false));
        let back = from
            .map(|_| ranked_edges(vocab, from, true))
            .unwrap_or_default()
            .into_iter()
            .map(|(e, count, dst)| (e, count, dst, true));
        let mut both: Vec<_> = out.chain(back).collect();
        both.sort_by_key(|(e, count, _, incoming)| {
            (std::cmp::Reverse(*count), *incoming, e.name.clone())
        });
        shapes.extend(both.iter().map(|(e, count, dst, incoming)| {
            let arrow = if *incoming { "<-" } else { "-" };
            let text = format!("{arrow}[:{}{}", e.name, hop_tail(*dst, var, *incoming));
            Suggestion {
                insert: text.clone(),
                text,
                detail: Some(edge_detail(*count, *dst)),
                kind: Kind::Snippet,
            }
        }));
    }
    let mut clauses = keywords(AFTER_PATTERN, word);
    if bound > 1 {
        clauses.extend(shapes);
        return clauses;
    }
    shapes.extend(clauses);
    shapes
}

/// What closes a hop: the bracket, the arrow, and the node it lands on.
fn hop_tail(dst: Option<&str>, var: &str, incoming: bool) -> String {
    let close = if incoming { "]-" } else { "]->" };
    match dst {
        Some(dst) => format!("{close}({var}:{dst})"),
        None => format!("{close}({var})"),
    }
}

fn edge_detail(count: u64, dst: Option<&str>) -> String {
    match dst {
        Some(dst) => format!("{count} → {dst}"),
        None => format!("{count} edges"),
    }
}

fn nodes(count: u64) -> String {
    format!("{count} node{}", if count == 1 { "" } else { "s" })
}

fn ranked_labels(vocab: &Vocab) -> Vec<&LabelInfo> {
    let mut out: Vec<&LabelInfo> = vocab.labels.iter().collect();
    out.sort_by_key(|l| (std::cmp::Reverse(l.count), l.name.clone()));
    out
}

/// One end of a connection: which label a hop starts at, or lands on.
type End = fn(&Connection) -> &String;

/// Every edge type that can leave (or, `incoming`, enter) `from`, with how
/// many such edges there are and the label they most often reach.
///
/// With no label to go on, every type counts, ranked by how many edges the
/// plane holds of it — which is the best a pattern with an unlabelled node can
/// be told.
fn ranked_edges<'a>(
    vocab: &'a Vocab,
    from: Option<&str>,
    incoming: bool,
) -> Vec<(&'a EdgeInfo, u64, Option<&'a str>)> {
    let mut out: Vec<(&EdgeInfo, u64, Option<&str>)> = vocab
        .edges
        .iter()
        .filter_map(|e| {
            let Some(from) = from else {
                return Some((e, e.count, None));
            };
            // Which end of a connection the label sits at, and which end the
            // hop lands on, swap with the arrow's direction.
            let (here, there): (End, End) = if incoming {
                (|c| &c.dst, |c| &c.src)
            } else {
                (|c| &c.src, |c| &c.dst)
            };
            let mut total = 0;
            let mut best: Option<(&str, u64)> = None;
            for c in e.connections.iter().filter(|c| here(c) == from) {
                total += c.count;
                if best.is_none_or(|(_, n)| c.count > n) {
                    best = Some((there(c).as_str(), c.count));
                }
            }
            (total > 0).then_some((e, total, best.map(|(l, _)| l)))
        })
        .collect();
    out.sort_by_key(|(e, count, _)| (std::cmp::Reverse(*count), e.name.clone()));
    out
}

/// The properties a label's nodes hold. With no label — an unlabelled node —
/// every label's properties are fair game, since any of them could be it.
fn properties(word: &str, vocab: &Vocab, label: Option<&str>) -> Vec<Suggestion> {
    let mut names: Vec<&str> = Vec::new();
    for l in &vocab.labels {
        if label.is_none_or(|want| want == l.name) {
            for p in &l.properties {
                if starts_with(p, word) && !names.contains(&p.as_str()) {
                    names.push(p);
                }
            }
        }
    }
    names
        .iter()
        .map(|p| Suggestion {
            insert: p[word.len()..].to_string(),
            text: (*p).to_string(),
            detail: label.map(|l| format!("of {l}")),
            kind: Kind::Property,
        })
        .collect()
}

/// Case-insensitive prefix match, with something left to add: a label is
/// `Function` however it is being typed, and a word already complete is not a
/// suggestion.
fn starts_with(candidate: &str, word: &str) -> bool {
    candidate.len() > word.len()
        && candidate
            .get(..word.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(word))
}
