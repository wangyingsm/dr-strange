//! The parser-level AST. Deliberately *not* core's `LogicalPlan`: a Cypher
//! pattern binds several variables, but core's linear-pipeline row model only
//! ever sees a single "current node" (arch/03 §2). So property/label access
//! here keeps its variable qualifier (`p.year`, `q:Paper`); the compiler
//! ([`crate::compile`]) is what decides *where* in the pipeline each predicate
//! belongs and drops the qualifier when it lands on that variable's slot.

use dr_strange_core::Metric;
use dr_strange_core::PropValue;
use dr_strange_core::compute::expr::{ArithOp, CmpOp, LogicOp, StrOp};
use dr_strange_core::types::Dir;

/// A whole parsed query: a source (a `MATCH` pattern or one of the retrieval
/// seeds), zero or more `BEAM` hops, then `[WHERE …] RETURN … [ORDER BY …]
/// [SKIP n] [LIMIT n] [AS OF …]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub source: QuerySource,
    pub beams: Vec<BeamClause>,
    pub where_clause: Option<PExpr>,
    pub ret: Return,
    pub order_by: Vec<OrderKey>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
    /// `AS OF <seq|"timestamp">` — read a past snapshot. Not part of the
    /// compiled plan: it addresses the *plane handle*, so it rides beside it.
    pub as_of: Option<AsOfSpec>,
}

/// The point `AS OF` pins reads to. A bare integer is a commit sequence; a
/// quoted RFC-3339 instant and `TIME <ms>` are both wall-clock addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsOfSpec {
    Seq(u64),
    /// Unix-epoch milliseconds.
    Time(i64),
}

/// `BEAM (result[:Label]) <OUT|IN|BOTH> [:TYPE] [ON prop] NEAR <query>
/// [METRIC m] WIDTH w DEPTH d` — similarity-guided beam traversal from the
/// current frontier (compiles to `Step::ExpandBeam`), binding `result` as the
/// new current node.
#[derive(Debug, Clone, PartialEq)]
pub struct BeamClause {
    pub node: NodePat,
    pub dir: Dir,
    pub edge_type: Option<String>,
    /// `None` ⇒ the conventional embedding property (see [`crate::compile`]).
    pub property: Option<String>,
    pub query: VecArg,
    pub metric: Metric,
    pub width: u32,
    pub depth: u32,
}

/// Where the query's rows originate. Every form binds exactly one node pattern
/// (`first`) and may continue with a relationship tail (`rest`) — so a typed
/// hop can follow a retrieval seed just as it follows a `MATCH` node, and the
/// rest of the query (WHERE/RETURN/ORDER BY/…) is shared by all of them.
#[derive(Debug, Clone, PartialEq)]
pub struct QuerySource {
    pub kind: SourceKind,
    pub first: NodePat,
    pub rest: Vec<(RelPat, NodePat)>,
}

/// What seeds a query's rows.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    /// `MATCH (a:Label)…` — a scan of the first node's label.
    Match,
    /// `SEARCH (v:Label) [ON prop] NEAR "text"|[..] [METRIC m] [TOPK k]` — the
    /// indexed vector seed (`Source::VectorTopK`).
    Search {
        /// `None` ⇒ the conventional embedding property (see [`crate::compile`]).
        property: Option<String>,
        query: VecArg,
        metric: Metric,
        k: u64,
    },
    /// `SEARCH (v:Label) ON prop MATCHING "text" [TOPK k]` — the BM25 keyword
    /// seed (`Source::KeywordTopK`). `ON` is required here: keyword properties
    /// follow no convention, so there is nothing sound to default to.
    Keyword {
        property: Option<String>,
        query: String,
        k: u64,
    },
    /// `HYBRID (v:Label) …` — fused retrieval (`Source::Hybrid`).
    Hybrid(HybridClause),
    /// `CALL name(args) ON (v:Label)` — a graph algorithm (`Source::Algo`).
    Call(CallClause),
}

/// `HYBRID (v:Label) [VECTOR ON p NEAR q [METRIC m] [WEIGHT w]]
/// [KEYWORD ON p MATCHING "text" [WEIGHT w]]
/// [GRAPH HOPS h DECAY d [SEEDS n] [WEIGHT w]] [CANDIDATES n] [TOPK k]` —
/// every channel optional, but at least one required.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridClause {
    pub vector: Option<HybridVector>,
    pub keyword: Option<HybridKeyword>,
    pub graph: Option<HybridGraph>,
    pub candidates: Option<u64>,
    pub k: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HybridVector {
    /// `None` ⇒ the conventional embedding property (see [`crate::compile`]).
    pub property: Option<String>,
    pub query: VecArg,
    pub metric: Metric,
    pub weight: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HybridKeyword {
    /// Required in practice — the compiler rejects `None` with a clear error,
    /// since keyword properties follow no convention to default to.
    pub property: Option<String>,
    pub query: String,
    pub weight: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HybridGraph {
    pub hops: u32,
    /// `None` ⇒ the same default the RPC/MCP/CLI surfaces use (see
    /// [`crate::compile`]).
    pub decay: Option<f32>,
    pub seeds: Option<u64>,
    pub weight: Option<f32>,
}

/// `CALL name(arg: value, …)` — an algorithm invocation. The compiler checks
/// the name and its arguments; the grammar accepts any named-argument list so
/// a typo reports as an unknown algorithm/argument rather than a parse error.
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    pub name: String,
    pub args: Vec<(String, Val)>,
}

/// A query-vector argument: either a literal vector (programmatic) or text to
/// embed server-side (the ergonomic default — no 1024-float client payload).
#[derive(Debug, Clone, PartialEq)]
pub enum VecArg {
    Vector(Vec<f32>),
    Text(String),
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
    /// A `$name` parameter placeholder used as a value — resolved from the
    /// caller's params at parse time.
    Param(String),
    Prop {
        var: String,
        key: String,
    },
    HasLabel {
        var: String,
        label: String,
    },
    /// `key(v)` — the node's external key. An equality (or `IN`) on the
    /// source variable compiles to a `SeekKeys` seek rather than a filter.
    ExternalKey {
        var: String,
    },
    /// `x IN [a, b, …]` — sugar for a chain of equalities.
    In {
        lhs: Box<PExpr>,
        list: Vec<PExpr>,
    },
    /// `x IN <expr>` where the right side is not a literal list — membership
    /// in a value only known per row (a `List` property, or a `Map`'s keys).
    /// Kept apart from [`PExpr::In`] because that one is sugar the compiler
    /// expands into equalities (and, on `key(n)`, into a seek); this one
    /// cannot be, since the haystack isn't known until the row is.
    InValue {
        lhs: Box<PExpr>,
        haystack: Box<PExpr>,
    },
    /// `a CONTAINS b`, `a STARTS WITH b`, `a ENDS WITH b`.
    StringMatch {
        op: StrOp,
        lhs: Box<PExpr>,
        rhs: Box<PExpr>,
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

// ---- write statements -----------------------------------------------------

/// A parsed top-level statement: a read (compiles to a `LogicalPlan`) or a
/// write (executed against a `WriteTxn`).
#[derive(Debug, Clone, PartialEq)]
pub enum StmtAst {
    // Boxed: a read `Query` is far larger than a `WriteAst`.
    Read(Box<Query>),
    Write(WriteAst),
}

/// A write statement: either a standalone `CREATE`, or a `MATCH … [WHERE …]`
/// that binds the terminal node variable, followed by mutation ops that operate
/// on it (`SET`/`REMOVE`/`DELETE`).
#[derive(Debug, Clone, PartialEq)]
pub struct WriteAst {
    pub match_clause: Option<MatchClause>,
    pub ops: Vec<WriteOp>,
}

/// The read half of a `MATCH … [WHERE …] SET/REMOVE/DELETE` — a pattern to find
/// nodes to mutate. Compiled to a read `LogicalPlan` whose terminal node is the
/// bound variable.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub pattern: Pattern,
    pub where_clause: Option<PExpr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    /// `CREATE (n:L {..}), (a)-[:T {..}]->(b), …`
    Create(Vec<CreatePath>),
    /// `MERGE (n:L {key:"k", ..}) [ON CREATE SET …] [ON MATCH SET …]`
    Merge(MergeClause),
    /// `SET n.p = v, n:Label, n += {..}`
    Set(Vec<SetItem>),
    /// `REMOVE n.p, n:Label`
    Remove(Vec<RemoveItem>),
    /// `[DETACH] DELETE n, m, …`
    Delete { detach: bool, vars: Vec<String> },
}

/// `MERGE (a {key}) [ON CREATE SET …] [ON MATCH SET …]` — upsert one node by
/// its external key — or `MERGE (a {key})-[:T]->(b {key})` — upsert each keyed
/// node and ensure the edge (element-wise, idempotent). `path` reuses the CREATE
/// path shape; `on_create`/`on_match` apply only to a single-node MERGE.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub path: CreatePath,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
}

/// A property value: a literal, or a `$name` parameter placeholder resolved
/// from the caller's params at parse time.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Lit(PropValue),
    Param(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.key = value`
    Prop {
        var: String,
        key: String,
        value: Val,
    },
    /// `n:Label` — add a label.
    Label { var: String, label: String },
    /// `n += { .. }` — merge properties.
    Merge {
        var: String,
        props: Vec<(String, Val)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `n.key` — remove a property.
    Prop { var: String, key: String },
    /// `n:Label` — remove a label.
    Label { var: String, label: String },
}

/// One CREATE path: a node, then directed `[:TYPE {..}]` hops to more nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePath {
    pub first: CreateNode,
    pub rest: Vec<(CreateRel, CreateNode)>,
}

/// A node in a CREATE: optional variable (to reference within the statement),
/// optional label, optional external `key` (from an inline `key: "…"`), and the
/// remaining inline properties.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateNode {
    pub var: Option<String>,
    pub label: Option<String>,
    pub key: Option<String>,
    pub props: Vec<(String, Val)>,
}

/// A relationship in a CREATE: a required type, a direction (`->`/`<-`; never
/// undirected — dr-strange edges are directed), and inline properties.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRel {
    pub dir: Dir,
    pub ty: String,
    pub props: Vec<(String, Val)>,
}
