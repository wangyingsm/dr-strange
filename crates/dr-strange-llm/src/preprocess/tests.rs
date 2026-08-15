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
