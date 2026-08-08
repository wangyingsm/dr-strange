//! Expression language for `Filter`/`Sort` (arch/03 §2).
//!
//! `Expr` is serializable (it rides in plans over the wire — arch/00 §2 — and
//! is the target the v2 query language will parse into) rather than a Rust
//! closure. Evaluation is **total**: it never errors. An expression over a
//! missing property, a type mismatch, or an incomparable pair yields `Null`
//! or `false` rather than failing the whole query — so a `Filter` simply
//! excludes the row. This keeps soft-schema data (where any property may be
//! absent or differently typed per node) queryable without per-row error
//! handling.
//!
//! v0 evaluates against a single "current node" (the linear-pipeline row
//! model, arch/03 §2); edge-property and multi-variable access arrive with
//! the richer binding model later.

use serde::{Deserialize, Serialize};

use crate::storage::vector::Metric;
use crate::types::{NodeRecord, PropValue};

/// A predicate or scalar over the row's current node.
///
/// `#[non_exhaustive]`: the language keeps gaining terms (`key(n)`, the scoring
/// functions), so a downstream match must carry a wildcard arm and each
/// addition stays a minor release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Expr {
    /// The current node's value for a property key (`Null` if absent).
    Property(String),
    /// A constant.
    Literal(PropValue),
    /// True iff the current node carries `label`.
    HasLabel(String),
    /// The current node's caller-supplied external key as a `Str` (`Null` if
    /// the node was created without one). The query language's `key(n)`; an
    /// equality on it at the source compiles to a `SeekKeys` seek instead.
    ExternalKey,
    /// The row's similarity score channel as a `Float` (`Null` if the row
    /// carries no score — arch/03 §4.5 `score()`).
    Score,
    /// Hops taken to reach the current node (trail length) as an `Int`
    /// (arch/03 §4.5 `hops()`), for structural fusion terms like `1/hops`.
    Hops,
    /// Distance from the current node's vector `property` to `query` under
    /// `metric`, as a `Float` (`Null` if the property is absent or not a
    /// vector). arch/03 §4.5 `distance()`.
    Distance {
        property: String,
        query: Vec<f32>,
        metric: Metric,
    },
    /// Similarity (higher = closer) — the negation-monotone twin of
    /// `Distance`. arch/03 §4.5 `similarity()`.
    Similarity {
        property: String,
        query: Vec<f32>,
        metric: Metric,
    },
    /// True iff the inner expression evaluates to `Null`.
    IsNull(Box<Expr>),
    Not(Box<Expr>),
    Compare {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Logic {
        op: LogicOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Arith {
        op: ArithOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogicOp {
    And,
    Or,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

// ---- ergonomic construction ----------------------------------------------

/// A property reference: `p("year").ge(2020)`.
pub fn p(key: impl Into<String>) -> Expr {
    Expr::Property(key.into())
}

/// A literal: `lit("Alice")`, `lit(42)`.
pub fn lit(value: impl Into<PropValue>) -> Expr {
    Expr::Literal(value.into())
}

impl From<PropValue> for Expr {
    fn from(v: PropValue) -> Self {
        Expr::Literal(v)
    }
}
macro_rules! expr_from_literal {
    ($($t:ty),*) => {$(
        impl From<$t> for Expr {
            fn from(v: $t) -> Self { Expr::Literal(PropValue::from(v)) }
        }
    )*};
}
expr_from_literal!(bool, i32, i64, f64, &str, String);

// These builder methods (`eq`, `add`, ...) deliberately share names with std
// trait methods — they read as query DSL (`p("x").ge(2020)`, `p("n").add(1)`)
// and return `Expr`, not the trait's type. Inherent methods take precedence,
// so this shadows nothing at call sites.
#[allow(clippy::should_implement_trait)]
impl Expr {
    fn cmp(self, op: CmpOp, rhs: impl Into<Expr>) -> Expr {
        Expr::Compare {
            op,
            lhs: Box::new(self),
            rhs: Box::new(rhs.into()),
        }
    }
    pub fn eq(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Eq, rhs)
    }
    pub fn ne(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Ne, rhs)
    }
    pub fn lt(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Lt, rhs)
    }
    pub fn le(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Le, rhs)
    }
    pub fn gt(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Gt, rhs)
    }
    pub fn ge(self, rhs: impl Into<Expr>) -> Expr {
        self.cmp(CmpOp::Ge, rhs)
    }

    pub fn and(self, rhs: impl Into<Expr>) -> Expr {
        Expr::Logic {
            op: LogicOp::And,
            lhs: Box::new(self),
            rhs: Box::new(rhs.into()),
        }
    }
    pub fn or(self, rhs: impl Into<Expr>) -> Expr {
        Expr::Logic {
            op: LogicOp::Or,
            lhs: Box::new(self),
            rhs: Box::new(rhs.into()),
        }
    }
    pub fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }
    pub fn is_null(self) -> Expr {
        Expr::IsNull(Box::new(self))
    }

    fn arith(self, op: ArithOp, rhs: impl Into<Expr>) -> Expr {
        Expr::Arith {
            op,
            lhs: Box::new(self),
            rhs: Box::new(rhs.into()),
        }
    }
    pub fn add(self, rhs: impl Into<Expr>) -> Expr {
        self.arith(ArithOp::Add, rhs)
    }
    pub fn sub(self, rhs: impl Into<Expr>) -> Expr {
        self.arith(ArithOp::Sub, rhs)
    }
    pub fn mul(self, rhs: impl Into<Expr>) -> Expr {
        self.arith(ArithOp::Mul, rhs)
    }
    pub fn div(self, rhs: impl Into<Expr>) -> Expr {
        self.arith(ArithOp::Div, rhs)
    }
}

/// True iff the current node carries `label`.
pub fn has_label(label: impl Into<String>) -> Expr {
    Expr::HasLabel(label.into())
}

/// The current node's external key — the query language's `key(n)`.
pub fn external_key() -> Expr {
    Expr::ExternalKey
}

/// The row's similarity score channel (arch/03 §4.5).
pub fn score() -> Expr {
    Expr::Score
}

/// Hops taken to reach the current node (arch/03 §4.5).
pub fn hops() -> Expr {
    Expr::Hops
}

/// Distance from the current node's `property` vector to `query` (arch/03
/// §4.5) — smaller is closer.
pub fn distance(property: impl Into<String>, query: impl Into<Vec<f32>>, metric: Metric) -> Expr {
    Expr::Distance {
        property: property.into(),
        query: query.into(),
        metric,
    }
}

/// Similarity from the current node's `property` vector to `query` (arch/03
/// §4.5) — larger is closer.
pub fn similarity(property: impl Into<String>, query: impl Into<Vec<f32>>, metric: Metric) -> Expr {
    Expr::Similarity {
        property: property.into(),
        query: query.into(),
        metric,
    }
}

// ---- evaluation ----------------------------------------------------------

/// What an `Expr` is evaluated against: the current node (or `None` if the
/// row points at a missing node), the row's score channel, and its hop
/// count. The linear-pipeline row's world (arch/03 §2).
#[derive(Clone, Copy)]
pub struct EvalCtx<'a> {
    pub node: Option<&'a NodeRecord>,
    pub score: Option<f32>,
    pub hops: usize,
}

impl<'a> EvalCtx<'a> {
    /// Context for a bare node with no score/hops (filters over a plain scan,
    /// projection helpers, tests).
    pub fn node(node: Option<&'a NodeRecord>) -> Self {
        Self {
            node,
            score: None,
            hops: 0,
        }
    }

    fn vector_prop(&self, property: &str) -> Option<&[f32]> {
        match self.node?.properties.get(property).map(|p| &p.value) {
            Some(PropValue::Vector(v)) => Some(v),
            _ => None,
        }
    }
}

/// Evaluates `expr` against `ctx`. Total: returns a value, never an error.
pub fn eval(expr: &Expr, ctx: &EvalCtx) -> PropValue {
    match expr {
        Expr::Property(key) => ctx
            .node
            .and_then(|n| n.properties.get(key))
            .map(|p| p.value.clone())
            .unwrap_or(PropValue::Null),
        Expr::Literal(v) => v.clone(),
        Expr::HasLabel(label) => PropValue::Bool(
            ctx.node
                .is_some_and(|n| n.labels.iter().any(|l| l == label)),
        ),
        Expr::ExternalKey => ctx
            .node
            .and_then(|n| n.external_key.clone())
            .map(PropValue::Str)
            .unwrap_or(PropValue::Null),
        Expr::Score => ctx
            .score
            .map(|s| PropValue::Float(s as f64))
            .unwrap_or(PropValue::Null),
        Expr::Hops => PropValue::Int(ctx.hops as i64),
        Expr::Distance {
            property,
            query,
            metric,
        } => ctx
            .vector_prop(property)
            .map(|v| PropValue::Float(metric.distance(query, v) as f64))
            .unwrap_or(PropValue::Null),
        Expr::Similarity {
            property,
            query,
            metric,
        } => ctx
            .vector_prop(property)
            .map(|v| PropValue::Float(metric.similarity(query, v) as f64))
            .unwrap_or(PropValue::Null),
        Expr::IsNull(e) => PropValue::Bool(matches!(eval(e, ctx), PropValue::Null)),
        Expr::Not(e) => PropValue::Bool(!is_true(&eval(e, ctx))),
        Expr::Compare { op, lhs, rhs } => {
            PropValue::Bool(compare(*op, &eval(lhs, ctx), &eval(rhs, ctx)))
        }
        Expr::Logic { op, lhs, rhs } => {
            let a = is_true(&eval(lhs, ctx));
            // Short-circuit — also avoids evaluating the rhs needlessly.
            let v = match op {
                LogicOp::And => a && is_true(&eval(rhs, ctx)),
                LogicOp::Or => a || is_true(&eval(rhs, ctx)),
            };
            PropValue::Bool(v)
        }
        Expr::Arith { op, lhs, rhs } => arith(*op, &eval(lhs, ctx), &eval(rhs, ctx)),
    }
}

/// Truthiness for `Filter`/logic: only `Bool(true)` is true. Everything else
/// — including `Null`, non-bool values — is false.
pub fn is_true(v: &PropValue) -> bool {
    matches!(v, PropValue::Bool(true))
}

/// Ordering used by filter comparisons (`<`, `<=`, …). `None` for
/// incomparable pairs (mismatched types, `Null`, `NaN`) — a comparison
/// against those is simply false, the SQL-flavored posture. `Sort` must NOT
/// use this: see [`total_cmp`].
pub fn partial_cmp(a: &PropValue, b: &PropValue) -> Option<std::cmp::Ordering> {
    use PropValue::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Str(x), Str(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// A value's rank in the canonical sort order — mismatched types sort by
/// type, in a fixed order (cf. SQLite), never "equal".
fn type_rank(v: &PropValue) -> u8 {
    use PropValue::*;
    match v {
        Null => 0,
        Bool(_) => 1,
        Int(_) | Float(_) => 2, // one numeric line, compared cross-type
        Str(_) => 3,
        Bytes(_) => 4,
        Vector(_) => 5,
        List(_) => 6,
        Map(_) => 7,
    }
}

/// Exact `i64` vs `f64` comparison on the IEEE total-order line. The naive
/// `(x as f64).total_cmp(&y)` alone is not total: distinct ints beyond 2⁵³
/// round to the same f64 and would all compare Equal to it while ordering
/// among themselves — the intransitivity [`total_cmp`] exists to rule out.
fn cmp_int_float(x: i64, y: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rounded = (x as f64).total_cmp(&y);
    if rounded != Ordering::Equal {
        // Rounding moves x by less than one ulp, which cannot carry it across
        // a *different* f64 — a strict f64 verdict is the true verdict. NaN
        // lands here too (never Equal to the non-NaN rounded x).
        return rounded;
    }
    // Rounded-equal ⇒ y is integer-valued; settle exactly in i64. The one
    // value that saturates the cast is 2⁶³ itself, above every i64.
    if y >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    x.cmp(&(y as i64))
}

/// The canonical **total** order over property values — what `Sort` uses.
/// Unlike [`partial_cmp`] it never gives up: mismatched types order by
/// [`type_rank`], `Null` first; Int/Float share one exactly-compared numeric
/// line; floats follow IEEE `totalOrder` (NaN sorts past the infinities);
/// sequences and maps compare lexicographically. A sort comparator must be
/// genuinely total — `unwrap_or(Equal)` on the partial order is not (NaN
/// "equal" to 1.0 and 2.0 while 1.0 < 2.0), and std's sort detects such
/// inconsistency and panics.
pub fn total_cmp(a: &PropValue, b: &PropValue) -> std::cmp::Ordering {
    use PropValue::*;
    use std::cmp::Ordering;
    match (a, b) {
        (Int(x), Int(y)) => x.cmp(y),
        (Float(x), Float(y)) => x.total_cmp(y),
        (Int(x), Float(y)) => cmp_int_float(*x, *y),
        (Float(x), Int(y)) => cmp_int_float(*y, *x).reverse(),
        (Bool(x), Bool(y)) => x.cmp(y),
        (Str(x), Str(y)) => x.cmp(y),
        (Bytes(x), Bytes(y)) => x.cmp(y),
        (Vector(x), Vector(y)) => {
            for (p, q) in x.iter().zip(y) {
                let ord = p.total_cmp(q);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            x.len().cmp(&y.len())
        }
        (List(x), List(y)) => {
            for (p, q) in x.iter().zip(y) {
                let ord = total_cmp(p, q);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            x.len().cmp(&y.len())
        }
        (Map(x), Map(y)) => {
            // BTreeMap iterates key-sorted: lexicographic over (key, value,
            // description) is deterministic and total.
            for ((ka, pa), (kb, pb)) in x.iter().zip(y) {
                let ord = ka
                    .cmp(kb)
                    .then_with(|| total_cmp(&pa.value, &pb.value))
                    .then_with(|| pa.description.cmp(&pb.description));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            x.len().cmp(&y.len())
        }
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

fn values_eq(a: &PropValue, b: &PropValue) -> bool {
    use PropValue::*;
    match (a, b) {
        // numeric equality across Int/Float
        (Int(_), _) | (Float(_), _) => partial_cmp(a, b) == Some(std::cmp::Ordering::Equal),
        // everything else: structural equality (Null==Null, Str, Bytes,
        // Vector, List, Map). NaN inside a Vector/List won't be equal — fine.
        _ => a == b,
    }
}

fn compare(op: CmpOp, a: &PropValue, b: &PropValue) -> bool {
    use std::cmp::Ordering;
    match op {
        CmpOp::Eq => values_eq(a, b),
        CmpOp::Ne => !values_eq(a, b),
        CmpOp::Lt => partial_cmp(a, b) == Some(Ordering::Less),
        CmpOp::Le => matches!(partial_cmp(a, b), Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => partial_cmp(a, b) == Some(Ordering::Greater),
        CmpOp::Ge => matches!(partial_cmp(a, b), Some(Ordering::Greater | Ordering::Equal)),
    }
}

fn arith(op: ArithOp, a: &PropValue, b: &PropValue) -> PropValue {
    use PropValue::*;
    match (a, b) {
        (Int(x), Int(y)) => match op {
            ArithOp::Add => Int(x.wrapping_add(*y)),
            ArithOp::Sub => Int(x.wrapping_sub(*y)),
            ArithOp::Mul => Int(x.wrapping_mul(*y)),
            // integer division by zero is Null, not a panic
            ArithOp::Div if *y == 0 => Null,
            ArithOp::Div => Int(x.wrapping_div(*y)),
        },
        // any float operand → float arithmetic (inf/NaN allowed, stays total)
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let x = as_f64(a);
            let y = as_f64(b);
            match op {
                ArithOp::Add => Float(x + y),
                ArithOp::Sub => Float(x - y),
                ArithOp::Mul => Float(x * y),
                ArithOp::Div => Float(x / y),
            }
        }
        // non-numeric operand → Null (comparisons against it then fail)
        _ => Null,
    }
}

fn as_f64(v: &PropValue) -> f64 {
    match v {
        PropValue::Int(i) => *i as f64,
        PropValue::Float(f) => *f,
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PlaneId, PropDesc};

    fn node(labels: &[&str], props: &[(&str, PropValue)]) -> NodeRecord {
        NodeRecord {
            id: crate::types::NodeId(1),
            plane: PlaneId::STARTUP,
            external_key: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
                .collect(),
        }
    }

    fn ev(expr: &Expr, node: Option<&NodeRecord>) -> PropValue {
        eval(expr, &EvalCtx::node(node))
    }

    fn b(expr: &Expr, n: &NodeRecord) -> bool {
        is_true(&ev(expr, Some(n)))
    }

    /// `total_cmp` must be a genuine total order — std's sort panics on a
    /// comparator that isn't. Verified the brute-force way: antisymmetry and
    /// transitivity over every pair/triple of a value set chosen to hit the
    /// past failure modes (NaN, mixed types, ints past 2⁵³ that round to the
    /// same f64, negative NaN, nested values).
    #[test]
    fn total_cmp_is_a_total_order() {
        use std::cmp::Ordering;
        let vals = [
            PropValue::Null,
            PropValue::Bool(false),
            PropValue::Bool(true),
            PropValue::Int(i64::MIN),
            PropValue::Int(-1),
            PropValue::Int(0),
            PropValue::Int(9_007_199_254_740_992), // 2^53
            PropValue::Int(9_007_199_254_740_993), // 2^53+1: rounds to 2^53
            PropValue::Int(i64::MAX),
            PropValue::Float(f64::NEG_INFINITY),
            PropValue::Float(-f64::NAN),
            PropValue::Float(-0.0),
            PropValue::Float(0.0),
            PropValue::Float(1.5),
            PropValue::Float(9_007_199_254_740_992.0), // == 2^53
            PropValue::Float(9.3e18),                  // > i64::MAX, saturating-cast territory
            PropValue::Float(f64::INFINITY),
            PropValue::Float(f64::NAN),
            PropValue::Str("a".into()),
            PropValue::Str("b".into()),
            PropValue::Bytes(vec![1]),
            PropValue::Vector(vec![f32::NAN, 1.0]),
            PropValue::List(vec![PropValue::Int(1)]),
            PropValue::List(vec![PropValue::Int(1), PropValue::Null]),
        ];
        for a in &vals {
            assert_eq!(total_cmp(a, a), Ordering::Equal);
            for b in &vals {
                assert_eq!(total_cmp(a, b), total_cmp(b, a).reverse());
                for c in &vals {
                    // transitivity: a<=b and b<=c ⇒ a<=c
                    if total_cmp(a, b) != Ordering::Greater && total_cmp(b, c) != Ordering::Greater
                    {
                        assert_ne!(
                            total_cmp(a, c),
                            Ordering::Greater,
                            "intransitive: {a:?} <= {b:?} <= {c:?} but {a:?} > {c:?}"
                        );
                    }
                }
            }
        }
    }

    /// The 2⁵³ trap concretely: both big ints round to the same f64, so a
    /// rounded comparison would call each Equal to it — while the ints order
    /// among themselves. The exact comparison keeps all three consistent.
    #[test]
    fn total_cmp_is_exact_past_f64_precision() {
        use std::cmp::Ordering;
        let f = PropValue::Float(9_007_199_254_740_992.0);
        let lo = PropValue::Int(9_007_199_254_740_992);
        let hi = PropValue::Int(9_007_199_254_740_993);
        assert_eq!(total_cmp(&lo, &f), Ordering::Equal);
        assert_eq!(total_cmp(&hi, &f), Ordering::Greater);
        assert_eq!(total_cmp(&lo, &hi), Ordering::Less);
    }

    #[test]
    fn property_and_missing_property() {
        let n = node(&[], &[("year", PropValue::Int(2020))]);
        assert_eq!(ev(&p("year"), Some(&n)), PropValue::Int(2020));
        assert_eq!(ev(&p("absent"), Some(&n)), PropValue::Null);
        assert_eq!(ev(&p("year"), None), PropValue::Null);
    }

    #[test]
    fn comparisons_with_numeric_coercion() {
        let n = node(&[], &[("year", PropValue::Int(2020))]);
        assert!(b(&p("year").ge(2020), &n));
        assert!(b(&p("year").ge(lit(2019.5)), &n)); // Int vs Float
        assert!(!b(&p("year").gt(2020), &n));
        assert!(b(&p("year").lt(3000), &n));
        assert!(b(&p("year").eq(2020), &n));
        assert!(b(&p("year").ne(1999), &n));
    }

    #[test]
    fn incomparable_and_missing_are_false_not_error() {
        let n = node(&[], &[("name", PropValue::Str("Alice".into()))]);
        // Str vs Int → incomparable → every ordering comparison is false
        assert!(!b(&p("name").gt(5), &n));
        assert!(!b(&p("name").lt(5), &n));
        assert!(!b(&p("name").eq(5), &n));
        assert!(b(&p("name").ne(5), &n)); // not-equal of incomparable is true
        // missing property compared → false
        assert!(!b(&p("missing").ge(0), &n));
    }

    #[test]
    fn null_equality() {
        let n = node(&[], &[]);
        assert!(b(&p("missing").eq(lit(PropValue::Null)), &n));
        assert!(b(&p("missing").is_null(), &n));
    }

    #[test]
    fn boolean_logic_short_circuits() {
        let n = node(&[], &[("a", PropValue::Int(1)), ("b", PropValue::Int(2))]);
        assert!(b(&p("a").eq(1).and(p("b").eq(2)), &n));
        assert!(!b(&p("a").eq(1).and(p("b").eq(99)), &n));
        assert!(b(&p("a").eq(99).or(p("b").eq(2)), &n));
        assert!(b(&p("a").eq(1).not().not(), &n));
        assert!(!b(&p("a").eq(1).not(), &n));
    }

    #[test]
    fn has_label_matches() {
        let n = node(&["Person", "Author"], &[]);
        assert!(b(&has_label("Person"), &n));
        assert!(!b(&has_label("Paper"), &n));
        assert!(!is_true(&ev(&has_label("Person"), None)));
    }

    #[test]
    fn arithmetic() {
        let n = node(&[], &[("x", PropValue::Int(10))]);
        assert_eq!(ev(&p("x").add(5), Some(&n)), PropValue::Int(15));
        assert_eq!(ev(&p("x").mul(3), Some(&n)), PropValue::Int(30));
        assert_eq!(ev(&p("x").div(0), Some(&n)), PropValue::Null);
        // float contaminates to float
        assert_eq!(ev(&p("x").add(lit(0.5)), Some(&n)), PropValue::Float(10.5));
        // arithmetic on non-numeric → Null
        assert_eq!(ev(&lit("s").add(1), Some(&n)), PropValue::Null);
        // and a computed comparison
        assert!(b(&p("x").add(5).ge(15), &n));
    }

    #[test]
    fn nan_never_compares_true() {
        let n = node(&[], &[("f", PropValue::Float(f64::NAN))]);
        assert!(!b(&p("f").eq(lit(f64::NAN)), &n));
        assert!(!b(&p("f").lt(0.0), &n));
        assert!(!b(&p("f").ge(0.0), &n));
    }

    #[test]
    fn expr_serde_roundtrip() {
        let e = p("year")
            .ge(2020)
            .and(has_label("Paper"))
            .or(p("x").is_null());
        let json = serde_json::to_string(&e).unwrap();
        let back: Expr = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn score_and_hops_read_the_row_channel() {
        let n = node(&[], &[]);
        let ctx = EvalCtx {
            node: Some(&n),
            score: Some(0.75),
            hops: 2,
        };
        assert_eq!(eval(&score(), &ctx), PropValue::Float(0.75));
        assert_eq!(eval(&hops(), &ctx), PropValue::Int(2));
        // score is Null when the row carries none
        let none = EvalCtx {
            node: Some(&n),
            score: None,
            hops: 0,
        };
        assert_eq!(eval(&score(), &none), PropValue::Null);
    }

    #[test]
    fn distance_and_similarity_over_a_vector_property() {
        let n = node(&[], &[("emb", PropValue::Vector(vec![1.0, 0.0]))]);
        let ctx = EvalCtx::node(Some(&n));
        // identical direction → cosine distance ~0, similarity ~1
        let d = eval(&distance("emb", vec![1.0, 0.0], Metric::Cosine), &ctx);
        let s = eval(&similarity("emb", vec![1.0, 0.0], Metric::Cosine), &ctx);
        assert!(matches!(d, PropValue::Float(x) if x.abs() < 1e-6));
        assert!(matches!(s, PropValue::Float(x) if (x - 1.0).abs() < 1e-6));
        // missing / non-vector property → Null
        assert_eq!(
            eval(&distance("absent", vec![1.0], Metric::L2), &ctx),
            PropValue::Null
        );
    }

    #[test]
    fn linear_fusion_of_score_and_hops() {
        // 0.7*score + 0.3/hops, a canonical GraphRAG rank (arch/03 §4.5)
        let n = node(&[], &[]);
        let ctx = EvalCtx {
            node: Some(&n),
            score: Some(1.0),
            hops: 2,
        };
        let fused = score().mul(lit(0.7)).add(lit(0.3).div(hops()));
        // 0.7*1.0 + 0.3/2 = 0.85
        match eval(&fused, &ctx) {
            PropValue::Float(v) => assert!((v - 0.85).abs() < 1e-6, "got {v}"),
            other => panic!("expected float, got {other:?}"),
        }
    }
}
