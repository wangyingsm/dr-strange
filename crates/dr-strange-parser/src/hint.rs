//! What to say beside a parse error: the grammar of the clauses the statement
//! reached for, not the whole language.
//!
//! An agent that pays per token should not carry the grammar in every tool
//! listing on the chance it one day writes a query. It should get the grammar
//! at the one moment it needs it — when a statement failed — and only the part
//! it was trying to use. The keywords a statement contains say which part that
//! is: a `SEARCH` that broke gets the `SEARCH` forms, a `CREATE` gets the
//! writes, and a statement that opens with no known keyword gets the overview.

/// The clauses a statement may open with, each with the one-line grammar an
/// agent can pattern a retry on.
const MATCH: &str = "\
MATCH (a:Label)-[:TYPE]->(b:Label)   one linear path; hops -[:T]->, <-[:T]-, -[:T]-, -->, --, -[:T*1..3]->
  e.g. MATCH (f:Fn)-[:CALLS]->(g:Fn) WHERE key(g) = \"m::run\" RETURN f";

const SEARCH: &str = "\
SEARCH (v:Label) [ON prop] NEAR \"text\"|[v, …] [METRIC cosine|l2] [TOPK k]   vector; ON defaults to `embedding`
SEARCH (v:Label) ON prop MATCHING \"words\" [TOPK k]                        BM25 keyword; label and ON required
  e.g. SEARCH (d:Doc) ON body MATCHING \"graph database\" TOPK 5 RETURN d";

const HYBRID: &str = "\
HYBRID (v:Label) [VECTOR [ON p] NEAR q [WEIGHT w]] [KEYWORD ON p MATCHING \"words\" [WEIGHT w]]
       [GRAPH HOPS h [DECAY d] [WEIGHT w]] [CANDIDATES n] [TOPK k]   channels in any order, at least one of VECTOR/KEYWORD";

const CALL: &str = "\
CALL pagerank|components|shortest_path|louvain(arg: v, …) ON (v[:Label])   the per-node result is score()
  e.g. CALL pagerank() ON (n:Fn) RETURN n ORDER BY score() DESC LIMIT 10";

const BEAM: &str = "\
BEAM (r[:Label]) OUT|IN|BOTH [:TYPE] ON prop NEAR \"text\"|[..] [METRIC m] WIDTH w DEPTH d   after any source";

const WHERE: &str = "\
WHERE one variable per predicate: = <> != < <= > >=, AND/OR/NOT, IS [NOT] NULL, x IN [a, b], a.prop, a:Label, key(a), score(), hops()";

const TAIL: &str = "\
RETURN [DISTINCT] var|*                whole records
RETURN a.prop, count(*) AS n, …        columns; folds count/sum/avg/min/max/collect [DISTINCT], grouped by the rest
ORDER BY expr|column|count(*)|sum(a.p)… [ASC|DESC]  SKIP n  LIMIT n  [AS OF seq|\"RFC-3339\"|TIME ms]";

const WRITES: &str = "\
CREATE (a:Label {key:\"k\", p:1})-[:TYPE]->(b:Label {key:$k2})   key is the external key: a string, or a $param bound to one
MERGE (a:Label {key:\"k\"}) [ON CREATE SET …] [ON MATCH SET …]
MATCH (n:Label) WHERE … SET n.p = v | SET n:Label | SET n += {…} | REMOVE n.p | REMOVE n:Label | [DETACH] DELETE n";

const OVERVIEW: &str = "\
a statement opens with one source — MATCH, SEARCH, HYBRID, CALL — or a write — CREATE, MERGE, MATCH … SET/REMOVE/DELETE";

/// How names and values are spelled — the part of the grammar every clause
/// shares, and the part a retry most often trips on: a quote inside a string,
/// a label with a space in it.
const LEXICAL: &str = "\
names: Unicode words, or `backticked` when they would not be (`order`, `first name`); keywords and function names are case-insensitive
strings: \"…\" or '…' with escapes \\\" \\' \\\\ \\n \\t \\uXXXX; $name is a bound parameter";

/// The grammar a failed statement was reaching for, chosen by the keywords it
/// contains. Sections are joined by blank lines; the result stands on its own
/// after an error message.
pub fn grammar_hint(input: &str) -> String {
    let words: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    let has = |k: &str| words.iter().any(|w| w == k);
    let opens = words.first().map(String::as_str);

    let mut sections: Vec<&str> = Vec::new();
    let is_write = matches!(opens, Some("CREATE" | "MERGE"))
        || (opens == Some("MATCH") && (has("SET") || has("REMOVE") || has("DELETE")));
    if is_write {
        sections.push(WRITES);
        if has("WHERE") {
            sections.push(WHERE);
        }
    } else {
        match opens {
            Some("MATCH") => sections.push(MATCH),
            Some("SEARCH") => sections.push(SEARCH),
            Some("HYBRID") => sections.push(HYBRID),
            Some("CALL") => sections.push(CALL),
            _ => {
                sections.push(OVERVIEW);
                sections.extend([MATCH, SEARCH, HYBRID, CALL]);
            }
        }
        if has("BEAM") {
            sections.push(BEAM);
        }
        if has("WHERE") {
            sections.push(WHERE);
        }
        sections.push(TAIL);
    }
    sections.push(LEXICAL);
    format!("grammar:\n{}", sections.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::grammar_hint;

    #[test]
    fn a_read_gets_its_source_and_the_tail_only() {
        let hint = grammar_hint("SEARCH (d:Doc) NEAR 'x' RETURN d");
        assert!(hint.contains("SEARCH (v:Label) [ON prop] NEAR"), "{hint}");
        assert!(hint.contains("RETURN [DISTINCT] var|*"), "{hint}");
        assert!(!hint.contains("HYBRID (v:Label)"), "{hint}");
        assert!(
            !hint.contains("WHERE one variable"),
            "no WHERE in the statement: {hint}"
        );
        assert!(!hint.contains("CREATE (a:Label"), "{hint}");
    }

    #[test]
    fn a_where_clause_brings_the_predicate_forms() {
        let hint = grammar_hint("MATCH (n) WHERE n.x ~ 3 RETURN n");
        assert!(hint.contains("MATCH (a:Label)"), "{hint}");
        assert!(hint.contains("WHERE one variable"), "{hint}");
    }

    #[test]
    fn a_write_gets_the_write_forms_not_the_read_tail() {
        for stmt in [
            "CREATE (a:P {key:'x'}",
            "MATCH (n:P) SET n.age = ",
            "MERGE (n:P)",
        ] {
            let hint = grammar_hint(stmt);
            assert!(hint.contains("MERGE (a:Label"), "{stmt}: {hint}");
            assert!(!hint.contains("RETURN [DISTINCT]"), "{stmt}: {hint}");
        }
    }

    /// The three readings of the language — the grammar, the completer and
    /// this — must agree on what may be written; the snippets here name what
    /// the grammar learned: a parameterised key, a fold as a sort key, backticks
    /// and escapes.
    #[test]
    fn the_snippets_name_what_the_grammar_reads() {
        let write = grammar_hint("MERGE (n:P {key: $k}");
        assert!(write.contains("{key:$k2}"), "{write}");
        assert!(write.contains("$param bound"), "{write}");
        let read = grammar_hint("MATCH (n:P) RETURN n ORDER BY count(");
        assert!(read.contains("ORDER BY expr|column|count(*)"), "{read}");
        for hint in [&write, &read, &grammar_hint("nonsense")] {
            assert!(hint.contains("`backticked`"), "{hint}");
            assert!(hint.contains("\\uXXXX"), "{hint}");
            assert!(hint.contains("case-insensitive"), "{hint}");
        }
    }

    #[test]
    fn an_unknown_opening_gets_the_overview() {
        let hint = grammar_hint("SELECT * FROM nodes");
        assert!(hint.contains("a statement opens with one source"), "{hint}");
        assert!(hint.contains("CALL pagerank"), "{hint}");
    }
}
