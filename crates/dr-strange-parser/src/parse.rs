//! The `nom` grammar: text → [`Query`] AST. Whitespace is skipped before every
//! token (each `symbol`/`kw`/`ident`/number leads with `multispace0`), so the
//! grammar rules read without threading whitespace explicitly.

use nom::IResult;
use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take_while};
use nom::character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, one_of};
use nom::combinator::{map, map_res, not, opt, recognize, value};
use nom::multi::{many0, separated_list1};
use nom::sequence::{delimited, pair, preceded, tuple};

use dr_strange_core::Dir;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};

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
    alt((paren_expr, literal, var_ref))(i)
}

fn paren_expr(i: &str) -> IResult<&str, PExpr> {
    let (i, _) = symbol("(")(i)?;
    let (i, e) = expr(i)?;
    let (i, _) = symbol(")")(i)?;
    Ok((i, e))
}

/// A variable reference: `v.key` (property) or `v:Label` (label predicate). A
/// bare variable is not a valid expression term in this cut.
fn var_ref(i: &str) -> IResult<&str, PExpr> {
    let (i, var) = ident(i)?;
    let (i, _) = multispace0(i)?;
    let (i, sep) = one_of(".:")(i)?;
    let (i, name) = ident(i)?;
    let out = if sep == '.' {
        PExpr::Prop { var, key: name }
    } else {
        PExpr::HasLabel { var, label: name }
    };
    Ok((i, out))
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

/// Parse a whole query. The public [`crate::parse`] wraps this and enforces
/// that all input was consumed.
pub fn query(i: &str) -> IResult<&str, Query> {
    let (i, _) = kw("match")(i)?;
    let (i, pattern) = pattern(i)?;
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
        Query {
            pattern,
            where_clause,
            ret: Return {
                distinct: distinct.is_some(),
                item,
            },
            order_by: order_by.unwrap_or_default(),
            skip,
            limit,
        },
    ))
}
