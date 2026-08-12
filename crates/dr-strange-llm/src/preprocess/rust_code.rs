//! Rust source → facts (ROADMAP §11), at the interface level.
//!
//! Emits what a reader of the *interface* would want — functions, types,
//! traits, constants, what implements what, what contains what, what imports
//! what, and who calls whom.
//!
//! ## Keys are module paths
//!
//! An item's identity is `dr_strange_core::compute::exec::execute` — what a
//! Rust programmer calls it, and what a model reading the graph will recognise —
//! rather than the file it happens to live in. Files move; module paths are the
//! name. The crate prefix matters in a workspace, where `crate::` alone would
//! make two crates' `Database` the same node.
//!
//! ## Two limits worth stating
//!
//! **Calls** resolve by simple name within the tree being read: a call becomes
//! an edge only when exactly one known function has that name. A real call
//! graph needs import, generic and method resolution, which is a compiler's job.
//!
//! **Traits** an `impl` names are resolved the same way, and when they are not
//! defined here — `Display`, `Iterator`, a trait from another crate — they
//! become nodes labelled `["Trait", "External"]`, keyed by the path as written.
//! Dropping them would lose the most useful thing an `impl` block says.
//!
//! Whatever is left unresolved is counted in the report, so a thin graph is
//! explained rather than mistaken for the whole truth.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use dr_strange_core::{PropDesc, PropValue, Properties};
use quote::ToTokens;
use rayon::prelude::*;
use syn::visit::Visit;

use super::{Host, Input, Manifest, Preprocessed, Preprocessor};
use crate::digest::{DigestEdge, DigestNode};

/// The Rust preprocessor.
pub struct RustCode {
    /// Store each function's raw source under `_code`.
    ///
    /// Off by default. Every body stored is roughly a copy of the codebase in
    /// the graph, and properties live in one blob per record, so this makes
    /// every node read of that record decode the body too (arch/01 §4's
    /// large-inline-values concern, made real). Underscore-prefixed so that
    /// even when it is on it stays out of the embedding text and the schema
    /// summary the model reads: it is for retrieval, not for search.
    pub include_source: bool,
}

impl Preprocessor for RustCode {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "rust",
            version: "1",
            extensions: &["rs"],
        }
    }

    fn preprocess(&self, input: &Input<'_>, host: &dyn Host) -> Result<Preprocessed> {
        let files: Vec<(String, String)> = match input {
            Input::Document { name, bytes } => {
                vec![(
                    name.to_string(),
                    String::from_utf8_lossy(bytes).into_owned(),
                )]
            }
            Input::Files { paths } => paths
                .par_iter()
                .filter_map(|p| {
                    let bytes = host.read(p).ok()?;
                    Some((p.clone(), String::from_utf8(bytes).ok()?))
                })
                .collect(),
        };

        // Resolve each crate once, before the parallel parse: every file under
        // one `src/` shares a prefix, and reading its manifest per file would
        // be the same answer over and over.
        let crates = crate_names(&files, host);
        // Pointed at `…/foo/src`, the manifest is one level up and outside the
        // grant, so the host is asked what to call this instead of every item
        // keying itself `crate::…` — which would merge two crates' `api` into
        // one node the moment both are ingested into one plane.
        let fallback = host
            .label()
            .map(|l| l.replace('-', "_"))
            .unwrap_or_else(|| "crate".into());

        // CPU-bound, so parse in parallel — collected in path order, never
        // completion order, since the fold below fixes the node order.
        let parsed: Vec<FileFacts> = files
            .par_iter()
            .map(|(path, text)| {
                let module = module_path(path, &crates, &fallback);
                parse_file(path, &module, text, self.include_source)
            })
            .collect();

        Ok(assemble(parsed))
    }
}

/// One file's contribution, before cross-file resolution.
#[derive(Default)]
struct FileFacts {
    nodes: Vec<DigestNode>,
    edges: Vec<DigestEdge>,
    /// This file's own module path, which resolution is relative to.
    module: String,
    /// `(caller key, what it called)`, resolved once every file is known.
    calls: Vec<(String, Call)>,
    /// `impl` blocks, held whole until every file is known — see [`walk_impl`].
    impls: Vec<ImplBlock>,
    /// `(module, macro path, arguments)` for each item-position invocation —
    /// the places where items exist that this parse cannot see.
    macro_calls: Vec<(String, String, String)>,
    /// `(name introduced, path named)` for every `pub use` — the re-exports
    /// that make a facade path like `crate::cache` exist without anything
    /// being declared under it.
    reexports: Vec<(String, String, String)>,
    /// `(module, path as written)` for every `use`/`extern crate` in the file,
    /// **including those inside an inline `mod`** — whose imports are scoped to
    /// that module and would otherwise reach the node's property without ever
    /// reaching an edge.
    ///
    /// The nearest thing to name resolution a parser has: when a file says
    /// `use super::Database`, that *is* where its `impl Database` points.
    imports: Vec<(String, String)>,
    /// Each key's enclosing **module**, which is what resolution narrows by.
    ///
    /// Recorded rather than recovered from the key, because a trait method's
    /// key is `<Type as Trait>::m` and the module it lives in cannot be read
    /// back out of that.
    scopes: BTreeMap<String, String>,
    /// A file that would not parse at all.
    unparsed: Option<String>,
}

/// One call site, as much of it as the source actually spelled out.
///
/// A path call writes where it is going — `std::fs::read(…)`, `Vec::new()`,
/// or a bare name the file imported — and that path is enough to record the
/// callee even when it lives in another crate. A method call writes only
/// `.read()`, and the receiver's type is exactly what a parser does not know,
/// so those stay unresolved rather than becoming a guess.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct Call {
    /// The final segment — what resolution against local items matches on.
    name: String,
    /// The path as written, when the call site wrote one.
    path: Option<String>,
}

fn parse_file(path: &str, module: &str, text: &str, include_source: bool) -> FileFacts {
    let ast = match syn::parse_file(text) {
        Ok(a) => a,
        // A file that does not parse is reported, not fatal: a repository may
        // hold a deliberately broken fixture, and one of them should not sink
        // the whole ingest.
        Err(e) => {
            return FileFacts {
                unparsed: Some(format!("{path}: {e}")),
                ..Default::default()
            };
        }
    };

    let mut f = FileFacts::default();
    let mut imports = Vec::new();
    let mut reexports = Vec::new();
    collect_imports(&ast.items, module, &mut imports, &mut reexports);
    f.nodes.push(DigestNode {
        key: module.to_string(),
        label: "Module".into(),
        extra_labels: Vec::new(),
        props: props([
            ("path", path.to_string()),
            ("doc_comment", docs_of(&ast.attrs)),
            ("imports", imports.join(", ")),
        ]),
    });
    // Kept, not just rendered: an `impl Database` in this file is resolved
    // against what the file said it imported.
    f.imports = imports
        .iter()
        .map(|path| (module.to_string(), path.clone()))
        .collect();
    f.reexports = reexports;
    f.module = module.to_string();
    walk_items(&ast.items, module, include_source, &mut f);
    f
}

/// Every `use` and `extern crate` path, as written.
///
/// Kept as a property rather than edges: an import usually names something in
/// another crate, and minting a node for every `std::collections::BTreeMap`
/// would bury the graph in vocabulary nobody asked about. The list is still
/// searchable, which answers "what uses `rayon`" without the node explosion.
fn collect_imports(
    items: &[syn::Item],
    module: &str,
    out: &mut Vec<String>,
    reexports: &mut Vec<(String, String, String)>,
) {
    for item in items {
        match item {
            syn::Item::Use(u) => {
                let mut named = Vec::new();
                flatten_use(&u.tree, &mut String::new(), &mut named);
                // A re-export does not import a name, it *republishes* one:
                // `pub use api::cache;` at the crate root is what makes
                // `crate::cache` a path at all, and nothing declares an item
                // under it. Recorded so those paths can be followed later.
                //
                // `pub(crate) use` counts. It creates the facade path for the
                // whole crate, which is precisely the scope being parsed — and
                // it is how a crate exposes an internal type to its own
                // modules without exposing it to the world. A bare `use` does
                // not: it is private to its module, and treating it as a
                // re-export would invent paths nothing can actually name.
                if !matches!(u.vis, syn::Visibility::Inherited) {
                    reexports.extend(
                        named
                            .iter()
                            .filter(|(name, _)| name != "*")
                            .map(|(name, path)| (module.to_string(), name.clone(), path.clone())),
                    );
                }
                out.extend(named.into_iter().map(|(_, path)| path));
            }
            syn::Item::ExternCrate(e) => out.push(e.ident.to_string()),
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    reexports.sort();
    reexports.dedup();
}

/// Flatten a `use` tree into `(the name it introduces, the path it names)`.
fn flatten_use(tree: &syn::UseTree, prefix: &mut String, out: &mut Vec<(String, String)>) {
    match tree {
        syn::UseTree::Path(p) => {
            let saved = prefix.len();
            if !prefix.is_empty() {
                prefix.push_str("::");
            }
            prefix.push_str(&p.ident.to_string());
            flatten_use(&p.tree, prefix, out);
            prefix.truncate(saved);
        }
        // `use a::b::{self, C}` brings in `a::b` *itself*, not a child called
        // `self` — a path naming no item, which would become a node nothing
        // could ever be.
        syn::UseTree::Name(n) if n.ident == "self" => {
            if let Some(name) = prefix.rsplit("::").next().filter(|_| !prefix.is_empty()) {
                out.push((name.to_string(), prefix.clone()));
            }
        }
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            out.push((name.clone(), joined(prefix, &name)));
        }
        // `use a::b::{self as c}` — the same module under another name.
        syn::UseTree::Rename(r) if r.ident == "self" => {
            if !prefix.is_empty() {
                out.push((r.rename.to_string(), prefix.clone()));
            }
        }
        // `use x as y` introduces `y` while naming `x`.
        syn::UseTree::Rename(r) => {
            out.push((r.rename.to_string(), joined(prefix, &r.ident.to_string())))
        }
        syn::UseTree::Glob(_) => out.push(("*".into(), joined(prefix, "*"))),
        syn::UseTree::Group(g) => {
            for t in &g.items {
                flatten_use(t, prefix, out);
            }
        }
    }
}

fn joined(prefix: &str, leaf: &str) -> String {
    if prefix.is_empty() {
        leaf.to_string()
    } else {
        format!("{prefix}::{leaf}")
    }
}

/// Walk one item list, attributing everything to the module path `parent`.
fn walk_items(items: &[syn::Item], parent: &str, include_source: bool, f: &mut FileFacts) {
    for item in items {
        match item {
            syn::Item::Fn(func) => {
                let key = format!("{parent}::{}", func.sig.ident);
                let (label, mut p) =
                    fn_facts(&func.sig, &func.attrs, Some(&func.vis), Some(&func.block));
                if include_source {
                    add_source(&mut p, item);
                }
                f.nodes.push(DigestNode {
                    key: key.clone(),
                    label: label.into(),
                    extra_labels: Vec::new(),
                    props: p,
                });
                f.scopes.insert(key.clone(), parent.to_string());
                f.edges.push(edge(parent, &key, "CONTAINS"));
                collect_calls(&key, &func.block, f);
            }
            syn::Item::Struct(s) => {
                simple(f, parent, &s.ident, "Struct", &s.attrs, &s.vis);
                set_fields(f, s.fields.iter());
                set_non_exhaustive(f, &s.attrs);
            }
            syn::Item::Enum(e) => {
                simple(f, parent, &e.ident, "Enum", &e.attrs, &e.vis);
                // The variants *are* the enum: a node saying only `Expr` says
                // almost nothing, while its variants are the whole shape of it.
                // A list rather than a joined string, so `CONTAINS` asks about
                // membership of a variant instead of a substring of a blob.
                // An empty enum has no shape to describe, and an empty list
                // property is noise on every read of the node.
                if let Some(n) = f.nodes.last_mut()
                    && !e.variants.is_empty()
                {
                    n.props.insert(
                        "variants".into(),
                        PropDesc::described(
                            "the enum's variants, each with its fields as written",
                            PropValue::List(
                                e.variants
                                    .iter()
                                    .map(|v| PropValue::Str(variant_of(v)))
                                    .collect(),
                            ),
                        ),
                    );
                }
                set_non_exhaustive(f, &e.attrs);
            }
            syn::Item::Union(u) => {
                simple(f, parent, &u.ident, "Union", &u.attrs, &u.vis);
                // A union is a struct whose fields overlap in memory; the
                // fields are just as much its shape.
                set_fields(f, u.fields.named.iter());
                set_non_exhaustive(f, &u.attrs);
            }
            syn::Item::Const(c) => {
                let key = format!("{parent}::{}", c.ident);
                node(f, parent, key, "Const", &c.attrs, &c.vis, ty_of(&c.ty));
                set_value(f, &c.expr);
            }
            syn::Item::Static(s) => {
                let key = format!("{parent}::{}", s.ident);
                node(f, parent, key, "Static", &s.attrs, &s.vis, ty_of(&s.ty));
                set_value(f, &s.expr);
            }
            syn::Item::Type(t) => {
                let key = format!("{parent}::{}", t.ident);
                node(f, parent, key, "TypeAlias", &t.attrs, &t.vis, ty_of(&t.ty));
            }
            syn::Item::Macro(m) => match &m.ident {
                // `macro_rules! name` — a definition, so it is an item.
                Some(ident) => {
                    let key = format!("{parent}::{ident}");
                    let mut p = props([("doc_comment", docs_of(&m.attrs))]);
                    if include_source {
                        add_source(&mut p, item);
                    }
                    f.nodes.push(DigestNode {
                        key: key.clone(),
                        label: "Macro".into(),
                        extra_labels: Vec::new(),
                        props: p,
                    });
                    f.scopes.insert(key.clone(), parent.to_string());
                    f.edges.push(edge(parent, &key, "CONTAINS"));
                }
                // An invocation *at item position* — `expr_from_literal!(bool,
                // i32);` — which defines items this parse will never see.
                //
                // Nothing expands it. Expansion is the compiler's: `macro_rules`
                // has hygiene and recursion, and a proc macro is arbitrary code
                // that must actually be run. So the items it declares are simply
                // absent from the graph.
                //
                // Recorded anyway, because a blind spot that is marked can be
                // reasoned about and a silent one cannot: the edge says this
                // module invokes that macro, and carries the arguments, which
                // is usually the shape of what was generated.
                None => f.macro_calls.push((
                    parent.to_string(),
                    path_of(&m.mac.path),
                    tidy(&m.mac.tokens.to_string()),
                )),
            },
            syn::Item::Trait(t) => {
                let key = format!("{parent}::{}", t.ident);
                node(
                    f,
                    parent,
                    key.clone(),
                    "Trait",
                    &t.attrs,
                    &t.vis,
                    // Likewise no signature: it would be the trait's own name.
                    String::new(),
                );
                for ti in &t.items {
                    if let syn::TraitItem::Fn(m) = ti {
                        let mkey = format!("{key}::{}", m.sig.ident);
                        // No visibility: a trait's items are as public as the
                        // trait. A default body is where the bindings come from
                        // when there is one.
                        let (label, props) = fn_facts(&m.sig, &m.attrs, None, m.default.as_ref());
                        f.nodes.push(DigestNode {
                            key: mkey.clone(),
                            label: label.into(),
                            extra_labels: Vec::new(),
                            props,
                        });
                        f.scopes.insert(mkey.clone(), parent.to_string());
                        f.edges.push(edge(&key, &mkey, "HAS_METHOD"));
                    }
                }
            }
            syn::Item::Impl(im) => walk_impl(im, parent, include_source, f),
            // An inline `mod` extends the path, which is exactly why two
            // `fn new` in sibling modules are two different nodes.
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let key = format!("{parent}::{}", m.ident);
                    let mut imports = Vec::new();
                    collect_imports(inner, &key, &mut imports, &mut f.reexports);
                    f.imports
                        .extend(imports.iter().map(|path| (key.clone(), path.clone())));
                    f.nodes.push(DigestNode {
                        key: key.clone(),
                        label: "Module".into(),
                        extra_labels: Vec::new(),
                        props: props([
                            ("doc_comment", docs_of(&m.attrs)),
                            ("visibility", visibility(&m.vis)),
                            ("imports", imports.join(", ")),
                        ]),
                    });
                    f.edges.push(edge(parent, &key, "CONTAINS"));
                    walk_items(inner, &key, include_source, f);
                }
            }
            _ => {}
        }
    }
}

/// Collect an `impl` block, resolving neither end of it.
///
/// Nothing in an `impl` is reliably declared in the module holding it. The
/// trait usually is not — `Display` lives in std, a project trait a module
/// away — and **neither is the type**: `impl Database` sits in
/// `api/snapshot.rs` while `Database` is declared in `api/mod.rs`. Assuming
/// `{parent}::{name}` for either builds a key nothing owns, which is not just
/// an imprecise edge but a *dangling* one, and a bulk write refuses those.
///
/// So the block is held whole until [`assemble`], where every file is known.
/// Its method keys wait with it, since each is built from the resolved type.
fn walk_impl(im: &syn::ItemImpl, parent: &str, include_source: bool, f: &mut FileFacts) {
    let Some(self_name) = type_name(&im.self_ty) else {
        return;
    };

    let methods = im
        .items
        .iter()
        .filter_map(|ii| match ii {
            syn::ImplItem::Fn(m) => {
                let (label, mut props) = fn_facts(&m.sig, &m.attrs, Some(&m.vis), Some(&m.block));
                if include_source {
                    add_source(&mut props, ii);
                }
                Some(ImplMethod {
                    name: m.sig.ident.to_string(),
                    label,
                    props,
                    calls: call_names(&m.block),
                })
            }
            _ => None,
        })
        .collect();

    f.impls.push(ImplBlock {
        module: parent.to_string(),
        self_name,
        trait_base: im.trait_.as_ref().map(|(_, p, _)| base_path(p)),
        trait_full: im.trait_.as_ref().map(|(_, p, _)| path_of(p)),
        methods,
    });
}

/// An `impl` block, held whole until its type and trait can be resolved.
struct ImplBlock {
    /// The module it was written in — where resolution starts looking.
    module: String,
    self_name: String,
    /// The trait path without generic arguments, which identifies the node.
    trait_base: Option<String>,
    /// The same path with its arguments, which tells two impls apart.
    trait_full: Option<String>,
    methods: Vec<ImplMethod>,
}

struct ImplMethod {
    name: String,
    /// `Method` when it takes a `self` receiver, `Function` otherwise — an
    /// associated function like `Database::open` is not a method.
    label: &'static str,
    props: Properties,
    calls: BTreeSet<Call>,
}

/// Record every call this body makes, for later resolution.
fn collect_calls(caller: &str, block: &syn::Block, f: &mut FileFacts) {
    for c in call_names(block) {
        f.calls.push((caller.to_string(), c));
    }
}

/// Everything this body calls, deduplicated and in a stable order.
fn call_names(block: &syn::Block) -> BTreeSet<Call> {
    struct Calls<'a>(&'a mut BTreeSet<Call>);
    impl<'ast> Visit<'ast> for Calls<'_> {
        fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
            if let syn::Expr::Path(p) = &*node.func
                && let Some(last) = p.path.segments.last()
            {
                self.0.insert(Call {
                    name: last.ident.to_string(),
                    path: Some(path_of(&p.path)),
                });
            }
            syn::visit::visit_expr_call(self, node);
        }
        fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
            self.0.insert(Call {
                name: node.method.to_string(),
                path: None,
            });
            syn::visit::visit_expr_method_call(self, node);
        }
    }
    let mut names = BTreeSet::new();
    Calls(&mut names).visit_block(block);
    names
}

/// Names bound by `let` in this body, deduplicated and sorted.
///
/// A flat list rather than nodes: statement-level *structure* is what §11
/// rules out, and a mid-sized repository would add hundreds of thousands of
/// nodes. The names themselves are cheap and make "which function binds
/// `write_gate`" answerable.
fn bindings_of(block: &syn::Block) -> String {
    struct Locals<'a>(&'a mut BTreeSet<String>);
    impl<'ast> Visit<'ast> for Locals<'_> {
        fn visit_local(&mut self, node: &'ast syn::Local) {
            collect_pat(&node.pat, self.0);
            syn::visit::visit_local(self, node);
        }
    }
    fn collect_pat(pat: &syn::Pat, out: &mut BTreeSet<String>) {
        match pat {
            syn::Pat::Ident(i) => {
                out.insert(i.ident.to_string());
                if let Some((_, sub)) = &i.subpat {
                    collect_pat(sub, out);
                }
            }
            syn::Pat::Tuple(t) => t.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::TupleStruct(t) => t.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::Struct(s) => s.fields.iter().for_each(|f| collect_pat(&f.pat, out)),
            syn::Pat::Slice(s) => s.elems.iter().for_each(|p| collect_pat(p, out)),
            syn::Pat::Reference(r) => collect_pat(&r.pat, out),
            syn::Pat::Type(t) => collect_pat(&t.pat, out),
            syn::Pat::Or(o) => o.cases.iter().for_each(|p| collect_pat(p, out)),
            _ => {}
        }
    }
    let mut names = BTreeSet::new();
    Locals(&mut names).visit_block(block);
    names.into_iter().collect::<Vec<_>>().join(", ")
}

/// Every re-export in the tree, as `facade path -> what it republishes`.
///
/// Built before anything else is resolved, since a facade declared in `lib.rs`
/// is used by files parsed long before it.
///
/// In two passes, because a re-export's *target* is written relative to the
/// module that wrote it and may itself be another facade: `lib.rs` says
/// `pub use compute::{Expr}` while `compute/mod.rs` says `pub use expr::Expr`,
/// so the first target only makes sense once the second alias is known. One
/// pass would settle `compute::Expr` against declared items alone, find
/// nothing, and leave a relative path naming no node at all.
fn build_aliases(files: &[FileFacts], declared: &BTreeSet<String>) -> BTreeMap<String, String> {
    let raw: Vec<(String, &String, String)> = files
        .iter()
        .flat_map(|f| {
            let imports = import_index(&f.imports);
            f.reexports.iter().map(move |(module, name, written)| {
                (
                    format!("{module}::{name}"),
                    module,
                    expand_path(written, &imports, module),
                )
            })
        })
        .collect();

    let alias_keys: BTreeSet<&str> = raw.iter().map(|(key, _, _)| key.as_str()).collect();
    let known = |p: &str| declared.contains(p) || alias_keys.contains(p);
    raw.iter()
        .map(|(key, module, expanded)| {
            let nested = format!("{module}::{expanded}");
            let target = match (known(expanded), known(&nested)) {
                (true, _) => expanded.clone(),
                (false, true) => nested,
                _ => expanded.clone(),
            };
            (key.clone(), target)
        })
        .collect()
}

/// Resolve calls and impl blocks across every file, then fold the facts.
///
/// In phases, because each one needs the last to be complete: an `impl` block
/// cannot be resolved until every type is known, its methods are not functions
/// that calls can resolve against until the block is, and an edge may not be
/// emitted until both its endpoints exist — a bulk write refuses a dangling one.
fn assemble(parsed: Vec<FileFacts>) -> Preprocessed {
    let mut unparsed = Vec::new();
    let mut files = Vec::new();
    for f in parsed {
        match f.unparsed {
            Some(why) => unparsed.push(why),
            None => files.push(f),
        }
    }

    // ---- phase 1: index what the files declare ----------------------------
    //
    // simple name → the keys defining it. Ambiguity is left unresolved rather
    // than guessed at.
    let mut fns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut traits: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut scopes: BTreeMap<String, String> = BTreeMap::new();
    // Every key declared anywhere, so a path written out in full can be matched
    // exactly rather than by its last segment.
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for f in &files {
        scopes.extend(f.scopes.iter().map(|(k, v)| (k.clone(), v.clone())));
        for n in &f.nodes {
            declared.insert(n.key.clone());
            let Some(simple) = n.key.rsplit("::").next() else {
                continue;
            };
            match n.label.as_str() {
                // Both kinds are callable and both are reachable by name, so
                // the split into `Function`/`Method` must not reach resolution.
                "Function" | "Method" => fns.entry(simple.into()).or_default().push(n.key.clone()),
                "Trait" => traits.entry(simple.into()).or_default().push(n.key.clone()),
                "Struct" | "Enum" | "Union" | "TypeAlias" => {
                    types.entry(simple.into()).or_default().push(n.key.clone())
                }
                _ => {}
            }
        }
    }

    // ---- phase 2: resolve impl blocks -------------------------------------
    let mut external: BTreeMap<String, DigestNode> = BTreeMap::new();
    let mut impl_nodes: Vec<DigestNode> = Vec::new();
    let mut impl_edges: Vec<DigestEdge> = Vec::new();
    let mut impl_calls: Vec<(String, Call)> = Vec::new();
    // module key -> what its `use` lines resolved to, replacing the as-written
    // list the parse put on the node.
    let mut import_targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut macro_invocations = 0usize;

    let aliases = build_aliases(&files, &declared);

    for f in &files {
        let imports = import_index(&f.imports);

        // Every `use` becomes an edge to what it names. An import that lands
        // on something this tree declares points at the real node; one that
        // does not — `std::collections::BTreeMap`, a dependency's type — gets
        // a node of its own, marked external and holding nothing but its path.
        // We do not read that code, so the path *is* the fact.
        //
        // A glob names no single target, so it stays in the `imports` property
        // and produces no edge rather than an invented one.
        // Grouped by the module that wrote the `use`, not by the file: an
        // inline `mod tests` has imports of its own, and they belong to it.
        for (module, written) in &f.imports {
            if written.ends_with("*") {
                // A glob names no single target, so it produces no edge. It
                // stays in the list as written, because the file did write it.
                import_targets
                    .entry(module.clone())
                    .or_default()
                    .push(written.clone());
                continue;
            }
            let target = resolve_path(
                &expand_path(written, &imports, module),
                module,
                &aliases,
                &declared,
            );
            // The list records the path **resolved**, not as written. A reader
            // of `crate::cache::GraphReader` cannot tell which crate's it is,
            // and — the reason this matters — a key that is not the node's key
            // cannot be followed to it. `as_written` on the edge keeps the
            // original for anyone who wants it.
            import_targets
                .entry(module.clone())
                .or_default()
                .push(target.clone());
            if !declared.contains(&target) {
                // `External` alone: a `use` says a name was brought into scope,
                // not whether it is a type, a trait, a function or a module. A
                // call site proves more, and says so.
                note_external(&mut external, &target, None);
            }
            let mut e = edge(module, &target, "IMPORTS");
            if written != &target {
                e.props.insert(
                    "as_written".into(),
                    PropDesc::new(PropValue::Str(written.clone())),
                );
            }
            impl_edges.push(e);
        }

        // Item-position macro invocations: an edge to the macro, so the places
        // where unseen items are defined are findable rather than invisible.
        for (module, path, args) in &f.macro_calls {
            let target = resolve_path(
                &expand_path(path, &imports, module),
                module,
                &aliases,
                &declared,
            );
            if !declared.contains(&target) {
                note_external(&mut external, &target, Some("Macro"));
            }
            let mut e = edge(module, &target, "INVOKES");
            if !args.is_empty() {
                e.props.insert(
                    "arguments".into(),
                    PropDesc::described(
                        "what the macro was given — usually the shape of what it generated",
                        PropValue::Str(args.clone()),
                    ),
                );
            }
            impl_edges.push(e);
            macro_invocations += 1;
        }

        for b in &f.impls {
            let self_key = resolve(
                &b.self_name,
                &b.module,
                &imports,
                &types,
                &declared,
                &scopes,
                "Type",
                &mut external,
            );

            if let (Some(base), Some(full)) = (&b.trait_base, &b.trait_full) {
                let trait_key = resolve(
                    base,
                    &b.module,
                    &imports,
                    &traits,
                    &declared,
                    &scopes,
                    "Trait",
                    &mut external,
                );
                let mut e = edge(&self_key, &trait_key, "IMPLEMENTS");
                // `From<i64>` on the edge rather than as a second `From` node:
                // which implementation is a fact about this impl, not a new
                // word in the vocabulary.
                if full != base {
                    e.props
                        .insert("impl".into(), PropDesc::new(PropValue::Str(full.clone())));
                }
                impl_edges.push(e);
            }

            for m in &b.methods {
                // `<Type as Trait>::method` for a trait impl — real qualified
                // path syntax, and the only thing keeping six `impl From<…> for
                // PropValue` blocks from all claiming the key `PropValue::from`.
                // An inherent impl needs no qualifying: there is only one.
                let mkey = match &b.trait_full {
                    Some(t) => format!("<{self_key} as {t}>::{}", m.name),
                    None => format!("{self_key}::{}", m.name),
                };
                impl_nodes.push(DigestNode {
                    key: mkey.clone(),
                    label: m.label.into(),
                    extra_labels: Vec::new(),
                    props: m.props.clone(),
                });
                impl_edges.push(edge(&self_key, &mkey, "HAS_METHOD"));
                scopes.insert(mkey.clone(), b.module.clone());
                fns.entry(m.name.clone()).or_default().push(mkey.clone());
                impl_calls.extend(m.calls.iter().map(|c| {
                    let path = c
                        .path
                        .as_deref()
                        .map(|p| expand_path(p, &imports, &b.module));
                    (
                        mkey.clone(),
                        Call {
                            path,
                            name: c.name.clone(),
                        },
                    )
                }));
            }
        }
    }

    // ---- phase 3: fold the nodes ------------------------------------------
    let mut out = Preprocessed::default();
    let (mut unresolved, mut ambiguous, mut cfg_dupes) = (0usize, 0usize, 0usize);
    let mut external_calls = 0usize;
    let mut seen: BTreeSet<String> = BTreeSet::new();

    // A key seen twice is nearly always two `#[cfg]` alternatives of one item —
    // `const DISK` for each backend, say. That is ordinary Rust, not the
    // plugin-versus-plugin collision the router reports, so it is settled here
    // and merely counted.
    for mut n in files
        .iter_mut()
        .flat_map(|f| std::mem::take(&mut f.nodes))
        .chain(impl_nodes)
    {
        // Swap the as-written import list for the resolved one, now that every
        // file is known and each `use` has a target. Done here so the property
        // and the `IMPORTS` edges cannot drift: both come from the same pass.
        if let Some(targets) = import_targets.get(&n.key)
            && n.label == "Module"
        {
            n.props.insert(
                "imports".into(),
                PropDesc::new(PropValue::Str(targets.join(", "))),
            );
        }
        if seen.insert(n.key.clone()) {
            out.nodes.push(n);
        } else {
            cfg_dupes += 1;
        }
    }
    // ---- phase 4: resolve calls -------------------------------------------
    for f in &mut files {
        out.edges.append(&mut f.edges);
    }
    out.edges.extend(impl_edges);

    // Expand each written path while the file that wrote it is still known:
    // `fs::read()` means `std::fs::read` only to the file that said `use
    // std::fs`. Impl methods arrive already expanded, for the same reason.
    let mut pending: Vec<(String, Call)> = Vec::new();
    for f in &mut files {
        let imports = import_index(&f.imports);
        let module = f.module.clone();
        for (caller, c) in std::mem::take(&mut f.calls) {
            let path = c.path.as_deref().map(|p| expand_path(p, &imports, &module));
            pending.push((caller, Call { path, ..c }));
        }
    }
    pending.extend(impl_calls);

    for (caller, call) in pending {
        // A path written out in full names its target exactly, which beats
        // matching on a last segment two functions may share.
        if let Some(p) = &call.path
            && declared.contains(p)
        {
            out.edges.push(edge(&caller, p, "CALLS"));
            continue;
        }

        if let Some(cands) = fns.get(&call.name) {
            match pick(&caller, cands, &scopes) {
                Some(one) => {
                    out.edges.push(edge(&caller, one, "CALLS"));
                    continue;
                }
                // Defined here under this name, but more than one candidate is
                // equally close — a guess would be worse than a gap.
                None => {
                    ambiguous += 1;
                    continue;
                }
            }
        }

        match &call.path {
            // Nothing here defines it and the call site wrote a path: it is
            // `std::fs::read`, or a dependency's. We do not read its code and
            // have no signature for it, so the node is the path and the fact
            // that it was called — which is what "this crate uses that" needs.
            Some(p) => {
                note_external(&mut external, p, Some("Function"));
                out.edges.push(edge(&caller, p, "CALLS"));
                external_calls += 1;
            }
            // A method call writes only `.read()`, and the receiver's type is
            // exactly what a parser cannot know. Counted, never guessed.
            None => unresolved += 1,
        }
    }

    // Last, because resolving calls is itself a source of external nodes: a
    // path into std is discovered at its call site, not before. Never written
    // over a real one — a type this crate defines is not external, and a
    // stand-in carrying its key would replace it.
    out.nodes
        .extend(external.into_values().filter(|n| !seen.contains(&n.key)));

    // Say what was not emitted, so a thin call graph is explained rather than
    // taken for the whole truth.
    if unresolved > 0 {
        out.report.notes.push(format!(
            "{unresolved} method call(s) left unresolved: a call written `.read()` \
             names no path, and the receiver's type is what a parser cannot know"
        ));
    }
    if external_calls > 0 {
        out.report.notes.push(format!(
            "{external_calls} call(s) into other crates and the standard library, \
             recorded as external nodes carrying the path and nothing else"
        ));
    }
    if ambiguous > 0 {
        out.report.notes.push(format!(
            "{ambiguous} call(s) matched more than one function by name and were \
             left out: resolving them needs import and generic resolution"
        ));
    }
    if macro_invocations > 0 {
        out.report.notes.push(format!(
            "{macro_invocations} macro invocation(s) at item level were not expanded — \
             whatever items they define are absent from the graph, and an `INVOKES` \
             edge marks where"
        ));
    }
    if cfg_dupes > 0 {
        out.report.notes.push(format!(
            "{cfg_dupes} item(s) shared a key with an earlier one and the first \
             was kept — usually two `#[cfg]` alternatives of the same item"
        ));
    }
    if !unparsed.is_empty() {
        out.report.skipped += unparsed.len();
        out.report
            .notes
            .push(format!("{} file(s) did not parse", unparsed.len()));
        out.report.notes.extend(unparsed);
    }
    out
}

/// A file's `use` statements as `simple name → path as written`.
///
/// This is the nearest thing to name resolution a parser has. `syn` sees one
/// file's tokens: `impl Database` is the bare identifier `Database` and nothing
/// more, and turning that into `dr_strange_core::api::Database` is name
/// resolution — the module tree, `use` in scope, globs, re-exports — which is a
/// compiler's job. But a file that says `use super::Database` has *written down*
/// where its `Database` comes from, and that is worth reading.
fn import_index(imports: &[(String, String)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (_, path) in imports {
        if let Some(last) = path.rsplit("::").next()
            && last != "*"
        {
            // First writing wins: a later glob or a re-import should not
            // displace an explicit one.
            out.entry(last.to_string()).or_insert_with(|| path.clone());
        }
    }
    out
}

/// Rewrite a written `use` path into this graph's key namespace.
///
/// `crate::`, `super::` and `self::` are relative to the file that wrote them,
/// and the key namespace is rooted at the package name instead.
fn absolute(written: &str, module: &str) -> String {
    let crate_root = module.split("::").next().unwrap_or(module);
    match written.split_once("::") {
        Some(("crate", rest)) => format!("{crate_root}::{rest}"),
        Some(("self", rest)) => format!("{module}::{rest}"),
        Some(("super", rest)) => match module.rsplit_once("::") {
            Some((parent, _)) => format!("{parent}::{rest}"),
            None => format!("{crate_root}::{rest}"),
        },
        _ => written.to_string(),
    }
}

/// The node a written path names, as far as this tree can tell.
///
/// Two readings are tried, in order: the path as expanded, then the same path
/// read as relative to the module that wrote it — `use snapshot::SnapshotStats`
/// in `api/mod.rs` names a child module, which neither an import nor a `crate::`
/// prefix will expand. Each is followed through re-exports, since a facade is
/// just as good an answer as a declaration and is far more common at the top of
/// a crate.
///
/// Falls back to the expanded path, which becomes an external node: better a
/// node keyed by what the source wrote than an edge pointing nowhere.
fn resolve_path(
    expanded: &str,
    module: &str,
    aliases: &BTreeMap<String, String>,
    declared: &BTreeSet<String>,
) -> String {
    [expanded.to_string(), format!("{module}::{expanded}")]
        .into_iter()
        .find_map(|candidate| follow_reexports(&candidate, aliases, declared))
        .unwrap_or_else(|| expanded.to_string())
}

/// Follow `pub use` re-exports until the path names something declared.
///
/// A facade is the normal shape of a Rust crate: `lib.rs` writes
/// `pub use api::cache;` and from then on everything says `crate::cache::…`,
/// while the items are declared under `api::cache`. Nothing is declared at the
/// facade path, so without following it every such import becomes a node that
/// looks external and is not — the crate's own type, filed under a foreign key.
///
/// Bounded rather than recursive-until-fixpoint: re-exports chain through
/// several layers in a large crate, and a mutual pair must not spin.
fn follow_reexports(
    key: &str,
    aliases: &BTreeMap<String, String>,
    declared: &BTreeSet<String>,
) -> Option<String> {
    let mut current = key.to_string();
    for _ in 0..8 {
        if declared.contains(&current) {
            return Some(current);
        }
        // The longest matching prefix, so a re-export of a module beats one of
        // its parent.
        let mut probe = current.as_str();
        let rewritten = loop {
            if let Some(target) = aliases.get(probe) {
                break Some(format!("{target}{}", &current[probe.len()..]));
            }
            match probe.rsplit_once("::") {
                Some((parent, _)) => probe = parent,
                None => break None,
            }
        };
        current = rewritten?;
    }
    None
}

/// A path as the file that wrote it meant it.
///
/// `fs::read` is `std::fs::read` only to a file that said `use std::fs`, and
/// `crate::` is relative to whoever wrote it. Anything already absolute —
/// `std::fs::read` written out in full — passes through untouched.
fn expand_path(path: &str, imports: &BTreeMap<String, String>, module: &str) -> String {
    if path.starts_with("crate::") || path.starts_with("super::") || path.starts_with("self::") {
        return absolute(path, module);
    }
    let (head, rest) = path.split_once("::").unwrap_or((path, ""));
    match imports.get(head) {
        Some(written) => {
            let base = absolute(written, module);
            if rest.is_empty() {
                base
            } else {
                format!("{base}::{rest}")
            }
        }
        None => path.to_string(),
    }
}

/// The key an `impl` block's type or trait refers to.
///
/// Three answers, in descending order of how much the source actually told us:
/// what the file imported, then the nearest declaration of that name, then a
/// node marked `external` — because `impl Display for X` is worth recording
/// even though `Display` lives in std, and because an edge whose endpoint does
/// not exist is refused by the write path rather than merely being imprecise.
#[allow(clippy::too_many_arguments)]
fn resolve(
    name: &str,
    module: &str,
    imports: &BTreeMap<String, String>,
    by_name: &BTreeMap<String, Vec<String>>,
    declared: &BTreeSet<String>,
    scopes: &BTreeMap<String, String>,
    external_label: &str,
    external: &mut BTreeMap<String, DigestNode>,
) -> String {
    // Written out in full, or imported: either way the source named it.
    let simple = name.rsplit("::").next().unwrap_or(name);
    for candidate in [
        Some(absolute(name, module)),
        imports.get(simple).map(|w| absolute(w, module)),
    ]
    .into_iter()
    .flatten()
    {
        if declared.contains(&candidate) {
            return candidate;
        }
    }

    if let Some(cands) = by_name.get(simple)
        && let Some(one) = pick_in_scope(module, cands, scopes)
    {
        return one.clone();
    }

    // Nothing here declares it, so it becomes a stand-in keyed by the **path**,
    // expanded through this file's imports exactly as a call into another crate
    // is. `HashSet` alone would merge every crate's `HashSet` into one node and
    // would not match `std::collections::HashSet` written out at another site.
    let key = expand_path(name, imports, module);
    note_external(external, &key, Some(external_label));
    key
}

/// Choose among same-named functions by locality.
///
/// A bare name is resolved against the caller's own module first, then each
/// enclosing one, taking the innermost scope that holds exactly one candidate.
/// That is roughly what a reader does, and it is what turns the hundreds of
/// `new`, `from` and `len` in a workspace from ambiguous into edges.
///
/// A name still ambiguous in the narrowest scope that has it is left
/// unresolved. Choosing between two equally close candidates needs import and
/// generic resolution — a compiler's job — and a guessed edge is worse than a
/// missing one, because nothing downstream can tell it apart from a known fact.
fn pick<'a>(
    caller: &str,
    cands: &'a [String],
    scopes: &BTreeMap<String, String>,
) -> Option<&'a String> {
    if let [one] = cands {
        return Some(one);
    }
    pick_in_scope(scopes.get(caller)?, cands, scopes)
}

/// The same narrowing, starting from a module rather than from a caller.
fn pick_in_scope<'a>(
    from: &str,
    cands: &'a [String],
    scopes: &BTreeMap<String, String>,
) -> Option<&'a String> {
    if let [one] = cands {
        return Some(one);
    }
    let mut scope = from;
    loop {
        let within: Vec<&String> = cands
            .iter()
            .filter(|c| {
                scopes
                    .get(*c)
                    .is_some_and(|s| s == scope || s.starts_with(&format!("{scope}::")))
            })
            .collect();
        match within.as_slice() {
            [one] => return Some(one),
            [] => scope = scope.rsplit_once("::")?.0,
            _ => return None,
        }
    }
}

// ---- module paths ---------------------------------------------------------

/// Resolve the crate name for every `src/` root in this batch, once each.
///
/// The declared `[package] name` is preferred over the directory name: cargo
/// convention makes them agree, but the manifest is what decides, and a crate
/// whose directory disagrees would otherwise get keys nobody could match.
fn crate_names(files: &[(String, String)], host: &dyn Host) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (path, _) in files {
        let Some(dir) = crate_dir(path) else { continue };
        if out.contains_key(&dir) {
            continue;
        }
        let manifest = if dir.is_empty() {
            "Cargo.toml".to_string()
        } else {
            format!("{dir}/Cargo.toml")
        };
        let name = host
            .read(&manifest)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|t| package_name(&t))
            .or_else(|| dir.rsplit('/').next().map(str::to_string))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "crate".into());
        out.insert(dir, name.replace('-', "_"));
    }
    out
}

/// The cargo target directories a `.rs` file can sit under.
const TARGET_ROOTS: &[&str] = &["src/", "benches/", "tests/", "examples/"];

/// Split a path into its package directory, the target root, and the rest.
///
/// The **leftmost** root wins, not the rightmost: `crates/foo/src/tests/mod.rs`
/// is a module called `tests` inside the library, not an integration test, and
/// matching from the right would call it one.
fn target_root(path: &str) -> Option<(String, &'static str, &str)> {
    let mut best: Option<(usize, &'static str)> = None;
    for root in TARGET_ROOTS {
        let mut from = 0;
        while let Some(rel) = path[from..].find(root) {
            let at = from + rel;
            // Only at a segment boundary, so `mysrc/` is not a target root.
            if at == 0 || path.as_bytes()[at - 1] == b'/' {
                if best.is_none_or(|(b, _)| at < b) {
                    best = Some((at, root));
                }
                break;
            }
            from = at + 1;
        }
    }
    let (at, root) = best?;
    Some((
        path[..at].trim_end_matches('/').to_string(),
        root,
        &path[at + root.len()..],
    ))
}

/// The directory holding a file's target root, or `None` when it has none.
fn crate_dir(path: &str) -> Option<String> {
    target_root(path).map(|(dir, _, _)| dir)
}

/// `name = "..."` from a manifest's `[package]` section.
///
/// A line scan rather than a TOML parse: this crate has no TOML dependency and
/// the alternative is adding one to read a single field.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(rest) = line.strip_prefix("name")
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// The module path a file defines: `crates/foo/src/a/b.rs` → `foo::a::b`.
///
/// `lib.rs`, `main.rs` and `mod.rs` name their parent rather than themselves,
/// which is the whole point of those filenames.
fn module_path(path: &str, crates: &BTreeMap<String, String>, fallback: &str) -> String {
    let split = target_root(path);
    let mut out = split
        .as_ref()
        .and_then(|(dir, _, _)| crates.get(dir).cloned())
        .unwrap_or_else(|| fallback.to_string());

    // A lone file under no target root — its own stem is the best name there is.
    let (root, rest) = split.map_or(("", path), |(_, r, rest)| (r, rest));
    let rest = rest.strip_suffix(".rs").unwrap_or(rest);

    let mut segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if matches!(segs.last(), Some(&"lib") | Some(&"main") | Some(&"mod")) {
        segs.pop();
    }

    // A bench, test or example is its own crate root to rustc, so nothing in it
    // has a path from the library at all. Keyed under the package and the
    // target directory instead: not a path rustc would accept, but it keeps two
    // packages' `benches/graph.rs` apart and puts them where a reader looks.
    if !root.is_empty() && root != "src/" {
        out.push_str("::");
        out.push_str(root.trim_end_matches('/'));
    }

    for s in segs {
        out.push_str("::");
        out.push_str(s);
    }
    out
}

// ---- small helpers --------------------------------------------------------

/// A named item whose declaration line says nothing the key does not.
///
/// No `signature`: for a struct, enum, union or trait it would be the bare
/// name, which the key already ends with — a property repeating the identity
/// of the node it sits on. What is worth recording about these is their
/// *contents* (an enum's `variants`) or their promises (`non_exhaustive`), and
/// those are attached by the caller.
fn simple(
    f: &mut FileFacts,
    parent: &str,
    ident: &syn::Ident,
    label: &str,
    attrs: &[syn::Attribute],
    vis: &syn::Visibility,
) {
    let key = format!("{parent}::{ident}");
    node(f, parent, key, label, attrs, vis, String::new());
}

fn node(
    f: &mut FileFacts,
    parent: &str,
    key: String,
    label: &str,
    attrs: &[syn::Attribute],
    vis: &syn::Visibility,
    signature: String,
) {
    f.nodes.push(DigestNode {
        key: key.clone(),
        label: label.into(),
        extra_labels: Vec::new(),
        props: props([
            ("signature", signature),
            ("doc_comment", docs_of(attrs)),
            ("visibility", visibility(vis)),
        ]),
    });
    f.scopes.insert(key.clone(), parent.to_string());
    f.edges.push(edge(parent, &key, "CONTAINS"));
}

/// A node standing in for something this tree does not declare.
///
/// Two labels rather than a property: `["Trait", "External"]` says both what it
/// is and that it is not ours, and both are things a reader asks for by label —
/// `MATCH (t:Trait)` should find `Display`, and `MATCH (n:External)` should find
/// everything foreign whatever its kind.
///
/// `External` is the whole answer when only a `use` was seen, since bringing a
/// name into scope says nothing about what kind of thing it is.
///
/// No `path` property: for a stand-in the key *is* the path, and a property
/// repeating the key states the same fact twice.
/// Record an external stand-in, **strengthening** one already recorded.
///
/// The same path is reached from several places, and they do not know the same
/// amount: a `use` says only that a name was brought into scope, while an
/// `impl` block or a call site says what kind of thing it is. Since the import
/// pass runs first, plain insertion would let the site that knows least win and
/// leave `std::collections::HashSet` labelled `External` alone even though an
/// `impl` proved it a type.
///
/// Order-independent by construction: a kind, once known, replaces no-kind and
/// nothing else, so which pass ran first cannot change the result.
fn note_external(external: &mut BTreeMap<String, DigestNode>, key: &str, kind: Option<&str>) {
    match external.get_mut(key) {
        Some(existing) if existing.extra_labels.is_empty() && kind.is_some() => {
            *existing = external_node(key, kind);
        }
        Some(_) => {}
        None => {
            external.insert(key.to_string(), external_node(key, kind));
        }
    }
}

fn external_node(key: &str, kind: Option<&str>) -> DigestNode {
    DigestNode {
        key: key.to_string(),
        label: kind.unwrap_or("External").into(),
        extra_labels: kind
            .map(|_| vec!["External".to_string()])
            .unwrap_or_default(),
        props: Properties::new(),
    }
}

/// One field: `pub id: NodeId`, `count: usize`, or `pub 0: i64` for a tuple
/// struct, whose fields are positions rather than names.
///
/// Visibility is included, unlike an enum variant's fields — which of a
/// struct's fields are `pub` is a large part of what the struct *is*, while a
/// variant's fields have no visibility of their own to state.
fn field_text(index: usize, field: &syn::Field) -> String {
    let name = field
        .ident
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| index.to_string());
    let vis = visibility(&field.vis);
    let head = if vis.is_empty() {
        name
    } else {
        format!("{vis} {name}")
    };
    format!("{head}: {}", ty_of(&field.ty))
}

/// Attach the fields of the struct or union just emitted.
///
/// The same argument as an enum's variants: a node saying only `NodeRecord`
/// says almost nothing, while its fields are the shape of it. A unit struct
/// has none, and an empty list property is noise on every read.
fn set_fields<'a>(f: &mut FileFacts, fields: impl Iterator<Item = &'a syn::Field>) {
    let items: Vec<PropValue> = fields
        .enumerate()
        .map(|(i, field)| PropValue::Str(field_text(i, field)))
        .collect();
    if items.is_empty() {
        return;
    }
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert(
            "fields".into(),
            PropDesc::described(
                "the fields it declares, each with its visibility and type as written",
                PropValue::List(items),
            ),
        );
    }
}

/// One variant, with whatever it carries: `Unit`, `Lit(i64)`,
/// `Prop { name: String }`, `A = 1`.
fn variant_of(v: &syn::Variant) -> String {
    let mut out = v.ident.to_string();
    match &v.fields {
        syn::Fields::Unit => {}
        syn::Fields::Unnamed(fields) => {
            let tys: Vec<String> = fields.unnamed.iter().map(|f| ty_of(&f.ty)).collect();
            out.push_str(&format!("({})", tys.join(", ")));
        }
        syn::Fields::Named(fields) => {
            let named: Vec<String> = fields
                .named
                .iter()
                .map(|f| {
                    let name = f
                        .ident
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    format!("{name}: {}", ty_of(&f.ty))
                })
                .collect();
            out.push_str(&format!(" {{ {} }}", named.join(", ")));
        }
    }
    if let Some((_, expr)) = &v.discriminant {
        out.push_str(&format!(" = {}", tidy(&expr.to_token_stream().to_string())));
    }
    out
}

/// Mark the item just emitted `#[non_exhaustive]`, when it is.
///
/// Recorded because it is a promise about the future rather than a detail of
/// the present: it says this type may gain variants or fields, so a downstream
/// `match` must keep a wildcard arm and the type cannot be constructed
/// literally outside its crate. Absent means exhaustive, following the
/// convention `visibility` and `is_async` already use.
fn set_non_exhaustive(f: &mut FileFacts, attrs: &[syn::Attribute]) {
    if !attrs.iter().any(|a| a.path().is_ident("non_exhaustive")) {
        return;
    }
    if let Some(n) = f.nodes.last_mut() {
        n.props.insert(
            "non_exhaustive".into(),
            PropDesc::described(
                "may gain variants or fields, so a match on it needs a wildcard arm",
                PropValue::Bool(true),
            ),
        );
    }
}

/// Record the initializer of the item just emitted.
///
/// A `const` is the one kind of item whose value is *knowable from the source
/// alone* — that is what makes it const — so leaving the graph with its type
/// and not its value drops the only fact about it a reader usually wants. The
/// same goes for a `static`, whose initializer is equally fixed.
///
/// Written as the source wrote it: `4`, `256 * 1024`, `Duration::from_secs(2)`.
/// A parser does not evaluate — `256 * 1024` is not folded to `262144` —
/// because folding would need const evaluation, and a wrong number is worse
/// than the expression that produced it.
fn set_value(f: &mut FileFacts, expr: &impl ToTokens) {
    let value = tidy(&expr.to_token_stream().to_string());
    if let Some(n) = f.nodes.last_mut() {
        n.props
            .insert("value".into(), PropDesc::new(PropValue::Str(value)));
    }
}

fn edge(src: &str, dst: &str, ty: &str) -> DigestEdge {
    DigestEdge {
        src: src.to_string(),
        dst: dst.to_string(),
        ty: ty.to_string(),
        props: Properties::new(),
    }
}

/// Build a property map, dropping entries that came back empty — an absent
/// property is cheaper and truer than one holding `""`.
fn props<const N: usize>(pairs: [(&str, String); N]) -> Properties {
    let mut out = Properties::new();
    for (k, v) in pairs {
        if !v.is_empty() {
            out.insert(k.to_string(), PropDesc::new(PropValue::Str(v)));
        }
    }
    out
}

/// Attach the item's own source under `_code` — retrieval only. The underscore
/// keeps it out of the embedding text and the schema summary the model reads.
fn add_source(props: &mut Properties, item: &impl ToTokens) {
    props.insert(
        "_code".to_string(),
        PropDesc::described(
            "source as written, for retrieval — not indexed or embedded",
            PropValue::Str(item.to_token_stream().to_string()),
        ),
    );
}

fn visibility(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Public(_) => "pub".into(),
        syn::Visibility::Restricted(r) => tidy(&r.to_token_stream().to_string()),
        syn::Visibility::Inherited => String::new(),
    }
}

fn sig_of(sig: &syn::Signature) -> String {
    tidy(&sig.to_token_stream().to_string())
}

/// What a function-like item is, and what it carries.
///
/// Returns the **label** alongside the properties, because the two decisions are
/// one: an item with a `self` receiver is a `Method` and an item without one is
/// a `Function`, which is the distinction Rust itself draws. Both are reachable
/// by name and both take part in the call graph; what changes is that "every
/// method on this type" is a label query rather than a scan for a leading
/// `self` inside a rendered signature.
///
/// Three facts are lifted out of that rendered signature and given properties
/// of their own, because a string is not a queryable thing:
///
/// - `returns` — the return type as written, absent when there is none. In Rust
///   `-> ()` and no arrow are the same function, so absence *is* the unit type.
/// - `receiver` — `self`, `&self` or `&mut self`; absent on a plain function.
///   An associated function such as `Database::open` has no receiver and so is
///   a `Function`, which is what a Rust programmer calls it.
/// - `is_async` — present and true only when the item is `async`, following the
///   same convention as `visibility`, where absent means private.
fn fn_facts(
    sig: &syn::Signature,
    attrs: &[syn::Attribute],
    vis: Option<&syn::Visibility>,
    body: Option<&syn::Block>,
) -> (&'static str, Properties) {
    let receiver = match sig.inputs.first() {
        Some(syn::FnArg::Receiver(r)) => {
            let mut s = String::new();
            if r.reference.is_some() {
                s.push('&');
            }
            if r.mutability.is_some() {
                s.push_str("mut ");
            }
            s.push_str("self");
            Some(s)
        }
        _ => None,
    };

    let returns = match &sig.output {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(_, ty) => ty_of(ty),
    };

    let mut props = props([
        ("signature", sig_of(sig)),
        ("returns", returns),
        ("receiver", receiver.clone().unwrap_or_default()),
        ("doc_comment", docs_of(attrs)),
        ("visibility", vis.map(visibility).unwrap_or_default()),
        ("local_bindings", body.map(bindings_of).unwrap_or_default()),
    ]);
    if sig.asyncness.is_some() {
        props.insert("is_async".into(), PropDesc::new(PropValue::Bool(true)));
    }

    let label = if receiver.is_some() {
        "Method"
    } else {
        "Function"
    };
    (label, props)
}

fn ty_of(ty: &syn::Type) -> String {
    tidy(&ty.to_token_stream().to_string())
}

fn path_of(path: &syn::Path) -> String {
    tidy(&path.to_token_stream().to_string()).replace(' ', "")
}

/// The same path with its generic arguments dropped: `From<i64>` → `From`.
///
/// A trait is one thing however many ways a type implements it, so this is what
/// identifies the *node*. The arguments are kept on the edge instead, where
/// they say which implementation without minting a trait per instantiation.
fn base_path(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// `to_token_stream` spaces every token; tighten the punctuation a reader (and
/// a model) would otherwise trip over.
fn tidy(text: &str) -> String {
    text.replace(" ,", ",")
        .replace(" :", ":")
        .replace(": :", "::")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" (", "(")
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ;", ";")
        .replace(" !", "!")
}

/// The doc comment, as one block of text.
fn docs_of(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &a.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            lines.push(s.value().trim().to_string());
        }
    }
    lines.join("\n").trim().to_string()
}

/// The bare name of a type, for `impl` blocks: `Foo<T>` → `Foo`.
/// The path an `impl` block's Self type names, generic arguments dropped.
///
/// The **whole** path, not its last segment: `impl … for
/// std::collections::HashSet<u64>` is about that type and not about any other
/// `HashSet`, and keeping only the tail both loses what the source said and
/// merges two crates' same-named types into one node. Resolution still matches
/// on the last segment, so a local `impl Database` is unaffected.
fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => Some(base_path(&p.path)),
        syn::Type::Reference(r) => type_name(&r.elem),
        _ => None,
    }
}
