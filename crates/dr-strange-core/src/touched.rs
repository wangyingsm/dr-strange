//! The join from a symbol or a path to the commits that changed it
//! (`ROADMAP` §11).
//!
//! A code plane holds the symbols as they are now, each recording the file it
//! is declared in; the history plane beside it holds the commits, each
//! recording the paths it changed. Planes are hard namespaces and a
//! cross-plane edge is rejected at the storage layer, so this is a *read* of
//! two planes rather than an edge — the shape [`crate::ops`] uses for the
//! other join, through a shared property instead of a traversal.
//!
//! ## What the thread is, and what it is not
//!
//! The thread is the path. A commit is listed when the paths it changed hold
//! the file the symbol is declared in, which answers "when was this touched"
//! at file granularity and says so in those words. The symbol's own
//! declaration is a second question, and the only honest way to settle it is
//! to read the declaration at each of these commits and the one before —
//! `recall <name> at <sha> vs <sha>~1`, over a list this answer has already
//! bounded to one file's history.
//!
//! That reading needs a checkout and the parsers, which this layer has
//! neither of, so a caller which does passes what it found back through
//! [`Marks`] rather than rendering a second answer of its own.
//!
//! ## Why a short list is not silence
//!
//! Three different facts can make a commit missing from an answer, and each
//! is stated rather than left to look like "it did not touch that file": a
//! plane folded before the git reader recorded any paths, a commit whose own
//! list stopped short, and a file that was renamed, whose earlier life a
//! history records under the other name.
//!
//! The answer can also be *longer* than git's, for one reason: a plane keeps
//! the commits a rewrite left behind, and `git log -- <path>` walks from the
//! refs and never reaches them. Those are marked, and the count git would
//! give is stated beside them — an unexplained disagreement with `git log`
//! reads as a wrong answer rather than a fuller one.

use std::collections::BTreeMap;

use crate::api::{Database, PlaneHandle};
use crate::compact::{self, Resolved, history_plane_name, prop_bool, prop_int, prop_str};
use crate::error::Result;
use crate::types::{NodeRecord, PropValue};

/// What a caller found out about the commits, folded into the answer.
///
/// A caller that can read the declaration at each revision knows two things
/// this module cannot: what it found per commit, and how much of the listing
/// it got through. Both come back here rather than into a second renderer,
/// because the plain answer and the annotated one are the same answer with
/// more known about it.
#[derive(Default)]
pub struct Marks {
    /// Per commit, by [`Row::short`] — appended to that commit's line.
    pub per_commit: BTreeMap<String, String>,
    /// One line about the pass itself: how many commits it read, and what it
    /// left unread. The caller owns it because the caller did the work.
    pub note: Option<String>,
}

/// The label every history reader mints a commit under.
const COMMIT: &str = "Commit";
/// The property holding the paths one commit changed.
const FILES: &str = "files";
/// How many paths that commit changed — the orderable form.
const FILES_COUNT: &str = "files_count";
/// Whether that commit's list of paths stopped short.
const FILES_PARTIAL: &str = "files_partial";
/// Commits listed when the caller names no limit.
const COMMITS_SHOWN: usize = 15;

/// Which planes to read, and what is being asked of them.
///
/// A struct rather than four arguments: three of them are planes or names
/// that would read the same at a call site, and swapping two of those
/// silently is the one mistake this verb cannot detect.
pub struct Asked<'a> {
    /// The history plane — where the commits are.
    pub history: &'a PlaneHandle<'a>,
    /// The code plane beside it, when there is one: a symbol is resolved to
    /// its file there. Absent, `name` can only be read as a path.
    pub code: Option<&'a PlaneHandle<'a>>,
    /// A symbol, or a repository-relative path.
    pub name: &'a str,
    /// Commits listed, newest first (default 15).
    pub limit: Option<usize>,
}

/// The history plane a name resolves to, and the code plane beside it.
///
/// Here rather than in each surface, because the MCP verb and the CLI arm
/// resolving the pair differently would be the one failure neither could see.
///
/// `myrepo` and `myrepo_git` both name the same repository: the suffix is
/// stripped and both siblings derived from what is left. A plane with no
/// `_git` sibling is read as the history plane itself. A missing code plane
/// is `None` rather than an error — a path answers without one, and saying
/// which planes were read is this verb's own job anyway.
pub fn planes<'db>(
    db: &'db Database,
    plane: &str,
) -> Result<(PlaneHandle<'db>, Option<PlaneHandle<'db>>)> {
    let code = plane.strip_suffix(compact::HISTORY_SUFFIX).unwrap_or(plane);
    Ok(match db.plane(&history_plane_name(code)) {
        Ok(history) => (history, db.plane(code).ok()),
        // No `_git` sibling, so the plane named is the history itself and
        // nothing beside it is the code it belongs to.
        Err(_) => (db.plane(plane)?, None),
    })
}

/// The path an answer is threaded on, and how the name reached it.
struct Target {
    /// The repository-relative path matched against each commit's `files`.
    path: String,
    /// The symbol the path was reached through, when the name named one.
    symbol: Option<String>,
    /// Whether the name was read as a path because nothing matched it —
    /// worth saying, since a typo and a path both land here.
    literal: bool,
}

/// What resolving the name ended in.
enum Named {
    /// A path to join on.
    Target(Target),
    /// The name matched several symbols; the candidates are the answer.
    Ambiguous(String),
    /// It matched one symbol, which records no file to join on.
    Placeless(String),
}

/// One commit of the answer, for a caller that wants to say more about it
/// than this module can — whether the symbol's own declaration moved there,
/// which needs a checkout and the parsers.
pub struct Row {
    /// The full sha, which is what opens a tree.
    pub sha: String,
    /// The abbreviation the listing prints, and the key a mark is given under.
    pub short: String,
}

/// The join, before it is rendered.
///
/// Returned by [`candidates`] so that a caller can annotate each commit and
/// still render through [`Touched::stated`]. One renderer on purpose: the
/// plain answer and the annotated one are the same answer with more known
/// about it, and two copies of a sentence are one sentence with a
/// disagreement waiting.
pub struct Touched {
    /// Resolved before the scan, so a caller can see what the join was made
    /// on without re-resolving the name.
    target: Target,
    found: Found,
    /// Set when the name could not be joined on at all; rendering says so
    /// and the caller has nothing to annotate.
    refused: Option<String>,
    /// How many commits the listing shows, resolved once here so that
    /// [`Touched::rows`] and the rendering cannot disagree about it — a
    /// caller that annotated more commits than are printed would pay for
    /// work no reader sees and then report having done it.
    limit: usize,
}

impl Touched {
    /// The path the commits were matched against.
    pub fn path(&self) -> &str {
        &self.target.path
    }

    /// The symbol the path was reached through, when the name named one.
    pub fn symbol(&self) -> Option<&str> {
        self.target.symbol.as_deref()
    }

    /// The matching commits, newest first and already cut to the limit —
    /// exactly the ones the listing will print, so a caller does not pay to
    /// annotate a commit no reader will see.
    pub fn rows(&self) -> Vec<Row> {
        self.shown()
            .into_iter()
            .take(self.limit)
            .map(|commit| Row {
                sha: prop_str(&commit.properties, "sha")
                    .unwrap_or_else(|| prop_str(&commit.properties, "short").unwrap_or_default())
                    .to_string(),
                short: prop_str(&commit.properties, "short")
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect()
    }

    /// The answer, with a note of its own per commit where `marks` has one,
    /// keyed by [`Row::short`].
    pub fn stated(&self, asked: &Asked<'_>, marks: &Marks) -> Result<String> {
        if let Some(refused) = &self.refused {
            return Ok(refused.clone());
        }
        let mut out = head(&self.target, &self.found);
        out.push_str(&rows(self.shown(), self.limit, &marks.per_commit));
        out.push_str(&notes(&self.target, &self.found, marks));
        out.push_str(&planes_read(asked)?);
        Ok(out)
    }

    /// The commits the listing shows: newest first, cut to the limit.
    fn shown(&self) -> Vec<&NodeRecord> {
        let mut commits: Vec<&NodeRecord> = self.found.rows.iter().collect();
        commits.sort_by(|a, b| {
            prop_int(&b.properties, "committed_ts")
                .unwrap_or(0)
                .cmp(&prop_int(&a.properties, "committed_ts").unwrap_or(0))
        });
        commits
    }
}

/// When was this touched, and by whom — `history <name>`.
///
/// Answers at file granularity and says so: the commits listed are the ones
/// whose paths hold the file `name` is declared in. Every way the answer can
/// be short of the truth is a note of its own rather than a silent omission,
/// and it ends by naming the planes it read, because a verb that joined the
/// wrong pair would otherwise look like one that found nothing.
pub fn touched(asked: &Asked<'_>) -> Result<String> {
    candidates(asked)?.stated(asked, &Marks::default())
}

/// The same join, left unrendered so each commit can be annotated first.
pub fn candidates(asked: &Asked<'_>) -> Result<Touched> {
    let refuse = |why: String| Touched {
        target: Target {
            path: String::new(),
            symbol: None,
            literal: false,
        },
        found: Found::default(),
        refused: Some(why),
        limit: 0,
    };
    let target = match resolve(asked)? {
        Named::Target(target) => target,
        Named::Ambiguous(candidates) => return Ok(refuse(candidates)),
        Named::Placeless(key) => {
            return Ok(refuse(format!(
                "`{key}` records no file, so there is no path to join its history on — \
                 `describe {key}` shows what it does record\n"
            )));
        }
    };
    let found = scan(asked.history, &target.path)?;
    Ok(Touched {
        target,
        found,
        refused: None,
        limit: asked.limit.unwrap_or(COMMITS_SHOWN).max(1),
    })
}

/// The path to join on, from a symbol, a file-level node, or the name itself.
///
/// The code plane holds the files as well as the symbols, so one lookup
/// serves both; a name it does not know is read as a path, which is what
/// makes the verb usable against a plane digested with `--no-git` or a path
/// no longer in the tree.
fn resolve(asked: &Asked<'_>) -> Result<Named> {
    let literal = |name: &str| {
        Named::Target(Target {
            path: name.to_string(),
            symbol: None,
            literal: true,
        })
    };
    let Some(code) = asked.code else {
        return Ok(literal(asked.name));
    };
    match compact::resolve(code, asked.name)? {
        Resolved::One(node) => Ok(placed(&node)),
        Resolved::Many(hits) => Ok(Named::Ambiguous(compact::candidates(asked.name, &hits))),
        Resolved::None => Ok(literal(asked.name)),
    }
}

/// One resolved node's path: `file` for a symbol, `path` for the file itself.
fn placed(node: &NodeRecord) -> Named {
    let key = node
        .external_key
        .as_deref()
        .unwrap_or("<keyless>")
        .to_string();
    match prop_str(&node.properties, "file") {
        Some(file) => Named::Target(Target {
            path: file.to_string(),
            symbol: Some(key),
            literal: false,
        }),
        None => match prop_str(&node.properties, "path") {
            Some(path) => Named::Target(Target {
                path: path.to_string(),
                symbol: None,
                literal: false,
            }),
            None => Named::Placeless(key),
        },
    }
}

/// What a scan of the history plane found, and what it could not say.
#[derive(Default)]
struct Found {
    /// The commits whose paths hold the one asked about.
    rows: Vec<NodeRecord>,
    /// How many there were, kept beside them so the notes still read after
    /// the rows have been rendered and given up.
    matched: usize,
    /// Commits in the plane altogether.
    total: usize,
    /// Commits recording no paths at all — folded before the git reader
    /// wrote them, so they are absent from the answer rather than innocent.
    silent: usize,
    /// Commits whose own list stopped short, whether or not the path is in
    /// it: exactly the case where a missing commit may still have touched it.
    partial: usize,
    /// Of the rows, how many no branch or tag can still reach — what a
    /// rewrite left behind. `git log -- <path>` walks from the refs and so
    /// never sees them, and an answer longer than git's with no reason given
    /// reads as a wrong answer.
    left_behind: usize,
}

/// Every commit of the plane, kept when its paths hold this one.
///
/// A label scan and a property test rather than a query: `files` is a list,
/// and a list membership test is not something the index answers, so the
/// whole of the work is reading the commits that a listing reads anyway.
fn scan(history: &PlaneHandle<'_>, path: &str) -> Result<Found> {
    let commits = history.query().scan_label(COMMIT).nodes()?;
    let mut found = Found {
        rows: Vec::new(),
        matched: 0,
        total: commits.len(),
        silent: 0,
        partial: 0,
        left_behind: 0,
    };
    for commit in commits {
        if prop_bool(&commit.properties, FILES_PARTIAL) {
            found.partial += 1;
        }
        match commit.properties.get(FILES).map(|d| &d.value) {
            Some(PropValue::List(paths)) => {
                if paths
                    .iter()
                    .any(|p| p.as_text().is_some_and(|text| text == path))
                {
                    if unreachable(&commit) {
                        found.left_behind += 1;
                    }
                    found.rows.push(commit);
                }
            }
            _ => found.silent += 1,
        }
    }
    found.matched = found.rows.len();
    Ok(found)
}

/// Whether no branch or tag can still reach this commit.
///
/// Only an explicit `false` counts: the property is absent when the run read
/// no reflog, and absent is not the same as unreachable.
fn unreachable(commit: &NodeRecord) -> bool {
    matches!(
        commit.properties.get("reachable").map(|d| &d.value),
        Some(PropValue::Bool(false))
    )
}

/// The line that states what was asked and how much of the plane answered.
fn head(target: &Target, found: &Found) -> String {
    let what = match &target.symbol {
        Some(symbol) => format!("{} — the file declaring `{symbol}`", target.path),
        None => target.path.clone(),
    };
    format!(
        "{what}\n{} of {} commit(s) changed it\n",
        found.matched, found.total
    )
}

/// The matching commits, newest first, one line each.
///
/// A mark from `marks` is appended to its commit's line, keyed by the short
/// sha — how a caller that read the declaration at each revision says what
/// it found without owning a second copy of this format.
fn rows(commits: Vec<&NodeRecord>, limit: usize, marks: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for commit in commits.iter().take(limit) {
        let p = &commit.properties;
        let short = prop_str(p, "short").unwrap_or_default();
        out.push_str(&format!(
            "  {short}  {}  {:<18} {}{}{}{}\n",
            compact::day(p, "committed_at"),
            prop_str(p, "author_name").unwrap_or_default(),
            prop_str(p, "summary").unwrap_or_default(),
            match prop_int(p, FILES_COUNT) {
                Some(n) if n > 1 => format!("  ({n} paths)"),
                _ => String::new(),
            },
            match (prop_bool(p, "is_merge"), unreachable(commit)) {
                (true, true) => "  [merge, left behind]",
                (true, false) => "  [merge]",
                (false, true) => "  [left behind]",
                (false, false) => "",
            },
            match marks.get(short) {
                Some(mark) => format!("  {mark}"),
                None => String::new(),
            },
        ));
    }
    if commits.len() > limit {
        out.push_str(&format!(
            "  … and {} older; raise `limit`\n",
            commits.len() - limit
        ));
    }
    out
}

/// What the answer is, and each way it could be short of the truth.
///
/// Where `marks` carries per-commit findings, a caller has already read the
/// declaration at each commit: the file-level caveat is then answered rather
/// than owed, and repeating it would tell a reader to go and do what was
/// just done.
fn notes(target: &Target, found: &Found, marks: &Marks) -> String {
    let settled = !marks.per_commit.is_empty();
    let mut out = String::new();
    if found.total == 0 {
        out.push_str(
            "note: this plane holds no commits — a repository's history lives in its own \
             plane, named after the code plane with `_git` appended\n",
        );
        return out;
    }
    if found.silent == found.total {
        out.push_str(
            "note: no commit of this plane records the paths it changed, so this answer is \
             empty for want of the facts rather than for want of commits — the git reader \
             writes them from version 1.1.0 on, and the next fold patches the commits \
             already here\n",
        );
        return out;
    }
    if found.silent > 0 {
        out.push_str(&format!(
            "note: {} of {} commit(s) record no paths — folded before the git reader wrote \
             them, so they are absent from this answer rather than clear of it\n",
            found.silent, found.total
        ));
    }
    if found.partial > 0 {
        out.push_str(&format!(
            "note: {} commit(s) record a list that stopped short, so a commit missing here \
             is not evidence it left the path alone\n",
            found.partial
        ));
    }
    if found.left_behind > 0 {
        out.push_str(&format!(
            "note: {} of these no branch or tag can still reach, marked [left behind] — what \
             a rewrite replaced, kept because nothing else remembers it. `git log -- <path>` \
             walks from the refs and lists {} instead\n",
            found.left_behind,
            found.matched.saturating_sub(found.left_behind)
        ));
    }
    if target.literal {
        out.push_str(&format!(
            "note: the code plane knows no `{}`, so it was read as a path; a symbol that \
             matched would have named its own file\n",
            target.path
        ));
    }
    match (&target.symbol, settled) {
        (Some(_), true) => out.push_str(
            "note: each commit was read at its own revision and at its first parent, so the \
             mark is about the declaration itself, not about the file\n",
        ),
        (Some(symbol), false) => out.push_str(&format!(
            "note: a commit is listed for changing the *file*; `recall {symbol} at <sha> vs \
             <sha>~1` says whether that commit moved the declaration itself, and `exact` \
             does it for every commit above\n"
        )),
        (None, _) => out.push_str(
            "note: git records a tree, not a rename, so a path the file was moved from \
             carries its earlier commits under that other name\n",
        ),
    }
    if let Some(note) = &marks.note {
        out.push_str(&format!("note: {note}\n"));
    }
    out
}

/// The planes an answer read, named so a wrong pairing is visible.
fn planes_read(asked: &Asked<'_>) -> Result<String> {
    let history = asked.history.name()?;
    Ok(match asked.code {
        Some(code) => format!(
            "note: read the history plane `{history}` against the code plane `{}`\n",
            code.name()?
        ),
        None => format!(
            "note: read the history plane `{history}` alone — with no code plane beside it, \
             a name resolves to no file\n"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Database;
    use crate::types::{PropDesc, Properties};

    /// One node of the code plane: what it is, its key, and its properties.
    struct Node<'a> {
        label: &'a str,
        key: &'a str,
        pairs: &'a [(&'a str, &'a str)],
    }

    impl<'a> Node<'a> {
        fn new(label: &'a str, key: &'a str, pairs: &'a [(&'a str, &'a str)]) -> Node<'a> {
            Node { label, key, pairs }
        }
    }

    /// One commit's facts. `files` absent models a fold from before the git
    /// reader wrote them; `partial` models a list that stopped short.
    struct Commit<'a> {
        short: &'a str,
        ts: i64,
        files: Option<&'a [&'a str]>,
        partial: bool,
        /// `Some(false)` is a commit a rewrite left behind; `None` is a run
        /// that read no reflog and so cannot say.
        reachable: Option<bool>,
    }

    impl<'a> Commit<'a> {
        /// A commit that recorded its paths in full.
        fn new(short: &'a str, ts: i64, files: &'a [&'a str]) -> Commit<'a> {
            Commit {
                short,
                ts,
                files: Some(files),
                partial: false,
                reachable: None,
            }
        }
    }

    /// A symbol, its file, and three commits: two that changed the file and
    /// one that did not. The third is what tells a filter from a listing.
    fn planes() -> Database {
        let db = Database::in_memory().expect("open");
        code(
            &db,
            &[
                Node::new("Function", "repo::alpha", &[("file", "src/a.rs")]),
                Node::new("File", "src/b.rs", &[("path", "src/b.rs")]),
                Node::new("Function", "repo::placeless", &[]),
            ],
        );
        history(
            &db,
            &[
                Commit::new("c1", 100, &["src/a.rs"]),
                Commit::new("c2", 200, &["src/a.rs", "src/b.rs"]),
                Commit::new("c3", 300, &["docs/x.md"]),
            ],
        );
        db
    }

    fn props(pairs: &[(&str, &str)]) -> Properties {
        let mut p = Properties::new();
        for (key, value) in pairs {
            p.insert(
                (*key).to_string(),
                PropDesc::new(PropValue::Str((*value).to_string())),
            );
        }
        p
    }

    /// The code plane.
    fn code(db: &Database, nodes: &[Node<'_>]) {
        let plane = db.create_plane("repo", Properties::new()).expect("plane");
        let mut txn = plane.write().expect("write");
        for node in nodes {
            txn.create_node_with_key(node.key, &[node.label], props(node.pairs))
                .expect("node");
        }
        txn.commit().expect("commit");
    }

    /// The history plane beside it.
    fn history(db: &Database, commits: &[Commit<'_>]) {
        let plane = db
            .create_plane("repo_git", Properties::new())
            .expect("plane");
        let mut txn = plane.write().expect("write");
        for c in commits {
            let mut p = props(&[
                ("short", c.short),
                ("summary", &format!("did {}", c.short)),
                ("author_name", "crabis"),
                ("committed_at", "2026-11-09T10:00:00+08:00"),
            ]);
            p.insert(
                "committed_ts".to_string(),
                PropDesc::new(PropValue::Int(c.ts)),
            );
            if let Some(files) = c.files {
                let list = files
                    .iter()
                    .map(|f| PropValue::Str((*f).to_string()))
                    .collect();
                p.insert("files".to_string(), PropDesc::new(PropValue::List(list)));
                p.insert(
                    "files_count".to_string(),
                    PropDesc::new(PropValue::Int(files.len() as i64)),
                );
            }
            if c.partial {
                p.insert(
                    "files_partial".to_string(),
                    PropDesc::new(PropValue::Bool(true)),
                );
            }
            if let Some(reachable) = c.reachable {
                p.insert(
                    "reachable".to_string(),
                    PropDesc::new(PropValue::Bool(reachable)),
                );
            }
            txn.create_node_with_key(&format!("commit:{}", c.short), &["Commit"], p)
                .expect("commit node");
        }
        txn.commit().expect("commit");
    }

    fn asked(db: &Database, name: &str) -> String {
        let history = db.plane("repo_git").expect("history");
        let code = db.plane("repo").expect("code");
        touched(&Asked {
            history: &history,
            code: Some(&code),
            name,
            limit: None,
        })
        .expect("touched")
    }

    #[test]
    fn a_symbol_reaches_the_commits_that_changed_its_file() {
        let db = planes();
        let out = asked(&db, "repo::alpha");
        assert!(
            out.contains("src/a.rs — the file declaring `repo::alpha`"),
            "{out}"
        );
        assert!(out.contains("2 of 3 commit(s) changed it"), "{out}");
        assert!(out.contains("c1") && out.contains("c2"), "{out}");
        // The commit that changed another file is the point of the filter.
        assert!(!out.contains("c3"), "{out}");
        // Newest first, and the file-level caveat names the exact next call.
        let (first, second) = (
            out.find("c2").expect("c2 listed"),
            out.find("c1").expect("c1 listed"),
        );
        assert!(first < second, "{out}");
        assert!(
            out.contains("recall repo::alpha at <sha> vs <sha>~1"),
            "{out}"
        );
        assert!(
            out.contains("read the history plane `repo_git` against the code plane `repo`"),
            "{out}"
        );
    }

    #[test]
    fn a_path_is_joined_on_directly_and_counts_what_else_moved_with_it() {
        let db = planes();
        let out = asked(&db, "src/b.rs");
        assert!(out.contains("1 of 3 commit(s) changed it"), "{out}");
        // `files_count` is what makes "(2 paths)" sayable without re-reading git.
        assert!(out.contains("(2 paths)"), "{out}");
        // A path, not a symbol, so the caveat is the one about renames.
        assert!(out.contains("git records a tree, not a rename"), "{out}");
        assert!(!out.contains("vs <sha>~1"), "{out}");
    }

    #[test]
    fn a_name_the_code_plane_does_not_know_is_read_as_a_path_and_says_so() {
        let db = planes();
        let out = asked(&db, "docs/x.md");
        assert!(out.contains("1 of 3 commit(s) changed it"), "{out}");
        assert!(
            out.contains("the code plane knows no `docs/x.md`, so it was read as a path"),
            "{out}"
        );
    }

    #[test]
    fn a_symbol_with_no_file_is_told_so_rather_than_joined_on_nothing() {
        let db = planes();
        let out = asked(&db, "repo::placeless");
        assert!(out.contains("records no file"), "{out}");
        assert!(out.contains("describe repo::placeless"), "{out}");
    }

    /// The failure this verb exists to not have: a plane folded before the
    /// git reader recorded any paths answers nothing, and an empty listing
    /// would read as "no commit touched it".
    #[test]
    fn a_plane_that_records_no_paths_says_that_rather_than_nothing() {
        let db = Database::in_memory().expect("open");
        code(
            &db,
            &[Node::new(
                "Function",
                "repo::alpha",
                &[("file", "src/a.rs")],
            )],
        );
        history(
            &db,
            &[Commit {
                short: "c1",
                ts: 100,
                files: None,
                partial: false,
                reachable: None,
            }],
        );
        let out = asked(&db, "repo::alpha");
        assert!(out.contains("0 of 1 commit(s)"), "{out}");
        assert!(
            out.contains("no commit of this plane records the paths it changed"),
            "{out}"
        );
    }

    /// A list that stopped short is the one case where absence is not
    /// evidence, so the count of such commits has to reach the reader.
    #[test]
    fn a_list_that_stopped_short_is_counted_against_the_answer() {
        let db = Database::in_memory().expect("open");
        code(
            &db,
            &[Node::new(
                "Function",
                "repo::alpha",
                &[("file", "src/a.rs")],
            )],
        );
        history(
            &db,
            &[
                Commit::new("c1", 100, &["src/a.rs"]),
                Commit {
                    short: "c2",
                    ts: 200,
                    files: Some(&["other.rs"]),
                    partial: true,
                    reachable: None,
                },
            ],
        );
        let out = asked(&db, "repo::alpha");
        assert!(out.contains("1 of 2 commit(s) changed it"), "{out}");
        assert!(
            out.contains("1 commit(s) record a list that stopped short"),
            "{out}"
        );
    }

    /// With no code plane the verb still answers a path, and says the name
    /// could not have been a symbol.
    #[test]
    fn with_no_code_plane_a_path_still_answers_and_the_note_says_why() {
        let db = planes();
        let history = db.plane("repo_git").expect("history");
        let out = touched(&Asked {
            history: &history,
            code: None,
            name: "src/a.rs",
            limit: None,
        })
        .expect("touched");
        assert!(out.contains("2 of 3 commit(s) changed it"), "{out}");
        assert!(
            out.contains("read the history plane `repo_git` alone"),
            "{out}"
        );
    }

    /// The answer is longer than `git log -- <path>` whenever a rewrite left
    /// commits behind, and an unexplained disagreement with git would read
    /// as a wrong answer rather than a fuller one.
    #[test]
    fn a_commit_a_rewrite_left_behind_is_marked_and_reconciled_with_git() {
        let db = Database::in_memory().expect("open");
        code(
            &db,
            &[Node::new(
                "Function",
                "repo::alpha",
                &[("file", "src/a.rs")],
            )],
        );
        history(
            &db,
            &[
                Commit::new("c1", 100, &["src/a.rs"]),
                Commit {
                    short: "old",
                    ts: 50,
                    files: Some(&["src/a.rs"]),
                    partial: false,
                    reachable: Some(false),
                },
            ],
        );
        let out = asked(&db, "repo::alpha");
        assert!(out.contains("2 of 2 commit(s) changed it"), "{out}");
        assert!(
            out.contains("old") && out.contains("[left behind]"),
            "{out}"
        );
        assert!(
            out.contains("1 of these no branch or tag can still reach"),
            "{out}"
        );
        assert!(out.contains("lists 1 instead"), "{out}");
        // A run that read no reflog records nothing, and absent is not false.
        assert!(
            !out.contains("c1  2026-11-09  crabis             did c1  [left"),
            "{out}"
        );
    }

    #[test]
    fn a_limit_says_how_many_older_commits_it_left_out() {
        let db = planes();
        let history = db.plane("repo_git").expect("history");
        let code = db.plane("repo").expect("code");
        let out = touched(&Asked {
            history: &history,
            code: Some(&code),
            name: "repo::alpha",
            limit: Some(1),
        })
        .expect("touched");
        assert!(out.contains("c2"), "{out}");
        assert!(out.contains("… and 1 older; raise `limit`"), "{out}");
    }
}
