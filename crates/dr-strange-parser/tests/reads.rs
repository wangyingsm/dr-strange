//! Read queries, end to end: parse → run the plan against an in-memory
//! database → assert which rows come back. `tests/parse.rs` pins the plan a
//! query compiles to; this file pins what that plan *does*, for the cases
//! where the two can disagree (where a predicate is placed, what NULL does).

use dr_strange_core::{Database, NodeId, PlaneHandle};
use dr_strange_parser::{Statement, parse, parse_statement};

fn plane(db: &Database) -> PlaneHandle<'_> {
    db.plane("startup").unwrap()
}

/// Apply a write statement.
fn write(db: &Database, q: &str) {
    match parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}")) {
        Statement::Write(w) => {
            w.apply(&plane(db))
                .unwrap_or_else(|e| panic!("apply `{q}`: {e}"));
        }
        Statement::Read(_) => panic!("expected a write for `{q}`"),
    }
}

/// Run a read query and return the external keys of its rows, sorted.
fn keys(db: &Database, q: &str) -> Vec<String> {
    let read = parse(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let p = plane(db);
    let ids: Vec<NodeId> = p
        .query_from_plan(read.plan)
        .ids()
        .unwrap_or_else(|e| panic!("run `{q}`: {e}"));
    let mut out: Vec<String> = ids
        .into_iter()
        .map(|id| {
            p.node(id)
                .unwrap()
                .unwrap()
                .external_key
                .unwrap_or_default()
        })
        .collect();
    out.sort();
    out
}

// ---- variable-free predicates sit on the last slot -------------------------

/// a -> b -> c -> d, one chain.
fn chain(db: &Database) {
    write(
        db,
        r#"CREATE (a:N {key:"a"}), (b:N {key:"b"}), (c:N {key:"c"}), (d:N {key:"d"}),
                 (a)-[:R]->(b), (b)-[:R]->(c), (c)-[:R]->(d)"#,
    );
}

#[test]
fn hops_predicate_counts_the_walk_not_the_source() {
    let db = Database::in_memory().unwrap();
    chain(&db);
    // From a, 1..3 hops reach b, c, d; `hops() = 2` keeps c alone. Placed at
    // the source it would read hops() = 0 and keep nothing.
    assert_eq!(
        keys(
            &db,
            r#"MATCH (a:N)-[:R*1..3]->(n) WHERE key(a) = "a" AND hops() = 2 RETURN n"#
        ),
        vec!["c"]
    );
    assert_eq!(
        keys(
            &db,
            r#"MATCH (a:N)-[:R*1..3]->(n) WHERE key(a) = "a" AND hops() >= 2 RETURN n"#
        ),
        vec!["c", "d"]
    );
    // Mixed with an earlier variable in one conjunct it cannot be split off;
    // it is still evaluated at the end of the walk, reading `a` from the
    // row's trail, not pushed down to `a`'s slot where hops() is 0.
    assert_eq!(
        keys(
            &db,
            r#"MATCH (a:N)-[:R*1..3]->(n) WHERE key(a) = "a" AND (hops() = 2 OR a.x = 1) RETURN n"#
        ),
        vec!["c"]
    );
    // A constant predicate is placed there too, and still means what it says.
    assert_eq!(
        keys(&db, r#"MATCH (a:N)-[:R]->(n) WHERE 1 = 2 RETURN n"#),
        Vec::<String>::new()
    );
    assert_eq!(
        keys(&db, r#"MATCH (a:N)-[:R]->(n) WHERE 1 = 1 RETURN n"#),
        vec!["b", "c", "d"]
    );
}

#[test]
fn score_predicate_reads_the_search_score() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (x:Doc {key:"x", emb:[1.0, 0.0]}), (y:Doc {key:"y", emb:[0.5, 0.0]}),
                 (z:Doc {key:"z", emb:[0.0, 1.0]}), (t:Tag {key:"t"}),
                 (x)-[:HAS]->(t), (y)-[:HAS]->(t), (z)-[:HAS]->(t)"#,
    );
    // Dot-product similarity against [1, 0]: x = 1, y = 0.5, z = 0.
    assert_eq!(
        keys(
            &db,
            "SEARCH (d:Doc) ON emb NEAR [1.0, 0.0] METRIC dot TOPK 3 WHERE score() > 0.4 RETURN d"
        ),
        vec!["x", "y"]
    );
    // A seed's score survives the hop, so the same predicate after one is
    // still about the seed.
    assert_eq!(
        keys(
            &db,
            "SEARCH (d:Doc) ON emb NEAR [1.0, 0.0] METRIC dot TOPK 3 -[:HAS]->(t) \
             WHERE score() > 0.4 RETURN t"
        ),
        vec!["t", "t"]
    );
}

#[test]
fn score_predicate_after_a_beam_reads_the_beam_score() {
    let db = Database::in_memory().unwrap();
    write(
        &db,
        r#"CREATE (s:Seed {key:"s"}), (x:Doc {key:"x", emb:[1.0, 0.0]}),
                 (y:Doc {key:"y", emb:[0.5, 0.0]}), (z:Doc {key:"z", emb:[0.0, 1.0]}),
                 (s)-[:R]->(x), (s)-[:R]->(y), (s)-[:R]->(z)"#,
    );
    // A MATCH source has no score; the BEAM sets one. A `score()` predicate
    // therefore belongs after the beam — at the source it is NULL and the
    // whole result vanishes.
    assert_eq!(
        keys(
            &db,
            r#"MATCH (s:Seed) BEAM (r:Doc) OUT :R ON emb NEAR [1.0, 0.0] METRIC dot WIDTH 3 DEPTH 1
               WHERE score() > 0.4 RETURN r"#
        ),
        vec!["x", "y"]
    );
}

// ---- NULL: a missing property never satisfies a negated predicate ----------

/// Three docs: `a` has year 2020 and tag "x"; `b` has year 2021 and tag "y";
/// `c` has neither property.
fn docs(db: &Database) {
    write(
        db,
        r#"CREATE (a:Doc {key:"a", year:2020, tag:"x"}), (b:Doc {key:"b", year:2021, tag:"y"}),
                 (c:Doc {key:"c"})"#,
    );
}

#[test]
fn a_missing_property_fails_not_equal_and_not() {
    let db = Database::in_memory().unwrap();
    docs(&db);
    let cases: [(&str, &[&str]); 9] = [
        // `<>` in both spellings: c has no year, so it is neither equal nor unequal.
        ("MATCH (d:Doc) WHERE d.year <> 2020 RETURN d", &["b"]),
        ("MATCH (d:Doc) WHERE d.year != 2020 RETURN d", &["b"]),
        // NOT over an equality, a membership, an ordering, a string predicate.
        ("MATCH (d:Doc) WHERE NOT d.year = 2020 RETURN d", &["b"]),
        (
            "MATCH (d:Doc) WHERE NOT d.year IN [2020, 2019] RETURN d",
            &["b"],
        ),
        ("MATCH (d:Doc) WHERE NOT d.year > 2020 RETURN d", &["a"]),
        (
            r#"MATCH (d:Doc) WHERE NOT d.tag STARTS WITH "x" RETURN d"#,
            &["b"],
        ),
        // NOT NOT is the predicate again.
        ("MATCH (d:Doc) WHERE NOT NOT d.year = 2020 RETURN d", &["a"]),
        // `= null` is never true; IS NULL is the way to ask.
        ("MATCH (d:Doc) WHERE d.year = null RETURN d", &[]),
        ("MATCH (d:Doc) WHERE d.year IS NULL RETURN d", &["c"]),
    ];
    for (q, want) in cases {
        assert_eq!(keys(&db, q), want, "for `{q}`");
    }
}

#[test]
fn the_positive_forms_and_is_null_are_unchanged() {
    let db = Database::in_memory().unwrap();
    docs(&db);
    let cases: [(&str, &[&str]); 5] = [
        ("MATCH (d:Doc) WHERE d.year = 2020 RETURN d", &["a"]),
        (
            "MATCH (d:Doc) WHERE d.year IN [2020, 2021] RETURN d",
            &["a", "b"],
        ),
        ("MATCH (d:Doc) WHERE d.year >= 2020 RETURN d", &["a", "b"]),
        // An explicit IS NULL still reaches the absent rows through OR.
        (
            "MATCH (d:Doc) WHERE d.year <> 2020 OR d.year IS NULL RETURN d",
            &["b", "c"],
        ),
        (
            "MATCH (d:Doc) WHERE NOT d.year IS NULL RETURN d",
            &["a", "b"],
        ),
    ];
    for (q, want) in cases {
        assert_eq!(keys(&db, q), want, "for `{q}`");
    }
}
