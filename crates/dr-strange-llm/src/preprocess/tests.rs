//! The **router's** tests: dispatch, merge, ignore rules, provenance, the
//! capability boundary. The Rust parser's own tests moved with it to the
//! extensions repository — what stays here is everything the host owns, probed
//! with a handler small enough to be obviously correct.

use super::*;

/// A scratch tree that cleans up after itself.
struct Tree(std::path::PathBuf);

impl Tree {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("drsg-router-{name}-{}", std::process::id()));
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

/// A handler small enough to be obviously correct: claims `.rs`, emits one
/// node per input named by its path, and one edge so edge-stamping has
/// something to stamp. What it does is beside the point — it exists so the
/// *router's* behaviour is what a failure implicates.
struct Probe;

impl Preprocessor for Probe {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "probe".into(),
            version: "1".into(),
            extensions: vec!["rs".into()],
            logo: None,
        }
    }

    fn preprocess(&self, input: &Input<'_>, _host: &dyn Host) -> Result<Preprocessed> {
        let mut out = Preprocessed::default();
        let names: Vec<String> = match input {
            Input::Document { name, .. } => vec![(*name).to_string()],
            Input::Files { paths } => paths.to_vec(),
        };
        for name in &names {
            out.nodes.push(DigestNode {
                key: format!("probe::{name}"),
                label: "Probed".into(),
                extra_labels: Vec::new(),
                props: Default::default(),
            });
        }
        if let Some(first) = names.first() {
            out.edges.push(DigestEdge {
                src: format!("probe::{first}"),
                dst: format!("probe::{first}"),
                ty: "SELF".into(),
                props: Default::default(),
            });
        }
        Ok(out)
    }
}

fn probe() -> Plugins {
    Plugins::from_handlers(vec![Box::new(Probe)])
}

/// Resolution is declared, never guessed: an explicit name wins, then a
/// declared extension, then the built-in document reader.
#[test]
fn routing_order_is_override_then_extension_then_builtin() {
    let t = Tree::new("routing");
    t.write("src/lib.rs", "pub fn a() {}");
    let host = t.host();
    let plugins = probe();

    // Extension picks the probe.
    let by_ext = route_document("thing.rs", b"pub fn a() {}", None, &host, &plugins).unwrap();
    assert_eq!(by_ext.nodes.len(), 1);
    assert_eq!(by_ext.nodes[0].label, "Probed");

    // No declared extension falls back to the document reader: prose, no facts.
    let fallback = route_document("notes.md", b"# Hi", None, &host, &plugins).unwrap();
    assert!(fallback.nodes.is_empty());
    assert!(fallback.prose.contains("Hi"));

    // An explicit handler wins over the extension.
    let forced = route_document("notes.md", b"anything", Some("probe"), &host, &plugins).unwrap();
    assert_eq!(forced.nodes.len(), 1);

    // And an unknown one is an error naming what exists, not a silent fallback.
    let err = match route_document("x.md", b"", Some("cobol"), &host, &plugins) {
        Ok(_) => panic!("an unknown handler must not fall through"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("probe"), "{err}");
}

/// A repository is code *and* markdown *and* a binary nobody can read.
#[test]
fn a_polyglot_tree_fans_out_and_merges() {
    let t = Tree::new("poly");
    t.write("src/lib.rs", "pub fn only() {}")
        .write("README.md", "# Title\n\nSome prose.")
        .write("notes.txt", "loose notes");
    std::fs::write(t.0.join("logo.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let out = route_tree(&t.host(), None, &probe()).unwrap();

    assert!(
        out.nodes.iter().any(|n| n.key == "probe::src/lib.rs"),
        "the handler's facts are missing"
    );
    assert!(out.prose.contains("Some prose"), "markdown prose missing");
    assert!(out.prose.contains("loose notes"), "text prose missing");
    // Each file's prose is announced, so no chunk straddles two of them.
    assert!(
        out.prose.matches(SOURCE_MARKER).count() >= 2,
        "{}",
        out.prose
    );
    assert!(out.report.skipped >= 1, "the binary should be counted");
    assert!(
        out.report.handlers.iter().any(|(n, _)| n == "probe@1"),
        "the report should name the handlers that ran: {:?}",
        out.report.handlers
    );
}

/// Everything a handler emits is marked by the router, so a later reader can
/// always separate a parsed fact from a model's guess — even when the handler
/// itself forgot.
#[test]
fn facts_carry_their_provenance() {
    let t = Tree::new("provenance");
    t.write("src/lib.rs", "pub fn a() {}");
    let out = route_tree(&t.host(), None, &probe()).unwrap();

    let stamped = |props: &dr_strange_core::Properties| {
        matches!(
            props.get("_generated_by").map(|d| &d.value),
            Some(dr_strange_core::PropValue::Str(s)) if s == "probe@1"
        )
    };
    assert!(out.nodes.iter().all(|n| stamped(&n.props)));
    assert!(out.edges.iter().all(|e| stamped(&e.props)));
}

/// Facts with no prose means the model is never called at all — the headline
/// of §11, and the property the caller keys its skip on.
#[test]
fn a_code_only_tree_needs_no_model() {
    let t = Tree::new("nomodel");
    t.write("src/lib.rs", "pub fn a() {}");
    let out = route_tree(&t.host(), None, &probe()).unwrap();

    assert!(!out.nodes.is_empty());
    assert!(!out.needs_model(), "no prose means no model call");
}

/// A manifest or a CI workflow is not a format the document reader converts,
/// but it is readable text that says real things about a project — so an
/// unclaimed text file becomes fenced prose rather than being dropped.
#[test]
fn config_files_become_fenced_prose_rather_than_being_dropped() {
    let t = Tree::new("configs");
    t.write("Cargo.toml", "[package]\nname = \"k\"\n")
        .write("deploy.yaml", "replicas: 3\n")
        .write("src/lib.rs", "pub fn a() {}");

    let out = route_tree(&t.host(), None, &probe()).unwrap();
    assert!(out.prose.contains("```toml"), "{}", out.prose);
    assert!(out.prose.contains("replicas: 3"), "{}", out.prose);
    // The code in the same tree is still facts, not prose.
    assert!(out.nodes.iter().any(|n| n.label == "Probed"));
    assert!(!out.prose.contains("pub fn a"));
}

/// When a whole class of source has no handler, the report says so by name —
/// a tree of `.rs` with no parser installed must not silently become prose.
#[test]
fn an_unclaimed_source_class_is_named_in_the_report() {
    let t = Tree::new("unclaimed");
    t.write("a.zig", "const x = 1;\n")
        .write("b.zig", "const y = 2;\n");

    // No handlers at all — the post-move default until something is installed.
    let out = route_tree(&t.host(), None, &Plugins::builtin()).unwrap();
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains(".zig (2)") && n.contains("plugin")),
        "{:?}",
        out.report.notes
    );
}

/// Two handlers claiming one key is a plugin bug, kept visible: the first
/// wins, the collision is named, and the ingest is not failed over it.
#[test]
fn a_cross_handler_collision_is_counted_not_fatal() {
    struct Same(&'static str);
    impl Preprocessor for Same {
        fn manifest(&self) -> Manifest {
            Manifest {
                name: self.0.into(),
                version: "1".into(),
                extensions: vec![match self.0 {
                    "one" => "aa".into(),
                    _ => "bb".into(),
                }],
                logo: None,
            }
        }
        fn preprocess(&self, _: &Input<'_>, _: &dyn Host) -> Result<Preprocessed> {
            let mut out = Preprocessed::default();
            out.nodes.push(DigestNode {
                key: "shared".into(),
                label: self.0.to_uppercase(),
                extra_labels: Vec::new(),
                props: Default::default(),
            });
            Ok(out)
        }
    }

    let t = Tree::new("collision");
    t.write("x.aa", "").write("y.bb", "");
    let plugins = Plugins::from_handlers(vec![Box::new(Same("one")), Box::new(Same("two"))]);
    let out = route_tree(&t.host(), None, &plugins).unwrap();

    assert_eq!(out.nodes.iter().filter(|n| n.key == "shared").count(), 1);
    assert_eq!(
        out.nodes[0].label, "ONE",
        "the first handler's node is kept"
    );
    assert!(
        out.report.collisions.iter().any(|c| c.contains("shared")),
        "{:?}",
        out.report.collisions
    );
}

/// A project's own ignore file is the best statement of what is derived — and
/// it has to be possible to disagree with it. This is the walker's behaviour,
/// so it is asserted on the walker.
#[test]
fn gitignore_is_honoured_and_can_be_turned_off() {
    let t = Tree::new("ignore");
    t.write(".gitignore", "generated.rs\n")
        .write("src/lib.rs", "pub fn kept() {}")
        .write("src/generated.rs", "pub fn generated() {}");

    let honoured = t.host().list("").unwrap();
    assert!(
        !honoured.iter().any(|p| p.contains("generated.rs")),
        "{honoured:?}"
    );

    let off = LocalFiles::with_policy(
        &t.0,
        IgnorePolicy {
            gitignore: false,
            ..Default::default()
        },
    )
    .unwrap();
    let listed = off.list("").unwrap();
    assert!(
        listed.iter().any(|p| p.contains("generated.rs")),
        "{listed:?}"
    );
}

/// `target/` is skipped even with no ignore file at all, because a build
/// directory can outweigh the source it came from by orders of magnitude.
#[test]
fn build_directories_are_skipped_without_any_ignore_file() {
    let t = Tree::new("builtin-dirs");
    t.write("src/lib.rs", "pub fn kept() {}")
        .write("target/debug/thing.rs", "pub fn derived() {}");

    let listed = t.host().list("").unwrap();
    assert!(!listed.iter().any(|p| p.contains("target/")), "{listed:?}");
}

/// What the host will answer *is* the capability grant, so it must not answer
/// for anything outside the directory it was given.
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
        "{msg}"
    );
}

// ---- sync (watch mode) ----------------------------------------------------

use dr_strange_core::Properties;

/// A code-shaped handler for the sync tests: claims `.aa`; each line of a
/// file is a symbol, `name->key` also asserts a CALLS edge onto `key`. Small
/// enough that what a failure implicates is the sync, not the parser.
struct AaLang;

impl AaLang {
    fn file_prop(path: &str) -> Properties {
        let mut p = Properties::new();
        p.insert(
            "file".into(),
            PropDesc::described("file", PropValue::Str(path.to_string())),
        );
        p
    }
}

impl Preprocessor for AaLang {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "aa".into(),
            version: "1".into(),
            extensions: vec!["aa".into()],
            logo: None,
        }
    }

    fn preprocess(&self, input: &Input<'_>, host: &dyn Host) -> Result<Preprocessed> {
        let mut out = Preprocessed::default();
        let Input::Files { paths } = input else {
            anyhow::bail!("sync routes files");
        };
        // A package-style umbrella node no file owns — its key is not a path
        // and it carries no `file` prop, so a sync never attributes it to a
        // commit. Its CONTAINS edges are re-asserted by every parse, which is
        // exactly the duplicate-edge trap the sync must not fall into.
        out.nodes.push(DigestNode {
            key: "pkg".into(),
            label: "Package".into(),
            extra_labels: Vec::new(),
            props: Properties::new(),
        });
        for path in *paths {
            out.nodes.push(DigestNode {
                key: path.clone(),
                label: "File".into(),
                extra_labels: Vec::new(),
                props: Properties::new(),
            });
            // A module-style node: keyed by a logical name, its file carried
            // under `path` (rust's convention for file-level modules) — the
            // sync must attribute it to the file all the same.
            let mut mprops = Properties::new();
            mprops.insert(
                "path".into(),
                PropDesc::described("file", PropValue::Str(path.clone())),
            );
            out.nodes.push(DigestNode {
                key: format!("mod::{path}"),
                label: "Module".into(),
                extra_labels: Vec::new(),
                props: mprops,
            });
            out.edges.push(DigestEdge {
                src: "pkg".into(),
                dst: path.clone(),
                ty: "CONTAINS".into(),
                props: Properties::new(),
            });
            let body = String::from_utf8(host.read(path)?)?;
            for line in body.lines().filter(|l| !l.trim().is_empty()) {
                let (name, callee) = match line.split_once("->") {
                    Some((n, c)) => (n.trim(), Some(c.trim())),
                    None => (line.trim(), None),
                };
                let key = format!("{path}::{name}");
                out.nodes.push(DigestNode {
                    key: key.clone(),
                    label: "Function".into(),
                    extra_labels: Vec::new(),
                    props: Self::file_prop(path),
                });
                out.edges.push(DigestEdge {
                    src: path.clone(),
                    dst: key.clone(),
                    ty: "CONTAINS".into(),
                    props: Properties::new(),
                });
                if let Some(callee) = callee {
                    out.edges.push(DigestEdge {
                        src: key,
                        dst: callee.to_string(),
                        ty: "CALLS".into(),
                        props: Properties::new(),
                    });
                }
            }
        }
        Ok(out)
    }
}

fn sync_fixture(name: &str) -> (Tree, dr_strange_core::Database, Plugins) {
    let tree = Tree::new(name);
    tree.write("a.aa", "f->b.aa::h\ng\n");
    tree.write("b.aa", "h->a.aa::f\n");
    let db = dr_strange_core::Database::in_memory().unwrap();
    db.create_plane("code", Properties::new()).unwrap();
    let plugins = Plugins::from_handlers(vec![Box::new(AaLang)]);
    let delta = CommitDelta {
        changed: vec!["a.aa".to_string(), "b.aa".to_string()],
        ..Default::default()
    };
    sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", "c0").unwrap();
    (tree, db, plugins)
}

fn key_id(db: &dr_strange_core::Database, key: &str) -> Option<dr_strange_core::NodeId> {
    db.plane("code")
        .unwrap()
        .node_by_key(key)
        .unwrap()
        .map(|n| n.id)
}

fn calls(db: &dr_strange_core::Database, src: &str) -> Vec<String> {
    let plane = db.plane("code").unwrap();
    let Some(id) = key_id(db, src) else {
        return Vec::new();
    };
    let mut out: Vec<String> = plane
        .neighbors(id, dr_strange_core::Dir::Out, Some("CALLS"))
        .unwrap()
        .into_iter()
        .filter_map(|n| plane.node(n.node).unwrap().and_then(|r| r.external_key))
        .collect();
    out.sort();
    out
}

#[test]
fn the_first_sync_loads_the_whole_set() {
    let (_tree, db, _plugins) = sync_fixture("first");
    // 2 files + 3 functions, and the cross-file calls resolved both ways.
    for key in ["a.aa", "b.aa", "a.aa::f", "a.aa::g", "b.aa::h"] {
        assert!(key_id(&db, key).is_some(), "missing {key}");
    }
    assert_eq!(calls(&db, "a.aa::f"), vec!["b.aa::h"]);
    assert_eq!(calls(&db, "b.aa::h"), vec!["a.aa::f"]);
}

#[test]
fn a_modified_file_drops_its_stale_symbols_and_keeps_incoming_edges() {
    let (tree, db, plugins) = sync_fixture("modify");
    // g disappears; f survives the rewrite.
    tree.write("a.aa", "f->b.aa::h\n");
    let delta = CommitDelta {
        changed: vec!["a.aa".to_string()],
        ..Default::default()
    };
    let stats = sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", "c1").unwrap();
    assert!(key_id(&db, "a.aa::g").is_none(), "stale symbol survived");
    assert!(key_id(&db, "a.aa::f").is_some());
    // The untouched file's assertion onto the re-created node was re-attached…
    assert_eq!(calls(&db, "b.aa::h"), vec!["a.aa::f"]);
    assert_eq!(stats.edges_reattached, 1);
    // …and the re-parsed file's own outgoing edge still resolves cross-file.
    assert_eq!(calls(&db, "a.aa::f"), vec!["b.aa::h"]);
}

#[test]
fn a_deleted_file_takes_its_nodes_and_dangling_calls_with_it() {
    let (tree, db, plugins) = sync_fixture("delete");
    std::fs::remove_file(tree.0.join("b.aa")).unwrap();
    let delta = CommitDelta {
        deleted: vec!["b.aa".to_string()],
        ..Default::default()
    };
    let stats = sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", "c1").unwrap();
    assert_eq!(
        stats.nodes_deleted, 3,
        "File node, its module node, and its one function"
    );
    assert!(key_id(&db, "b.aa").is_none());
    assert!(key_id(&db, "b.aa::h").is_none());
    // f's call onto the deleted symbol cascaded away with its target.
    assert_eq!(calls(&db, "a.aa::f"), Vec::<String>::new());
}

/// A node keyed by a logical name whose file lives under `path` (rust's
/// file-level module) must be attributed — and so deleted — with its file.
#[test]
fn path_prop_attributes_a_node_to_its_file() {
    let (tree, db, plugins) = sync_fixture("pathprop");
    assert!(key_id(&db, "mod::b.aa").is_some(), "module node missing");
    std::fs::remove_file(tree.0.join("b.aa")).unwrap();
    let delta = CommitDelta {
        deleted: vec!["b.aa".to_string()],
        ..Default::default()
    };
    sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", "c1").unwrap();
    assert!(
        key_id(&db, "mod::b.aa").is_none(),
        "a path-attributed node outlived its file"
    );
}

/// The trap the first live drill caught: a fact edge whose source the fold
/// did not re-create (the package umbrella, or any node without file
/// attribution) still has its old copy standing, and every fold re-asserts
/// it. One copy must survive, however many folds run.
#[test]
fn edges_from_untouched_sources_never_duplicate() {
    let (tree, db, plugins) = sync_fixture("dedup");
    for run in ["c1", "c2", "c3"] {
        tree.write(
            "a.aa",
            &format!(
                "f->b.aa::h
// {run}
"
            ),
        );
        let delta = CommitDelta {
            changed: vec!["a.aa".to_string()],
            ..Default::default()
        };
        sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", run).unwrap();
    }
    let plane = db.plane("code").unwrap();
    let pkg = plane.node_by_key("pkg").unwrap().unwrap();
    let onto_a: Vec<_> = plane
        .neighbors(pkg.id, dr_strange_core::Dir::Out, Some("CONTAINS"))
        .unwrap()
        .into_iter()
        .filter(|n| plane.node(n.node).unwrap().and_then(|r| r.external_key) == Some("a.aa".into()))
        .collect();
    assert_eq!(onto_a.len(), 1, "one CONTAINS per fold would stack forever");
}

#[test]
fn reserved_and_vector_props_survive_the_reload() {
    let (tree, db, plugins) = sync_fixture("carry");
    {
        let plane = db.plane("code").unwrap();
        let id = key_id(&db, "a.aa::f").unwrap();
        let mut txn = plane.write().unwrap();
        txn.set_prop(
            id,
            "emb",
            PropDesc::described("embedding", PropValue::Vector(vec![1.0, 0.0])),
        )
        .unwrap();
        txn.set_prop(
            id,
            "_reviewed",
            PropDesc::described("mark", PropValue::Bool(true)),
        )
        .unwrap();
        txn.commit().unwrap();
    }
    tree.write("a.aa", "f->b.aa::h\n");
    let delta = CommitDelta {
        changed: vec!["a.aa".to_string()],
        ..Default::default()
    };
    sync_paths(&db, "code", &tree.host(), &delta, &plugins, "test", "c1").unwrap();
    let node = db
        .plane("code")
        .unwrap()
        .node_by_key("a.aa::f")
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            node.properties.get("emb").map(|d| &d.value),
            Some(PropValue::Vector(_))
        ),
        "embedding lost in the reload"
    );
    assert!(node.properties.contains_key("_reviewed"));
    // The parser's own view is fresh, not carried: _run moved to the new sync.
    assert!(
        matches!(node.properties.get("_run").map(|d| &d.value), Some(PropValue::Str(s)) if s == "c1")
    );
}
