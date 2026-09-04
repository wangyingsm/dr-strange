//! What a half-typed query may become. The vocabulary below is a small
//! version of a digested code plane — the counts are what make one suggestion
//! better than another, so they are as real as the shapes.
//!
//! Every completed query in here is also *parsed*, which is the point: a
//! suggestion that does not fit the grammar is worse than none, and only the
//! grammar can say.

use dr_strange_parser::{Connection, EdgeInfo, Expect, Kind, LabelInfo, Vocab, complete, parse};

/// A code plane's catalog, in miniature: Rust items and the edges between
/// them, in the proportions a real one has.
fn code() -> Vocab {
    let label = |name: &str, count: u64, props: &[&str]| LabelInfo {
        name: name.into(),
        count,
        properties: props.iter().map(|p| (*p).to_string()).collect(),
    };
    let conn = |src: &str, dst: &str, count: u64| Connection {
        src: src.into(),
        dst: dst.into(),
        count,
    };
    Vocab {
        labels: vec![
            label(
                "Function",
                2909,
                &["file", "line", "signature", "doc_comment", "visibility"],
            ),
            // Everything outside the tree: called, imported, never calling.
            label("External", 1197, &[]),
            label("Method", 994, &["file", "line", "receiver", "signature"]),
            label("Struct", 504, &["file", "line", "fields"]),
            label("Module", 192, &["path", "imports", "doc_comment"]),
        ],
        edges: vec![
            EdgeInfo {
                name: "CALLS".into(),
                count: 17157,
                connections: vec![
                    conn("Function", "Function", 6300),
                    conn("Function", "External", 3105),
                    conn("Function", "Method", 1296),
                    conn("Method", "Function", 921),
                    conn("Method", "Method", 570),
                ],
            },
            EdgeInfo {
                name: "CONTAINS".into(),
                count: 3706,
                connections: vec![
                    conn("Module", "Function", 1839),
                    conn("Module", "Struct", 349),
                    conn("Struct", "Method", 52),
                ],
            },
            EdgeInfo {
                name: "IMPORTS".into(),
                count: 1933,
                connections: vec![
                    conn("Module", "External", 582),
                    conn("Module", "Struct", 519),
                ],
            },
            EdgeInfo {
                name: "HAS_METHOD".into(),
                count: 771,
                connections: vec![conn("Struct", "Method", 529)],
            },
        ],
    }
}

fn best(prefix: &str) -> String {
    complete(prefix, &code())
        .best
        .unwrap_or_else(|| panic!("nothing suggested for `{prefix}`"))
}

fn texts(prefix: &str) -> Vec<String> {
    complete(prefix, &code())
        .suggestions
        .into_iter()
        .map(|s| s.text)
        .collect()
}

#[test]
fn an_empty_query_opens_with_the_keyword_most_queries_open_with() {
    assert_eq!(best(""), "MATCH");
    assert_eq!(
        texts("").first().map(String::as_str),
        Some("MATCH"),
        "a read before a write"
    );
    assert!(texts("").contains(&"CREATE".to_string()));
}

/// The compiler folds case, so a query written in lower case should stay in
/// lower case: `matCH` is nobody's idea of a completion.
#[test]
fn a_keyword_completes_in_the_case_it_is_being_typed() {
    assert_eq!(best("mat"), "ch");
    assert_eq!(best("MAT"), "CH");
    assert_eq!(best("Mat"), "CH");
}

/// The heart of it: after `MATCH `, the best guess is the label this plane
/// holds most of — which no editor could know on its own.
#[test]
fn match_offers_a_whole_node_of_the_commonest_label() {
    assert_eq!(best("MATCH "), "(n:Function)");
    assert_eq!(best("match "), "(n:Function)");
    assert_eq!(
        texts("MATCH "),
        [
            "(n:Function)",
            "(n:External)",
            "(n:Method)",
            "(n:Struct)",
            "(n:Module)"
        ],
        "most nodes first"
    );
    let c = complete("MATCH ", &code());
    assert_eq!(c.suggestions[0].detail.as_deref(), Some("2909 nodes"));
}

/// And after a node, the edge that most often leaves *that* label, landing
/// where such edges most often land.
#[test]
fn a_closed_node_offers_the_hop_that_most_often_leaves_it() {
    assert_eq!(best("MATCH (n:Function) "), "-[:CALLS]->(m:Function)");
    let c = complete("MATCH (n:Function) ", &code());
    assert_eq!(
        c.suggestions[0].detail.as_deref(),
        Some("10701 → Function"),
        "how many leave a Function, and where most of them land"
    );
    // A Module calls nothing; it contains.
    assert_eq!(best("MATCH (n:Module) "), "-[:CONTAINS]->(m:Function)");
    // A Struct's commonest edge is the one it has more of than CONTAINS.
    assert_eq!(best("MATCH (n:Struct) "), "-[:HAS_METHOD]->(m:Method)");
}

/// An unlabelled node knows nothing about direction, so the ranking falls
/// back to how many edges of each type the plane holds at all.
#[test]
fn an_unlabelled_node_ranks_by_the_whole_planes_edges() {
    assert_eq!(best("MATCH (n) "), "-[:CALLS]->(m)");
    assert_eq!(
        texts("MATCH (n) ")[..4],
        [
            "-[:CALLS]->(m)",
            "-[:CONTAINS]->(m)",
            "-[:IMPORTS]->(m)",
            "-[:HAS_METHOD]->(m)"
        ]
    );
}

#[test]
fn an_edge_type_completes_into_the_node_it_lands_on() {
    assert_eq!(best("MATCH (n:Function)-[:"), "CALLS]->(m:Function)");
    assert_eq!(best("MATCH (n:Function)-[:CA"), "LLS]->(m:Function)");
    assert_eq!(best("MATCH (n:Function)-[:ca"), "LLS]->(m:Function)");
    assert_eq!(
        texts("MATCH (n:Struct)-[:"),
        ["HAS_METHOD", "CONTAINS"],
        "only the types a Struct actually has"
    );
}

/// `<-` reverses which end of a connection the label sits at, and which
/// bracket closes it.
#[test]
fn an_incoming_hop_ranks_by_what_arrives_and_closes_the_other_way() {
    assert_eq!(best("MATCH (n:Function)<-[:"), "CALLS]-(m:Function)");
    assert_eq!(
        best("MATCH (n:Method)<-[:"),
        "CALLS]-(m:Function)",
        "more Functions call a Method than Methods do"
    );
    assert_eq!(best("MATCH (n:Method)<-[:HAS"), "_METHOD]-(m:Struct)");
}

/// A bare `*` is the one variable-length shape the compiler refuses, so what
/// follows one is a bound rather than the bracket that would close it.
#[test]
fn a_variable_length_hop_is_given_bounds_it_can_walk() {
    assert_eq!(best("MATCH (n:Function)-[:CALLS*"), "1..3]->");
    assert_eq!(best("MATCH (n:Function)-[:CALLS*1..3"), "]->");
    assert_eq!(best("MATCH (n:Function)<-[:CALLS*"), "1..3]-");
}

/// A relationship's brackets take no spaces, so once the caret has left one
/// there is nothing honest to add: `-[:CALLS ]->` is not a relationship.
#[test]
fn a_caret_that_left_the_brackets_is_offered_nothing() {
    let c = complete("MATCH (n:Function)-[:CALLS ", &code());
    assert_eq!(
        c.expects,
        Expect::RelEnd {
            incoming: false,
            ranged: false,
        }
    );
    assert!(c.best.is_none());
    assert!(
        complete("MATCH (n:Function)-[:CALLS] ", &code())
            .best
            .is_none()
    );
}

#[test]
fn a_label_is_completed_from_the_plane_and_closes_its_node() {
    assert_eq!(best("MATCH (n:Fun"), "ction)");
    assert_eq!(best("MATCH (n:M"), "ethod)");
    assert_eq!(
        texts("MATCH (n:M"),
        ["Method", "Module"],
        "most nodes first"
    );
    assert_eq!(best("MATCH (n:"), "Function)");
}

/// A node whose name is written but not its label: the label narrows it, or
/// the bracket ends it.
#[test]
fn a_named_node_offers_its_label_or_its_close() {
    assert_eq!(best("MATCH (n "), ":Function)");
    assert_eq!(
        texts("MATCH (n "),
        [
            ":Function)",
            ":External)",
            ":Method)",
            ":Struct)",
            ":Module)",
            ")"
        ],
        "every label, then the node that names none"
    );
}

#[test]
fn a_property_is_completed_from_its_variables_label() {
    assert_eq!(
        texts("MATCH (n:Module) WHERE n."),
        ["path", "imports", "doc_comment"],
        "a Module's properties, and no other label's"
    );
    assert_eq!(best("MATCH (n:Module) WHERE n.pa"), "th");
    let c = complete("MATCH (n:Function) WHERE n.", &code());
    assert_eq!(c.suggestions[0].text, "file");
    assert_eq!(c.suggestions[0].detail.as_deref(), Some("of Function"));
    assert_eq!(c.suggestions[0].kind, Kind::Property);
}

/// An unlabelled node could be anything, so every label's properties are fair
/// game — deduplicated, since most labels carry a `file`.
#[test]
fn an_unlabelled_variable_offers_every_labels_properties_once() {
    let props = texts("MATCH (n) WHERE n.");
    assert_eq!(props.iter().filter(|p| *p == "file").count(), 1);
    assert!(props.contains(&"receiver".to_string()));
    assert!(props.contains(&"path".to_string()));
}

/// A pattern's rows *are* its terminal node: returning an earlier variable's
/// rows is the one thing the compiler refuses, so the last one bound is the
/// one to offer.
#[test]
fn return_offers_the_terminal_variable_first() {
    assert_eq!(best("MATCH (n:Function) RETURN "), "n");
    assert_eq!(
        texts("MATCH (n:Function)-[:CALLS]->(m:Function) RETURN ")[..2],
        ["m", "n"]
    );
    assert!(texts("MATCH (n:Function) RETURN ").contains(&"count(*)".to_string()));
}

#[test]
fn a_predicate_offers_the_variables_in_the_order_they_were_bound() {
    assert_eq!(
        texts("MATCH (n:Function)-[:CALLS]->(m:Function) WHERE ")[..2],
        ["n", "m"]
    );
}

/// Words typed inside a string are data. Completing them would be noise, and
/// the language has no escapes, so the scan is exact.
#[test]
fn a_string_literal_is_data_not_syntax() {
    let c = complete(r#"MATCH (n:Function) WHERE n.file = "exec"#, &code());
    assert_eq!(c.expects, Expect::Nothing);
    assert!(c.best.is_none());
    // Closed again, the query goes on as before.
    let c = complete(r#"MATCH (n:Function) WHERE n.file = "exec.rs" "#, &code());
    assert_eq!(c.expects, Expect::Clause { done: vec![] });
}

/// The right-hand side of a comparison is the author's alone: no count in any
/// catalog says what they meant to look for.
#[test]
fn nothing_is_offered_for_a_value() {
    let c = complete("MATCH (n:Function) WHERE n.file = ", &code());
    assert_eq!(c.expects, Expect::Value);
    assert!(c.best.is_none());
}

/// So is a variable's name.
#[test]
fn a_variable_being_named_is_the_authors_own() {
    let c = complete("MATCH (fn", &code());
    assert!(c.best.is_none(), "{:?}", c.suggestions);
    // Until they have started one, though, a name is worth offering.
    assert_eq!(best("MATCH ("), "n");
}

#[test]
fn a_second_node_binds_a_name_the_first_did_not() {
    assert_eq!(best("MATCH (n:Function) "), "-[:CALLS]->(m:Function)");
    assert_eq!(
        best("MATCH (n:Function)-[:CALLS]->(m:Function)-[:"),
        "CALLS]->(o:Function)",
        "a third node is `o`, not another `m`"
    );
}

/// A plane nobody has digested still gets the language — and `(n)`, which
/// names no label and so cannot be wrong.
#[test]
fn an_empty_plane_still_completes_the_language() {
    let empty = Vocab::default();
    assert_eq!(complete("", &empty).best.as_deref(), Some("MATCH"));
    assert_eq!(complete("MATCH ", &empty).best.as_deref(), Some("(n)"));
    assert_eq!(
        complete("MATCH (n) ", &empty).best.as_deref(),
        Some("RETURN"),
        "no edges to offer, so the clause that ends the pattern"
    );
    assert!(complete("MATCH (n:", &empty).best.is_none());
}

/// Past one hop, the clause that ends the pattern comes first: a two-hop
/// pattern is already a question, and a third hop is rarely one.
#[test]
fn a_pattern_that_is_already_a_question_offers_its_clause_first() {
    assert_eq!(best("MATCH (n:Function)-[:CALLS]->(m:Function) "), "RETURN");
    assert!(
        texts("MATCH (n:Function)-[:CALLS]->(m:Function) ")
            .contains(&"-[:CALLS]->(o:Function)".to_string()),
        "the hop is still there, just not first"
    );
}

#[test]
fn what_the_caret_sits_at_is_said_plainly() {
    let at = |prefix: &str| complete(prefix, &code()).expects;
    assert_eq!(at(""), Expect::Statement);
    assert_eq!(at("MATCH "), Expect::Node { var: "n".into() });
    assert_eq!(at("MATCH ("), Expect::NodeVar { var: "n".into() });
    // The caret is still inside the name, which is the author's to finish.
    assert_eq!(at("MATCH (n"), Expect::NodeVar { var: "n".into() });
    assert_eq!(at("MATCH (n "), Expect::NodeEnd);
    assert_eq!(at("MATCH (n:"), Expect::Label);
    assert_eq!(
        at("MATCH (n:Function) "),
        Expect::Hop {
            from: Some("Function".into()),
            var: "m".into(),
            bound: 1,
        }
    );
    assert_eq!(
        at("MATCH (n:Function)-[:"),
        Expect::EdgeType {
            from: Some("Function".into()),
            var: "m".into(),
            incoming: false,
            lead: "",
        }
    );
    assert_eq!(
        at("MATCH (n:Function)<-"),
        Expect::EdgeType {
            from: Some("Function".into()),
            var: "m".into(),
            incoming: true,
            lead: "[:",
        }
    );
    // Mid-word, the caret is still choosing a type; a `*` ends the word and
    // opens the variable-length range.
    assert_eq!(
        at("MATCH (n:Function)-[:CALLS*"),
        Expect::RelEnd {
            incoming: false,
            ranged: true,
        }
    );
    assert_eq!(
        at("MATCH (n:Function)-[:CALLS]"),
        Expect::Arrow { incoming: false }
    );
    assert_eq!(
        at("MATCH (n:Function) WHERE n."),
        Expect::Property {
            var: "n".into(),
            label: Some("Function".into()),
        }
    );
    assert_eq!(
        at("MATCH (n:Function) RETURN "),
        Expect::Projection {
            vars: vec!["n".into()]
        }
    );
    assert_eq!(
        at("MATCH (n:Function) RETURN n "),
        Expect::Clause { done: vec![] }
    );
}

/// A plane with three hundred labels has no useful three-hundredth
/// suggestion.
#[test]
fn the_list_is_capped_at_a_readable_length() {
    let many = Vocab {
        labels: (0..40)
            .map(|i| LabelInfo {
                name: format!("L{i}"),
                count: 100 - i,
                properties: vec![],
            })
            .collect(),
        edges: vec![],
    };
    assert_eq!(complete("MATCH ", &many).suggestions.len(), 12);
}

/// The property that matters: what completion writes, the grammar reads.
/// Accepting the best guess over and over — exactly as an editor does, a
/// space after each — walks from an empty box to a query that parses.
#[test]
fn accepting_the_best_guess_writes_a_query_that_parses() {
    let vocab = code();
    let mut text = String::new();
    let mut steps = 0;
    while parse(text.trim()).is_err() {
        let Some(insert) = complete(&text, &vocab).best else {
            panic!("stuck at `{text}`");
        };
        text.push_str(&insert);
        text.push(' ');
        steps += 1;
        assert!(
            steps < 12,
            "still not a query after {steps} steps: `{text}`"
        );
    }
    assert_eq!(
        text.trim(),
        "MATCH (n:Function) -[:CALLS]->(m:Function) RETURN m"
    );
}

/// Every shape a position offers has to parse once the query is finished off,
/// not just the first one.
#[test]
fn every_shape_offered_fits_the_grammar() {
    let vocab = code();
    for prefix in [
        "MATCH ",
        "MATCH (n:Function) ",
        "MATCH (n) ",
        "MATCH (n:Struct) ",
    ] {
        for s in complete(prefix, &vocab).suggestions {
            if s.kind != Kind::Snippet {
                continue;
            }
            let query = format!("{prefix}{} RETURN *", s.insert);
            assert!(
                parse(&query).is_ok(),
                "`{query}` does not parse: {:?}",
                parse(&query).err()
            );
        }
    }
}

/// And the same for a half-typed label or edge type, where the suggestion is
/// spliced into the middle of a word.
#[test]
fn a_spliced_completion_fits_the_grammar() {
    let vocab = code();
    for (prefix, rest) in [
        ("MATCH (n:Fun", " RETURN *"),
        ("MATCH (n:Function)-[:CA", " RETURN *"),
        ("MATCH (n:Function)<-[:CA", " RETURN *"),
        ("MATCH (n:Struct)-[:HAS", " RETURN *"),
        ("MATCH (n:Function)-[:CALLS*", "(m) RETURN *"),
        ("MATCH (n:Function)-[:CALLS]", "(m) RETURN *"),
        ("MATCH (n ", " RETURN *"),
    ] {
        let insert = complete(prefix, &vocab).best.expect(prefix);
        let query = format!("{prefix}{insert}{rest}");
        assert!(
            parse(&query).is_ok(),
            "`{query}` does not parse: {:?}",
            parse(&query).err()
        );
    }
}

/// A hop is completed from however much of it is written, punctuation and
/// all: the bracket has to be opened when the arrow is all there is, and the
/// colon when the bracket is.
#[test]
fn a_hop_is_completed_from_however_much_of_it_is_written() {
    assert_eq!(best("MATCH (n:Function)-"), "[:CALLS]->(m:Function)");
    assert_eq!(best("MATCH (n:Function)<-"), "[:CALLS]-(m:Function)");
    // `<` on its own is half an arrow: the hyphen is owed too.
    assert_eq!(best("MATCH (n:Function)<"), "-[:CALLS]-(m:Function)");
    assert_eq!(best("MATCH (n:Function)-["), ":CALLS]->(m:Function)");
    assert_eq!(best("MATCH (n:Function)<-["), ":CALLS]-(m:Function)");
    // `-[r` names the relationship; the type follows that name rather than
    // completing it.
    assert_eq!(best("MATCH (n:Function)-[r"), ":CALLS]->(m:Function)");
    assert_eq!(best("MATCH (n:Function)-[:"), "CALLS]->(m:Function)");

    // And each one parses, spliced in exactly where it was offered.
    let vocab = code();
    for prefix in [
        "MATCH (n:Function)-",
        "MATCH (n:Function)<",
        "MATCH (n:Function)<-",
        "MATCH (n:Function)-[",
        "MATCH (n:Function)<-[",
        "MATCH (n:Function)-[r",
        "MATCH (n:Function)-[:",
    ] {
        let insert = complete(prefix, &vocab).best.expect(prefix);
        let query = format!("{prefix}{insert} RETURN *");
        assert!(
            parse(&query).is_ok(),
            "`{query}`: {:?}",
            parse(&query).err()
        );
    }
}

/// A word where no bracket has been opened is not the start of a type: there
/// is nowhere for it to go.
#[test]
fn a_word_with_no_bracket_to_go_in_completes_to_nothing() {
    assert!(complete("MATCH (n:Function)<-C", &code()).best.is_none());
    assert!(complete("MATCH (n:Function)<C", &code()).best.is_none());
}

/// Plenty of labels are only ever arrived at: an `External` is called and
/// calls nothing. Offering such a node no hop at all would offer it nothing
/// it wants, so both directions are ranked together.
#[test]
fn a_node_that_is_only_ever_arrived_at_is_offered_the_way_in() {
    assert_eq!(best("MATCH (n:External) "), "<-[:CALLS]-(m:Function)");
    let query = format!(
        "MATCH (n:External) {} RETURN *",
        best("MATCH (n:External) ")
    );
    assert!(
        parse(&query).is_ok(),
        "`{query}`: {:?}",
        parse(&query).err()
    );

    // A Function is both, and the commoner direction leads.
    assert_eq!(
        texts("MATCH (n:Function) ")[..2],
        ["-[:CALLS]->(m:Function)", "<-[:CALLS]-(m:Function)"]
    );

    // An unlabelled node has no direction to tell apart, so it is not shown
    // each type twice.
    let out = texts("MATCH (n) ");
    assert_eq!(
        out.iter().filter(|t| t.contains("CALLS")).count(),
        1,
        "{out:?}"
    );
}

/// The property both reported bugs broke: a query cut short anywhere is a
/// query the completion can finish.
///
/// Each was a truncation — `<` and `<-` of a `<-[:CALLS]-` — offered a
/// completion that assumed punctuation nobody had typed. So: cut a known-good
/// query at every character, accept the best guess over and over as an editor
/// does, and require what comes out to parse.
///
/// A truncation that offers *nothing* is skipped rather than failed: the two
/// places that happens are inside a variable's name, which is the author's to
/// finish and no vocabulary can guess.
#[test]
fn any_truncation_of_a_query_can_be_finished_from_where_it_was_cut() {
    let vocab = code();
    for whole in [
        "MATCH (n:Function)-[:CALLS]->(m:Function) RETURN m",
        "MATCH (n:Struct)<-[:CONTAINS]-(m:Module) RETURN m",
        "MATCH (n:Module)-[:IMPORTS]->(m:Struct) RETURN m",
    ] {
        for cut in 0..=whole.len() {
            let start = &whole[..cut];
            if complete(start, &vocab).best.is_none() {
                continue;
            }
            let mut text = start.to_string();
            let mut steps = 0;
            while parse(text.trim()).is_err() {
                let Some(insert) = complete(&text, &vocab).best else {
                    break;
                };
                text.push_str(&insert);
                text.push(' ');
                steps += 1;
                assert!(steps < 10, "from `{start}`: no end in sight at `{text}`");
            }
            assert!(
                parse(text.trim()).is_ok(),
                "from `{start}`: `{text}` does not parse — {:?}",
                parse(text.trim()).err()
            );
        }
    }
}

/// A tail clause comes once — a query has one `LIMIT` — and what `ORDER BY`
/// sorts on is a key, not another `ORDER BY`.
#[test]
fn a_finished_query_is_not_offered_the_same_clause_again() {
    assert_eq!(best("MATCH (n:Function) RETURN n "), "ORDER BY");
    assert_eq!(
        best("MATCH (n:Function) RETURN n ORDER BY "),
        "n",
        "a key to sort on"
    );
    let after = texts("MATCH (n:Function) RETURN n ORDER BY n ");
    assert!(!after.contains(&"ORDER BY".to_string()), "{after:?}");
    assert_eq!(after.first().map(String::as_str), Some("LIMIT"));
    let after = texts("MATCH (n:Function) RETURN n ORDER BY n LIMIT 10 ");
    assert!(!after.contains(&"LIMIT".to_string()), "{after:?}");
}
