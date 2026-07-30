//! The parser-level AST. Deliberately *not* core's `LogicalPlan`: a Cypher
//! pattern binds several variables, but core's linear-pipeline row model only
//! ever sees a single "current node" (arch/03 §2). So property/label access
//! here keeps its variable qualifier (`p.year`, `q:Paper`); the compiler
//! ([`crate::compile`]) is what decides *where* in the pipeline each predicate
//! belongs and drops the qualifier when it lands on that variable's slot.

use dr_strange_core::Metric;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp};
use dr_strange_core::types::Dir;

/// A whole parsed query: a source (`MATCH` pattern or a `SEARCH` vector seed),
/// then `[WHERE …] RETURN … [ORDER BY …] [SKIP n] [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub source: QuerySource,
    pub where_clause: Option<PExpr>,
    pub ret: Return,
    pub order_by: Vec<OrderKey>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
}

/// Where the query's rows originate: a graph pattern (`MATCH`) or an indexed
/// vector search (`SEARCH`). Both bind their node variables the same way, so
/// the rest of the query (WHERE/RETURN/ORDER BY/…) is shared.
#[derive(Debug, Clone, PartialEq)]
pub enum QuerySource {
    Match(Pattern),
    Search(SearchClause),
}

/// A query-vector argument: either a literal vector (programmatic) or text to
/// embed server-side (the ergonomic default — no 1024-float client payload).
#[derive(Debug, Clone, PartialEq)]
pub enum VecArg {
    Vector(Vec<f32>),
    Text(String),
}

/// `SEARCH (v:Label) ON prop NEAR <query> [METRIC m] [TOPK k]` — the indexed
/// similarity seed (compiles to `Source::VectorTopK`). One node, no traversal
/// tail in this cut. `<query>` is `"text"` (embedded) or `[..]` (literal).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    pub node: NodePat,
    pub property: String,
    pub query: VecArg,
    pub metric: Metric,
    pub k: u64,
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
    // ---- scoring terms (vector search) ----
    /// The row's similarity score channel — `score()`.
    Score,
    /// Hops taken to reach the current node — `hops()`.
    Hops,
    /// `similarity(v.prop, <query>, metric)` — higher is closer.
    Similarity {
        var: String,
        property: String,
        query: VecArg,
        metric: Metric,
    },
    /// `distance(v.prop, <query>, metric)` — lower is closer.
    Distance {
        var: String,
        property: String,
        query: VecArg,
        metric: Metric,
    },
}
