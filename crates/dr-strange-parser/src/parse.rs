//! The `nom` grammar: text → [`Query`] AST. Whitespace is skipped before every
//! token (each `symbol`/`kw`/`ident`/number leads with `multispace0`), so the
//! grammar rules read without threading whitespace explicitly.

use nom::IResult;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take_while};
use nom::character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, one_of};
use nom::combinator::{cut, map, map_res, not, opt, recognize, value};
use nom::multi::{many0, many1, separated_list0, separated_list1};
use nom::sequence::{delimited, pair, preceded, tuple};

use dr_strange_core::Metric;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp, StrOp};
use dr_strange_core::types::Dir;

use crate::ast::*;

// ---- token helpers --------------------------------------------------------

/// A literal symbol, tolerant of leading whitespace: `symbol("(")`.
fn symbol<'a>(s: &'static str) -> impl Fn(&'a str) -> IResult<&'a str, &'a str> {
    move |i: &'a str| preceded(multispace0, tag(s))(i)
}

/// A case-insensitive keyword with a word boundary, so `RETURN` doesn't match
/// the start of an identifier like `returned`.
fn kw<'a>(word: &'static str) -> impl Fn(&'a str) -> IResult<&'a str, ()> {
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, _) = tag_no_case(word)(i)?;
        let (i, _) = not(alt((alphanumeric1, tag("_"))))(i)?;
        Ok((i, ()))
    }
}

/// An identifier (variable / label / type / property key).
fn ident(i: &str) -> IResult<&str, String> {
    let (i, _) = multispace0(i)?;
    let (i, s) = recognize(pair(
        alt((alpha1, tag("_"))),
        many0(alt((alphanumeric1, tag("_")))),
    ))(i)?;
    Ok((i, s.to_string()))
}

fn uint(i: &str) -> IResult<&str, u64> {
    map_res(preceded(multispace0, digit1), str::parse::<u64>)(i)
}

// Operator lexers as concrete-typed fns so nom can infer the error type when
// they're called inline (an `impl Fn` return, or an untyped `let`, leaves the
// `ParseError` impl ambiguous).
fn add_op(i: &str) -> IResult<&str, char> {
    preceded(multispace0, one_of("+-"))(i)
}

fn mul_op(i: &str) -> IResult<&str, char> {
    preceded(multispace0, one_of("*/"))(i)
}

fn cmp_op(i: &str) -> IResult<&str, CmpOp> {
    // Longest match first (`<=` before `<`), each mapped straight to its op so
    // the caller needs no re-match.
    preceded(
        multispace0,
        alt((
            value(CmpOp::Le, tag("<=")),
            value(CmpOp::Ge, tag(">=")),
            value(CmpOp::Ne, tag("<>")),
            value(CmpOp::Ne, tag("!=")),
            value(CmpOp::Eq, tag("=")),
            value(CmpOp::Lt, tag("<")),
            value(CmpOp::Gt, tag(">")),
        )),
    )(i)
}

/// `CONTAINS` / `STARTS WITH` / `ENDS WITH`. Two-word forms are a keyword
/// pair, so `STARTS  WITH` and `starts with` both lex; `kw` supplies the word
/// boundary, so a property named `contains_x` is not mistaken for the operator.
fn str_op(i: &str) -> IResult<&str, StrOp> {
    alt((
        value(StrOp::Contains, kw("contains")),
        value(StrOp::StartsWith, pair(kw("starts"), kw("with"))),
        value(StrOp::EndsWith, pair(kw("ends"), kw("with"))),
    ))(i)
}

// ---- literals -------------------------------------------------------------

fn number(i: &str) -> IResult<&str, PropValue> {
    let (i, _) = multispace0(i)?;
    let float = map_res(recognize(tuple((digit1, char('.'), digit1))), |s: &str| {
        s.parse::<f64>().map(PropValue::Float)
    });
    let int = map_res(digit1, |s: &str| s.parse::<i64>().map(PropValue::Int));
    alt((float, int))(i)
}

fn quoted<'a>(q: char) -> impl Fn(&'a str) -> IResult<&'a str, PropValue> {
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, s) = delimited(char(q), take_while(|c| c != q), char(q))(i)?;
        Ok((i, PropValue::Str(s.to_string())))
    }
}

fn literal(i: &str) -> IResult<&str, PExpr> {
    alt((
        map(number, PExpr::Lit),
        map(quoted('\''), PExpr::Lit),
        map(quoted('"'), PExpr::Lit),
        map(kw("true"), |_| PExpr::Lit(PropValue::Bool(true))),
        map(kw("false"), |_| PExpr::Lit(PropValue::Bool(false))),
        map(kw("null"), |_| PExpr::Lit(PropValue::Null)),
    ))(i)
}

// ---- expression grammar (precedence climbing) -----------------------------
//
// or < and < not < comparison < additive < multiplicative < unary < primary

fn expr(i: &str) -> IResult<&str, PExpr> {
    or_expr(i)
}

fn or_expr(i: &str) -> IResult<&str, PExpr> {
    let (mut i, mut lhs) = and_expr(i)?;
    while let Ok((rest, _)) = kw("or")(i) {
        let (rest, rhs) = and_expr(rest)?;
        lhs = PExpr::Logic {
            op: LogicOp::Or,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
        i = rest;
    }
    Ok((i, lhs))
}

fn and_expr(i: &str) -> IResult<&str, PExpr> {
    let (mut i, mut lhs) = not_expr(i)?;
    while let Ok((rest, _)) = kw("and")(i) {
        let (rest, rhs) = not_expr(rest)?;
        lhs = PExpr::Logic {
            op: LogicOp::And,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
        i = rest;
    }
    Ok((i, lhs))
}

fn not_expr(i: &str) -> IResult<&str, PExpr> {
    if let Ok((rest, _)) = kw("not")(i) {
        let (rest, e) = not_expr(rest)?;
        return Ok((rest, PExpr::Not(Box::new(e))));
    }
    comparison(i)
}

fn comparison(i: &str) -> IResult<&str, PExpr> {
    let (i, lhs) = additive(i)?;

    // `IS NULL` / `IS NOT NULL`
    if let Ok((rest, _)) = kw("is")(i) {
        let (rest, negated) = opt(kw("not"))(rest)?;
        let (rest, _) = kw("null")(rest)?;
        let is_null = PExpr::IsNull(Box::new(lhs));
        let out = if negated.is_some() {
            PExpr::Not(Box::new(is_null))
        } else {
            is_null
        };
        return Ok((rest, out));
    }

    if let Ok((rest, _)) = kw("in")(i) {
        // `x IN [a, b, …]` — sugar the compiler expands into equalities (and,
        // on `key(n)` at the source, into a multi-key seek).
        if let Ok((rest, _)) = symbol("[")(rest) {
            let (rest, list) = separated_list0(symbol(","), expr)(rest)?;
            let (rest, _) = symbol("]")(rest)?;
            return Ok((
                rest,
                PExpr::In {
                    lhs: Box::new(lhs),
                    list,
                },
            ));
        }
        // `x IN <expr>` — membership in a value the row supplies (a `List`
        // property, or a `Map`'s keys). Not expandable into equalities: the
        // haystack isn't known until the row is.
        let (rest, haystack) = additive(rest)?;
        return Ok((
            rest,
            PExpr::InValue {
                lhs: Box::new(lhs),
                haystack: Box::new(haystack),
            },
        ));
    }

    // `a CONTAINS b` / `STARTS WITH` / `ENDS WITH`, at comparison precedence
    // like openCypher.
    if let Ok((rest, op)) = str_op(i) {
        let (rest, rhs) = additive(rest)?;
        return Ok((
            rest,
            PExpr::StringMatch {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        ));
    }

    // an optional binary comparison
    match cmp_op(i) {
        Ok((rest, op)) => {
            let (rest, rhs) = additive(rest)?;
            Ok((
                rest,
                PExpr::Compare {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            ))
        }
        Err(_) => Ok((i, lhs)),
    }
}

fn additive(i: &str) -> IResult<&str, PExpr> {
    let (mut i, mut lhs) = multiplicative(i)?;
    while let Ok((rest, op)) = add_op(i) {
        let (rest, rhs) = multiplicative(rest)?;
        let op = if op == '+' {
            ArithOp::Add
        } else {
            ArithOp::Sub
        };
        lhs = PExpr::Arith {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
        i = rest;
    }
    Ok((i, lhs))
}

fn multiplicative(i: &str) -> IResult<&str, PExpr> {
    let (mut i, mut lhs) = unary(i)?;
    while let Ok((rest, op)) = mul_op(i) {
        let (rest, rhs) = unary(rest)?;
        let op = if op == '*' {
            ArithOp::Mul
        } else {
            ArithOp::Div
        };
        lhs = PExpr::Arith {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        };
        i = rest;
    }
    Ok((i, lhs))
}

fn unary(i: &str) -> IResult<&str, PExpr> {
    if let Ok((rest, _)) = symbol("-")(i) {
        let (rest, e) = unary(rest)?;
        return Ok((rest, PExpr::Neg(Box::new(e))));
    }
    primary(i)
}

fn primary(i: &str) -> IResult<&str, PExpr> {
    alt((
        paren_expr,
        map(param_name, PExpr::Param),
        literal,
        func_or_var,
    ))(i)
}

fn paren_expr(i: &str) -> IResult<&str, PExpr> {
    let (i, _) = symbol("(")(i)?;
    let (i, e) = expr(i)?;
    let (i, _) = symbol(")")(i)?;
    Ok((i, e))
}

/// An identifier at expression head: either a function call `name(...)` (a
/// scoring term), a property `v.key`, or a label predicate `v:Label`. A bare
/// variable is not a valid term in this cut.
fn func_or_var(i: &str) -> IResult<&str, PExpr> {
    let (after_name, name) = ident(i)?;
    let (after_ws, _) = multispace0(after_name)?;
    if after_ws.starts_with('(') {
        // Committed to a function call once we see `(` — a bad one is a hard
        // error, not a fall-through to the property/label form.
        return func_call(&name, after_ws);
    }
    let (i, sep) = one_of(".:")(after_ws)?;
    let (i, member) = ident(i)?;
    let out = if sep == '.' {
        PExpr::Prop {
            var: name,
            key: member,
        }
    } else {
        PExpr::HasLabel {
            var: name,
            label: member,
        }
    };
    Ok((i, out))
}

/// The recognized scoring functions: `score()`, `hops()`,
/// `similarity(v.prop, <vector>[, metric])`, `distance(v.prop, <vector>[, metric])`.
fn func_call<'a>(name: &str, i: &'a str) -> IResult<&'a str, PExpr> {
    let (i, _) = symbol("(")(i)?;
    match name {
        "score" => {
            let (i, _) = symbol(")")(i)?;
            Ok((i, PExpr::Score))
        }
        "hops" => {
            let (i, _) = symbol(")")(i)?;
            Ok((i, PExpr::Hops))
        }
        // `key(v)` — the node's external key.
        "key" => {
            let (i, var) = ident(i)?;
            let (i, _) = symbol(")")(i)?;
            Ok((i, PExpr::ExternalKey { var }))
        }
        "similarity" | "distance" => {
            let (i, (var, key)) = prop_ref(i)?;
            let (i, _) = symbol(",")(i)?;
            let (i, query) = vec_arg(i)?;
            let (i, metric) = opt(preceded(symbol(","), metric_ident))(i)?;
            let (i, _) = symbol(")")(i)?;
            let metric = metric.unwrap_or(Metric::Cosine);
            let out = if name == "similarity" {
                PExpr::Similarity {
                    var,
                    property: key,
                    query,
                    metric,
                }
            } else {
                PExpr::Distance {
                    var,
                    property: key,
                    query,
                    metric,
                }
            };
            Ok((i, out))
        }
        // Unknown function → a syntax error at this position.
        _ => Err(nom::Err::Error(nom::error::Error::new(
            i,
            nom::error::ErrorKind::Tag,
        ))),
    }
}

/// A property reference `v.key`.
fn prop_ref(i: &str) -> IResult<&str, (String, String)> {
    let (i, var) = ident(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char('.')(i)?;
    let (i, key) = ident(i)?;
    Ok((i, (var, key)))
}

/// A vector literal `[f, f, …]` (empty allowed). Ints and floats both coerce
/// to f32; a leading `-` negates.
fn vector_literal(i: &str) -> IResult<&str, Vec<f32>> {
    let (i, _) = symbol("[")(i)?;
    let (i, xs) = separated_list0(symbol(","), f32_num)(i)?;
    let (i, _) = symbol("]")(i)?;
    Ok((i, xs))
}

/// A query-vector argument: `"text"` (embedded server-side) or `[..]` (literal).
fn vec_arg(i: &str) -> IResult<&str, VecArg> {
    alt((
        map(quoted_str('\''), VecArg::Text),
        map(quoted_str('"'), VecArg::Text),
        map(vector_literal, VecArg::Vector),
    ))(i)
}

/// A quoted string's contents (no escapes in this cut).
fn quoted_str<'a>(q: char) -> impl Fn(&'a str) -> IResult<&'a str, String> {
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, s) = delimited(char(q), take_while(|c| c != q), char(q))(i)?;
        Ok((i, s.to_string()))
    }
}

fn f32_num(i: &str) -> IResult<&str, f32> {
    let (i, neg) = opt(symbol("-"))(i)?;
    let (i, v) = number(i)?;
    let mut f = match v {
        PropValue::Float(x) => x as f32,
        PropValue::Int(n) => n as f32,
        _ => 0.0, // `number` only ever yields Int/Float
    };
    if neg.is_some() {
        f = -f;
    }
    Ok((i, f))
}

/// A metric name: `cosine` (default), `dot`, or `l2` (case-insensitive).
fn metric_ident(i: &str) -> IResult<&str, Metric> {
    let (rest, name) = ident(i)?;
    let m = match name.to_ascii_lowercase().as_str() {
        "cosine" => Metric::Cosine,
        "dot" => Metric::Dot,
        "l2" => Metric::L2,
        _ => {
            return Err(nom::Err::Error(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Tag,
            )));
        }
    };
    Ok((rest, m))
}

// ---- pattern grammar ------------------------------------------------------

fn node_pat(i: &str) -> IResult<&str, NodePat> {
    let (i, _) = symbol("(")(i)?;
    let (i, var) = opt(ident)(i)?;
    let (i, label) = opt(preceded(preceded(multispace0, char(':')), ident))(i)?;
    let (i, _) = symbol(")")(i)?;
    Ok((i, NodePat { var, label }))
}

fn var_range(i: &str) -> IResult<&str, VarLen> {
    let (i, _) = preceded(multispace0, char('*'))(i)?;
    let (i, lo) = opt(map_res(preceded(multispace0, digit1), str::parse::<u32>))(i)?;
    let (i, dots) = opt(preceded(multispace0, tag("..")))(i)?;
    if dots.is_some() {
        let (i, hi) = opt(map_res(preceded(multispace0, digit1), str::parse::<u32>))(i)?;
        Ok((
            i,
            VarLen {
                min: lo.unwrap_or(1),
                max: hi, // None ⇒ unbounded; the compiler rejects it
            },
        ))
    } else {
        match lo {
            Some(n) => Ok((
                i,
                VarLen {
                    min: n,
                    max: Some(n),
                },
            )),
            None => Ok((i, VarLen { min: 1, max: None })), // bare `*` ⇒ unbounded
        }
    }
}

/// The bracketed body of a relationship: `[relvar? (:Type)? (*range)?]`. The
/// relationship variable is accepted but ignored (this cut can't bind it).
fn rel_body(i: &str) -> IResult<&str, (Option<String>, Option<VarLen>)> {
    let (i, _relvar) = opt(ident)(i)?;
    let (i, ty) = opt(preceded(preceded(multispace0, char(':')), ident))(i)?;
    let (i, var_len) = opt(var_range)(i)?;
    Ok((i, (ty, var_len)))
}

fn rel_pat(i: &str) -> IResult<&str, RelPat> {
    let (i, _) = multispace0(i)?;
    let (i, left) = opt(char('<'))(i)?;
    let (i, _) = char('-')(i)?;
    let (i, body) = opt(delimited(char('['), rel_body, char(']')))(i)?;
    let (i, _) = char('-')(i)?;
    let (i, right) = opt(char('>'))(i)?;

    let dir = match (left.is_some(), right.is_some()) {
        (false, true) => Dir::Out,
        (true, false) => Dir::In,
        (false, false) => Dir::Both,
        // `<-...->` is meaningless; fail so the caller reports a syntax error.
        (true, true) => {
            return Err(nom::Err::Error(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Verify,
            )));
        }
    };
    let (ty, var_len) = body.unwrap_or((None, None));
    Ok((i, RelPat { dir, ty, var_len }))
}

fn pattern(i: &str) -> IResult<&str, Pattern> {
    let (i, first) = node_pat(i)?;
    let (i, rest) = many0(pair(rel_pat, node_pat))(i)?;
    Ok((i, Pattern { first, rest }))
}

// ---- clauses --------------------------------------------------------------

fn return_item(i: &str) -> IResult<&str, ReturnItem> {
    alt((
        map(symbol("*"), |_| ReturnItem::Star),
        map(ident, ReturnItem::Var),
    ))(i)
}

fn order_key(i: &str) -> IResult<&str, OrderKey> {
    let (i, e) = expr(i)?;
    let (i, dir) = opt(alt((map(kw("desc"), |_| true), map(kw("asc"), |_| false))))(i)?;
    Ok((
        i,
        OrderKey {
            expr: e,
            descending: dir.unwrap_or(false),
        },
    ))
}

/// The clauses shared by every query, after its source: an optional WHERE, a
/// RETURN, then optional ORDER BY / SKIP / LIMIT / AS OF.
type Tail = (
    Option<PExpr>,
    Return,
    Vec<OrderKey>,
    Option<u64>,
    Option<u64>,
    Option<AsOfSpec>,
);

fn query_tail(i: &str) -> IResult<&str, Tail> {
    let (i, where_clause) = opt(preceded(kw("where"), expr))(i)?;
    // Every read ends in RETURN, so a miss here is the query's actual fault —
    // committing reports it at this position rather than unwinding to the top
    // and blaming the first token.
    let (i, _) = cut(kw("return"))(i)?;
    let (i, distinct) = opt(kw("distinct"))(i)?;
    let (i, item) = return_item(i)?;
    let (i, order_by) = opt(preceded(
        pair(kw("order"), kw("by")),
        separated_list1(symbol(","), order_key),
    ))(i)?;
    let (i, skip) = opt(preceded(kw("skip"), uint))(i)?;
    let (i, limit) = opt(preceded(kw("limit"), uint))(i)?;
    let (i, as_of) = opt(as_of_clause)(i)?;
    Ok((
        i,
        (
            where_clause,
            Return {
                distinct: distinct.is_some(),
                item,
            },
            order_by.unwrap_or_default(),
            skip,
            limit,
            as_of,
        ),
    ))
}

/// `AS OF <seq>` (a commit sequence), `AS OF "2026-07-01T00:00:00Z"` (an
/// RFC-3339 instant) or `AS OF TIME <ms>` (unix-epoch milliseconds). Last
/// clause in a query, so it reads as a modifier over the whole thing.
fn as_of_clause(i: &str) -> IResult<&str, AsOfSpec> {
    let (i, _) = kw("as")(i)?;
    let (i, _) = kw("of")(i)?;
    alt((
        // `TIME <ms>` — a raw epoch, the same address the RPC `as_of_ms` takes.
        map(preceded(kw("time"), int), AsOfSpec::Time),
        map_res(alt((quoted_str('\''), quoted_str('"'))), |s: String| {
            rfc3339_to_epoch_ms(&s).map(AsOfSpec::Time).ok_or(())
        }),
        map(uint, AsOfSpec::Seq),
    ))(i)
}

/// A signed integer (epoch milliseconds may predate 1970).
fn int(i: &str) -> IResult<&str, i64> {
    let (i, neg) = opt(symbol("-"))(i)?;
    let (i, n) = map_res(preceded(multispace0, digit1), str::parse::<i64>)(i)?;
    Ok((i, if neg.is_some() { -n } else { n }))
}

fn assemble(
    source: QuerySource,
    beams: Vec<BeamClause>,
    (where_clause, ret, order_by, skip, limit, as_of): Tail,
) -> Query {
    Query {
        source,
        beams,
        where_clause,
        ret,
        order_by,
        skip,
        limit,
        as_of,
    }
}

/// Sort an optional part's result into "didn't start" (`None` — try something
/// else) and "started but is malformed" (`Err::Failure` — report it here). A
/// plain `Err::Error` means the part's leading keyword didn't match at all.
#[allow(clippy::type_complexity)]
fn committed<T>(
    r: IResult<&str, T>,
) -> Result<Option<(&str, T)>, nom::Err<nom::error::Error<&str>>> {
    match r {
        Ok((rest, v)) => Ok(Some((rest, v))),
        Err(e @ nom::Err::Failure(_)) => Err(e),
        Err(_) => Ok(None),
    }
}

/// A source's relationship tail — the typed hops that may follow *any* seed,
/// not just a `MATCH` node.
fn source_tail(i: &str) -> IResult<&str, Vec<(RelPat, NodePat)>> {
    many0(pair(rel_pat, node_pat))(i)
}

fn match_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("match")(i)?;
    let (i, pattern) = pattern(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    let source = QuerySource {
        kind: SourceKind::Match,
        first: pattern.first,
        rest: pattern.rest,
    };
    Ok((i, assemble(source, beams, tail)))
}

/// `SEARCH (v:Label) ON prop NEAR "text"|[..] [METRIC m] [TOPK k]` (the vector
/// seed, `Source::VectorTopK`) or `SEARCH (v:Label) ON prop MATCHING "text"
/// [TOPK k]` (the BM25 seed, `Source::KeywordTopK`). One verb, two operators:
/// `NEAR` compares meaning, `MATCHING` compares words.
fn search_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("search")(i)?;
    let (i, first) = node_pat(i)?;
    // Optional for `NEAR` (the compiler fills in the conventional embedding
    // property); the compiler insists on it for `MATCHING`.
    let (i, property) = opt(preceded(kw("on"), ident))(i)?;
    let (i, kind) = alt((
        |i| {
            let (i, _) = kw("near")(i)?;
            let (i, query) = vec_arg(i)?;
            let (i, metric) = opt(preceded(kw("metric"), metric_ident))(i)?;
            let (i, k) = opt(preceded(kw("topk"), uint))(i)?;
            Ok((
                i,
                SourceKind::Search {
                    property: property.clone(),
                    query,
                    metric: metric.unwrap_or(Metric::Cosine),
                    k: k.unwrap_or(DEFAULT_TOPK),
                },
            ))
        },
        |i| {
            let (i, _) = kw("matching")(i)?;
            let (i, query) = alt((quoted_str('\''), quoted_str('"')))(i)?;
            let (i, k) = opt(preceded(kw("topk"), uint))(i)?;
            Ok((
                i,
                SourceKind::Keyword {
                    property: property.clone(),
                    query,
                    k: k.unwrap_or(DEFAULT_TOPK),
                },
            ))
        },
    ))(i)?;
    let (i, rest) = source_tail(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    Ok((i, assemble(QuerySource { kind, first, rest }, beams, tail)))
}

/// `HYBRID (v:Label) [VECTOR …] [KEYWORD …] [GRAPH …] [CANDIDATES n] [TOPK k]`
/// — fused retrieval (`Source::Hybrid`). Channels may appear in any order.
fn hybrid_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("hybrid")(i)?;
    let (i, first) = node_pat(i)?;
    let (mut i, mut clause) = (
        i,
        HybridClause {
            vector: None,
            keyword: None,
            graph: None,
            candidates: None,
            k: None,
        },
    );
    // Each part is optional and order-free; stop at the first token that starts
    // none of them (WHERE/RETURN/a relationship tail). A channel that *did*
    // start — its leading keyword matched — but is malformed fails hard
    // (`Err::Failure`, via `cut` inside the channel), so the error points at the
    // broken channel instead of unwinding to the top of the query.
    loop {
        if let Some((rest, v)) = committed(hybrid_vector(i))? {
            clause.vector = Some(v);
            i = rest;
            continue;
        }
        if let Some((rest, k)) = committed(hybrid_keyword(i))? {
            clause.keyword = Some(k);
            i = rest;
            continue;
        }
        if let Some((rest, g)) = committed(hybrid_graph(i))? {
            clause.graph = Some(g);
            i = rest;
            continue;
        }
        if let Ok((rest, n)) = preceded(kw("candidates"), uint)(i) {
            clause.candidates = Some(n);
            i = rest;
        } else if let Ok((rest, n)) = preceded(kw("topk"), uint)(i) {
            clause.k = Some(n);
            i = rest;
        } else {
            break;
        }
    }
    let (i, rest) = source_tail(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    let source = QuerySource {
        kind: SourceKind::Hybrid(clause),
        first,
        rest,
    };
    Ok((i, assemble(source, beams, tail)))
}

/// `VECTOR [ON prop] NEAR "text"|[..] [METRIC m] [WEIGHT w]`
fn hybrid_vector(i: &str) -> IResult<&str, HybridVector> {
    let (i, _) = kw("vector")(i)?;
    // Past the keyword this channel is committed: `NEAR <query>` is what makes
    // it a vector channel, so a miss here is an error to report, not a retry.
    let (i, property) = opt(preceded(kw("on"), ident))(i)?;
    let (i, _) = cut(kw("near"))(i)?;
    let (i, query) = cut(vec_arg)(i)?;
    let (i, metric) = opt(preceded(kw("metric"), metric_ident))(i)?;
    let (i, weight) = opt(preceded(kw("weight"), f32_num))(i)?;
    Ok((
        i,
        HybridVector {
            property,
            query,
            metric: metric.unwrap_or(Metric::Cosine),
            weight,
        },
    ))
}

/// `KEYWORD ON prop MATCHING "text" [WEIGHT w]`
fn hybrid_keyword(i: &str) -> IResult<&str, HybridKeyword> {
    let (i, _) = kw("keyword")(i)?;
    let (i, property) = opt(preceded(kw("on"), ident))(i)?;
    let (i, _) = cut(kw("matching"))(i)?;
    let (i, query) = cut(alt((quoted_str('\''), quoted_str('"'))))(i)?;
    let (i, weight) = opt(preceded(kw("weight"), f32_num))(i)?;
    Ok((
        i,
        HybridKeyword {
            property,
            query,
            weight,
        },
    ))
}

/// `GRAPH HOPS h [DECAY d] [SEEDS n] [WEIGHT w]`
fn hybrid_graph(i: &str) -> IResult<&str, HybridGraph> {
    let (i, _) = kw("graph")(i)?;
    // `HOPS <n>` is what makes it a graph channel; everything after is tuning.
    let (i, _) = cut(kw("hops"))(i)?;
    let (i, hops) = cut(map_res(preceded(multispace0, digit1), str::parse::<u32>))(i)?;
    let (i, decay) = opt(preceded(kw("decay"), f32_num))(i)?;
    let (i, seeds) = opt(preceded(kw("seeds"), uint))(i)?;
    let (i, weight) = opt(preceded(kw("weight"), f32_num))(i)?;
    Ok((
        i,
        HybridGraph {
            hops,
            decay,
            seeds,
            weight,
        },
    ))
}

/// `CALL name(arg: value, …) ON (v[:Label])` — a graph algorithm as a source
/// (`Source::Algo`). The `ON` node pattern both scopes the algorithm to a
/// label and binds the variable the rest of the query names.
fn call_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("call")(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = symbol("(")(i)?;
    let (i, args) = separated_list0(symbol(","), call_arg)(i)?;
    let (i, _) = symbol(")")(i)?;
    let (i, _) = kw("on")(i)?;
    let (i, first) = node_pat(i)?;
    let (i, rest) = source_tail(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    let source = QuerySource {
        kind: SourceKind::Call(CallClause { name, args }),
        first,
        rest,
    };
    Ok((i, assemble(source, beams, tail)))
}

/// One `name: value` algorithm argument.
fn call_arg(i: &str) -> IResult<&str, (String, Val)> {
    let (i, name) = ident(i)?;
    let (i, _) = symbol(":")(i)?;
    let (i, value) = prop_value(i)?;
    Ok((i, (name, value)))
}

/// Convert an RFC-3339 instant to unix-epoch milliseconds, or `None` if it
/// isn't one. Hand-rolled (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`) so the
/// parser keeps no date-time dependency; the date part uses Howard Hinnant's
/// days-from-civil algorithm, which is exact for the proleptic Gregorian
/// calendar.
fn rfc3339_to_epoch_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || (b[10] != b'T' && b[10] != b't' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if s.as_bytes()[4] != b'-'
        || s.as_bytes()[7] != b'-'
        || s.as_bytes()[13] != b':'
        || s.as_bytes()[16] != b':'
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || min > 59
        || sec > 60
    {
        return None;
    }

    // Optional fractional seconds, then a mandatory zone.
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return None;
        }
        // Truncate/pad to milliseconds.
        let ms: String = digits.chars().chain("000".chars()).take(3).collect();
        millis = ms.parse::<i64>().ok()?;
        rest = &rest[1 + digits.len()..];
    }
    let offset_min = match rest.as_bytes().first() {
        Some(b'Z') | Some(b'z') if rest.len() == 1 => 0,
        Some(sign @ (b'+' | b'-')) if rest.len() == 6 && rest.as_bytes()[3] == b':' => {
            let h = rest.get(1..3)?.parse::<i64>().ok()?;
            let m = rest.get(4..6)?.parse::<i64>().ok()?;
            if h > 23 || m > 59 {
                return None;
            }
            if *sign == b'-' {
                -(h * 60 + m)
            } else {
                h * 60 + m
            }
        }
        _ => return None,
    };

    // days_from_civil: days since 1970-01-01, shifting the year to start in
    // March so the leap day lands at the end of the era.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(((days * 86_400 + hour * 3600 + min * 60 + sec - offset_min * 60) * 1000) + millis)
}

/// A traversal direction keyword for `BEAM`.
fn beam_dir(i: &str) -> IResult<&str, Dir> {
    alt((
        value(Dir::Out, kw("out")),
        value(Dir::In, kw("in")),
        value(Dir::Both, kw("both")),
    ))(i)
}

/// `BEAM (result[:Label]) <OUT|IN|BOTH> [:TYPE] ON prop NEAR <q> [METRIC m]
/// WIDTH w DEPTH d`.
fn beam_clause(i: &str) -> IResult<&str, BeamClause> {
    let (i, _) = kw("beam")(i)?;
    let (i, node) = node_pat(i)?;
    let (i, dir) = beam_dir(i)?;
    let (i, edge_type) = opt(preceded(preceded(multispace0, char(':')), ident))(i)?;
    let (i, property) = opt(preceded(kw("on"), ident))(i)?;
    let (i, _) = kw("near")(i)?;
    let (i, query) = vec_arg(i)?;
    let (i, metric) = opt(preceded(kw("metric"), metric_ident))(i)?;
    let (i, _) = kw("width")(i)?;
    let (i, width) = map_res(preceded(multispace0, digit1), str::parse::<u32>)(i)?;
    let (i, _) = kw("depth")(i)?;
    let (i, depth) = map_res(preceded(multispace0, digit1), str::parse::<u32>)(i)?;
    Ok((
        i,
        BeamClause {
            node,
            dir,
            edge_type,
            property,
            query,
            metric: metric.unwrap_or(Metric::Cosine),
            width,
            depth,
        },
    ))
}

/// Default `TOPK` when the `SEARCH` clause omits it.
const DEFAULT_TOPK: u64 = 10;

/// Parse a whole read query. The public [`crate::parse`] wraps this and
/// enforces that all input was consumed.
pub fn query(i: &str) -> IResult<&str, Query> {
    alt((match_query, search_query, hybrid_query, call_query))(i)
}

// ---- write statements -----------------------------------------------------

/// A property value inside `{ … }`: a `$name` parameter, or a literal — number
/// (with optional `-`), string, bool, null, or a vector literal.
fn prop_value(i: &str) -> IResult<&str, Val> {
    alt((map(param_name, Val::Param), map(prop_literal, Val::Lit)))(i)
}

fn prop_literal(i: &str) -> IResult<&str, PropValue> {
    alt((
        map(preceded(symbol("-"), number), negate_number),
        number,
        quoted('\''),
        quoted('"'),
        value(PropValue::Bool(true), kw("true")),
        value(PropValue::Bool(false), kw("false")),
        value(PropValue::Null, kw("null")),
        map(vector_literal, PropValue::Vector),
    ))(i)
}

/// A `$name` parameter placeholder — returns the bare name.
fn param_name(i: &str) -> IResult<&str, String> {
    preceded(symbol("$"), ident)(i)
}

fn negate_number(v: PropValue) -> PropValue {
    match v {
        PropValue::Int(n) => PropValue::Int(-n),
        PropValue::Float(f) => PropValue::Float(-f),
        other => other,
    }
}

/// An inline property map `{ key: value, … }` (empty allowed).
fn prop_map(i: &str) -> IResult<&str, Vec<(String, Val)>> {
    let (i, _) = symbol("{")(i)?;
    let (i, entries) = separated_list0(symbol(","), prop_entry)(i)?;
    let (i, _) = symbol("}")(i)?;
    Ok((i, entries))
}

fn prop_entry(i: &str) -> IResult<&str, (String, Val)> {
    let (i, k) = ident(i)?;
    let (i, _) = symbol(":")(i)?;
    let (i, v) = prop_value(i)?;
    Ok((i, (k, v)))
}

fn create_node(i: &str) -> IResult<&str, CreateNode> {
    let (i, _) = symbol("(")(i)?;
    let (i, var) = opt(ident)(i)?;
    let (i, label) = opt(preceded(preceded(multispace0, char(':')), ident))(i)?;
    let (i, raw) = opt(prop_map)(i)?;
    let (i, _) = symbol(")")(i)?;

    // A literal string `key` sets the external key; everything else is a
    // property. (A `$param` key stays a property — keys must be literals.)
    let mut key = None;
    let mut props = Vec::new();
    for (k, v) in raw.unwrap_or_default() {
        match (k.as_str(), &v) {
            ("key", Val::Lit(PropValue::Str(s))) => key = Some(s.clone()),
            _ => props.push((k, v)),
        }
    }
    Ok((
        i,
        CreateNode {
            var,
            label,
            key,
            props,
        },
    ))
}

fn create_rel(i: &str) -> IResult<&str, CreateRel> {
    let (i, _) = multispace0(i)?;
    let (i, left) = opt(char('<'))(i)?;
    let (i, _) = char('-')(i)?;
    let (i, _) = char('[')(i)?;
    let (i, _) = preceded(multispace0, char(':'))(i)?; // a CREATE edge needs a type
    let (i, ty) = ident(i)?;
    let (i, props) = opt(prop_map)(i)?;
    let (i, _) = symbol("]")(i)?;
    let (i, _) = char('-')(i)?;
    let (i, right) = opt(char('>'))(i)?;
    let dir = match (left.is_some(), right.is_some()) {
        (false, true) => Dir::Out,
        (true, false) => Dir::In,
        // Undirected / two-headed: dr-strange edges are directed.
        _ => {
            return Err(nom::Err::Error(nom::error::Error::new(
                i,
                nom::error::ErrorKind::Verify,
            )));
        }
    };
    Ok((
        i,
        CreateRel {
            dir,
            ty,
            props: props.unwrap_or_default(),
        },
    ))
}

fn create_path(i: &str) -> IResult<&str, CreatePath> {
    let (i, first) = create_node(i)?;
    let (i, rest) = many0(pair(create_rel, create_node))(i)?;
    Ok((i, CreatePath { first, rest }))
}

/// `CREATE (n:L {..}), (a)-[:T {..}]->(b), …` — usable standalone or as a
/// clause after `MATCH` (anchoring new nodes/edges to the matched node).
fn create_clause(i: &str) -> IResult<&str, WriteOp> {
    let (i, _) = kw("create")(i)?;
    let (i, paths) = separated_list1(symbol(","), create_path)(i)?;
    Ok((i, WriteOp::Create(paths)))
}

fn create_stmt(i: &str) -> IResult<&str, WriteAst> {
    let (i, op) = create_clause(i)?;
    Ok((
        i,
        WriteAst {
            match_clause: None,
            ops: vec![op],
        },
    ))
}

/// `MERGE (n:L {key:"k", ..}) [ON CREATE SET …] [ON MATCH SET …]` — upsert one
/// node, or a path `MERGE (a {key})-[:T]->(b {key})`.
fn merge_clause(i: &str) -> IResult<&str, WriteOp> {
    let (i, _) = kw("merge")(i)?;
    let (i, path) = create_path(i)?;
    let (i, clauses) = many0(merge_on)(i)?;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();
    for (is_create, items) in clauses {
        if is_create {
            on_create.extend(items);
        } else {
            on_match.extend(items);
        }
    }
    Ok((
        i,
        WriteOp::Merge(MergeClause {
            path,
            on_create,
            on_match,
        }),
    ))
}

fn merge_stmt(i: &str) -> IResult<&str, WriteAst> {
    let (i, op) = merge_clause(i)?;
    Ok((
        i,
        WriteAst {
            match_clause: None,
            ops: vec![op],
        },
    ))
}

/// `ON CREATE SET …` (true) or `ON MATCH SET …` (false).
fn merge_on(i: &str) -> IResult<&str, (bool, Vec<SetItem>)> {
    let (i, _) = kw("on")(i)?;
    let (i, is_create) = alt((value(true, kw("create")), value(false, kw("match"))))(i)?;
    let (i, _) = kw("set")(i)?;
    let (i, items) = separated_list1(symbol(","), set_item)(i)?;
    Ok((i, (is_create, items)))
}

/// `MATCH pattern [WHERE …] (SET|REMOVE|DELETE)…` — find nodes, then mutate them.
fn match_write_stmt(i: &str) -> IResult<&str, WriteAst> {
    let (i, _) = kw("match")(i)?;
    let (i, pattern) = pattern(i)?;
    let (i, where_clause) = opt(preceded(kw("where"), expr))(i)?;
    let (i, ops) = many1(mutate_op)(i)?;
    Ok((
        i,
        WriteAst {
            match_clause: Some(MatchClause {
                pattern,
                where_clause,
            }),
            ops,
        },
    ))
}

fn mutate_op(i: &str) -> IResult<&str, WriteOp> {
    alt((set_op, remove_op, delete_op, create_clause, merge_clause))(i)
}

fn set_op(i: &str) -> IResult<&str, WriteOp> {
    let (i, _) = kw("set")(i)?;
    let (i, items) = separated_list1(symbol(","), set_item)(i)?;
    Ok((i, WriteOp::Set(items)))
}

fn set_item(i: &str) -> IResult<&str, SetItem> {
    let (i, var) = ident(i)?;
    // `n += { .. }`
    if let Ok((rest, _)) = symbol("+=")(i) {
        let (rest, props) = prop_map(rest)?;
        return Ok((rest, SetItem::Merge { var, props }));
    }
    let (i, _) = multispace0(i)?;
    let (i, sep) = one_of(".:")(i)?;
    if sep == '.' {
        // `n.key = value`
        let (i, key) = ident(i)?;
        let (i, _) = symbol("=")(i)?;
        let (i, value) = prop_value(i)?;
        Ok((i, SetItem::Prop { var, key, value }))
    } else {
        // `n:Label`
        let (i, label) = ident(i)?;
        Ok((i, SetItem::Label { var, label }))
    }
}

fn remove_op(i: &str) -> IResult<&str, WriteOp> {
    let (i, _) = kw("remove")(i)?;
    let (i, items) = separated_list1(symbol(","), remove_item)(i)?;
    Ok((i, WriteOp::Remove(items)))
}

fn remove_item(i: &str) -> IResult<&str, RemoveItem> {
    let (i, var) = ident(i)?;
    let (i, _) = multispace0(i)?;
    let (i, sep) = one_of(".:")(i)?;
    let (i, name) = ident(i)?;
    Ok((
        i,
        if sep == '.' {
            RemoveItem::Prop { var, key: name }
        } else {
            RemoveItem::Label { var, label: name }
        },
    ))
}

fn delete_op(i: &str) -> IResult<&str, WriteOp> {
    let (i, detach) = opt(kw("detach"))(i)?;
    let (i, _) = kw("delete")(i)?;
    let (i, vars) = separated_list1(symbol(","), ident)(i)?;
    Ok((
        i,
        WriteOp::Delete {
            detach: detach.is_some(),
            vars,
        },
    ))
}

/// Parse a whole statement — a read query or a write. The public
/// [`crate::parse_statement`] wraps this and enforces all input is consumed.
pub fn statement(i: &str) -> IResult<&str, StmtAst> {
    alt((
        map(create_stmt, StmtAst::Write),
        map(merge_stmt, StmtAst::Write),
        map(match_write_stmt, StmtAst::Write),
        map(query, |q| StmtAst::Read(Box::new(q))),
    ))(i)
}
