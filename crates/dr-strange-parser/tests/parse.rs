//! Text → `LogicalPlan` assertions. Expected plans are built with core's own
//! `Source`/`Step`/`Expr` helpers, so these tests double as a spec for how each
//! Cypher construct maps onto the pipeline.

use dr_strange_core::{
    Algo, Dir, GraphChannel, HybridSpec, HybridWeights, KeywordChannel, LogicalPlan, Metric,
    NodeId, NodeRef, PropValue, SortKey, Source, Step, VectorChannel, distance, external_key,
    has_label, hops, lit, p, score, similarity,
};
use dr_strange_parser::{
    Embedder, Params, ParseError, Statement, parse, parse_statement, parse_statement_full,
    parse_with_embedder,
};

/// A deterministic stand-in for a real embedding provider: text → a tiny
/// vector derived from it, so tests can assert exact plans.
struct MockEmbedder;
impl Embedder for MockEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let first = text.chars().next().map_or(0.0, |c| c as u32 as f32);
        Ok(vec![text.len() as f32, first])
    }
}

fn plan(q: &str) -> LogicalPlan {
    parse(q)
        .unwrap_or_else(|e| panic!("parse failed for `{q}`: {e}"))
        .plan
}

#[test]
fn scan_label_only() {
    assert_eq!(
        plan("MATCH (n:Person) RETURN n"),
        LogicalPlan {
            source: Source::ScanLabel("Person".into()),
            steps: vec![],
        }
    );
}

#[test]
fn scan_all_star() {
    assert_eq!(
        plan("MATCH (n) RETURN *"),
        LogicalPlan {
            source: Source::ScanAll,
            steps: vec![],
        }
    );
}

#[test]
fn where_pushdown_on_source() {
    assert_eq!(
        plan("MATCH (p:Paper) WHERE p.year >= 2020 RETURN p"),
        LogicalPlan {
            source: Source::ScanLabel("Paper".into()),
            steps: vec![Step::Filter(p("year").ge(2020))],
        }
    );
}

#[test]
fn expand_out() {
    assert_eq!(
        plan("MATCH (p:Paper)-[:CITES]->(q) RETURN q").steps,
        vec![Step::Expand {
            dir: Dir::Out,
            edge_type: Some("CITES".into()),
        }]
    );
}

#[test]
fn where_pushes_to_the_right_slot() {
    // p.year is on the source node → filter BEFORE the expand; q's label is on
    // the frontier → HasLabel filter AFTER the expand.
    assert_eq!(
        plan("MATCH (p:Paper)-[:CITES]->(q:Paper) WHERE p.year >= 2020 RETURN q"),
        LogicalPlan {
            source: Source::ScanLabel("Paper".into()),
            steps: vec![
                Step::Filter(p("year").ge(2020)),
                Step::Expand {
                    dir: Dir::Out,
                    edge_type: Some("CITES".into()),
                },
                Step::Filter(has_label("Paper")),
            ],
        }
    );
}

#[test]
fn directions_and_bare_relationships() {
    let steps = |q: &str| plan(q).steps;
    assert_eq!(
        steps("MATCH (a)<-[:KNOWS]-(b) RETURN b"),
        vec![Step::Expand {
            dir: Dir::In,
            edge_type: Some("KNOWS".into()),
        }]
    );
    assert_eq!(
        steps("MATCH (a)-[:KNOWS]-(b) RETURN b"),
        vec![Step::Expand {
            dir: Dir::Both,
            edge_type: Some("KNOWS".into()),
        }]
    );
    assert_eq!(
        steps("MATCH (a)-->(b) RETURN b"),
        vec![Step::Expand {
            dir: Dir::Out,
            edge_type: None,
        }]
    );
    assert_eq!(
        steps("MATCH (a)--(b) RETURN b"),
        vec![Step::Expand {
            dir: Dir::Both,
            edge_type: None,
        }]
    );
    assert_eq!(
        steps("MATCH (a)<--(b) RETURN b"),
        vec![Step::Expand {
            dir: Dir::In,
            edge_type: None,
        }]
    );
}

#[test]
fn variable_length() {
    let steps = |q: &str| plan(q).steps;
    assert_eq!(
        steps("MATCH (a)-[:R*1..3]->(b) RETURN b"),
        vec![Step::ExpandVar {
            dir: Dir::Out,
            edge_type: Some("R".into()),
            min: 1,
            max: 3,
        }]
    );
    assert_eq!(
        steps("MATCH (a)-[:R*2]->(b) RETURN b"),
        vec![Step::ExpandVar {
            dir: Dir::Out,
            edge_type: Some("R".into()),
            min: 2,
            max: 2,
        }]
    );
    assert_eq!(
        steps("MATCH (a)-[:R*..4]->(b) RETURN b"),
        vec![Step::ExpandVar {
            dir: Dir::Out,
            edge_type: Some("R".into()),
            min: 1,
            max: 4,
        }]
    );
}

#[test]
fn distinct_order_skip_limit() {
    let pl = plan("MATCH (n:Person) RETURN DISTINCT n ORDER BY n.age DESC SKIP 5 LIMIT 10");
    assert_eq!(pl.source, Source::ScanLabel("Person".into()));
    assert_eq!(
        pl.steps,
        vec![
            Step::Distinct,
            Step::Sort(vec![SortKey {
                expr: p("age"),
                descending: true,
            }]),
            Step::Skip(5),
            Step::Limit(10),
        ]
    );
}

#[test]
fn where_logic_arithmetic_and_isnull() {
    let pl = plan("MATCH (n) WHERE n.a >= 1 AND (n.b < 2 OR NOT n.c = 3) AND n.d IS NULL RETURN n");
    assert_eq!(
        pl.steps,
        vec![
            Step::Filter(p("a").ge(1)),
            Step::Filter(p("b").lt(2).or(p("c").eq(3).not())),
            Step::Filter(p("d").is_null()),
        ]
    );
}

#[test]
fn arithmetic_precedence() {
    // a + b * 2 → a + (b*2)
    assert_eq!(
        plan("MATCH (n) WHERE n.a + n.b * 2 > 10 RETURN n").steps,
        vec![Step::Filter(p("a").add(p("b").mul(2)).gt(10))]
    );
}

#[test]
fn negative_literal_folds() {
    assert_eq!(
        plan("MATCH (n) WHERE n.x >= -5 RETURN n").steps,
        vec![Step::Filter(p("x").ge(-5))]
    );
}

#[test]
fn string_bool_and_label_predicates() {
    assert_eq!(
        plan("MATCH (n) WHERE n.name = 'Alice' RETURN n").steps,
        vec![Step::Filter(p("name").eq("Alice"))]
    );
    assert_eq!(
        plan("MATCH (n) WHERE n.active = true RETURN n").steps,
        vec![Step::Filter(p("active").eq(lit(true)))]
    );
    assert_eq!(
        plan("MATCH (n) WHERE n:Person RETURN n").steps,
        vec![Step::Filter(has_label("Person"))]
    );
}

#[test]
fn ne_operators_both_spellings() {
    assert_eq!(
        plan("MATCH (n) WHERE n.x <> 1 RETURN n").steps,
        plan("MATCH (n) WHERE n.x != 1 RETURN n").steps
    );
}

#[test]
fn case_insensitive_keywords() {
    let a = plan("match (n:Person) where n.age > 18 return n order by n.age limit 3");
    let b = plan("MATCH (n:Person) WHERE n.age > 18 RETURN n ORDER BY n.age LIMIT 3");
    assert_eq!(a, b);
}

// ---- rejected (clear errors, not silent mis-compiles) ---------------------

fn err(q: &str) -> ParseError {
    parse(q).expect_err(&format!("expected `{q}` to be rejected"))
}

#[test]
fn rejects_cross_variable_predicate() {
    assert!(matches!(
        err("MATCH (p)-[:R]->(q) WHERE p.x < q.x RETURN q"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_returning_earlier_variable() {
    assert!(matches!(
        err("MATCH (p:Paper)-[:R]->(q) RETURN p"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_unbounded_variable_length() {
    assert!(matches!(
        err("MATCH (a)-[:R*]->(b) RETURN b"),
        ParseError::Compile(_)
    ));
    assert!(matches!(
        err("MATCH (a)-[:R*2..]->(b) RETURN b"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_duplicate_variable() {
    assert!(matches!(
        err("MATCH (a)-[:R]->(a) RETURN a"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_trailing_and_missing_clauses() {
    assert!(matches!(
        err("MATCH (n) RETURN n EXTRA"),
        ParseError::Syntax(_)
    ));
    assert!(matches!(err("MATCH (n)"), ParseError::Syntax(_)));
    assert!(matches!(err("RETURN n"), ParseError::Syntax(_)));
}

/// Appendix B promises projections and aggregation are "a clear error, never
/// a silent mis-compile". A position in the middle of the clause is not that
/// error: `RETURN f.file, f.line, key(f)` stops the parse at the first dot and
/// used to surface as "unexpected trailing input near `.file, …`", which reads
/// like a typo in a query that has none. Each shape now names itself, and says
/// what this subset takes instead — these queries arrive from agents, and the
/// message is the only documentation they get.
#[test]
fn an_unsupported_return_says_which_shape_and_what_to_write() {
    let cases = [
        ("MATCH (f:Fn) RETURN f.file, f.line, key(f)", "projection"),
        ("MATCH (f:Fn) RETURN f, key(f)", "column list"),
        ("MATCH (f:Fn) RETURN key(f)", "key(…)"),
        ("MATCH (f:Fn) RETURN count(f)", "count(…)"),
        ("MATCH (f:Fn) RETURN f AS name", "aliasing"),
    ];
    for (query, shape) in cases {
        let e = err(query);
        assert!(
            matches!(e, ParseError::Compile(_)),
            "`{query}` is unsupported, not mistyped: {e}"
        );
        let said = e.to_string();
        assert!(
            said.contains(shape) && said.contains("RETURN takes one variable or `*`"),
            "`{query}` should name the shape and the rule: {said}"
        );
    }
    // The position survives, because a long query needs one.
    assert!(
        err("MATCH (f:Fn) RETURN f.file")
            .to_string()
            .contains(".file")
    );
}

/// Trailing text that is *not* a RETURN this subset lacks stays a plain syntax
/// error — including `AS OF` with an argument it cannot read, which is a
/// broken clause rather than an alias.
#[test]
fn other_trailing_input_is_still_a_syntax_error() {
    assert!(matches!(
        err("MATCH (n) RETURN n AS OF yesterday"),
        ParseError::Syntax(_)
    ));
    assert!(matches!(
        err("MATCH (n) RETURN n LIMIT 5 EXTRA"),
        ParseError::Syntax(_)
    ));
}

#[test]
fn rejects_contradictory_relationship_direction() {
    // `<-...->` has two arrowheads and is meaningless.
    assert!(matches!(
        err("MATCH (a)<-[:R]->(b) RETURN b"),
        ParseError::Syntax(_)
    ));
}

#[test]
fn rejects_empty_variable_length_range() {
    assert!(matches!(
        err("MATCH (a)-[:R*3..1]->(b) RETURN b"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_order_by_non_returned_variable() {
    assert!(matches!(
        err("MATCH (a)-[:R]->(b) RETURN b ORDER BY a.x"),
        ParseError::Compile(_)
    ));
}

// ---- literal / operator coverage ------------------------------------------

#[test]
fn float_literal() {
    assert_eq!(
        plan("MATCH (n) WHERE n.score >= 1.5 RETURN n").steps,
        vec![Step::Filter(p("score").ge(lit(1.5)))]
    );
}

#[test]
fn is_not_null() {
    assert_eq!(
        plan("MATCH (n) WHERE n.x IS NOT NULL RETURN n").steps,
        vec![Step::Filter(p("x").is_null().not())]
    );
}

#[test]
fn subtraction_and_division() {
    assert_eq!(
        plan("MATCH (n) WHERE n.a - n.b > 0 RETURN n").steps,
        vec![Step::Filter(p("a").sub(p("b")).gt(0))]
    );
    assert_eq!(
        plan("MATCH (n) WHERE n.a / 2 > 1 RETURN n").steps,
        vec![Step::Filter(p("a").div(2).gt(1))]
    );
}

#[test]
fn constant_predicate_lands_at_source() {
    assert_eq!(
        plan("MATCH (n) WHERE 1 = 1 RETURN n").steps,
        vec![Step::Filter(lit(1).eq(1))]
    );
}

#[test]
fn negation_of_float_literal_and_expression() {
    // -literal folds to a literal…
    assert_eq!(
        plan("MATCH (n) WHERE n.x >= -1.5 RETURN n").steps,
        vec![Step::Filter(p("x").ge(lit(-1.5)))]
    );
    // …but negating a non-literal becomes `0 - x` (core has no negate node).
    assert_eq!(
        plan("MATCH (n) WHERE -n.x > 0 RETURN n").steps,
        vec![Step::Filter(lit(0).sub(p("x")).gt(0))]
    );
}

#[test]
fn false_and_null_literals() {
    assert_eq!(
        plan("MATCH (n) WHERE n.active = false RETURN n").steps,
        vec![Step::Filter(p("active").eq(lit(false)))]
    );
    assert_eq!(
        plan("MATCH (n) WHERE n.x = null RETURN n").steps,
        vec![Step::Filter(p("x").eq(lit(PropValue::Null)))]
    );
}

#[test]
fn double_quoted_string() {
    assert_eq!(
        plan("MATCH (n) WHERE n.name = \"Bob\" RETURN n").steps,
        vec![Step::Filter(p("name").eq("Bob"))]
    );
}

#[test]
fn rejects_unknown_variable_in_where() {
    assert!(matches!(
        err("MATCH (n) WHERE m.x = 1 RETURN n"),
        ParseError::Compile(_)
    ));
}

#[test]
fn rejects_unknown_variable_in_return_and_order_by() {
    assert!(matches!(
        err("MATCH (n) RETURN zzz"),
        ParseError::Compile(_)
    ));
    assert!(matches!(
        err("MATCH (n) RETURN n ORDER BY zzz.x"),
        ParseError::Compile(_)
    ));
}

// ---- vector search --------------------------------------------------------

#[test]
fn search_compiles_to_vector_topk() {
    assert_eq!(
        plan("SEARCH (p:Paper) ON embedding NEAR [1.0, 0.0, -0.5] METRIC cosine TOPK 5 RETURN p"),
        LogicalPlan {
            source: Source::VectorTopK {
                label: Some("Paper".into()),
                property: "embedding".into(),
                query: vec![1.0, 0.0, -0.5],
                metric: Metric::Cosine,
                k: 5,
            },
            steps: vec![],
        }
    );
}

#[test]
fn search_defaults_metric_cosine_topk_10_and_no_label() {
    assert_eq!(
        plan("SEARCH (n) ON emb NEAR [1.0] RETURN n"),
        LogicalPlan {
            source: Source::VectorTopK {
                label: None,
                property: "emb".into(),
                query: vec![1.0],
                metric: Metric::Cosine,
                k: 10,
            },
            steps: vec![],
        }
    );
}

#[test]
fn search_with_where_and_order_by_score() {
    let pl = plan(
        "SEARCH (p:Paper) ON emb NEAR [1.0, 0.0] TOPK 20 \
         WHERE p.year >= 2020 RETURN p ORDER BY score() DESC LIMIT 5",
    );
    assert_eq!(
        pl.source,
        Source::VectorTopK {
            label: Some("Paper".into()),
            property: "emb".into(),
            query: vec![1.0, 0.0],
            metric: Metric::Cosine,
            k: 20,
        }
    );
    assert_eq!(
        pl.steps,
        vec![
            Step::Filter(p("year").ge(2020)),
            Step::Sort(vec![SortKey {
                expr: score(),
                descending: true,
            }]),
            Step::Limit(5),
        ]
    );
}

#[test]
fn search_metric_l2_and_dot() {
    let m = |q: &str| match plan(q).source {
        Source::VectorTopK { metric, .. } => metric,
        _ => panic!("expected VectorTopK"),
    };
    assert_eq!(
        m("SEARCH (n) ON e NEAR [1.0] METRIC l2 RETURN n"),
        Metric::L2
    );
    assert_eq!(
        m("SEARCH (n) ON e NEAR [1.0] METRIC dot RETURN n"),
        Metric::Dot
    );
}

#[test]
fn similarity_in_order_by_brute_force_rank() {
    assert_eq!(
        plan(
            "MATCH (n:Paper) RETURN n ORDER BY similarity(n.embedding, [1.0, 0.0], cosine) DESC LIMIT 10"
        ),
        LogicalPlan {
            source: Source::ScanLabel("Paper".into()),
            steps: vec![
                Step::Sort(vec![SortKey {
                    expr: similarity("embedding", vec![1.0, 0.0], Metric::Cosine),
                    descending: true,
                }]),
                Step::Limit(10),
            ],
        }
    );
}

#[test]
fn distance_in_where_defaults_cosine() {
    assert_eq!(
        plan("MATCH (n) WHERE distance(n.emb, [1.0]) < 0.5 RETURN n").steps,
        vec![Step::Filter(
            distance("emb", vec![1.0], Metric::Cosine).lt(lit(0.5))
        )]
    );
}

#[test]
fn score_hops_fusion_in_order_by() {
    assert_eq!(
        plan("MATCH (a)-[:R*1..2]->(n) RETURN n ORDER BY score() * 0.7 + hops() DESC").steps,
        vec![
            Step::ExpandVar {
                dir: Dir::Out,
                edge_type: Some("R".into()),
                min: 1,
                max: 2,
            },
            Step::Sort(vec![SortKey {
                expr: score().mul(lit(0.7)).add(hops()),
                descending: true,
            }]),
        ]
    );
}

#[test]
fn search_by_text_embeds_via_the_embedder() {
    // "hi" → MockEmbedder → [len=2, first='h'=104].
    let pl = parse_with_embedder(
        "SEARCH (p:Paper) ON embedding NEAR \"hi\" TOPK 5 RETURN p",
        &MockEmbedder,
    )
    .unwrap();
    assert_eq!(
        pl.plan.source,
        Source::VectorTopK {
            label: Some("Paper".into()),
            property: "embedding".into(),
            query: vec![2.0, 'h' as u32 as f32],
            metric: Metric::Cosine,
            k: 5,
        }
    );
}

#[test]
fn text_search_without_an_embedder_is_a_clear_error() {
    let e = parse("SEARCH (n) ON e NEAR \"hello\" RETURN n").unwrap_err();
    assert!(matches!(e, ParseError::Compile(_)));
    assert!(
        e.to_string().contains("embedding provider"),
        "message should point at the missing provider: {e}"
    );
}

#[test]
fn similarity_by_text_embeds_too() {
    // "cat" → [3, 'c'=99].
    let pl = parse_with_embedder(
        "MATCH (n:Paper) RETURN n ORDER BY similarity(n.embedding, \"cat\") DESC LIMIT 3",
        &MockEmbedder,
    )
    .unwrap();
    assert_eq!(
        pl.plan.steps,
        vec![
            Step::Sort(vec![SortKey {
                expr: similarity("embedding", vec![3.0, 'c' as u32 as f32], Metric::Cosine),
                descending: true,
            }]),
            Step::Limit(3),
        ]
    );
}

#[test]
fn beam_after_match_compiles_to_expand_beam() {
    assert_eq!(
        plan(
            "MATCH (a:Paper) \
             BEAM (b:Paper) OUT :CITES ON embedding NEAR [1.0, 0.0] METRIC cosine WIDTH 4 DEPTH 3 \
             RETURN b"
        ),
        LogicalPlan {
            source: Source::ScanLabel("Paper".into()),
            steps: vec![
                Step::ExpandBeam {
                    dir: Dir::Out,
                    edge_type: Some("CITES".into()),
                    property: "embedding".into(),
                    query: vec![1.0, 0.0],
                    metric: Metric::Cosine,
                    width: 4,
                    depth: 3,
                },
                // b's label → a HasLabel filter on the beam frontier.
                Step::Filter(has_label("Paper")),
            ],
        }
    );
}

#[test]
fn beam_defaults_metric_and_supports_directions() {
    // No METRIC → cosine; IN direction; no edge type; anonymous result.
    assert_eq!(
        plan("MATCH (a) BEAM (b) IN ON emb NEAR [1.0] WIDTH 2 DEPTH 1 RETURN b").steps,
        vec![Step::ExpandBeam {
            dir: Dir::In,
            edge_type: None,
            property: "emb".into(),
            query: vec![1.0],
            metric: Metric::Cosine,
            width: 2,
            depth: 1,
        }]
    );
}

#[test]
fn beam_embeds_text_and_composes_after_search() {
    // "cat" → MockEmbedder → [3, 'c'].
    let pl = parse_with_embedder(
        "SEARCH (a:Paper) ON embedding NEAR [1.0, 0.0] TOPK 5 \
         BEAM (b) OUT :CITES ON embedding NEAR \"cat\" WIDTH 3 DEPTH 2 \
         RETURN b ORDER BY score() DESC",
        &MockEmbedder,
    )
    .unwrap();
    assert_eq!(
        pl.plan.source,
        Source::VectorTopK {
            label: Some("Paper".into()),
            property: "embedding".into(),
            query: vec![1.0, 0.0],
            metric: Metric::Cosine,
            k: 5,
        }
    );
    assert_eq!(
        pl.plan.steps,
        vec![
            Step::ExpandBeam {
                dir: Dir::Out,
                edge_type: Some("CITES".into()),
                property: "embedding".into(),
                query: vec![3.0, 'c' as u32 as f32],
                metric: Metric::Cosine,
                width: 3,
                depth: 2,
            },
            Step::Sort(vec![SortKey {
                expr: score(),
                descending: true,
            }]),
        ]
    );
}

#[test]
fn beam_requires_width_and_depth() {
    assert!(matches!(
        err("MATCH (a) BEAM (b) OUT ON emb NEAR [1.0] WIDTH 2 RETURN b"),
        ParseError::Syntax(_)
    ));
}

#[test]
fn rejects_unknown_function_and_metric() {
    assert!(matches!(
        err("MATCH (n) RETURN n ORDER BY foo(n.x)"),
        ParseError::Syntax(_)
    ));
    assert!(matches!(
        err("SEARCH (n) ON e NEAR [1.0] METRIC banana RETURN n"),
        ParseError::Syntax(_)
    ));
}

#[test]
fn where_param_resolves_at_parse_time() {
    let mut params = Params::new();
    params.insert("min".into(), PropValue::Int(25));
    params.insert("who".into(), PropValue::Str("Alice".into()));
    let stmt = parse_statement_full(
        "MATCH (n:Person) WHERE n.age > $min AND n.name = $who RETURN n",
        None,
        &params,
    )
    .unwrap();
    let plan = match stmt {
        Statement::Read(r) => r.plan,
        Statement::Write(_) => panic!("expected read"),
    };
    assert_eq!(
        plan.steps,
        vec![
            Step::Filter(p("age").gt(25)),
            Step::Filter(p("name").eq("Alice")),
        ]
    );
}

#[test]
fn unbound_param_in_a_read_errors() {
    // No params → `$missing` can't resolve.
    assert!(matches!(
        parse_statement("MATCH (n) WHERE n.x = $missing RETURN n"),
        Err(ParseError::Compile(_))
    ));
}

#[test]
fn error_display_distinguishes_syntax_from_compile() {
    assert!(
        parse("MATCH (n)")
            .unwrap_err()
            .to_string()
            .starts_with("syntax error:")
    );
    assert!(
        parse("MATCH (a)-[:R*]->(b) RETURN b")
            .unwrap_err()
            .to_string()
            .starts_with("unsupported query:")
    );
}

// ---- key-seek (ROADMAP §7) -----------------------------------------------

#[test]
fn key_equality_on_the_source_becomes_a_seek() {
    // The seek replaces the scan; the label survives as a filter, because
    // SeekKeys resolves by key alone.
    assert_eq!(
        plan(r#"MATCH (n:Doc) WHERE key(n) = "paper-42" RETURN n"#),
        LogicalPlan {
            source: Source::SeekKeys(vec!["paper-42".into()]),
            steps: vec![Step::Filter(has_label("Doc"))],
        }
    );
}

#[test]
fn unlabelled_key_seek_has_no_residual_filter() {
    assert_eq!(
        plan(r#"MATCH (n) WHERE key(n) = "paper-42" RETURN n"#),
        LogicalPlan {
            source: Source::SeekKeys(vec!["paper-42".into()]),
            steps: vec![],
        }
    );
}

#[test]
fn key_in_list_seeks_every_key() {
    assert_eq!(
        plan(r#"MATCH (n) WHERE key(n) IN ["a", "b", "c"] RETURN n"#),
        LogicalPlan {
            source: Source::SeekKeys(vec!["a".into(), "b".into(), "c".into()]),
            steps: vec![],
        }
    );
}

#[test]
fn key_seek_keeps_the_other_conjuncts_as_filters() {
    assert_eq!(
        plan(r#"MATCH (n:Doc) WHERE key(n) = "k" AND n.year >= 2020 RETURN n"#),
        LogicalPlan {
            source: Source::SeekKeys(vec!["k".into()]),
            steps: vec![
                Step::Filter(has_label("Doc")),
                Step::Filter(p("year").ge(2020)),
            ],
        }
    );
}

#[test]
fn key_seek_anchors_a_traversal() {
    assert_eq!(
        plan(r#"MATCH (n)-[:KNOWS]->(m:Person) WHERE key(n) = "ada" RETURN m"#),
        LogicalPlan {
            source: Source::SeekKeys(vec!["ada".into()]),
            steps: vec![
                Step::Expand {
                    dir: Dir::Out,
                    edge_type: Some("KNOWS".into()),
                },
                Step::Filter(has_label("Person")),
            ],
        }
    );
}

#[test]
fn key_on_a_later_variable_stays_a_filter() {
    // Only the *source* variable can become a seek; elsewhere `key()` is an
    // ordinary expression over the current node.
    assert_eq!(
        plan(r#"MATCH (a:Person)-[:KNOWS]->(b) WHERE key(b) = "alan" RETURN b"#),
        LogicalPlan {
            source: Source::ScanLabel("Person".into()),
            steps: vec![
                Step::Expand {
                    dir: Dir::Out,
                    edge_type: Some("KNOWS".into()),
                },
                Step::Filter(external_key().eq("alan")),
            ],
        }
    );
}

#[test]
fn key_seek_resolves_a_parameter() {
    let mut params = Params::new();
    params.insert("k".into(), PropValue::Str("paper-7".into()));
    let stmt =
        parse_statement_full(r#"MATCH (n) WHERE key(n) = $k RETURN n"#, None, &params).unwrap();
    let Statement::Read(read) = stmt else {
        panic!("expected a read")
    };
    assert_eq!(read.plan.source, Source::SeekKeys(vec!["paper-7".into()]));
}

#[test]
fn key_is_usable_as_an_ordinary_term() {
    // Not an equality, so no seek — just a predicate over the key.
    assert_eq!(
        plan("MATCH (n) WHERE key(n) IS NOT NULL RETURN n"),
        LogicalPlan {
            source: Source::ScanAll,
            steps: vec![Step::Filter(external_key().is_null().not())],
        }
    );
}

#[test]
fn in_over_a_property_expands_to_equalities() {
    assert_eq!(
        plan("MATCH (n) WHERE n.year IN [2020, 2021] RETURN n"),
        LogicalPlan {
            source: Source::ScanAll,
            steps: vec![Step::Filter(p("year").eq(2020).or(p("year").eq(2021)))],
        }
    );
}

#[test]
fn string_predicates_parse_at_comparison_precedence() {
    for (query, expected) in [
        (
            r#"MATCH (n) WHERE n.title CONTAINS "graph" RETURN n"#,
            p("title").contains("graph"),
        ),
        (
            r#"MATCH (n) WHERE n.name STARTS WITH "Al" RETURN n"#,
            p("name").starts_with("Al"),
        ),
        (
            r#"MATCH (n) WHERE n.file ENDS WITH ".pdf" RETURN n"#,
            p("file").ends_with(".pdf"),
        ),
    ] {
        assert_eq!(
            plan(query).steps,
            vec![Step::Filter(expected)],
            "for `{query}`"
        );
    }
}

#[test]
fn string_predicates_are_case_insensitive_and_compose() {
    // Keywords lex case-insensitively like the rest of the language, and sit
    // at comparison precedence so AND binds looser — which the compiler then
    // splits into one pushable Filter per conjunct.
    assert_eq!(
        plan(r#"MATCH (n) WHERE n.a starts with "x" AND n.b contains "y" RETURN n"#).steps,
        vec![
            Step::Filter(p("a").starts_with("x")),
            Step::Filter(p("b").contains("y")),
        ]
    );
}

/// `contains` is only an operator on a word boundary — a property called
/// `contains_pii` must still parse as a property.
#[test]
fn a_property_named_like_an_operator_is_not_an_operator() {
    assert_eq!(
        plan("MATCH (n) WHERE n.contains_pii = true RETURN n").steps,
        vec![Step::Filter(p("contains_pii").eq(true))]
    );
}

/// `IN` over a literal list stays sugar for equalities; `IN` over anything
/// else is membership evaluated per row, which cannot be expanded that way.
#[test]
fn in_over_a_non_literal_is_membership_not_sugar() {
    assert_eq!(
        plan(r#"MATCH (n) WHERE "graph" IN n.tags RETURN n"#).steps,
        vec![Step::Filter(lit("graph").is_in(p("tags")))]
    );
    // The literal-list form is untouched.
    assert_eq!(
        plan("MATCH (n) WHERE n.year IN [2020] RETURN n").steps,
        vec![Step::Filter(p("year").eq(2020))]
    );
}

// ---- keyword search (ROADMAP §7) -----------------------------------------

#[test]
fn keyword_search_compiles_to_a_bm25_source() {
    assert_eq!(
        plan(r#"SEARCH (d:Doc) ON body MATCHING "graph databases" TOPK 5 RETURN d"#),
        LogicalPlan {
            source: Source::KeywordTopK {
                label: "Doc".into(),
                property: "body".into(),
                query: "graph databases".into(),
                k: 5,
            },
            steps: vec![],
        }
    );
}

#[test]
fn keyword_search_defaults_topk_and_chains_a_typed_hop() {
    assert_eq!(
        plan(r#"SEARCH (d:Doc) ON body MATCHING "rust" -[:CITES]->(p:Paper) RETURN p"#),
        LogicalPlan {
            source: Source::KeywordTopK {
                label: "Doc".into(),
                property: "body".into(),
                query: "rust".into(),
                k: 10,
            },
            steps: vec![
                Step::Expand {
                    dir: Dir::Out,
                    edge_type: Some("CITES".into()),
                },
                Step::Filter(has_label("Paper")),
            ],
        }
    );
}

#[test]
fn keyword_search_needs_a_label() {
    assert!(matches!(
        err(r#"SEARCH (d) ON body MATCHING "rust" RETURN d"#),
        ParseError::Compile(_)
    ));
}

#[test]
fn vector_search_chains_a_typed_hop_too() {
    assert_eq!(
        plan("SEARCH (d:Doc) ON emb NEAR [1.0, 2.0] TOPK 3 -[:CITES]->(p) RETURN p"),
        LogicalPlan {
            source: Source::VectorTopK {
                label: Some("Doc".into()),
                property: "emb".into(),
                query: vec![1.0, 2.0],
                metric: Metric::Cosine,
                k: 3,
            },
            steps: vec![Step::Expand {
                dir: Dir::Out,
                edge_type: Some("CITES".into()),
            }],
        }
    );
}

// ---- hybrid retrieval (ROADMAP §7) ---------------------------------------

#[test]
fn hybrid_all_three_channels() {
    assert_eq!(
        plan(
            r#"HYBRID (d:Doc)
                 VECTOR ON embedding NEAR [1.0, 2.0] METRIC dot WEIGHT 2.0
                 KEYWORD ON body MATCHING "graph databases" WEIGHT 1.5
                 GRAPH HOPS 2 DECAY 0.5 SEEDS 5 WEIGHT 0.25
                 CANDIDATES 50 TOPK 7
               RETURN d"#
        ),
        LogicalPlan {
            source: Source::Hybrid(Box::new(HybridSpec {
                label: Some("Doc".into()),
                vector: Some(VectorChannel {
                    property: "embedding".into(),
                    query: vec![1.0, 2.0],
                    metric: Metric::Dot,
                }),
                keyword: Some(KeywordChannel {
                    property: "body".into(),
                    query: "graph databases".into(),
                }),
                graph: Some(GraphChannel {
                    hops: 2,
                    decay: 0.5,
                    seeds: 5,
                }),
                weights: HybridWeights {
                    vector: 2.0,
                    keyword: 1.5,
                    graph: 0.25,
                },
                candidates: 50,
                k: 7,
            })),
            steps: vec![],
        }
    );
}

#[test]
fn hybrid_defaults_and_channel_subset() {
    let LogicalPlan { source, steps } =
        plan(r#"HYBRID (d:Doc) KEYWORD ON body MATCHING "rust" RETURN d"#);
    assert!(steps.is_empty());
    let Source::Hybrid(spec) = source else {
        panic!("expected a hybrid source")
    };
    assert!(spec.vector.is_none() && spec.graph.is_none());
    assert_eq!(spec.weights, HybridWeights::default());
    assert_eq!((spec.candidates, spec.k), (100, 10));
}

#[test]
fn hybrid_composes_with_the_rest_of_the_query() {
    let steps = plan(
        r#"HYBRID (d:Doc) KEYWORD ON body MATCHING "rust" TOPK 20
             -[:CITES]->(p:Paper)
           WHERE p.year >= 2020
           RETURN p ORDER BY score() DESC LIMIT 5"#,
    )
    .steps;
    assert_eq!(
        steps,
        vec![
            Step::Expand {
                dir: Dir::Out,
                edge_type: Some("CITES".into()),
            },
            Step::Filter(has_label("Paper")),
            Step::Filter(p("year").ge(2020)),
            Step::Sort(vec![SortKey {
                expr: score(),
                descending: true,
            }]),
            Step::Limit(5),
        ]
    );
}

#[test]
fn hybrid_needs_a_retrieval_channel() {
    assert!(matches!(
        err("HYBRID (d:Doc) GRAPH HOPS 2 DECAY 0.5 RETURN d"),
        ParseError::Compile(_)
    ));
}

#[test]
fn hybrid_keyword_channel_needs_a_label() {
    assert!(matches!(
        err(r#"HYBRID (d) KEYWORD ON body MATCHING "rust" RETURN d"#),
        ParseError::Compile(_)
    ));
}

// ---- algorithms (ROADMAP §7) ---------------------------------------------

#[test]
fn call_pagerank_with_arguments() {
    assert_eq!(
        plan(
            "CALL pagerank(damping: 0.5, iterations: 3, tolerance: 0.01) ON (n:Paper) \
             RETURN n ORDER BY score() DESC LIMIT 10"
        ),
        LogicalPlan {
            source: Source::Algo {
                label: Some("Paper".into()),
                algo: Algo::PageRank {
                    damping: 0.5,
                    max_iters: 3,
                    tolerance: 0.01,
                },
            },
            steps: vec![
                Step::Sort(vec![SortKey {
                    expr: score(),
                    descending: true,
                }]),
                Step::Limit(10),
            ],
        }
    );
}

#[test]
fn call_defaults_match_the_builder_api() {
    assert_eq!(
        plan("CALL pagerank() ON (n) RETURN n").source,
        Source::Algo {
            label: None,
            algo: Algo::PageRank {
                damping: 0.85,
                max_iters: 20,
                tolerance: 1e-6,
            },
        }
    );
}

#[test]
fn call_components_and_louvain() {
    assert_eq!(
        plan("CALL components() ON (n:Doc) RETURN n").source,
        Source::Algo {
            label: Some("Doc".into()),
            algo: Algo::ConnectedComponents,
        }
    );
    assert_eq!(
        plan("CALL louvain(max_levels: 3) ON (n) RETURN n").source,
        Source::Algo {
            label: None,
            algo: Algo::Louvain {
                max_levels: 3,
                min_gain: 1e-9,
            },
        }
    );
}

#[test]
fn call_shortest_path_by_key_and_by_id() {
    assert_eq!(
        plan(r#"CALL shortest_path(from: "ada", to: 7, dir: "both") ON (n) RETURN n"#).source,
        Source::Algo {
            label: None,
            algo: Algo::ShortestPath {
                from: NodeRef::Key("ada".into()),
                to: NodeRef::Id(NodeId(7)),
                dir: Dir::Both,
                weight: None,
            },
        }
    );
}

#[test]
fn call_result_composes_with_a_typed_hop() {
    assert_eq!(
        plan("CALL pagerank() ON (n:Paper) -[:CITES]->(q:Paper) RETURN q").steps,
        vec![
            Step::Expand {
                dir: Dir::Out,
                edge_type: Some("CITES".into()),
            },
            Step::Filter(has_label("Paper")),
        ]
    );
}

#[test]
fn call_rejects_unknown_algorithms_and_arguments() {
    assert!(matches!(
        err("CALL betweenness() ON (n) RETURN n"),
        ParseError::Compile(_)
    ));
    assert!(matches!(
        err("CALL pagerank(dampening: 0.85) ON (n) RETURN n"),
        ParseError::Compile(_)
    ));
    // shortest_path needs both endpoints.
    assert!(matches!(
        err(r#"CALL shortest_path(from: "ada") ON (n) RETURN n"#),
        ParseError::Compile(_)
    ));
    // and a direction it understands.
    assert!(matches!(
        err(r#"CALL shortest_path(from: "a", to: "b", dir: "sideways") ON (n) RETURN n"#),
        ParseError::Compile(_)
    ));
}

// ---- AS OF (ROADMAP §7) ---------------------------------------------------

fn as_of(q: &str) -> Option<dr_strange_parser::AsOfSpec> {
    parse(q)
        .unwrap_or_else(|e| panic!("parse failed for `{q}`: {e}"))
        .as_of
}

#[test]
fn as_of_reads_a_commit_sequence() {
    assert_eq!(
        as_of("MATCH (n:Doc) RETURN n LIMIT 5 AS OF 41337"),
        Some(dr_strange_parser::AsOfSpec::Seq(41337))
    );
}

#[test]
fn as_of_reads_an_rfc3339_instant() {
    // 2026-07-01T00:00:00Z
    assert_eq!(
        as_of(r#"MATCH (n) RETURN n AS OF "2026-07-01T00:00:00Z""#),
        Some(dr_strange_parser::AsOfSpec::Time(1_782_864_000_000))
    );
    // The unix epoch itself, and a fractional-second offset form.
    assert_eq!(
        as_of(r#"MATCH (n) RETURN n AS OF "1970-01-01T00:00:00Z""#),
        Some(dr_strange_parser::AsOfSpec::Time(0))
    );
    assert_eq!(
        as_of(r#"MATCH (n) RETURN n AS OF "1970-01-01T01:30:00.250+01:30""#),
        Some(dr_strange_parser::AsOfSpec::Time(250))
    );
}

#[test]
fn as_of_time_takes_epoch_milliseconds() {
    assert_eq!(
        as_of("MATCH (n) RETURN n AS OF TIME 1782864000000"),
        Some(dr_strange_parser::AsOfSpec::Time(1_782_864_000_000))
    );
    assert_eq!(
        as_of("MATCH (n) RETURN n AS OF TIME -1000"),
        Some(dr_strange_parser::AsOfSpec::Time(-1000))
    );
}

#[test]
fn as_of_is_absent_by_default_and_applies_to_every_source() {
    assert_eq!(as_of("MATCH (n) RETURN n"), None);
    assert_eq!(
        as_of("CALL pagerank() ON (n) RETURN n AS OF 12"),
        Some(dr_strange_parser::AsOfSpec::Seq(12))
    );
    assert_eq!(
        as_of(r#"SEARCH (d:Doc) ON body MATCHING "rust" RETURN d AS OF 12"#),
        Some(dr_strange_parser::AsOfSpec::Seq(12))
    );
}

#[test]
fn as_of_rejects_a_malformed_timestamp() {
    assert!(matches!(
        err(r#"MATCH (n) RETURN n AS OF "yesterday""#),
        ParseError::Syntax(_)
    ));
    assert!(matches!(
        err(r#"MATCH (n) RETURN n AS OF "2026-07-01""#),
        ParseError::Syntax(_)
    ));
    // No zone designator.
    assert!(matches!(
        err(r#"MATCH (n) RETURN n AS OF "2026-07-01T00:00:00""#),
        ParseError::Syntax(_)
    ));
}

// ---- the `ON <property>` default -----------------------------------------

#[test]
fn near_defaults_the_property_to_embedding() {
    // `embedding` is what the digest pipeline writes, so a NEAR clause need
    // not repeat it. Explicit and implicit compile identically.
    assert_eq!(
        plan("SEARCH (d:Doc) NEAR [1.0, 2.0] TOPK 3 RETURN d"),
        plan("SEARCH (d:Doc) ON embedding NEAR [1.0, 2.0] TOPK 3 RETURN d")
    );
    assert_eq!(
        plan("SEARCH (d:Doc) NEAR [1.0] RETURN d").source,
        Source::VectorTopK {
            label: Some("Doc".into()),
            property: "embedding".into(),
            query: vec![1.0],
            metric: Metric::Cosine,
            k: 10,
        }
    );
}

#[test]
fn hybrid_vector_channel_defaults_the_property_too() {
    assert_eq!(
        plan(r#"HYBRID (d:Doc) VECTOR NEAR [1.0] GRAPH HOPS 2 DECAY 0.5 RETURN d"#),
        plan(r#"HYBRID (d:Doc) VECTOR ON embedding NEAR [1.0] GRAPH HOPS 2 DECAY 0.5 RETURN d"#)
    );
}

#[test]
fn beam_defaults_the_property_too() {
    assert_eq!(
        plan("MATCH (a:Doc) BEAM (b) OUT :CITES NEAR [1.0] WIDTH 4 DEPTH 2 RETURN b"),
        plan("MATCH (a:Doc) BEAM (b) OUT :CITES ON embedding NEAR [1.0] WIDTH 4 DEPTH 2 RETURN b")
    );
}

#[test]
fn matching_still_requires_an_explicit_property() {
    // Keyword properties follow no convention (body/text/description), so
    // guessing would silently search nothing.
    let e = err(r#"SEARCH (d:Doc) MATCHING "rust" RETURN d"#);
    assert!(
        matches!(&e, ParseError::Compile(m) if m.contains("ON <property>")),
        "expected a clear compile error, got {e}"
    );
    assert!(matches!(
        err(r#"HYBRID (d:Doc) KEYWORD MATCHING "rust" RETURN d"#),
        ParseError::Syntax(_) | ParseError::Compile(_)
    ));
}

// ---- GRAPH decay default -------------------------------------------------

#[test]
fn hybrid_graph_channel_defaults_decay() {
    // The RPC, MCP and CLI surfaces all default graph decay to 0.5; the
    // language now agrees, so HOPS alone is enough to enable the channel.
    assert_eq!(
        plan(r#"HYBRID (d:Doc) VECTOR NEAR [1.0] GRAPH HOPS 2 RETURN d"#),
        plan(r#"HYBRID (d:Doc) VECTOR NEAR [1.0] GRAPH HOPS 2 DECAY 0.5 RETURN d"#)
    );
    let Source::Hybrid(spec) =
        plan(r#"HYBRID (d:Doc) VECTOR NEAR [1.0] GRAPH HOPS 3 RETURN d"#).source
    else {
        panic!("expected a hybrid source")
    };
    assert_eq!(
        spec.graph,
        Some(GraphChannel {
            hops: 3,
            decay: 0.5,
            seeds: 10,
        })
    );
}

// ---- error positions -----------------------------------------------------
//
// A clause that has started — its leading keyword matched — must report its
// own failure, not unwind to the top and blame the query's first token.

/// The `near \`…\`` fragment of a syntax error.
fn err_at(q: &str) -> String {
    match err(q) {
        ParseError::Syntax(m) => m,
        other => panic!("expected a syntax error for `{q}`, got {other}"),
    }
}

#[test]
fn a_malformed_channel_reports_at_the_channel() {
    // VECTOR without NEAR: the error points at what follows VECTOR.
    let m = err_at(r#"HYBRID (n) VECTOR "model" GRAPH HOPS 2 RETURN n"#);
    assert!(m.contains(r#""model""#), "{m}");
    assert!(
        !m.contains("HYBRID (n) VECTOR"),
        "blamed the whole query: {m}"
    );

    // GRAPH without HOPS.
    let m = err_at("HYBRID (n:Doc) GRAPH DECAY 0.5 RETURN n");
    assert!(m.starts_with("near `DECAY"), "{m}");
}

#[test]
fn a_missing_return_reports_at_the_end() {
    for q in [
        "MATCH (n)",
        "MATCH (n:Doc) WHERE n.year > 2000",
        r#"SEARCH (d:Doc) ON body MATCHING "rust" TOPK 10"#,
    ] {
        assert_eq!(err_at(q), "unexpected end of query", "for `{q}`");
    }
}

#[test]
fn committing_a_read_clause_does_not_break_writes() {
    // `MATCH … SET/REMOVE/DELETE` is tried before the read grammar, so
    // committing to RETURN inside a read must not capture a write.
    for q in [
        "MATCH (n:Doc) WHERE n.x = 1 SET n.y = 2",
        "MATCH (n:Doc) REMOVE n.y",
        "MATCH (n:Doc) DETACH DELETE n",
        "MATCH (n:Doc) CREATE (n)-[:R]->(m:Other {key: \"k\"})",
    ] {
        assert!(
            matches!(parse_statement(q), Ok(Statement::Write(_))),
            "expected a write for `{q}`"
        );
    }
}
