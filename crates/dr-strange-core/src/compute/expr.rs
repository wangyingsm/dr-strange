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

use crate::types::{NodeRecord, PropValue};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// The current node's value for a property key (`Null` if absent).
    Property(String),
    /// A constant.
    Literal(PropValue),
    /// True iff the current node carries `label`.
    HasLabel(String),
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

// ---- evaluation ----------------------------------------------------------

/// Evaluates `expr` against `node` (the current row's node, `None` if the row
/// points at a missing node). Total: returns a value, never an error.
pub fn eval(expr: &Expr, node: Option<&NodeRecord>) -> PropValue {
    match expr {
        Expr::Property(key) => node
            .and_then(|n| n.properties.get(key))
            .map(|p| p.value.clone())
            .unwrap_or(PropValue::Null),
        Expr::Literal(v) => v.clone(),
        Expr::HasLabel(label) => {
            PropValue::Bool(node.is_some_and(|n| n.labels.iter().any(|l| l == label)))
        }
        Expr::IsNull(e) => PropValue::Bool(matches!(eval(e, node), PropValue::Null)),
        Expr::Not(e) => PropValue::Bool(!is_true(&eval(e, node))),
        Expr::Compare { op, lhs, rhs } => {
            PropValue::Bool(compare(*op, &eval(lhs, node), &eval(rhs, node)))
        }
        Expr::Logic { op, lhs, rhs } => {
            let a = is_true(&eval(lhs, node));
            // Short-circuit — also avoids evaluating the rhs needlessly.
            let v = match op {
                LogicOp::And => a && is_true(&eval(rhs, node)),
                LogicOp::Or => a || is_true(&eval(rhs, node)),
            };
            PropValue::Bool(v)
        }
        Expr::Arith { op, lhs, rhs } => arith(*op, &eval(lhs, node), &eval(rhs, node)),
    }
}

/// Truthiness for `Filter`/logic: only `Bool(true)` is true. Everything else
/// — including `Null`, non-bool values — is false.
pub fn is_true(v: &PropValue) -> bool {
    matches!(v, PropValue::Bool(true))
}

/// Total ordering used by comparisons and `Sort`. `None` for incomparable
/// pairs (mismatched types, `Null`, `NaN`).
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

    fn b(expr: &Expr, n: &NodeRecord) -> bool {
        is_true(&eval(expr, Some(n)))
    }

    #[test]
    fn property_and_missing_property() {
        let n = node(&[], &[("year", PropValue::Int(2020))]);
        assert_eq!(eval(&p("year"), Some(&n)), PropValue::Int(2020));
        assert_eq!(eval(&p("absent"), Some(&n)), PropValue::Null);
        assert_eq!(eval(&p("year"), None), PropValue::Null);
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
        assert!(!is_true(&eval(&has_label("Person"), None)));
    }

    #[test]
    fn arithmetic() {
        let n = node(&[], &[("x", PropValue::Int(10))]);
        assert_eq!(eval(&p("x").add(5), Some(&n)), PropValue::Int(15));
        assert_eq!(eval(&p("x").mul(3), Some(&n)), PropValue::Int(30));
        assert_eq!(eval(&p("x").div(0), Some(&n)), PropValue::Null);
        // float contaminates to float
        assert_eq!(
            eval(&p("x").add(lit(0.5)), Some(&n)),
            PropValue::Float(10.5)
        );
        // arithmetic on non-numeric → Null
        assert_eq!(eval(&lit("s").add(1), Some(&n)), PropValue::Null);
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
}
