//! The `nom` grammar: text → [`Query`] AST. Whitespace is skipped before every
//! token (each `symbol`/`kw`/`ident`/number leads with `multispace0`), so the
//! grammar rules read without threading whitespace explicitly.

use nom::IResult;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take_while};
use nom::character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, one_of};
use nom::combinator::{map, map_res, not, opt, recognize, value};
use nom::multi::{many0, many1, separated_list0, separated_list1};
use nom::sequence::{delimited, pair, preceded, tuple};

use dr_strange_core::Metric;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};
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
    alt((paren_expr, literal, func_or_var))(i)
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
/// RETURN, then optional ORDER BY / SKIP / LIMIT.
type Tail = (
    Option<PExpr>,
    Return,
    Vec<OrderKey>,
    Option<u64>,
    Option<u64>,
);

fn query_tail(i: &str) -> IResult<&str, Tail> {
    let (i, where_clause) = opt(preceded(kw("where"), expr))(i)?;
    let (i, _) = kw("return")(i)?;
    let (i, distinct) = opt(kw("distinct"))(i)?;
    let (i, item) = return_item(i)?;
    let (i, order_by) = opt(preceded(
        pair(kw("order"), kw("by")),
        separated_list1(symbol(","), order_key),
    ))(i)?;
    let (i, skip) = opt(preceded(kw("skip"), uint))(i)?;
    let (i, limit) = opt(preceded(kw("limit"), uint))(i)?;
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
        ),
    ))
}

fn assemble(
    source: QuerySource,
    beams: Vec<BeamClause>,
    (where_clause, ret, order_by, skip, limit): Tail,
) -> Query {
    Query {
        source,
        beams,
        where_clause,
        ret,
        order_by,
        skip,
        limit,
    }
}

fn match_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("match")(i)?;
    let (i, pattern) = pattern(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    Ok((i, assemble(QuerySource::Match(pattern), beams, tail)))
}

/// `SEARCH (v:Label) ON prop NEAR "text"|[..] [METRIC m] [TOPK k]` — the
/// indexed vector seed (`Source::VectorTopK`).
fn search_query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("search")(i)?;
    let (i, node) = node_pat(i)?;
    let (i, _) = kw("on")(i)?;
    let (i, property) = ident(i)?;
    let (i, _) = kw("near")(i)?;
    let (i, query) = vec_arg(i)?;
    let (i, metric) = opt(preceded(kw("metric"), metric_ident))(i)?;
    let (i, k) = opt(preceded(kw("topk"), uint))(i)?;
    let (i, beams) = many0(beam_clause)(i)?;
    let (i, tail) = query_tail(i)?;
    let clause = SearchClause {
        node,
        property,
        query,
        metric: metric.unwrap_or(Metric::Cosine),
        k: k.unwrap_or(DEFAULT_TOPK),
    };
    Ok((i, assemble(QuerySource::Search(clause), beams, tail)))
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
    let (i, _) = kw("on")(i)?;
    let (i, property) = ident(i)?;
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
    alt((match_query, search_query))(i)
}

// ---- write statements -----------------------------------------------------

/// A literal property value inside `{ … }`: number (with optional `-`), string,
/// bool, null, or a vector literal (e.g. an embedding).
fn prop_value(i: &str) -> IResult<&str, PropValue> {
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

fn negate_number(v: PropValue) -> PropValue {
    match v {
        PropValue::Int(n) => PropValue::Int(-n),
        PropValue::Float(f) => PropValue::Float(-f),
        other => other,
    }
}

/// An inline property map `{ key: value, … }` (empty allowed).
fn prop_map(i: &str) -> IResult<&str, Vec<(String, PropValue)>> {
    let (i, _) = symbol("{")(i)?;
    let (i, entries) = separated_list0(symbol(","), prop_entry)(i)?;
    let (i, _) = symbol("}")(i)?;
    Ok((i, entries))
}

fn prop_entry(i: &str) -> IResult<&str, (String, PropValue)> {
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

    // A string-valued `key` sets the external key; everything else is a property.
    let mut key = None;
    let mut props = Vec::new();
    for (k, v) in raw.unwrap_or_default() {
        match (k.as_str(), &v) {
            ("key", PropValue::Str(s)) => key = Some(s.clone()),
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
    alt((set_op, remove_op, delete_op, create_clause))(i)
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
