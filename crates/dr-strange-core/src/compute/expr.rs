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
//! Evaluation addresses the row's **current node** by default, and any earlier
//! pattern binding through [`Expr::At`]. The linear pipeline never stopped
//! carrying those bindings — a row is a current node plus the trail of hops
//! that reached it (arch/03 §2), so the node at pattern slot *i* and the edge
//! of hop *i* are already in every row. `At` is the term that names them; the
//! executor resolves only the slots a plan actually mentions.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::storage::vector::Metric;
use crate::types::{Dir, EdgeRecord, NodeRecord, PropValue};

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
    /// The current node's internal id as an `Int` — the query language's
    /// `id(n)`. Unlike [`Expr::ExternalKey`] every node has one, so this is
    /// what makes a node usable as a grouping key when it carries no
    /// projectable property.
    NodeId,
    /// Evaluate `inner` against an earlier binding of the row instead of its
    /// current node — the query language's `a.name` where `a` is not the
    /// pattern's last variable.
    ///
    /// A [`Binding::Node`] swaps the node `inner` reads; a [`Binding::Edge`]
    /// swaps the edge, so `At { Edge(0), EdgeType }` is "the type of the first
    /// hop's relationship". Nests: the swap applies to the whole subtree.
    /// Total like everything else here — an out-of-range or unresolved binding
    /// makes `inner` read against nothing, which yields `Null`.
    At {
        binding: Binding,
        inner: Box<Expr>,
    },
    /// The traversed edge's type name as a `Str` — `type(r)`. Bare, it is the
    /// edge the row arrived on; wrapped in [`Expr::At`] it is that hop's.
    EdgeType,
    /// Which way the row actually traversed the edge — `"OUT"` or `"IN"` —
    /// the query language's `direction(r)`.
    ///
    /// Never `"BOTH"`: `Both` is what the *query* asked for, while a row
    /// always took one concrete direction. Grouping a `-[r]-` pattern by
    /// direction therefore yields the two concrete groups, which is the
    /// question worth asking. The one indistinguishable case is a self-loop,
    /// where both readings are true and the row reports `"OUT"`.
    EdgeDir,
    /// The traversed edge's value for a property key (`Null` if absent) —
    /// `r.weight`.
    EdgeProperty(String),
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
    /// Substring predicate over two strings — the query language's
    /// `CONTAINS` / `STARTS WITH` / `ENDS WITH`.
    ///
    /// Total, like [`Expr::Compare`]: an operand that is missing or not a
    /// string makes the predicate *false* rather than an error, so a filter
    /// over a heterogeneous plane simply doesn't match instead of failing the
    /// whole query.
    StringMatch {
        op: StrOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Membership — the query language's `needle IN haystack`.
    ///
    /// Deliberately *not* folded into [`StrOp::Contains`]. For a list of
    /// strings, "contains" could mean either "has this element" or "some
    /// element has this substring", and nothing in the syntax picks one; a
    /// silently-wrong row set is worse than two operators. openCypher splits
    /// them the same way, so a query written against it means the same thing
    /// here.
    ///
    /// A `List` haystack tests elements by the same equality `=` uses, so
    /// `1 IN [1.0]` holds. A `Map` haystack tests **keys** — the values side
    /// is what `IN` would be ambiguous about, which is why `HashMap` has
    /// `contains_key` and no bare `contains`. Anything else is false.
    In {
        needle: Box<Expr>,
        haystack: Box<Expr>,
    },
}

/// Which of a row's bindings an [`Expr::At`] addresses.
///
/// Both index the pattern in path order, the same order the compiler already
/// assigns slots in when it pushes filters down: node 0 is the source, node
/// *i* is current right after the *i*-th hop, and edge *i* is the relationship
/// of that hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    /// The node at a pattern slot; 0 is the source.
    Node(u32),
    /// The relationship of a hop; 0 is the first.
    Edge(u32),
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

/// Which substring relation [`Expr::StringMatch`] tests. Byte-wise, so
/// matching is exact rather than Unicode-normalised or case-folded — the same
/// posture as equality, which does not case-fold either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrOp {
    Contains,
    StartsWith,
    EndsWith,
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

    fn string_match(self, op: StrOp, rhs: impl Into<Expr>) -> Expr {
        Expr::StringMatch {
            op,
            lhs: Box::new(self),
            rhs: Box::new(rhs.into()),
        }
    }
    /// `p("title").contains("graph")`.
    pub fn contains(self, rhs: impl Into<Expr>) -> Expr {
        self.string_match(StrOp::Contains, rhs)
    }
    /// `p("name").starts_with("Al")`.
    pub fn starts_with(self, rhs: impl Into<Expr>) -> Expr {
        self.string_match(StrOp::StartsWith, rhs)
    }
    /// `p("file").ends_with(".pdf")`.
    pub fn ends_with(self, rhs: impl Into<Expr>) -> Expr {
        self.string_match(StrOp::EndsWith, rhs)
    }

    /// `p("tag").is_in(lit(PropValue::List(vec![...])))` — the language's
    /// `IN`. Named `is_in` because `in` is a keyword.
    pub fn is_in(self, haystack: impl Into<Expr>) -> Expr {
        Expr::In {
            needle: Box::new(self),
            haystack: Box::new(haystack.into()),
        }
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

/// The current node's internal id — the query language's `id(n)`.
pub fn node_id() -> Expr {
    Expr::NodeId
}

/// The traversed edge's type name — `type(r)`.
pub fn edge_type() -> Expr {
    Expr::EdgeType
}

/// Which way the row traversed the edge, `"OUT"` or `"IN"` — `direction(r)`.
pub fn edge_dir() -> Expr {
    Expr::EdgeDir
}

/// An edge property reference: `ep("weight").gt(0.5)`.
pub fn ep(key: impl Into<String>) -> Expr {
    Expr::EdgeProperty(key.into())
}

/// Read `inner` against the node bound at pattern slot `slot`:
/// `at_node(0, p("name"))` is the source node's name.
pub fn at_node(slot: u32, inner: Expr) -> Expr {
    Expr::At {
        binding: Binding::Node(slot),
        inner: Box::new(inner),
    }
}

/// Read `inner` against the relationship of hop `hop`:
/// `at_edge(0, edge_type())` is the first hop's edge type.
pub fn at_edge(hop: u32, inner: Expr) -> Expr {
    Expr::At {
        binding: Binding::Edge(hop),
        inner: Box::new(inner),
    }
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

/// A row's resolved pattern bindings — what [`Expr::At`] addresses.
///
/// Sparse on purpose: the executor resolves only the slots the plan actually
/// mentions (see `plan::referenced_bindings`), so a query that never names an
/// earlier variable pays nothing. An index the plan didn't ask for, or a hop
/// the row never took, reads as `None` and evaluates to `Null`.
#[derive(Clone, Copy, Default)]
pub struct Bindings<'a> {
    /// Node per pattern slot, slot 0 being the source.
    pub nodes: &'a [Option<Arc<NodeRecord>>],
    /// Relationship per hop, with the direction the row actually traversed it.
    pub edges: &'a [Option<(Arc<EdgeRecord>, Dir)>],
    /// The edge the row arrived on — what a bare [`Expr::EdgeType`] reads.
    pub edge: Option<(&'a EdgeRecord, Dir)>,
}

impl<'a> Bindings<'a> {
    fn node(self, slot: u32) -> Option<&'a NodeRecord> {
        // `self` by value so the `'a` in the field survives: reborrowing
        // through `&self` would shorten it to this call.
        self.nodes.get(slot as usize)?.as_deref()
    }

    fn edge_at(self, hop: u32) -> Option<(&'a EdgeRecord, Dir)> {
        let (edge, dir) = self.edges.get(hop as usize)?.as_ref()?;
        Some((edge, *dir))
    }
}

/// Which bindings a plan's expressions actually name.
///
/// High-water marks rather than a set, because a pattern's slots are dense:
/// naming slot 3 means the walk that reaches it already passed through 0..3,
/// so resolving up to the maximum costs nothing extra and keeps the resolved
/// vectors directly indexable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BindingNeed {
    /// Highest node slot named, if any.
    pub nodes: Option<u32>,
    /// Highest hop named, if any.
    pub edges: Option<u32>,
    /// A bare edge term — the edge the row arrived on.
    pub last_edge: bool,
}

impl BindingNeed {
    /// Whether anything needs resolving at all. The common query names no
    /// binding, and then a row pays nothing.
    pub fn any(&self) -> bool {
        self.nodes.is_some() || self.edges.is_some() || self.last_edge
    }

    /// Fold in every binding `expr` names.
    pub fn add(&mut self, expr: &Expr) {
        self.walk(expr, false);
    }

    /// `in_edge_swap` tracks whether an enclosing `At { Edge(_), .. }` already
    /// bound the edge channel — inside one, a bare `type(r)` is that hop's,
    /// not the row's last, so it must not force the last-edge lookup.
    fn walk(&mut self, expr: &Expr, in_edge_swap: bool) {
        let bump = |slot: &mut Option<u32>, v: u32| *slot = Some(slot.map_or(v, |m| m.max(v)));
        match expr {
            Expr::At { binding, inner } => match binding {
                Binding::Node(slot) => {
                    bump(&mut self.nodes, *slot);
                    self.walk(inner, in_edge_swap);
                }
                Binding::Edge(hop) => {
                    bump(&mut self.edges, *hop);
                    self.walk(inner, true);
                }
            },
            Expr::EdgeType | Expr::EdgeDir | Expr::EdgeProperty(_) => {
                if !in_edge_swap {
                    self.last_edge = true;
                }
            }
            Expr::IsNull(e) | Expr::Not(e) => self.walk(e, in_edge_swap),
            Expr::Compare { lhs, rhs, .. }
            | Expr::Logic { lhs, rhs, .. }
            | Expr::Arith { lhs, rhs, .. }
            | Expr::StringMatch { lhs, rhs, .. } => {
                self.walk(lhs, in_edge_swap);
                self.walk(rhs, in_edge_swap);
            }
            Expr::In { needle, haystack } => {
                self.walk(needle, in_edge_swap);
                self.walk(haystack, in_edge_swap);
            }
            // Leaves that read the current node or the row's channels.
            Expr::Property(_)
            | Expr::Literal(_)
            | Expr::HasLabel(_)
            | Expr::NodeId
            | Expr::ExternalKey
            | Expr::Score
            | Expr::Hops
            | Expr::Distance { .. }
            | Expr::Similarity { .. } => {}
        }
    }
}

/// What an `Expr` is evaluated against: the current node (or `None` if the
/// row points at a missing node), the row's score channel, its hop count, and
/// the earlier bindings [`Expr::At`] can reach (arch/03 §2).
///
/// `#[non_exhaustive]`: the row's addressable world keeps growing, so a
/// downstream constructor goes through [`EvalCtx::node`] or `..Default`
/// rather than listing every field.
#[derive(Clone, Copy, Default)]
#[non_exhaustive]
pub struct EvalCtx<'a> {
    pub node: Option<&'a NodeRecord>,
    pub score: Option<f32>,
    pub hops: usize,
    pub bindings: Bindings<'a>,
}

impl<'a> EvalCtx<'a> {
    /// Context for a bare node with no score/hops/bindings (filters over a
    /// plain scan, projection helpers, tests).
    pub fn node(node: Option<&'a NodeRecord>) -> Self {
        Self {
            node,
            ..Default::default()
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
        Expr::NodeId => ctx
            .node
            .map(|n| PropValue::Int(n.id.0 as i64))
            .unwrap_or(PropValue::Null),
        Expr::At { binding, inner } => {
            // Swap only the channel the binding names, so the rest of the
            // row's world (score, hops, the other bindings) stays addressable
            // inside the subtree.
            let mut sub = *ctx;
            match binding {
                Binding::Node(slot) => sub.node = ctx.bindings.node(*slot),
                Binding::Edge(hop) => sub.bindings.edge = ctx.bindings.edge_at(*hop),
            }
            eval(inner, &sub)
        }
        Expr::EdgeType => ctx
            .bindings
            .edge
            .map(|(e, _)| PropValue::Str(e.ty.clone()))
            .unwrap_or(PropValue::Null),
        Expr::EdgeDir => ctx
            .bindings
            .edge
            .map(|(_, dir)| {
                PropValue::Str(
                    match dir {
                        Dir::In => "IN",
                        // A row's traversal is concrete; `Both` never reaches
                        // here (see `Expr::EdgeDir`), and a resolver that
                        // could not tell a self-loop apart reports `Out`.
                        _ => "OUT",
                    }
                    .to_string(),
                )
            })
            .unwrap_or(PropValue::Null),
        Expr::EdgeProperty(key) => ctx
            .bindings
            .edge
            .and_then(|(e, _)| e.properties.get(key))
            .map(|p| p.value.clone())
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
        Expr::StringMatch { op, lhs, rhs } => {
            PropValue::Bool(string_match(*op, &eval(lhs, ctx), &eval(rhs, ctx)))
        }
        Expr::In { needle, haystack } => {
            PropValue::Bool(member_of(&eval(needle, ctx), &eval(haystack, ctx)))
        }
    }
}

/// The value as text, for the string predicates — [`PropValue::as_text`],
/// which is also what an entity's embedded text is built from, so a value a
/// filter can match on is a value that reached the vector.
fn as_text(v: &PropValue) -> Option<Cow<'_, str>> {
    v.as_text()
}

/// `CONTAINS` / `STARTS WITH` / `ENDS WITH`, byte-wise over the text forms.
///
/// Byte-wise means exact: no case folding and no Unicode normalisation, the
/// same posture `=` takes. Anything without a text form is false, not an
/// error. One consequence worth knowing: a missing property and a present
/// non-matching one are indistinguishable here, so `NOT (p CONTAINS "x")`
/// holds for a node with no `p` at all — use `IS NULL` when that matters.
fn string_match(op: StrOp, lhs: &PropValue, rhs: &PropValue) -> bool {
    let (Some(hay), Some(needle)) = (as_text(lhs), as_text(rhs)) else {
        return false;
    };
    match op {
        StrOp::Contains => hay.contains(needle.as_ref()),
        StrOp::StartsWith => hay.starts_with(needle.as_ref()),
        StrOp::EndsWith => hay.ends_with(needle.as_ref()),
    }
}

/// `needle IN haystack`: elements of a `List` by `=` equality, or keys of a
/// `Map`. Everything else — including a `Str` haystack, which is what
/// `CONTAINS` is for — is false.
fn member_of(needle: &PropValue, haystack: &PropValue) -> bool {
    match haystack {
        PropValue::List(items) => items.iter().any(|item| compare(CmpOp::Eq, item, needle)),
        // Keys, not values: see the note on `Expr::In`.
        PropValue::Map(entries) => match as_text(needle) {
            Some(key) => entries.contains_key(key.as_ref()),
            None => false,
        },
        _ => false,
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

    /// Soft-schema data stores the same field as `Int` on one node and `Str`
    /// on the next, so the string predicates promote any scalar with a
    /// canonical text form rather than silently missing half the rows.
    #[test]
    fn string_predicates_promote_scalars() {
        let n = node(
            &["Doc"],
            &[
                ("title", PropValue::Str("graph database".into())),
                ("year", PropValue::Int(2026)),
                ("ratio", PropValue::Float(1.5)),
                ("live", PropValue::Bool(true)),
            ],
        );
        assert!(b(&p("title").contains("data"), &n));
        assert!(b(&p("title").starts_with("graph"), &n));
        assert!(b(&p("title").ends_with("base"), &n));
        assert!(!b(&p("title").contains("sql"), &n));

        // Promoted scalars, no cast at the call site.
        assert!(b(&p("year").starts_with("20"), &n));
        assert!(b(&p("year").contains("02"), &n));
        assert!(b(&p("ratio").contains("."), &n));
        assert!(b(&p("live").eq(true), &n));
        assert!(b(&p("live").starts_with("tru"), &n));

        // The needle promotes too, so both sides are symmetric.
        assert!(b(&p("year").contains(lit(2026)), &n));
    }

    /// Values with no canonical text form make the predicate false rather
    /// than matching some `Debug` rendering — that rendering is an
    /// implementation detail and must not leak into query semantics.
    #[test]
    fn string_predicates_reject_values_without_text() {
        let n = node(
            &["Doc"],
            &[
                ("raw", PropValue::Bytes(vec![1, 2, 3])),
                ("emb", PropValue::Vector(vec![0.5, 0.5])),
                ("tags", PropValue::List(vec![PropValue::Str("a".into())])),
                ("nil", PropValue::Null),
            ],
        );
        for key in ["raw", "emb", "tags", "nil", "absent"] {
            assert!(
                !b(&p(key).contains("1"), &n),
                "{key} has no text form, so CONTAINS must be false"
            );
        }
        // Null must not promote to "": otherwise every missing property would
        // match `CONTAINS ""`.
        assert!(!b(&p("absent").contains(""), &n));
        assert!(b(&p("absent").is_null(), &n));
        // The sharp edge this implies, pinned deliberately: a missing property
        // is indistinguishable from a non-matching one under negation.
        assert!(b(&p("absent").contains("x").not(), &n));
    }

    /// `IN` is membership, kept separate from `CONTAINS` because "list
    /// contains x" has two defensible readings and nothing picks one.
    #[test]
    fn in_tests_list_elements_by_equality() {
        let n = node(
            &["Doc"],
            &[
                (
                    "tags",
                    PropValue::List(vec![PropValue::Str("graph".into()), PropValue::Int(7)]),
                ),
                ("title", PropValue::Str("graph database".into())),
            ],
        );
        assert!(b(&lit("graph").is_in(p("tags")), &n));
        assert!(!b(&lit("graphs").is_in(p("tags")), &n));
        // Element equality, not substring — that is what CONTAINS is for.
        assert!(!b(&lit("gra").is_in(p("tags")), &n));
        // Same numeric coercion `=` uses, so 7 and 7.0 are the same member.
        assert!(b(&lit(7).is_in(p("tags")), &n));
        assert!(b(&lit(7.0).is_in(p("tags")), &n));

        // A string haystack is false: substring matching has its own operator,
        // and overloading IN onto it would make the two indistinguishable.
        assert!(!b(&lit("graph").is_in(p("title")), &n));
        assert!(b(&p("title").contains("graph"), &n));
        // Absent or non-collection haystacks are false, not errors.
        assert!(!b(&lit("x").is_in(p("absent")), &n));
    }

    /// A map's *keys*, not its values — `HashMap` has `contains_key` and no
    /// bare `contains` for exactly this reason.
    #[test]
    fn in_tests_map_keys_not_values() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "colour".to_string(),
            PropDesc::new(PropValue::Str("red".into())),
        );
        m.insert("2026".to_string(), PropDesc::new(PropValue::Int(1)));
        let n = node(&["Doc"], &[("meta", PropValue::Map(m))]);

        assert!(b(&lit("colour").is_in(p("meta")), &n));
        assert!(
            !b(&lit("red").is_in(p("meta")), &n),
            "values must not match"
        );
        // The needle promotes, so an int key can be tested without a cast.
        assert!(b(&lit(2026).is_in(p("meta")), &n));
        assert!(!b(&lit("missing").is_in(p("meta")), &n));
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

    // ---- bindings ---------------------------------------------------------

    fn edge(ty: &str, src: u64, dst: u64, props: &[(&str, PropValue)]) -> EdgeRecord {
        EdgeRecord {
            id: crate::types::EdgeId(1),
            plane: PlaneId::STARTUP,
            src: crate::types::NodeId(src),
            dst: crate::types::NodeId(dst),
            ty: ty.to_string(),
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), PropDesc::new(v.clone())))
                .collect(),
        }
    }

    #[test]
    fn at_reads_an_earlier_slot_not_the_current_node() {
        let head = node(&["Function"], &[("name", PropValue::Str("callee".into()))]);
        let src = node(&["Function"], &[("name", PropValue::Str("caller".into()))]);
        let bound = [Some(Arc::new(src))];
        let ctx = EvalCtx {
            node: Some(&head),
            bindings: Bindings {
                nodes: &bound,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(eval(&p("name"), &ctx), PropValue::Str("callee".into()));
        assert_eq!(
            eval(&at_node(0, p("name")), &ctx),
            PropValue::Str("caller".into())
        );
        // The swap is scoped to the subtree: the outer term still reads the
        // current node, so a cross-variable comparison means what it says.
        assert!(is_true(&eval(&at_node(0, p("name")).ne(p("name")), &ctx)));
    }

    /// The whole point of `At` being total: a slot the row never bound reads
    /// as `Null` rather than failing the query, exactly like a missing
    /// property does.
    #[test]
    fn an_unbound_slot_is_null() {
        let n = node(&[], &[("x", PropValue::Int(1))]);
        let ctx = EvalCtx::node(Some(&n));
        assert_eq!(eval(&at_node(3, p("x")), &ctx), PropValue::Null);
        assert_eq!(eval(&edge_type(), &ctx), PropValue::Null);
        assert_eq!(eval(&edge_dir(), &ctx), PropValue::Null);
        assert_eq!(eval(&ep("weight"), &ctx), PropValue::Null);
    }

    #[test]
    fn edge_terms_read_the_arrival_edge_and_a_named_hop() {
        let n = node(&[], &[]);
        let first = edge("IMPORTS", 1, 2, &[("weight", PropValue::Int(3))]);
        let last = edge("CALLS", 9, 2, &[]);
        let hops_bound = [Some((Arc::new(first), Dir::Out))];
        let ctx = EvalCtx {
            node: Some(&n),
            bindings: Bindings {
                edges: &hops_bound,
                edge: Some((&last, Dir::In)),
                ..Default::default()
            },
            ..Default::default()
        };
        // Bare terms are the edge the row arrived on …
        assert_eq!(eval(&edge_type(), &ctx), PropValue::Str("CALLS".into()));
        assert_eq!(eval(&edge_dir(), &ctx), PropValue::Str("IN".into()));
        // … and `At` names a specific hop instead.
        assert_eq!(
            eval(&at_edge(0, edge_type()), &ctx),
            PropValue::Str("IMPORTS".into())
        );
        assert_eq!(
            eval(&at_edge(0, edge_dir()), &ctx),
            PropValue::Str("OUT".into())
        );
        assert_eq!(eval(&at_edge(0, ep("weight")), &ctx), PropValue::Int(3));
    }

    #[test]
    fn node_id_is_always_available_as_a_grouping_key() {
        let n = node(&[], &[]);
        assert_eq!(ev(&node_id(), Some(&n)), PropValue::Int(1));
        assert_eq!(ev(&node_id(), None), PropValue::Null);
        // …unlike the external key, which most nodes never carry.
        assert_eq!(ev(&external_key(), Some(&n)), PropValue::Null);
    }

    /// The executor resolves only what a plan names, so this walk is what
    /// keeps an ordinary query paying nothing for the feature.
    #[test]
    fn binding_need_collects_high_water_marks() {
        let mut need = BindingNeed::default();
        assert!(!need.any());
        need.add(&p("x").eq(1));
        assert!(!need.any(), "a plain property names no binding");

        need.add(&at_node(2, p("a")).eq(at_node(5, p("b"))));
        assert_eq!(need.nodes, Some(5));
        need.add(&at_edge(1, ep("w")).gt(0));
        assert_eq!(need.edges, Some(1));
        assert!(!need.last_edge, "a hop-qualified edge term is not the last");

        // A bare edge term, though, is the row's arrival edge.
        let mut bare = BindingNeed::default();
        bare.add(&edge_type().eq("CALLS"));
        assert!(bare.last_edge);
        assert_eq!(bare.nodes, None);
    }

    #[test]
    fn binding_terms_serde_roundtrip() {
        let e = at_node(0, p("name"))
            .ne(p("name"))
            .and(at_edge(1, edge_dir()).eq("OUT"))
            .and(edge_type().eq("CALLS"))
            .and(node_id().gt(0));
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(e, serde_json::from_str::<Expr>(&json).unwrap());
    }

    #[test]
    fn score_and_hops_read_the_row_channel() {
        let n = node(&[], &[]);
        let ctx = EvalCtx {
            node: Some(&n),
            score: Some(0.75),
            hops: 2,
            ..Default::default()
        };
        assert_eq!(eval(&score(), &ctx), PropValue::Float(0.75));
        assert_eq!(eval(&hops(), &ctx), PropValue::Int(2));
        // score is Null when the row carries none
        let none = EvalCtx {
            node: Some(&n),
            score: None,
            hops: 0,
            ..Default::default()
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
            ..Default::default()
        };
        let fused = score().mul(lit(0.7)).add(lit(0.3).div(hops()));
        // 0.7*1.0 + 0.3/2 = 0.85
        match eval(&fused, &ctx) {
            PropValue::Float(v) => assert!((v - 0.85).abs() < 1e-6, "got {v}"),
            other => panic!("expected float, got {other:?}"),
        }
    }
}
