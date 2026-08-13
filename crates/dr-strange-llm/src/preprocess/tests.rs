use super::*;

/// A scratch tree that cleans up after itself.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-pre-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }

    fn write(&self, rel: &str, body: &str) -> &Self {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        self
    }

    fn host(&self) -> LocalFiles {
        LocalFiles::new(&self.0).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn keys(p: &Preprocessed) -> Vec<&str> {
    p.nodes.iter().map(|n| n.key.as_str()).collect()
}

fn prop<'a>(p: &'a Preprocessed, key: &str, name: &str) -> Option<&'a str> {
    p.nodes
        .iter()
        .find(|n| n.key == key)?
        .props
        .get(name)
        .and_then(|d| match &d.value {
            PropValue::Str(s) => Some(s.as_str()),
            _ => None,
        })
}

const LIB: &str = r#"
//! The crate.
use std::fmt;

/// Adds.
pub fn add(a: i64, b: i64) -> i64 { let sum = a + b; sum }

pub fn caller() -> i64 { add(1, 2) }

pub const LIMIT: usize = 4;
pub static NAME: &str = "x";
pub type Pair = (i64, i64);

pub struct Thing;

impl fmt::Display for Thing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { Ok(()) }
}

pub trait Local {}
impl Local for Thing {}
"#;

/// An item's identity is its module path, not the file it lives in — that is
/// what a Rust programmer calls it, and what a model will recognise.
#[test]
fn keys_are_module_paths_with_the_crate_name() {
    let t = Tree::new("keys");
    t.write("Cargo.toml", "[package]\nname = \"my-crate\"\n")
        .write("src/lib.rs", LIB)
        .write("src/deep/thing.rs", "pub fn helper() {}");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let k = keys(&out);

    // `-` becomes `_`, as it does in code.
    assert!(k.contains(&"my_crate::add"), "{k:?}");
    assert!(k.contains(&"my_crate::LIMIT"), "{k:?}");
    assert!(k.contains(&"my_crate::Pair"), "{k:?}");
    // `lib.rs` names the crate root rather than a module called `lib`.
    assert!(k.contains(&"my_crate"), "{k:?}");
    // A nested file is a nested module.
    assert!(k.contains(&"my_crate::deep::thing::helper"), "{k:?}");
}

/// Pointed at `…/foo/src`, the manifest is one level up and outside the grant.
/// Falling back to a bare `crate::` there would merge two crates' `api::Thing`
/// into one node the moment both are ingested into the same plane.
#[test]
fn a_src_directory_is_named_after_the_crate_holding_it() {
    let outer = Tree::new("srcroot");
    outer
        .write("my-crate/Cargo.toml", "[package]\nname = \"my-crate\"\n")
        .write("my-crate/src/lib.rs", "pub fn only() {}");

    let host = LocalFiles::new(outer.0.join("my-crate/src")).unwrap();
    let out = route_tree(&host, None, &Plugins::builtin()).unwrap();
    assert!(keys(&out).contains(&"my_crate::only"), "{:?}", keys(&out));
}

/// Consts, statics, type aliases and macros are interface too — a reader
/// looking for `LIMIT` should find it.
#[test]
fn items_beyond_functions_and_types_are_emitted() {
    let t = Tree::new("items");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    for (key, label) in [
        ("k::LIMIT", "Const"),
        ("k::NAME", "Static"),
        ("k::Pair", "TypeAlias"),
        ("k::Thing", "Struct"),
        ("k::Local", "Trait"),
    ] {
        let node = out.nodes.iter().find(|n| n.key == key);
        assert_eq!(node.map(|n| n.label.as_str()), Some(label), "{key}");
    }
    // Imports are recorded on the module rather than as nodes for std paths.
    assert!(
        prop(&out, "k", "imports").is_some_and(|i| i.contains("std::fmt")),
        "imports missing"
    );
}

/// `impl Display for Thing` names a trait from another crate. Assuming it lived
/// in this module would point the edge at a node that does not exist; dropping
/// it would lose the most useful thing the impl block says.
#[test]
fn a_foreign_trait_becomes_an_external_node() {
    let t = Tree::new("impls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    let external = out
        .nodes
        .iter()
        .find(|n| n.key.ends_with("Display"))
        .expect("Display should exist as a node");
    // Two labels: what it is, and that it is not ours. Both are things a reader
    // asks for by label — `MATCH (t:Trait)` should find `Display`, and
    // `MATCH (n:External)` should find everything foreign whatever its kind.
    assert_eq!(external.label, "Trait");
    assert_eq!(external.extra_labels, vec!["External".to_string()]);
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "IMPLEMENTS" && e.dst == external.key),
        "the IMPLEMENTS edge should point at it"
    );
    // A trait defined here resolves to its own module-path key instead.
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "IMPLEMENTS" && e.dst == "k::Local"),
        "a local trait should resolve locally"
    );
}

#[test]
fn calls_resolve_by_name_and_locals_are_listed() {
    let t = Tree::new("calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.src == "k::caller" && e.dst == "k::add"),
        "caller -> add should be an edge"
    );
    assert_eq!(prop(&out, "k::add", "local_bindings"), Some("sum"));
}

const SHAPES: &str = r#"
pub struct T;

impl T {
    /// An associated function: no receiver, so not a method.
    pub fn open() -> T { T }
    pub fn read(&self) -> usize { 0 }
    pub fn write(&mut self, n: usize) {}
    pub fn consume(self) {}
    pub async fn fetch(&self) -> Result<usize, ()> { Ok(0) }
}

pub fn free() {}
pub async fn free_async() -> i64 { 0 }
"#;

/// A return type is a fact worth querying, and it is not one while it is a
/// substring of a rendered signature.
#[test]
fn a_function_records_what_it_returns() {
    let t = Tree::new("returns");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert_eq!(prop(&out, "k::T::open", "returns"), Some("T"));
    assert_eq!(prop(&out, "k::T::read", "returns"), Some("usize"));
    assert_eq!(prop(&out, "k::free_async", "returns"), Some("i64"));
    assert_eq!(
        prop(&out, "k::T::fetch", "returns"),
        Some("Result<usize,()>")
    );
    // `-> ()` and no arrow are the same function in Rust, so absence is the
    // unit type rather than a missing answer.
    assert_eq!(prop(&out, "k::free", "returns"), None);
    assert_eq!(prop(&out, "k::T::write", "returns"), None);
}

/// "Every method on this type" should be a label query, not a scan for a
/// leading `self` inside a rendered signature.
#[test]
fn a_method_is_labelled_apart_from_a_function() {
    let t = Tree::new("methods");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    let label = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .map(|n| n.label.as_str())
    };
    assert_eq!(label("k::T::read"), Some("Method"));
    assert_eq!(label("k::T::write"), Some("Method"));
    assert_eq!(label("k::T::consume"), Some("Method"));
    // An associated function is not a method, which is the distinction Rust
    // itself draws — `T::open()` takes no receiver.
    assert_eq!(label("k::T::open"), Some("Function"));
    assert_eq!(label("k::free"), Some("Function"));

    // The receiver's own form is kept, since `&self` and `&mut self` are a
    // real difference between two methods.
    assert_eq!(prop(&out, "k::T::read", "receiver"), Some("&self"));
    assert_eq!(prop(&out, "k::T::write", "receiver"), Some("&mut self"));
    assert_eq!(prop(&out, "k::T::consume", "receiver"), Some("self"));
    assert_eq!(prop(&out, "k::T::open", "receiver"), None);
}

/// Splitting the label must not split the call graph: both kinds are callable.
#[test]
fn methods_still_resolve_as_call_targets() {
    let t = Tree::new("method-calls-label");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub struct T;\nimpl T { pub fn go(&self) { self.inner(); }\n pub fn inner(&self) {} }",
    );
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "CALLS" && e.src == "k::T::go" && e.dst == "k::T::inner"),
        "a method must still be reachable as a call target"
    );
}

/// `async fn` and `fn` are different things to call, and the difference is
/// otherwise buried in the signature string.
#[test]
fn an_async_function_says_so() {
    let t = Tree::new("asyncness");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", SHAPES);
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    let is_async = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .and_then(|n| n.props.get("is_async"))
            .map(|d| d.value.clone())
    };
    assert_eq!(is_async("k::free_async"), Some(PropValue::Bool(true)));
    assert_eq!(is_async("k::T::fetch"), Some(PropValue::Bool(true)));
    // Absent means synchronous, the same convention `visibility` uses for
    // private — a property that says "no" on every node is mostly noise.
    assert_eq!(is_async("k::free"), None);
    assert_eq!(is_async("k::T::read"), None);
}

/// We do not read std's code and never will, but "this crate calls that" is
/// worth knowing. A call that writes a path stops at a node holding the path.
#[test]
fn a_call_into_another_crate_stops_at_a_node() {
    let t = Tree::new("external-calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "use std::fs;\npub fn go() { std::mem::swap(); fs::read(); let v = x.trim(); }",
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let calls: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS" && e.src == "k::go")
        .map(|e| e.dst.as_str())
        .collect();

    assert!(calls.contains(&"std::mem::swap"), "{calls:?}");
    // Written `fs::read`, but the file said `use std::fs` — so it is recorded
    // under the path that identifies it, not the abbreviation.
    assert!(calls.contains(&"std::fs::read"), "{calls:?}");
    // A method call names no path and the receiver's type is unknowable here.
    assert!(!calls.iter().any(|c| c.contains("trim")), "{calls:?}");

    // A call site proves the target is callable, so it is labelled as both what
    // it is and as foreign.
    let node = out.nodes.iter().find(|n| n.key == "std::fs::read").unwrap();
    assert_eq!(node.label, "Function");
    assert_eq!(node.extra_labels, vec!["External".to_string()]);
    // No signature: we would have to read std's source to know one, and
    // inventing one is worse than its absence.
    assert!(!node.props.contains_key("signature"));
    every_edge_has_endpoints(&out);
}

/// A module's imports are edges to what they name, and the property lists the
/// same resolved keys — so each entry can be followed to its node.
#[test]
fn imports_become_edges_to_resolved_keys() {
    let t = Tree::new("imports-edges");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod thing;\npub mod user;")
        .write("src/thing.rs", "pub struct Thing;")
        .write(
            "src/user.rs",
            "use crate::thing::Thing;\nuse std::sync::Arc;\nuse std::io::*;",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let imported: Vec<&str> = out
        .edges
        .iter()
        .filter(|e| e.ty == "IMPORTS" && e.src == "k::user")
        .map(|e| e.dst.as_str())
        .collect();

    // A local import lands on the real node; a foreign one on a stand-in.
    assert!(imported.contains(&"k::thing::Thing"), "{imported:?}");
    assert!(imported.contains(&"std::sync::Arc"), "{imported:?}");
    // A glob names no single target, so it is no edge.
    assert!(!imported.iter().any(|i| i.contains('*')), "{imported:?}");

    // The property carries the resolved keys, not `crate::thing::Thing` — a
    // key that is not the node's key cannot be followed to it.
    let list = prop(&out, "k::user", "imports").unwrap();
    assert!(list.contains("k::thing::Thing"), "{list}");
    assert!(!list.contains("crate::thing"), "{list}");
    // The glob is still listed: the file did write it.
    assert!(list.contains("std::io::*"), "{list}");

    // A stand-in for something only ever imported says nothing about its kind.
    let arc = out
        .nodes
        .iter()
        .find(|n| n.key == "std::sync::Arc")
        .unwrap();
    // Only ever imported, so its kind is unknown and `External` is the whole
    // honest answer — no second label invented to sit beside it.
    assert_eq!(arc.label, "External");
    assert!(arc.extra_labels.is_empty());
    every_edge_has_endpoints(&out);
}

/// A facade is the normal shape of a Rust crate: `lib.rs` republishes a module
/// and everything then refers to it by the short path. Nothing is declared
/// there, so an unfollowed re-export files the crate's own type under a
/// foreign-looking key.
#[test]
fn a_reexported_path_resolves_to_the_real_item() {
    let t = Tree::new("reexports");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod api;\npub use api::cache;")
        .write("src/api/mod.rs", "pub mod cache;")
        // `pub(crate)` counts: it creates the facade path for the whole crate,
        // which is exactly the scope being parsed.
        .write(
            "src/api/cache/mod.rs",
            "pub mod store;\npub(crate) use store::GraphCache;\npub struct CachedReader;",
        )
        .write("src/api/cache/store.rs", "pub struct GraphCache;")
        .write(
            "src/user.rs",
            "use crate::cache::{CachedReader, GraphCache};",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let list = prop(&out, "k::user", "imports").unwrap();

    assert!(list.contains("k::api::cache::CachedReader"), "{list}");
    // Two hops: `crate::cache` → `api::cache`, then `GraphCache` → `store`.
    assert!(list.contains("k::api::cache::store::GraphCache"), "{list}");
    // And neither became an external stand-in for the crate's own type.
    assert!(
        !out.nodes.iter().any(|n| n.label == "External"),
        "{:?}",
        out.nodes
            .iter()
            .filter(|n| n.label == "External")
            .map(|n| &n.key)
            .collect::<Vec<_>>()
    );
    every_edge_has_endpoints(&out);
}

/// A re-export's target is written relative to the module that wrote it and
/// may itself be a facade. `lib.rs` says `pub use compute::{Expr}` while
/// `compute/mod.rs` says `pub use expr::Expr` — the first only makes sense
/// once the second is known, so one pass leaves a relative path naming nothing.
#[test]
fn a_reexport_chain_resolves_through_its_own_facades() {
    let t = Tree::new("chain");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod compute;\npub use compute::Expr;")
        .write("src/compute/mod.rs", "pub mod expr;\npub use expr::Expr;")
        .write("src/compute/expr.rs", "pub enum Expr { Lit }")
        .write("src/user.rs", "use crate::Expr;");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert_eq!(
        prop(&out, "k::user", "imports"),
        Some("k::compute::expr::Expr")
    );
    // The crate's own enum must not appear as somebody else's type.
    assert!(
        !out.nodes.iter().any(|n| n.label == "External"),
        "{:?}",
        keys(&out)
    );
    every_edge_has_endpoints(&out);
}

/// `use a::b::{self, C}` imports `a::b` itself. Read as a child named `self`
/// it becomes a path naming no item — a node nothing could ever be.
#[test]
fn a_self_import_names_the_module_itself() {
    let t = Tree::new("self-import");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod algo;")
        .write("src/algo.rs", "pub struct Options;")
        .write("src/user.rs", "use crate::algo::{self, Options};");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let list = prop(&out, "k::user", "imports").unwrap();
    assert!(list.contains("k::algo"), "{list}");
    assert!(!list.contains("::self"), "{list}");
    assert!(
        !keys(&out).iter().any(|k| k.ends_with("::self")),
        "{:?}",
        keys(&out)
    );
    every_edge_has_endpoints(&out);
}

/// A node saying only `Expr` says almost nothing — the variants are the shape
/// of the type, and each one's fields are part of that shape.
#[test]
fn an_enum_records_its_variants() {
    let t = Tree::new("variants");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum Expr {
    Unit,
    Lit(i64),
    Pair(i64, String),
    Prop { name: String, value: i64 },
}

#[repr(u8)]
pub enum Code { Ok = 0, Bad = 1 }
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let variants = |key: &str| match out
        .nodes
        .iter()
        .find(|n| n.key == key)
        .and_then(|n| n.props.get("variants"))
        .map(|d| &d.value)
    {
        Some(PropValue::List(items)) => items
            .iter()
            .map(|v| match v {
                PropValue::Str(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    assert_eq!(
        variants("k::Expr"),
        vec![
            "Unit",
            "Lit(i64)",
            "Pair(i64, String)",
            "Prop { name: String, value: i64 }",
        ]
    );
    // A discriminant is part of what the variant is.
    assert_eq!(variants("k::Code"), vec!["Ok = 0", "Bad = 1"]);
}

/// The same argument as an enum's variants: a node saying only `NodeRecord`
/// says almost nothing, while its fields are the shape of it.
#[test]
fn a_struct_records_its_fields() {
    let t = Tree::new("fields");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub struct Record {
    pub id: NodeId,
    pub(crate) labels: Vec<String>,
    seq: u64,
}

pub struct Wrapper(pub i64, String);

pub struct Marker;
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let fields = |key: &str| match out
        .nodes
        .iter()
        .find(|n| n.key == key)
        .and_then(|n| n.props.get("fields"))
        .map(|d| &d.value)
    {
        Some(PropValue::List(items)) => items
            .iter()
            .map(|v| match v {
                PropValue::Str(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };

    // Visibility is part of what a struct is — which fields a caller may touch.
    assert_eq!(
        fields("k::Record"),
        vec![
            "pub id: NodeId",
            "pub(crate) labels: Vec<String>",
            "seq: u64"
        ]
    );
    // A tuple struct's fields are positions rather than names.
    assert_eq!(fields("k::Wrapper"), vec!["pub 0: i64", "1: String"]);
    // A unit struct has none, and an empty list is noise on every read.
    assert!(
        out.nodes
            .iter()
            .find(|n| n.key == "k::Marker")
            .is_some_and(|n| !n.props.contains_key("fields")),
        "a unit struct should carry no fields property"
    );
}

/// `#[non_exhaustive]` is a promise about the future rather than a detail of
/// the present: it decides whether a downstream `match` needs a wildcard arm.
#[test]
fn non_exhaustive_is_recorded_where_it_is_written() {
    let t = Tree::new("nonexhaustive");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
#[non_exhaustive]
pub enum Open { A, B }

pub enum Closed { A }

#[non_exhaustive]
pub struct Growing { pub a: i64 }

pub struct Fixed;
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let flagged = |key: &str| {
        out.nodes
            .iter()
            .find(|n| n.key == key)
            .and_then(|n| n.props.get("non_exhaustive"))
            .map(|d| d.value.clone())
    };

    assert_eq!(flagged("k::Open"), Some(PropValue::Bool(true)));
    // The same attribute means the same thing on a struct.
    assert_eq!(flagged("k::Growing"), Some(PropValue::Bool(true)));
    // Absent means exhaustive, as absent `visibility` means private.
    assert_eq!(flagged("k::Closed"), None);
    assert_eq!(flagged("k::Fixed"), None);
}

/// Nothing expands macros — that is the compiler's job, and a proc macro is
/// arbitrary code that would have to be *run*. So the items an invocation
/// declares are absent, and the point of this test is that their absence is
/// marked rather than silent.
#[test]
fn an_item_macro_invocation_is_marked_not_expanded() {
    let t = Tree::new("macros");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum Expr { Lit }

macro_rules! expr_from_literal {
    ($($t:ty),*) => {$(
        impl From<$t> for Expr { fn from(v: $t) -> Self { Expr::Lit } }
    )*};
}

expr_from_literal!(bool, i64, String);
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    // The definition is an item and gets a node.
    assert!(
        keys(&out).contains(&"k::expr_from_literal"),
        "{:?}",
        keys(&out)
    );

    // The invocation is an edge to it, carrying what it was given — which is
    // the shape of what it generated.
    let invoke = out
        .edges
        .iter()
        .find(|e| e.ty == "INVOKES")
        .expect("the invocation should be recorded");
    assert_eq!(invoke.src, "k");
    assert_eq!(invoke.dst, "k::expr_from_literal");
    assert!(
        matches!(invoke.props.get("arguments").map(|d| &d.value),
                 Some(PropValue::Str(s)) if s.contains("bool") && s.contains("String")),
        "{:?}",
        invoke.props
    );

    // The three `impl From<…> for Expr` blocks it generates are *not* there,
    // and the report says so rather than leaving a reader to wonder.
    assert!(
        !out.edges.iter().any(|e| e.ty == "IMPLEMENTS"),
        "generated impls cannot be known without expanding"
    );
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("not expanded") && n.contains("INVOKES")),
        "{:?}",
        out.report.notes
    );
    every_edge_has_endpoints(&out);
}

/// A `macro_rules!` written inside a function body is a local helper, not an
/// item of the module — three in a crate should not become three nodes.
#[test]
fn a_macro_inside_a_body_is_not_an_item() {
    let t = Tree::new("localmacro");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub fn go() { macro_rules! run { () => { 1 } } run!(); }",
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        !keys(&out).iter().any(|k| k.contains("run")),
        "{:?}",
        keys(&out)
    );
}

/// A trait's own items are its interface — the thing an implementor must
/// supply — and are as much a part of it as an impl block's are.
#[test]
fn a_traits_own_items_are_emitted() {
    let t = Tree::new("trait-items");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub trait Reader {
    /// Required.
    fn get(&self, id: u64) -> Option<u64>;
    /// Provided, with a body.
    fn count(&self) -> usize { let n = 0; n }
    fn make() -> Self where Self: Sized;
}
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let node = |key: &str| out.nodes.iter().find(|n| n.key == key);

    // A `self` receiver makes it a method here exactly as in an impl block.
    assert_eq!(
        node("k::Reader::get").map(|n| n.label.as_str()),
        Some("Method")
    );
    assert_eq!(prop(&out, "k::Reader::get", "receiver"), Some("&self"));
    assert_eq!(prop(&out, "k::Reader::get", "returns"), Some("Option<u64>"));
    // An associated function without a receiver is a `Function`.
    assert_eq!(
        node("k::Reader::make").map(|n| n.label.as_str()),
        Some("Function")
    );
    // A default body is where the bindings come from when there is one.
    assert_eq!(prop(&out, "k::Reader::count", "local_bindings"), Some("n"));
    assert!(
        out.edges
            .iter()
            .any(|e| e.ty == "HAS_METHOD" && e.src == "k::Reader" && e.dst == "k::Reader::get"),
        "the trait should own its items"
    );
    every_edge_has_endpoints(&out);
}

/// A union is a struct whose fields overlap in memory, and is described the
/// same way.
#[test]
fn a_union_records_its_fields() {
    let t = Tree::new("union");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "pub union Word { pub bits: u64, pub halves: [u32; 2] }",
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let node = out.nodes.iter().find(|n| n.key == "k::Word").unwrap();
    assert_eq!(node.label, "Union");
    assert!(matches!(
        node.props.get("fields").map(|d| &d.value),
        Some(PropValue::List(items)) if items.len() == 2
    ));
}

/// `extern crate`, `use x as y` and `use a::{self as b}` all bring a name into
/// scope under a name of the caller's choosing.
#[test]
fn renamed_and_extern_imports_are_recorded() {
    let t = Tree::new("renames");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod inner;")
        .write("src/inner.rs", "pub struct Thing;")
        .write(
            "src/user.rs",
            "extern crate alloc;\nuse crate::inner::Thing as Renamed;\nuse crate::{inner as nested};",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let list = prop(&out, "k::user", "imports").unwrap();

    // The rename names the *original*, which is what the node is keyed by.
    assert!(list.contains("k::inner::Thing"), "{list}");
    // `{inner as nested}` is the module itself, not a child called `self`.
    assert!(list.contains("k::inner"), "{list}");
    assert!(list.contains("alloc"), "{list}");
    every_edge_has_endpoints(&out);
}

/// A `let` can bind several names at once, and every one of them is a name the
/// function introduced.
#[test]
fn destructuring_bindings_are_all_listed() {
    let t = Tree::new("patterns");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub struct P { pub x: i64, pub y: i64 }
pub fn go(p: P) {
    let (first, second) = (1, 2);
    let P { x, y } = p;
    let [head, tail] = [3, 4];
    let &borrowed = &5;
    let typed: i64 = 6;
    let Some(inner) = None::<i64> else { return };
}
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let bound = prop(&out, "k::go", "local_bindings").unwrap();
    for name in [
        "first", "second", "x", "y", "head", "tail", "borrowed", "typed", "inner",
    ] {
        assert!(bound.contains(name), "{name} missing from {bound}");
    }
}

/// Off by default: every body stored is roughly a copy of the codebase.
#[test]
fn source_is_stored_only_when_asked_for() {
    let t = Tree::new("source");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB);

    let off = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(prop(&off, "k::add", "_code").is_none());

    let mut options = std::collections::BTreeMap::new();
    options.insert(
        "rust".to_string(),
        vec![("include_source".to_string(), "true".to_string())],
    );
    let on = route_tree(&t.host(), None, &Plugins::with_options(&options)).unwrap();
    assert!(
        prop(&on, "k::add", "_code").is_some_and(|c| c.contains("sum")),
        "the body should be retrievable when asked for"
    );
    // Underscore-prefixed, so it stays out of embeddings and the schema summary.
    assert!(prop(&on, "k::add", "source_code_raw").is_none());
}

/// A repository is Rust *and* markdown *and* a binary nobody can read.
#[test]
fn a_polyglot_tree_fans_out_and_merges() {
    let t = Tree::new("poly");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub fn only() {}")
        .write("README.md", "# Title\n\nSome prose.")
        .write("notes.txt", "loose notes");
    std::fs::write(t.0.join("logo.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert!(keys(&out).contains(&"k::only"), "rust facts missing");
    assert!(out.prose.contains("Some prose"), "markdown prose missing");
    assert!(out.prose.contains("loose notes"), "text prose missing");
    // Each file's prose is announced, so no chunk straddles two of them.
    assert!(
        out.prose.matches(SOURCE_MARKER).count() >= 2,
        "each source should be named: {}",
        out.prose
    );
    assert!(out.report.skipped >= 1, "the binary should be counted");
    assert!(
        out.report
            .handlers
            .iter()
            .any(|(n, _)| n.starts_with("rust")),
        "the report should name the handlers that ran: {:?}",
        out.report.handlers
    );
}

/// Determinism is the reason the walk is sorted and the parallel results are
/// collected in order rather than as they finish.
#[test]
fn the_same_tree_twice_gives_the_same_result() {
    let t = Tree::new("determinism");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", LIB)
        .write("src/a.rs", "pub fn a() {}")
        .write("src/b.rs", "pub fn b() {}")
        .write("docs/one.md", "# One")
        .write("docs/two.md", "# Two");

    let first = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let second = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert_eq!(keys(&first), keys(&second), "node order must be stable");
    assert_eq!(first.prose, second.prose, "prose order must be stable");
    let e = |p: &Preprocessed| {
        p.edges
            .iter()
            .map(|e| format!("{}-{}->{}", e.src, e.ty, e.dst))
            .collect::<Vec<_>>()
    };
    assert_eq!(e(&first), e(&second), "edge order must be stable");
}

/// A project's own ignore file is the best statement of what is derived — and
/// it has to be possible to disagree with it.
#[test]
fn gitignore_is_honoured_and_can_be_turned_off() {
    let t = Tree::new("ignore");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write(".gitignore", "generated.rs\n")
        .write("src/lib.rs", "pub fn kept() {}")
        .write("src/generated.rs", "pub fn generated() {}");

    let honoured = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        !keys(&honoured).contains(&"k::generated::generated"),
        "{:?}",
        keys(&honoured)
    );

    let host = LocalFiles::with_policy(
        &t.0,
        IgnorePolicy {
            gitignore: false,
            ..Default::default()
        },
    )
    .unwrap();
    let ignored = route_tree(&host, None, &Plugins::builtin()).unwrap();
    assert!(
        keys(&ignored).contains(&"k::generated::generated"),
        "turning gitignore off should surface it: {:?}",
        keys(&ignored)
    );
}

/// `target/` is skipped even with no ignore file at all, because a build
/// directory can outweigh the source it came from by orders of magnitude.
#[test]
fn build_directories_are_skipped_without_any_ignore_file() {
    let t = Tree::new("builtin");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub fn kept() {}")
        .write("target/debug/thing.rs", "pub fn derived() {}");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        !keys(&out).iter().any(|k| k.contains("derived")),
        "{:?}",
        keys(&out)
    );
}

/// What the host will answer for *is* the capability grant, so it must not
/// answer for anything outside the directory it was given.
#[test]
fn the_host_refuses_to_read_outside_its_root() {
    let t = Tree::new("escape");
    t.write("src/lib.rs", "pub fn a() {}");
    let host = t.host();

    assert!(host.read("src/lib.rs").is_ok());
    let err = host
        .read("../../../etc/passwd")
        .expect_err("a path outside the root must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("outside") || msg.contains("resolving"),
        "unhelpful: {msg}"
    );
}

/// Resolution is declared, never guessed.
#[test]
fn routing_order_is_override_then_extension_then_builtin() {
    let t = Tree::new("routing");
    t.write("src/lib.rs", "pub fn a() {}");
    let host = t.host();
    let opts = Plugins::builtin();

    // Extension picks the Rust handler.
    let by_ext = route_document("thing.rs", b"pub fn a() {}", None, &host, &opts).unwrap();
    assert_eq!(
        by_ext
            .nodes
            .iter()
            .filter(|n| n.label == "Function")
            .count(),
        1
    );

    // No declared extension falls back to the document reader: prose, no facts.
    let fallback = route_document("notes.md", b"# Hi", None, &host, &opts).unwrap();
    assert!(fallback.nodes.is_empty());
    assert!(fallback.prose.contains("Hi"));

    // An explicit handler wins over the extension.
    let forced = route_document("notes.md", b"pub fn a() {}", Some("rust"), &host, &opts).unwrap();
    assert!(forced.nodes.iter().any(|n| n.label == "Function"));

    // And an unknown one is an error naming what exists, not a silent fallback.
    let err = route_document("x.md", b"", Some("cobol"), &host, &opts).unwrap_err();
    assert!(err.to_string().contains("rust"), "{err}");
}

/// Everything a preprocessor emits is marked, so a later reader can always
/// separate a parsed fact from a model's guess.
#[test]
fn facts_carry_their_provenance() {
    let t = Tree::new("provenance");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub fn a() {}");
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert_eq!(prop(&out, "k::a", "_generated_by"), Some("rust@1"));
    assert!(
        out.edges
            .iter()
            .all(|e| e.props.contains_key("_generated_by")),
        "edges need it too"
    );
}

/// A file that will not parse is reported, not fatal: one broken fixture should
/// not sink an ingest.
#[test]
fn an_unparsable_file_is_reported_rather_than_fatal() {
    let t = Tree::new("broken");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub fn fine() {}")
        .write("src/broken.rs", "pub fn ( ) ) {{{");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        keys(&out).contains(&"k::fine"),
        "the good file still parses"
    );
    assert!(
        out.report.notes.iter().any(|n| n.contains("did not parse")),
        "{:?}",
        out.report.notes
    );
}

/// Facts with no prose means the model is never called at all — the headline of
/// §11, and the property the caller keys its skip on.
#[test]
fn a_code_only_tree_needs_no_model() {
    let t = Tree::new("nomodel");
    t.write("src/lib.rs", "pub fn a() {}");
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();

    assert!(!out.nodes.is_empty(), "there should be facts");
    assert!(
        !out.needs_model(),
        "no prose means no model call: {:?}",
        out.prose
    );
    // With no manifest to read, the directory names the crate.
    assert!(
        keys(&out).iter().any(|k| k.ends_with("::a")),
        "{:?}",
        keys(&out)
    );
}

/// A manifest, a lockfile or a CI workflow is not a format `to_markdown`
/// converts, but it is readable text that says real things about a project.
/// Refusing all of them would leave a repository's own configuration out of its
/// graph — so an unclaimed text file becomes fenced prose.
#[test]
fn config_files_become_fenced_prose_rather_than_being_dropped() {
    let t = Tree::new("configs");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("deploy.yaml", "replicas: 3\n")
        .write("src/lib.rs", "pub fn a() {}");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(out.prose.contains("```toml"), "{}", out.prose);
    assert!(out.prose.contains("replicas: 3"), "{}", out.prose);
    // And the code in the same tree is still facts, not prose.
    assert!(keys(&out).contains(&"k::a"));
    assert!(!out.prose.contains("pub fn a"));
}

/// Two packages both have a `benches/graph.rs`, and neither one is reachable
/// from its library's module tree — a bench is its own crate root.
#[test]
fn non_library_targets_are_keyed_under_their_package() {
    let t = Tree::new("targets");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("benches/graph.rs", "fn timed() {}")
        .write("tests/api.rs", "fn checks() {}")
        // A module *called* tests, inside the library — not an integration test.
        .write("src/tests/mod.rs", "pub fn inner() {}");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let k = keys(&out);
    assert!(k.contains(&"k::benches::graph::timed"), "{k:?}");
    assert!(k.contains(&"k::tests::api::checks"), "{k:?}");
    assert!(k.contains(&"k::tests::inner"), "{k:?}");
}

/// Six `impl From<…> for PropValue` blocks are six different functions. Keyed
/// by the type alone they would be one node, and five would be dropped as
/// collisions.
#[test]
fn trait_impl_methods_are_keyed_by_qualified_path() {
    let t = Tree::new("qualified");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
pub enum V { I(i64), F(f64) }
impl From<i64> for V { fn from(v: i64) -> Self { V::I(v) } }
impl From<f64> for V { fn from(v: f64) -> Self { V::F(v) } }
impl V { pub fn new() -> Self { V::I(0) } }
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let k = keys(&out);
    assert!(k.contains(&"<k::V as From<i64>>::from"), "{k:?}");
    assert!(k.contains(&"<k::V as From<f64>>::from"), "{k:?}");
    // An inherent impl needs no qualifying: there is only one.
    assert!(k.contains(&"k::V::new"), "{k:?}");
    assert!(
        out.report.collisions.is_empty(),
        "{:?}",
        out.report.collisions
    );

    // One `From` node, with the arguments on the edges instead.
    assert_eq!(out.nodes.iter().filter(|n| n.key == "From").count(), 1);
    let args: Vec<_> = out
        .edges
        .iter()
        .filter(|e| e.ty == "IMPLEMENTS")
        .filter_map(|e| e.props.get("impl"))
        .map(|d| format!("{:?}", d.value))
        .collect();
    assert_eq!(args.len(), 2, "{args:?}");
}

/// Hundreds of `new`/`from`/`len` share a name across a workspace. Resolving
/// them by the caller's own module first is what turns most of those calls from
/// ambiguous into edges.
/// An impl that spells its trait out in full must land on the real node. Going
/// by simple name alone, two traits sharing a name make the lookup ambiguous,
/// and the "external" stand-in it falls back to would carry a key one of them
/// already owns — replacing a real trait with a placeholder.
#[test]
fn a_fully_qualified_impl_finds_the_trait_it_names() {
    let t = Tree::new("qualified-impl");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod one;\npub mod two;\npub struct T;")
        .write("src/one.rs", "pub trait Shared {}")
        // A second trait of the same name, which is what defeats a by-name
        // lookup — and the impl below names its target in full.
        .write("src/two.rs", "pub trait Shared {}")
        .write(
            "src/uses.rs",
            "impl crate::one::Shared for crate::T {}\npub struct Other;",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        out.report.collisions.is_empty(),
        "{:?}",
        out.report.collisions
    );
    // The real trait, not a stand-in wearing its key.
    let node = out
        .nodes
        .iter()
        .find(|n| n.key == "k::one::Shared")
        .unwrap();
    assert!(
        !node.props.contains_key("external"),
        "a trait this crate defines is not external"
    );
}

/// `impl Database` sits in `api/snapshot.rs` while `Database` is declared in
/// `api/mod.rs`. Keying the block by the module holding it invents
/// `k::api::snapshot::Database`, and every method and `IMPLEMENTS` edge then
/// hangs off a node that does not exist — which a bulk write refuses outright.
#[test]
fn an_impl_finds_a_type_declared_in_another_file() {
    let t = Tree::new("impl-elsewhere");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod api;")
        .write("src/api/mod.rs", "pub mod snapshot;\npub struct Database;")
        .write(
            "src/api/snapshot.rs",
            "use super::Database;\nimpl Database { pub fn snapshot(&self) {} }",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let k = keys(&out);
    assert!(k.contains(&"k::api::Database::snapshot"), "{k:?}");
    assert!(
        !k.iter().any(|k| k.contains("snapshot::Database")),
        "the impl must not invent a type in its own module: {k:?}"
    );
    every_edge_has_endpoints(&out);
}

/// The one invariant the write path enforces: an edge naming a node that does
/// not exist is refused, so a graph with one is not merely imprecise but
/// unwritable.
fn every_edge_has_endpoints(out: &Preprocessed) {
    let keys: std::collections::BTreeSet<&str> = out.nodes.iter().map(|n| n.key.as_str()).collect();
    for e in &out.edges {
        assert!(keys.contains(e.src.as_str()), "dangling src: {}", e.src);
        assert!(keys.contains(e.dst.as_str()), "dangling dst: {}", e.dst);
    }
}

/// A method's calls have to resolve too, and they cannot until the block its
/// method belongs to has been resolved — which is why it happens in phases.
#[test]
fn a_method_body_resolves_its_calls() {
    let t = Tree::new("method-calls");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod thing;\npub fn helper() {}")
        .write(
            "src/thing.rs",
            "pub struct T;\nimpl T { pub fn go(&self) { crate::helper(); self.inner(); }\n pub fn inner(&self) {} }",
        );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let calls: Vec<(&str, &str)> = out
        .edges
        .iter()
        .filter(|e| e.ty == "CALLS")
        .map(|e| (e.src.as_str(), e.dst.as_str()))
        .collect();
    assert!(
        calls.contains(&("k::thing::T::go", "k::helper")),
        "{calls:?}"
    );
    // A method calling a sibling method resolves to the qualified key.
    assert!(
        calls.contains(&("k::thing::T::go", "k::thing::T::inner")),
        "{calls:?}"
    );
    every_edge_has_endpoints(&out);
}

/// An `impl` block's Self type is keyed by the path the source wrote, expanded
/// through the file's imports — the same key a call or a `use` would produce.
/// Keeping only `HashSet` would merge every crate's `HashSet` into one node and
/// would not match `std::collections::HashSet` written out elsewhere.
#[test]
fn an_external_impl_target_keeps_its_whole_path() {
    let t = Tree::new("impl-target-path");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::collections::BTreeMap;
pub trait Filter {}
impl Filter for std::collections::HashSet<u64> {}
impl Filter for BTreeMap<u64, u64> {}
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let k = keys(&out);

    // Written out in full at the impl site.
    assert!(k.contains(&"std::collections::HashSet"), "{k:?}");
    // Written short, but the file said where it came from.
    assert!(k.contains(&"std::collections::BTreeMap"), "{k:?}");
    assert!(!k.contains(&"HashSet"), "the bare tail is not a key: {k:?}");

    // A use site proves it is a type; it does not prove which kind, so `Type`
    // rather than a guess at `Struct` — `impl … for Option<T>` would be an enum.
    let hs = out
        .nodes
        .iter()
        .find(|n| n.key == "std::collections::HashSet")
        .unwrap();
    assert_eq!(hs.label, "Type");
    assert_eq!(hs.extra_labels, vec!["External".to_string()]);
    every_edge_has_endpoints(&out);
}

/// The same path is reached from several places that know different amounts: a
/// `use` says only that a name exists, while an `impl` says it is a type. The
/// import pass runs first, so plain insertion would let the site that knows
/// least win and leave the node labelled `External` alone.
#[test]
fn a_later_site_strengthens_an_external_node() {
    let t = Tree::new("strengthen");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        r#"
use std::collections::HashSet;
use std::fs;
pub trait Filter {}
impl Filter for HashSet<u64> {}
pub fn go() { fs::read(); }
"#,
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let node = |key: &str| out.nodes.iter().find(|n| n.key == key).unwrap();

    // Imported *and* used as an impl target: the impl knows more, and wins.
    let hs = node("std::collections::HashSet");
    assert_eq!(hs.label, "Type");
    assert_eq!(hs.extra_labels, vec!["External".to_string()]);

    // Imported *and* called through: the call proves it is callable.
    let read = node("std::fs::read");
    assert_eq!(read.label, "Function");
    assert_eq!(read.extra_labels, vec!["External".to_string()]);
    every_edge_has_endpoints(&out);
}

/// Nothing this repository's own tree produces may be unwritable.
#[test]
fn a_foreign_type_still_gets_an_endpoint() {
    let t = Tree::new("foreign");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n").write(
        "src/lib.rs",
        "use std::fmt;\npub trait Mine {}\nimpl Mine for Vec<u8> {}\nimpl fmt::Display for String { fn fmt(&self) {} }",
    );

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    every_edge_has_endpoints(&out);
    // `Vec` is nobody's here, so it exists as an external stand-in rather than
    // as a key nothing owns.
    let v = out.nodes.iter().find(|n| n.key == "Vec").expect("Vec node");
    assert_eq!(v.extra_labels, vec!["External".to_string()]);
}

#[test]
fn calls_resolve_to_the_nearest_definition() {
    let t = Tree::new("locality");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("src/lib.rs", "pub mod a;\npub mod b;")
        .write("src/a.rs", "pub fn helper() {}\npub fn go() { helper(); }")
        .write("src/b.rs", "pub fn helper() {}\npub fn go() { helper(); }");

    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    let call = |src: &str| {
        out.edges
            .iter()
            .find(|e| e.ty == "CALLS" && e.src == src)
            .map(|e| e.dst.clone())
    };
    assert_eq!(call("k::a::go").as_deref(), Some("k::a::helper"));
    assert_eq!(call("k::b::go").as_deref(), Some("k::b::helper"));
}
