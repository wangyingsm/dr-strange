//! Text → `LogicalPlan` assertions. Expected plans are built with core's own
//! `Source`/`Step`/`Expr` helpers, so these tests double as a spec for how each
//! Cypher construct maps onto the pipeline.

use dr_strange_core::{
    Dir, LogicalPlan, Metric, PropValue, SortKey, Source, Step, distance, has_label, hops, lit, p,
    score, similarity,
};
use dr_strange_parser::{Embedder, ParseError, parse, parse_with_embedder};

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
    parse(q).unwrap_or_else(|e| panic!("parse failed for `{q}`: {e}"))
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
        pl.source,
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
        pl.steps,
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
        pl.source,
        Source::VectorTopK {
            label: Some("Paper".into()),
            property: "embedding".into(),
            query: vec![1.0, 0.0],
            metric: Metric::Cosine,
            k: 5,
        }
    );
    assert_eq!(
        pl.steps,
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
