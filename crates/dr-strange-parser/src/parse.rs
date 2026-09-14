//! The `nom` grammar: text → [`Query`] AST. Whitespace is skipped before every
//! token (each `symbol`/`kw`/`ident`/number leads with `multispace0`), so the
//! grammar rules read without threading whitespace explicitly.

use std::cell::{Cell, RefCell};

use nom::IResult;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take_while};
use nom::character::complete::{char, digit1, multispace0, one_of, satisfy};
use nom::combinator::{consumed, cut, map, map_res, not, opt, recognize, value, verify};
use nom::multi::{many0, many1, separated_list0, separated_list1};
use nom::sequence::{delimited, pair, preceded, tuple};

use dr_strange_core::AggFunc;
use dr_strange_core::Metric;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp, StrOp};
use dr_strange_core::time::rfc3339_to_epoch_ms;
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
        let (i, _) = not(satisfy(is_ident_char))(i)?;
        Ok((i, ()))
    }
}

/// Identifier characters are Unicode alphanumerics (plus `_`), not ASCII:
/// a label named in Chinese or a property named `café` is an identifier too.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// An identifier (variable / label / type / property key): a plain word, or
/// anything at all between backticks (`` `order` ``, `` `has space` ``), the
/// openCypher escape for names the plain form cannot spell.
fn ident(i: &str) -> IResult<&str, String> {
    let (i, _) = multispace0(i)?;
    alt((
        map(
            recognize(pair(satisfy(is_ident_start), take_while(is_ident_char))),
            str::to_string,
        ),
        map(
            delimited(
                char('`'),
                verify(take_while(|c| c != '`'), |s: &str| !s.is_empty()),
                char('`'),
            ),
            str::to_string,
        ),
    ))(i)
}

// ---- nesting depth ----------------------------------------------------------
//
// The expression grammar recurses (parentheses, `NOT` chains, unary minus), and
// a `nom` parser recurses on the machine stack. Without a bound, ten thousand
// `(` overflow the stack and abort the process — a denial of service from one
// query. The bound is far above any query a person or model writes, and well
// inside what a 2 MiB thread holds even unoptimised (where a level costs
// ~16 KiB of `nom` frames and ~120 levels overflow). A guard per recursive
// descent keeps the count exact even when a branch backtracks.

/// The deepest expression nesting the parser accepts.
pub const MAX_NESTING: usize = 64;

thread_local! {
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Holds one level of nesting for as long as it lives.
struct Nesting;

impl Drop for Nesting {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Enter one level of nesting; past [`MAX_NESTING`] this is a hard failure
/// (`ErrorKind::TooLarge`, which [`crate`] names in its message) rather than a
/// soft error, so no enclosing `alt` re-descends the same input looking for
/// another reading.
fn descend(i: &str) -> IResult<&str, Nesting> {
    let depth = DEPTH.with(|d| d.get());
    if depth >= MAX_NESTING {
        return Err(nom::Err::Failure(nom::error::Error::new(
            i,
            nom::error::ErrorKind::TooLarge,
        )));
    }
    DEPTH.with(|d| d.set(depth + 1));
    Ok((i, Nesting))
}

// ---- unsupported shapes -----------------------------------------------------
//
// Some openCypher the grammar can *recognise* but this cut cannot run: a
// second MATCH, WITH, UNION, a relationship variable, a list literal outside
// IN. Left to the plain grammar they would surface as "near `WITH n`" — true,
// and useless. Instead the point where the shape is recognised fails *hard*
// (so no enclosing `alt` re-reads the text as something else) and leaves a
// message behind for [`crate`] to report as an unsupported-query error with
// the rewrite that works.

thread_local! {
    static UNSUPPORTED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Refuse a recognised-but-unsupported shape at `i`: a hard failure tagged
/// `ErrorKind::Fail`, with `msg` parked for [`take_unsupported`].
fn unsupported<T>(i: &str, msg: impl Into<String>) -> IResult<&str, T> {
    UNSUPPORTED.with(|u| *u.borrow_mut() = Some(msg.into()));
    Err(nom::Err::Failure(nom::error::Error::new(
        i,
        nom::error::ErrorKind::Fail,
    )))
}

/// The message behind the last [`unsupported`] failure on this thread, if a
/// parse ended in one. [`statement`] clears it on entry, so a message never
/// outlives the parse that produced it.
pub fn take_unsupported() -> Option<String> {
    UNSUPPORTED.with(|u| u.borrow_mut().take())
}

/// Where a source's clauses end and its RETURN should begin: the clause words
/// openCypher allows here and this cut does not each get their own message,
/// rather than "expected RETURN".
fn clause_boundary(i: &str) -> IResult<&str, ()> {
    if kw("optional")(i).is_ok() {
        return unsupported(
            i,
            "OPTIONAL MATCH isn't supported; a pattern either matches or drops the row \
             — run one MATCH per pattern and merge the results client-side",
        );
    }
    if kw("match")(i).is_ok() {
        return unsupported(
            i,
            "a second MATCH isn't supported; a query holds one linear path — chain the \
             hop onto the first pattern (`MATCH (a)-[:T]->(b)`) or run one MATCH per pattern",
        );
    }
    if kw("with")(i).is_ok() {
        return unsupported(
            i,
            "WITH isn't supported; a projection ends the query, so nothing can follow it \
             — fold the second stage into RETURN (`RETURN a.name, count(*) AS n`) or run two queries",
        );
    }
    if kw("unwind")(i).is_ok() {
        return unsupported(
            i,
            "UNWIND isn't supported; there are no list rows — use `x IN [..]` in WHERE, \
             or run one query per value",
        );
    }
    if kw("union")(i).is_ok() {
        return unsupported(
            i,
            "UNION isn't supported; run each side as its own query and concatenate the results",
        );
    }
    if pair(symbol(","), symbol("("))(i).is_ok() {
        return unsupported(
            i,
            "a pattern with several paths (`(a)-->(b), (a)-->(c)`) isn't supported; a query \
             holds one linear path — run one MATCH per branch",
        );
    }
    Ok((i, ()))
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

/// The arithmetic operators openCypher has and this cut lacks.
fn other_op(i: &str) -> IResult<&str, char> {
    preceded(multispace0, one_of("%^"))(i)
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

/// An unsigned number: `42`, `3.5`, `1e9`, `2.5E-3`. A fraction or an
/// exponent makes it a float; a bare run of digits is an int.
fn number(i: &str) -> IResult<&str, PropValue> {
    let (i, _) = multispace0(i)?;
    let (rest, text) = recognize(tuple((
        digit1,
        opt(pair(char('.'), digit1)),
        opt(tuple((one_of("eE"), opt(one_of("+-")), digit1))),
    )))(i)?;
    let parsed = if text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse::<i64>().map(PropValue::Int).ok()
    } else {
        text.parse::<f64>().map(PropValue::Float).ok()
    };
    match parsed {
        Some(v) => Ok((rest, v)),
        // Out of range for the type (an int past i64) — not a number we hold.
        None => Err(nom::Err::Error(nom::error::Error::new(
            i,
            nom::error::ErrorKind::Digit,
        ))),
    }
}

fn quoted<'a>(q: char) -> impl Fn(&'a str) -> IResult<&'a str, PropValue> {
    move |i: &'a str| map(quoted_str(q), PropValue::Str)(i)
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
    let (i, _nesting) = descend(i)?;
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
        let (rest, _nesting) = descend(rest)?;
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
    // openCypher's other two: neither has an `ArithOp`, so say so here rather
    // than leave `%` to be reported as trailing input.
    if let Ok((rest, op)) = other_op(i) {
        let msg = match op {
            '%' => "the `%` (modulo) operator isn't supported; the arithmetic is `+ - * /`",
            _ => {
                "the `^` (power) operator isn't supported; the arithmetic is `+ - * /` \
                  — spell a small power out as a product (`x * x`)"
            }
        };
        return unsupported(rest, msg);
    }
    Ok((i, lhs))
}

fn unary(i: &str) -> IResult<&str, PExpr> {
    if let Ok((rest, _)) = symbol("-")(i) {
        let (rest, _nesting) = descend(rest)?;
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
        composite_literal,
    ))(i)
}

/// A `[`/`{` where a term should be: a list or map literal. Neither is a value
/// the expression language holds (a list is only sugar after `IN`), so name
/// the shape rather than fail on the bracket.
fn composite_literal(i: &str) -> IResult<&str, PExpr> {
    let (i, _) = multispace0(i)?;
    if i.starts_with('[') {
        return unsupported(
            i,
            "a list literal is only supported on the right of IN (`x IN [1, 2]`), or \
             as a vector in NEAR / similarity(); it isn't a value elsewhere",
        );
    }
    if i.starts_with('{') {
        return unsupported(
            i,
            "a map literal isn't a value in an expression; set properties one by one \
             (`SET n.a = 1, n.b = 2` or `SET n += {a: 1}`)",
        );
    }
    Err(nom::Err::Error(nom::error::Error::new(
        i,
        nom::error::ErrorKind::Tag,
    )))
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
/// Names are case-insensitive, like keywords: `Score()` is `score()`.
fn func_call<'a>(name: &str, i: &'a str) -> IResult<&'a str, PExpr> {
    let (i, _) = symbol("(")(i)?;
    let name = name.to_ascii_lowercase();
    match name.as_str() {
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

/// A quoted string's contents, in either quote style, with the openCypher
/// escapes: `\'`, `\"`, `\\`, `\n`, `\t`, `\r`, `\b`, `\f`, `\uXXXX`.
/// An escaped quote is part of the string, never its end — so a value
/// containing a quote cannot break out of the literal and into the grammar.
fn quoted_str<'a>(q: char) -> impl Fn(&'a str) -> IResult<&'a str, String> {
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (mut i, _) = char(q)(i)?;
        let mut out = String::new();
        loop {
            // The plain run up to the next quote or backslash, copied whole.
            let run = i.find([q, '\\']).unwrap_or(i.len());
            out.push_str(&i[..run]);
            i = &i[run..];
            match i.chars().next() {
                Some(c) if c == q => return Ok((&i[c.len_utf8()..], out)),
                Some(_) => {
                    let (rest, c) = escape(&i[1..])?;
                    out.push(c);
                    i = rest;
                }
                // Unterminated: the string ran to the end of the query.
                None => {
                    return Err(nom::Err::Error(nom::error::Error::new(
                        i,
                        nom::error::ErrorKind::Char,
                    )));
                }
            }
        }
    }
}

/// The character an escape (after its backslash) stands for. An unknown
/// escape is a hard failure: the string can't be read any other way, so
/// backtracking would only blame something else.
fn escape(i: &str) -> IResult<&str, char> {
    let fail = || nom::Err::Failure(nom::error::Error::new(i, nom::error::ErrorKind::Escaped));
    let c = i.chars().next().ok_or_else(fail)?;
    let rest = &i[c.len_utf8()..];
    let out = match c {
        '\\' | '\'' | '"' => c,
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'u' => {
            let hex = rest.get(..4).ok_or_else(fail)?;
            let code = u32::from_str_radix(hex, 16).map_err(|_| fail())?;
            let c = char::from_u32(code).ok_or_else(fail)?;
            return Ok((&rest[4..], c));
        }
        _ => return Err(fail()),
    };
    Ok((rest, out))
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
    // `(n:L {name: "x"})` is a predicate in openCypher; here predicates live
    // in WHERE, so point there instead of failing on the brace.
    let (at, _) = multispace0(i)?;
    if at.starts_with('{') {
        let v = var.as_deref().unwrap_or("n");
        return unsupported(
            at,
            format!(
                "inline properties in a MATCH pattern aren't supported; write the \
                 predicate in WHERE: `WHERE {v}.name = \"…\"` (or `key({v}) = \"…\"`)"
            ),
        );
    }
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

/// The bracketed body of a relationship: `[(:Type)? (*range)?]`. A
/// relationship variable (`[r:T]`) is refused, not ignored: the row model
/// binds nodes only, so nothing could read `r` — silently dropping it would
/// let `RETURN r.since` fail somewhere else for an unrelated reason.
fn rel_body(i: &str) -> IResult<&str, (Option<String>, Option<VarLen>)> {
    let (i, _) = multispace0(i)?;
    if let Ok((_, relvar)) = ident(i) {
        return unsupported(
            i,
            format!(
                "relationship variables aren't supported (`[{relvar}:T]`); an edge can't \
                 be bound, returned or filtered on yet — drop the variable: `-[:T]->`"
            ),
        );
    }
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

/// The aggregate functions a `RETURN` item may name.
fn agg_func(name: &str) -> Option<AggFunc> {
    Some(match name.to_ascii_lowercase().as_str() {
        "count" => AggFunc::Count,
        "sum" => AggFunc::Sum,
        "avg" => AggFunc::Avg,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "collect" => AggFunc::Collect,
        _ => return None,
    })
}

/// `count(*)` / `count(DISTINCT n.file)` / `sum(n.year)`. Only `count` takes
/// `*`; every other fold reads a value.
fn agg_call(i: &str) -> IResult<&str, (AggFunc, Option<PExpr>, bool)> {
    let (i, name) = ident(i)?;
    let fail = || nom::Err::Error(nom::error::Error::new(i, nom::error::ErrorKind::Tag));
    let func = agg_func(&name).ok_or_else(fail)?;
    let (i, _) = symbol("(")(i)?;
    let (i, distinct) = opt(kw("distinct"))(i)?;
    let (i, star) = opt(symbol("*"))(i)?;
    let (i, arg) = match star {
        Some(_) if func == AggFunc::Count => (i, None),
        Some(_) => return Err(fail()),
        None => {
            let (i, e) = expr(i)?;
            (i, Some(e))
        }
    };
    let (i, _) = symbol(")")(i)?;
    Ok((i, (func, arg, distinct.is_some())))
}

/// `AS <alias>`. Not `AS OF`: that clause may follow a `RETURN` item, so an
/// `of` here belongs to it.
fn alias(i: &str) -> IResult<&str, Option<String>> {
    opt(preceded(
        kw("as"),
        verify(ident, |name: &String| !name.eq_ignore_ascii_case("of")),
    ))(i)
}

/// A column's header when the query gave it no alias: the item as written.
fn as_written(text: &str) -> String {
    text.trim().to_string()
}

fn return_item(i: &str) -> IResult<&str, ReturnItem> {
    if let Ok((rest, _)) = symbol("*")(i) {
        return Ok((rest, ReturnItem::Star));
    }
    // Before a plain expression: `count(…)` is not one of the expression
    // language's functions.
    if let Ok((rest, (text, (func, arg, distinct)))) = consumed(agg_call)(i) {
        let (rest, alias) = alias(rest)?;
        return Ok((
            rest,
            ReturnItem::Agg {
                func,
                arg,
                distinct,
                name: alias.unwrap_or_else(|| as_written(text)),
            },
        ));
    }
    match consumed(expr)(i) {
        Ok((rest, (text, expr))) => {
            let (rest, alias) = alias(rest)?;
            return Ok((
                rest,
                ReturnItem::Value {
                    expr,
                    name: alias.unwrap_or_else(|| as_written(text)),
                },
            ));
        }
        // A hard failure (unsupported shape, nesting) is the answer; only a
        // soft miss falls through to the bare-variable reading.
        Err(e @ nom::Err::Failure(_)) => return Err(e),
        Err(_) => {}
    }
    // A bare variable: the rows themselves.
    map(ident, ReturnItem::Var)(i)
}

/// Words that end an `ORDER BY` key, so a bare name never swallows one.
fn is_clause_word(word: &str) -> bool {
    ["asc", "desc", "skip", "limit", "as"]
        .iter()
        .any(|w| word.eq_ignore_ascii_case(w))
}

fn order_key(i: &str) -> IResult<&str, OrderKey> {
    let (i, (text, target)) = consumed(alt((
        // `count(*)` first: not one of the expression language's functions.
        map(agg_call, |(func, arg, distinct)| SortTarget::Agg {
            func,
            arg,
            distinct,
        }),
        map(expr, SortTarget::Expr),
        // A bare name is a RETURN alias.
        map(
            verify(ident, |name: &String| !is_clause_word(name)),
            SortTarget::Name,
        ),
    )))(i)?;
    let (i, dir) = opt(alt((map(kw("desc"), |_| true), map(kw("asc"), |_| false))))(i)?;
    Ok((
        i,
        OrderKey {
            target,
            text: as_written(text),
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
    // and blaming the first token. The clauses openCypher would allow here
    // first get their own, more useful refusal.
    let (i, _) = clause_boundary(i)?;
    let (i, _) = cut(kw("return"))(i)?;
    let (i, distinct) = opt(kw("distinct"))(i)?;
    let (i, items) = separated_list1(symbol(","), return_item)(i)?;
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
                items,
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
    // Committed past the brace: a map has no other reading, so a bad entry
    // is reported where it is, not as trailing input after the whole clause.
    let (i, entries) = separated_list0(symbol(","), prop_entry)(i)?;
    let (i, _) = cut(symbol("}"))(i)?;
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
    // Inside a CREATE/MERGE the parenthesis has one reading; commit so the
    // error lands on the node, not on the statement's first token.
    let (i, _) = cut(symbol(")"))(i)?;

    // A string `key` — literal or `$param` — sets the external key; everything
    // else is a property. (The param's type is checked once it resolves.)
    let mut key = None;
    let mut props = Vec::new();
    for (k, v) in raw.unwrap_or_default() {
        match (k.as_str(), &v) {
            ("key", Val::Lit(PropValue::Str(_)) | Val::Param(_)) => key = Some(v),
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
    // Past `-[` this is an edge; commit (a CREATE edge needs a type).
    let (i, _) = cut(preceded(multispace0, char(':')))(i)?;
    let (i, ty) = cut(ident)(i)?;
    let (i, props) = opt(prop_map)(i)?;
    let (i, _) = cut(symbol("]"))(i)?;
    let (i, _) = cut(char('-'))(i)?;
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
    // The keyword commits: what follows can only be paths, so a typo in the
    // third one is reported there — `separated_list1` would back out to the
    // comma and leave the rest to be called trailing input.
    let (i, first) = cut(create_path)(i)?;
    let (i, more) = many0(preceded(symbol(","), cut(create_path)))(i)?;
    let mut paths = vec![first];
    paths.extend(more);
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
    let (i, path) = cut(create_path)(i)?;
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
    let (i, is_create) = cut(alt((value(true, kw("create")), value(false, kw("match")))))(i)?;
    let (i, _) = cut(kw("set"))(i)?;
    let (i, items) = cut(separated_list1(symbol(","), set_item))(i)?;
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
    // Each mutating clause commits on its keyword, for the same reason as
    // CREATE: the error should name the item, not the clause.
    let (i, items) = cut(separated_list1(symbol(","), set_item))(i)?;
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
    let (i, items) = cut(separated_list1(symbol(","), remove_item))(i)?;
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
    let (i, vars) = cut(separated_list1(symbol(","), ident))(i)?;
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
///
/// Dispatch is by the leading keyword rather than a blind `alt`: `alt`
/// reports its *last* alternative's error, which for a mistyped CREATE was the
/// read grammar failing to see MATCH at token 0. A MATCH still has two
/// readings (write, read); when both fail the one that got further is the
/// one that understood the query.
pub fn statement(i: &str) -> IResult<&str, StmtAst> {
    take_unsupported();
    if kw("optional")(i).is_ok() {
        clause_boundary(i)?;
    }
    let parsed = if kw("create")(i).is_ok() {
        map(create_stmt, StmtAst::Write)(i)
    } else if kw("merge")(i).is_ok() {
        map(merge_stmt, StmtAst::Write)(i)
    } else {
        match map(match_write_stmt, StmtAst::Write)(i) {
            Ok(ok) => Ok(ok),
            Err(e @ nom::Err::Failure(_)) => Err(e),
            Err(write) => {
                map(query, |q| StmtAst::Read(Box::new(q)))(i).map_err(|read| furthest(write, read))
            }
        }
    };
    let (rest, stmt) = parsed?;
    // What may follow a complete statement in openCypher, and not here.
    let (rest, _) = clause_boundary(rest)?;
    Ok((rest, stmt))
}

/// Of two failed readings of the same text, the one that got further: a
/// hard failure over a soft one, then the shorter remaining input. Ties go to
/// `b`, the reading tried last.
fn furthest<'a>(
    a: nom::Err<nom::error::Error<&'a str>>,
    b: nom::Err<nom::error::Error<&'a str>>,
) -> nom::Err<nom::error::Error<&'a str>> {
    use nom::Err::{Error, Failure};
    match (&a, &b) {
        (Failure(_), Error(_)) => a,
        (Error(_), Failure(_)) => b,
        (Error(x) | Failure(x), Error(y) | Failure(y)) if x.input.len() < y.input.len() => a,
        _ => b,
    }
}
