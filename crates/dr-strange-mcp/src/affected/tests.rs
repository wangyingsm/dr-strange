use std::path::Path;
use std::process::Command;

use dr_strange_core::{Database, PropDesc, PropValue, Properties};
use dr_strange_llm::{DigestNode, Host, Preprocessed};

use super::*;

/// A parser that must never be asked: the working-tree reading takes its
/// declarations from the plane, and a call here would mean it re-parsed.
struct Unasked;

impl Parsers for Unasked {
    fn parse(&self, _host: &dyn Host, files: &[String]) -> AnyResult<Preprocessed> {
        panic!("the working tree was re-parsed, for {files:?}");
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn at(file: &str, line: i64, end: i64) -> Properties {
    let mut props = Properties::new();
    props.insert(
        "file".into(),
        PropDesc::new(PropValue::Str(file.to_string())),
    );
    props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
    props.insert("end_line".into(), PropDesc::new(PropValue::Int(end)));
    props
}

/// A plane holding one file's module, two functions in it, and a test that
/// calls the first.
fn planed() -> Database {
    planed_at(Properties::new())
}

/// The same plane, recording what tree it was parsed from.
fn synced(commit: &str, modified: i64) -> Properties {
    let mut props = Properties::new();
    props.insert(
        "synced_commit".into(),
        PropDesc::new(PropValue::Str(commit.to_string())),
    );
    props.insert(
        "synced_modified".into(),
        PropDesc::new(PropValue::Int(modified)),
    );
    props
}

fn planed_at(plane_props: Properties) -> Database {
    let db = Database::in_memory().unwrap();
    let p = db.create_plane("code", plane_props).unwrap();
    let mut txn = p.write().unwrap();
    txn.create_node_with_key("m", &["Module"], at("src/lib.rs", 1, 100))
        .unwrap();
    let one = txn
        .create_node_with_key("m::one", &["Function"], at("src/lib.rs", 10, 20))
        .unwrap();
    txn.create_node_with_key("m::two", &["Function"], at("src/lib.rs", 30, 40))
        .unwrap();
    let mut test = at("src/lib.rs", 80, 90);
    test.insert(
        "test_flag".into(),
        PropDesc::new(PropValue::Str("attribute".into())),
    );
    let t = txn
        .create_node_with_key("m::tests::one_works", &["Function"], test)
        .unwrap();
    txn.create_edge(t, one, "CALLS", Properties::new()).unwrap();
    txn.commit().unwrap();
    db
}

#[test]
fn hunks_read_the_new_side_of_every_file() {
    let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n+x\n\
                diff --git a/src/b.rs b/src/b.rs\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -40 +41 @@\n+y\n";
    let out = hunks(diff);
    assert_eq!(out["src/a.rs"], vec![(1, 3)]);
    assert_eq!(out["src/b.rs"], vec![(41, 41)]);
}

/// A hunk that only deletes has no new lines, and `+9,0` would read as an
/// empty range that contains nothing. It is the line the deletion was taken
/// from that the change reaches.
#[test]
fn a_hunk_that_only_deletes_points_at_where_it_was_taken_from() {
    let diff = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -10,3 +9,0 @@\n-gone\n";
    assert_eq!(hunks(diff)["src/a.rs"], vec![(9, 9)]);
}

/// A file the change deletes has no new side to map lines onto.
#[test]
fn a_deleted_file_contributes_no_range() {
    let diff = "--- a/src/gone.rs\n+++ /dev/null\n@@ -1,5 +0,0 @@\n-all of it\n";
    assert!(hunks(diff).is_empty(), "{:?}", hunks(diff));
}

/// The change touched a line inside a function inside a module. The function
/// is the seed; the module holds it and would walk the same ground twice.
#[test]
fn the_innermost_declaration_is_the_seed() {
    let db = planed();
    let plane = db.plane("code").unwrap();
    let spans = BTreeMap::from([("src/lib.rs".to_string(), vec![(12, 12)])]);
    assert_eq!(declared_in(&plane, &spans).unwrap(), vec!["m::one"]);
}

/// One hunk across two functions is two seeds — neither holds the other.
#[test]
fn two_declarations_one_hunk_spans_are_both_seeds() {
    let db = planed();
    let plane = db.plane("code").unwrap();
    let spans = BTreeMap::from([("src/lib.rs".to_string(), vec![(15, 35)])]);
    assert_eq!(
        declared_in(&plane, &spans).unwrap(),
        vec!["m::one".to_string(), "m::two".to_string()]
    );
}

/// A change between declarations — a `use` line, a constant — is held by the
/// module alone, and that is then the honest seed rather than nothing.
#[test]
fn a_change_in_no_declaration_falls_back_to_what_holds_it() {
    let db = planed();
    let plane = db.plane("code").unwrap();
    let spans = BTreeMap::from([("src/lib.rs".to_string(), vec![(25, 25)])]);
    assert_eq!(declared_in(&plane, &spans).unwrap(), vec!["m"]);
}

#[test]
fn a_package_is_the_nearest_manifest_above_a_changed_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/one/src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(root.join("crates/one/Cargo.toml"), "[package]\n").unwrap();
    let found = packages(root, &["crates/one/src/lib.rs".to_string()]);
    assert_eq!(found, vec!["crates/one/".to_string()]);
}

/// A workspace root declares members, not a package. Taking it would make
/// the package of every root-level file the whole repository, and `--safe`
/// on a changed `.gitignore` would mean the entire suite.
#[test]
fn a_cargo_workspace_root_is_not_a_package() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    assert!(packages(root, &[".gitignore".to_string()]).is_empty());

    // One that does declare a package still answers.
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    assert_eq!(packages(root, &["src/lib.rs".to_string()]), vec![""]);
}

/// Nothing above the file is a package, so nothing is widened to — better
/// than widening to the whole tree on a guess.
#[test]
fn a_file_under_no_manifest_widens_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("loose")).unwrap();
    assert!(packages(dir.path(), &["loose/thing.rs".to_string()]).is_empty());
}

/// End to end over a real checkout: an edit inside `m::one` names the test
/// that calls it, and the working tree is never re-parsed to find out.
#[test]
fn an_uncommitted_edit_names_the_test_that_reaches_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "x\n".repeat(100)).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    // Line 12 is inside `m::one` as the plane records it.
    let mut lines: Vec<String> = std::fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines[11] = "edited".into();
    std::fs::write(root.join("src/lib.rs"), lines.join("\n")).unwrap();

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(out.contains("the working tree against HEAD"), "{out}");
    assert!(out.contains("affected tests: 1"), "{out}");
    assert!(out.contains("m::tests::one_works  [CALLS]"), "{out}");
}

/// The hazard this verb's working-tree reading carries: lines come from the
/// diff and extents from the plane, so a plane that has not seen the edit can
/// name the declaration that used to be at that line. Nothing in the answer
/// would look wrong, so it is said.
#[test]
fn a_plane_behind_the_working_tree_warns_that_a_seed_may_be_stale() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "x\n".repeat(100)).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    std::fs::write(root.join("src/lib.rs"), "y\n".repeat(100)).unwrap();

    // A plane parsed from a clean tree cannot be holding an uncommitted edit.
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let db = planed_at(synced(&head, 0));
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(
        out.contains("the plane is behind this working tree"),
        "{out}"
    );
    assert!(
        out.contains("1 file(s) differ from HEAD and it recorded 0"),
        "{out}"
    );
}

/// A plane that was parsed from this very tree, edit included, has nothing to
/// warn about — and a warning on every answer would be one nobody reads.
#[test]
fn a_plane_that_holds_the_edit_is_quiet_about_being_behind() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "x\n".repeat(100)).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    let mut lines: Vec<String> = std::fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines[11] = "edited".into();
    std::fs::write(root.join("src/lib.rs"), lines.join("\n")).unwrap();

    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let db = planed_at(synced(&head, 1));
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(!out.contains("behind this working tree"), "{out}");
    assert!(out.contains("m::tests::one_works"), "{out}");
}

/// A clean tree has no change to answer about, and says so rather than
/// returning an empty set that reads like "no tests are affected".
#[test]
fn a_clean_tree_says_there_is_no_change_to_start_from() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("src.rs"), "x\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(
        out.contains("no changed declaration to start from"),
        "{out}"
    );
}

/// An untracked file is all new, so every declaration in it is a seed — and
/// the answer says why that file was read whole.
#[test]
fn an_untracked_file_is_read_whole_and_said_to_be() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("seed.txt"), "x\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "new\n").unwrap();

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(out.contains("1 untracked file(s) are read whole"), "{out}");
    assert!(out.contains("m::tests::one_works"), "{out}");
}

/// What a CI template loops over: the changed declarations' keys, one per
/// line, and not one word of prose — a note in this stream would become a
/// symbol the next verb is asked about.
#[test]
fn the_seeds_are_the_keys_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "x\n".repeat(100)).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    let mut lines: Vec<String> = std::fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines[11] = "edited".into();
    std::fs::write(root.join("src/lib.rs"), lines.join("\n")).unwrap();

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: true,
        },
    )
    .unwrap();
    assert_eq!(out, "m::one\n");
}

/// The report carries the change's own note as a quote above the set: what
/// was read, what could not be, and whether the plane is behind the tree.
#[test]
fn a_report_quotes_the_change_above_the_set() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "x\n".repeat(100)).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);
    let mut lines: Vec<String> = std::fs::read_to_string(root.join("src/lib.rs"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines[11] = "edited".into();
    std::fs::write(root.join("src/lib.rs"), lines.join("\n")).unwrap();

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: true,
            seeds: false,
        },
    )
    .unwrap();
    assert!(
        out.starts_with("> change: the working tree against HEAD"),
        "{out}"
    );
    assert!(out.contains("### Tests this change reaches"), "{out}");
    assert!(out.contains("| `m::tests::one_works` | CALLS |"), "{out}");
}

/// A clean tree is the case a template must not post a comment for, so the
/// report of one is empty — the sentence the compact answer gives would
/// otherwise be a pull-request comment saying nothing.
#[test]
fn a_clean_tree_reports_nothing_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("src.rs"), "x\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "one"]);

    let db = planed();
    for want in [Output::Report, Output::Seeds] {
        let out = affected_logic(
            &db,
            Some(root),
            &Unasked,
            AffectedReq {
                plane: "code".into(),
                vs: None,
                safe: false,
                depth: None,
                report: want == Output::Report,
                seeds: want == Output::Seeds,
            },
        )
        .unwrap();
        assert_eq!(out, "", "{want:?}");
    }
}

/// A parser that calls every `fn <name>` line a declaration running to the
/// next one, reading the text the host hands it. Enough for a range, where
/// what matters is which keys each revision holds and whether their bodies
/// differ — not how a language is really parsed.
struct Lines;

impl Parsers for Lines {
    fn parse(&self, host: &dyn Host, files: &[String]) -> AnyResult<Preprocessed> {
        let mut out = Preprocessed::default();
        for path in files {
            let text = String::from_utf8(host.read(path)?)?;
            let lines: Vec<&str> = text.lines().collect();
            let starts: Vec<usize> = (0..lines.len())
                .filter(|i| lines[*i].starts_with("fn "))
                .collect();
            for (n, &i) in starts.iter().enumerate() {
                let end = starts.get(n + 1).copied().unwrap_or(lines.len());
                out.nodes.push(DigestNode {
                    key: format!("m::{}", lines[i][3..].trim()),
                    label: "Function".into(),
                    props: at(path, i as i64 + 1, end as i64),
                    ..Default::default()
                });
            }
        }
        Ok(out)
    }
}

/// A parser that claims nothing, so no revision can be read and every file
/// is one the comparison cannot settle.
struct Blind;

impl Parsers for Blind {
    fn parse(&self, _host: &dyn Host, _files: &[String]) -> AnyResult<Preprocessed> {
        Err(anyhow::anyhow!("no installed parser claims these"))
    }
}

/// A checkout with `one` and `two` declared the way `Lines` reads them.
fn checkout() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

fn write(root: &Path, one: &str, two: &str) {
    std::fs::write(
        root.join("src/lib.rs"),
        format!("fn one\n{one}\nfn two\n{two}\n"),
    )
    .unwrap();
}

fn committed(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
}

fn asked(db: &Database, root: Option<&Path>, parsers: &dyn Parsers, vs: Option<&str>) -> String {
    affected_logic(
        db,
        root,
        parsers,
        AffectedReq {
            plane: "code".into(),
            vs: vs.map(str::to_string),
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap()
}

/// The path every CI run takes: two revisions, both parsed, and only the
/// declaration whose body differs is a seed. The working tree is clean, so
/// nothing here could have come from reading it.
#[test]
fn a_range_seeds_the_declaration_whose_body_changed() {
    let dir = checkout();
    let root = dir.path();
    write(root, "x", "y");
    committed(root, "one");
    write(root, "edited", "y");
    committed(root, "two");

    let db = planed();
    let out = asked(&db, Some(root), &Lines, Some("HEAD~1"));
    assert!(out.contains("1 file(s), 1 declaration(s)"), "{out}");
    assert!(out.contains("affected tests: 1"), "{out}");
    assert!(out.contains("m::tests::one_works"), "{out}");
    // Both revisions are named, abbreviated, so the answer says what it compared.
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    assert!(out.contains(&head[..12]), "{out}");
}

/// A range where nothing differs says which two revisions agreed, rather
/// than only that there was nothing.
#[test]
fn a_range_with_no_difference_names_the_revisions_it_compared() {
    let dir = checkout();
    let root = dir.path();
    write(root, "x", "y");
    committed(root, "one");

    let db = planed();
    let out = asked(&db, Some(root), &Lines, Some("HEAD"));
    assert!(out.contains("no file differs"), "{out}");
    assert!(
        out.contains("no changed declaration to start from"),
        "{out}"
    );
}

/// A declaration the range deleted is counted and not seeded: it is not in
/// the tree the tests would run against.
#[test]
fn a_declaration_the_range_removed_is_counted_rather_than_seeded() {
    let dir = checkout();
    let root = dir.path();
    write(root, "x", "y");
    committed(root, "one");
    std::fs::write(root.join("src/lib.rs"), "fn one\nx\n").unwrap();
    committed(root, "two");

    let db = planed();
    let out = asked(&db, Some(root), &Lines, Some("HEAD~1"));
    assert!(
        out.contains("1 declaration(s) the range removed are not seeds"),
        "{out}"
    );
    assert!(!out.contains("m::two  "), "{out}");
}

/// Files the comparison cannot settle are one note per reason rather than
/// one per file, and past six the rest are counted instead of listed.
#[test]
fn files_no_parser_claims_are_one_note_that_names_six() {
    let dir = checkout();
    let root = dir.path();
    for n in 0..8 {
        std::fs::write(root.join(format!("src/f{n}.rs")), "first\n").unwrap();
    }
    committed(root, "one");
    for n in 0..8 {
        std::fs::write(root.join(format!("src/f{n}.rs")), "second\n").unwrap();
    }
    committed(root, "two");

    let db = planed();
    let out = asked(&db, Some(root), &Blind, Some("HEAD~1"));
    assert_eq!(
        out.matches("no installed parser claims").count(),
        1,
        "one reason, not eight: {out}"
    );
    assert!(
        out.contains("8 file(s), so what they declare is not"),
        "{out}"
    );
    assert!(
        out.contains("and 2 more"),
        "six named, the rest counted: {out}"
    );
}

/// A range does not see uncommitted work. Saying so is the difference
/// between an answer about the range and an answer about the edit in hand.
#[test]
fn a_range_says_it_cannot_see_the_uncommitted_edit() {
    let dir = checkout();
    let root = dir.path();
    write(root, "x", "y");
    committed(root, "one");
    write(root, "edited", "y");
    committed(root, "two");
    write(root, "edited again", "y");

    let db = planed();
    let out = asked(&db, Some(root), &Lines, Some("HEAD~1"));
    assert!(
        out.contains("this working tree has uncommitted changes"),
        "{out}"
    );
    assert!(
        out.contains("drop `vs` to ask about those instead"),
        "{out}"
    );
}

/// `--safe` widens to the package above each changed file, which is how a
/// test that reaches the change only from inside a macro is recovered.
#[test]
fn safe_widens_to_the_package_the_change_touched() {
    let dir = checkout();
    let root = dir.path();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
    write(root, "x", "y");
    committed(root, "one");
    write(root, "edited", "y");
    committed(root, "two");

    let db = planed();
    let out = affected_logic(
        &db,
        Some(root),
        &Lines,
        AffectedReq {
            plane: "code".into(),
            vs: Some("HEAD~1".into()),
            safe: true,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .unwrap();
    assert!(out.contains("widened:"), "{out}");
    assert!(out.contains("m::tests::one_works"), "{out}");
}

/// A plane parsed at another commit is behind in a way no file count can
/// show, so the warning names the two commits instead of counting files.
#[test]
fn a_plane_parsed_at_another_commit_names_both_commits() {
    let dir = checkout();
    let root = dir.path();
    write(root, "x", "y");
    committed(root, "one");
    let first = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    write(root, "edited", "y");
    committed(root, "two");

    let db = planed_at(synced(&first, 0));
    let out = asked(&db, Some(root), &Unasked, None);
    assert!(out.contains("it was parsed at"), "{out}");
    assert!(out.contains(&first[..12]), "{out}");
}

/// `affected` reads what changed from git, so a plane with no tree to read
/// is an error that says what to do — never an empty set, which would read
/// as "this change reaches no test".
#[test]
fn a_plane_with_no_source_tree_refuses_rather_than_answering_nothing() {
    let db = planed();
    let err = affected_logic(
        &db,
        None,
        &Unasked,
        AffectedReq {
            plane: "code".into(),
            vs: None,
            safe: false,
            depth: None,
            report: false,
            seeds: false,
        },
    )
    .expect_err("no tree, no answer");
    let said = err.to_string();
    assert!(said.contains("no source tree for plane `code`"), "{said}");
    assert!(said.contains("serve watch"), "{said}");
}
