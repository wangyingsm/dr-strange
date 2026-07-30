//! The parser-level AST. Deliberately *not* core's `LogicalPlan`: a Cypher
//! pattern binds several variables, but core's linear-pipeline row model only
//! ever sees a single "current node" (arch/03 §2). So property/label access
//! here keeps its variable qualifier (`p.year`, `q:Paper`); the compiler
//! ([`crate::compile`]) is what decides *where* in the pipeline each predicate
//! belongs and drops the qualifier when it lands on that variable's slot.

use dr_strange_core::Dir;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};

/// A whole parsed query: `MATCH … [WHERE …] RETURN … [ORDER BY …] [SKIP n] [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub pattern: Pattern,
    pub where_clause: Option<PExpr>,
    pub ret: Return,
    pub order_by: Vec<OrderKey>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// A single linear path: one node, then zero or more `(relationship, node)`
/// hops. Branching patterns are not supported in this cut.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub first: NodePat,
    pub rest: Vec<(RelPat, NodePat)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePat {
    pub var: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPat {
    pub dir: Dir,
    pub ty: Option<String>,
    /// `Some` for a variable-length relationship (`*`, `*n`, `*n..m`, `*..m`).
    pub var_len: Option<VarLen>,
}

/// Variable-length bounds. `max = None` is an unbounded `*` / `*n..`, which the
/// compiler rejects (core's `ExpandVar` needs a concrete upper bound).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VarLen {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Return {
    pub distinct: bool,
    pub item: ReturnItem,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReturnItem {
    Star,
    Var(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: PExpr,
    pub descending: bool,
}

/// A parsed expression. Mirrors core's `Expr` but with variable-qualified
/// property/label access so the compiler can attribute each term to a pattern
/// variable. Reuses core's operator enums so compilation is a plain 1:1 map.
#[derive(Debug, Clone, PartialEq)]
pub enum PExpr {
    Lit(PropValue),
    Prop {
        var: String,
        key: String,
    },
    HasLabel {
        var: String,
        label: String,
    },
    IsNull(Box<PExpr>),
    Not(Box<PExpr>),
    Neg(Box<PExpr>),
    Compare {
        op: CmpOp,
        lhs: Box<PExpr>,
        rhs: Box<PExpr>,
    },
    Logic {
        op: LogicOp,
        lhs: Box<PExpr>,
        rhs: Box<PExpr>,
    },
    Arith {
        op: ArithOp,
        lhs: Box<PExpr>,
        rhs: Box<PExpr>,
    },
}
