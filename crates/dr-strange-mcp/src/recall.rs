//! `recall`: code as it was at a git revision. The revision is resolved over
//! the plane's `<plane>_git` history; the bytes come from git's object store.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{Result as AnyResult, anyhow, bail};
use dr_strange_core::compact::{self, Resolved};
use dr_strange_core::rev::{self, CommitRef, RevAnswer, RevExpr, Sha};
use dr_strange_core::{Database, PlaneHandle, PropValue, Properties};
use dr_strange_llm::git::{Entry, Git, GitTree, ObjKind, RelPath};
use dr_strange_llm::{Host, LivePlugins, Plugins, Preprocessed, route_paths};
use serde_json::Value;

use crate::{SNIPPET_CAP, default_plane, numbered, parse_range, source_root};

/// Lines a read returns when the caller names none.
const DEFAULT_LINES: usize = 40;
/// Bytes probed for a NUL before a file is called binary.
const BINARY_PROBE: usize = 8 << 10;
/// Most entries a directory listing shows.
const ENTRY_CAP: usize = 200;
/// Most same-named symbols an answer about a missing one lists.
const SAME_NAME_CAP: usize = 5;

/// `recall`'s request: something in a repository, read as it was at a revision.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RecallReq {
    /// The code plane; the `<plane>_git` plane beside it names the revisions.
    #[serde(default = "default_plane")]
    pub plane: String,
    /// What to read, relative to the tree's root: a file, `path:line` /
    /// `path:start-end`, a directory (`""` for the root), or a symbol (fuzzy,
    /// as `snippet` takes it), found by parsing its file as it was.
    pub name: String,
    /// The revision: a sha (4+ hex digits), a branch, a tag, HEAD, a date
    /// (YYYY-MM-DD or RFC-3339) or `<rev>@{<date>}`, each optionally followed
    /// by `~n` / `^n`.
    pub at: String,
    /// Lines returned (default 40 for a file, a symbol's own extent; capped at
    /// 400); a `path:start-end` range returns its own lines. The answer says
    /// how to read on.
    #[serde(default)]
    pub lines: Option<usize>,
}

/// Where `recall` gets the preprocessors that find a symbol in a file as it was.
pub trait Parsers: Send + Sync {
    /// Route `file`, read through `host`, through the installed preprocessors.
    fn parse(&self, host: &dyn Host, file: &str) -> AnyResult<Preprocessed>;
}

impl Parsers for Plugins {
    fn parse(&self, host: &dyn Host, file: &str) -> AnyResult<Preprocessed> {
        route_paths(host, vec![file.to_string()], None, self)
    }
}

impl Parsers for Mutex<LivePlugins> {
    fn parse(&self, host: &dyn Host, file: &str) -> AnyResult<Preprocessed> {
        let mut live = self.lock().unwrap_or_else(PoisonError::into_inner);
        route_paths(host, vec![file.to_string()], None, live.current()?)
    }
}

/// `recall`'s body. `fallback_root` is the tree the process was started with,
/// used only when the plane does not record one of its own.
pub fn recall_logic(
    db: &Database,
    fallback_root: Option<&Path>,
    parsers: &dyn Parsers,
    req: RecallReq,
) -> AnyResult<Value> {
    let plane = db.plane(&req.plane)?;
    let Some(root) = source_root(&plane, fallback_root) else {
        bail!(
            "no source tree for plane `{}` — recall reads the git checkout the plane was \
             parsed from; digest it from a checkout, or attach the tree (`serve watch` does)",
            req.plane
        );
    };
    let (commit, note) = resolve(db, &plane, &root, &req)?;
    let tree = GitTree::open(&root, commit.sha.clone()).map_err(|e| {
        if commit.reachable {
            e
        } else {
            anyhow!("{e:#} — the history plane marks it reachable: false, so gc has pruned it")
        }
    })?;
    let mut out = rev::commit_header(&commit, &req.at);
    out.push_str(&note);
    out.push_str(&read(&plane, &tree, parsers, &req)?);
    Ok(Value::String(out))
}

/// The commit `req.at` names, and a note when git rather than the history plane named it.
fn resolve(
    db: &Database,
    plane: &PlaneHandle<'_>,
    root: &Path,
    req: &RecallReq,
) -> AnyResult<(CommitRef, String)> {
    let history_name = compact::history_plane_name(&req.plane);
    let Ok(history) = db.plane(&history_name) else {
        let note = format!(
            "note: no `{history_name}` plane — git resolved the revision; a digest of this \
             checkout records its history\n"
        );
        return Ok((resolve_with_git(root, &req.at)?, note));
    };
    match rev::resolve_rev(&history, synced_commit(plane).as_ref(), &req.at)? {
        RevAnswer::One(commit) => Ok((commit, String::new())),
        RevAnswer::Many(commits) => bail!("{}", rev::ambiguous(&req.at, &commits).trim_end()),
        RevAnswer::None(why) => bail!(
            "{why} (`history {}` lists the branches, tags and newest commits)",
            req.plane
        ),
    }
}

/// The commit the plane last folded, which stands in for a detached HEAD.
fn synced_commit(plane: &PlaneHandle<'_>) -> Option<Sha> {
    str_prop(&plane.properties().ok()?, "synced_commit").and_then(|sha| Sha::parse(&sha))
}

/// The commit git itself resolves `at` to, for a checkout no history plane describes.
fn resolve_with_git(root: &Path, at: &str) -> AnyResult<CommitRef> {
    if at.contains("@{") {
        bail!("`{at}` walks the history plane, and there is none — digest this checkout first");
    }
    let git = Git::new(root);
    let sha = if matches!(rev::parse_rev(at), Ok(RevExpr::AtDate(..))) {
        let before = match at.len() {
            10 => format!("--before={at}T23:59:59Z"),
            _ => format!("--before={at}"),
        };
        git.run(&["rev-list", "-1", "--first-parent", &before, "HEAD"])?
    } else {
        let spec = format!("{at}^{{commit}}");
        git.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])
        .map_err(|_| anyhow!("git knows no commit `{at}`"))?
    };
    let sha = String::from_utf8_lossy(&sha).trim().to_string();
    if sha.is_empty() {
        bail!("nothing on HEAD's first-parent line is as old as {at}");
    }
    let shown = git.run(&["show", "-s", "--format=%H%x00%cs%x00%an%x00%s", &sha])?;
    let shown = String::from_utf8_lossy(&shown);
    let mut field = shown.trim_end_matches('\n').split('\0');
    let (Some(full), Some(date), Some(author), Some(summary)) =
        (field.next(), field.next(), field.next(), field.next())
    else {
        bail!("git described {sha} in a shape recall does not read");
    };
    Ok(CommitRef {
        sha: Sha::parse(full).ok_or_else(|| anyhow!("git named `{full}`, not a full sha"))?,
        date: date.into(),
        author: author.into(),
        summary: summary.into(),
        reachable: true,
    })
}

/// The body of the answer: a range, a file, a directory, or — when the name is
/// none of those at that commit — a symbol.
fn read(
    plane: &PlaneHandle<'_>,
    tree: &GitTree,
    parsers: &dyn Parsers,
    req: &RecallReq,
) -> AnyResult<String> {
    let at = tree.sha().short();
    if let Some((path, start, end)) = parse_range(&req.name) {
        let rel = RelPath::parse(path)?;
        return match tree.kind(&rel)? {
            Some(ObjKind::File | ObjKind::Link) => path_lines(tree, &rel, Some((start, end)), None),
            Some(_) => bail!("`{path}` is not a file at {at}; a line range needs one"),
            None => bail!("`{path}` does not exist at {at} — recall its directory to see what did"),
        };
    }
    let rel = match RelPath::parse(&req.name) {
        Ok(rel) => rel,
        Err(_) if req.name.starts_with("::") => return symbol_lines(plane, tree, parsers, req),
        Err(e) => return Err(e),
    };
    match tree.kind(&rel)? {
        Some(ObjKind::Dir) => Ok(listing(&rel, &tree.ls(&rel)?, at)),
        Some(ObjKind::File | ObjKind::Link) => path_lines(tree, &rel, None, req.lines),
        Some(ObjKind::Submodule) => bail!(
            "`{}` is a submodule at {at} — another repository's commit, which recall does \
             not follow",
            req.name
        ),
        None => symbol_lines(plane, tree, parsers, req),
    }
}

/// A file's lines at the tree's commit: the range asked for, else from the top.
fn path_lines(
    tree: &GitTree,
    rel: &RelPath,
    span: Option<(usize, usize)>,
    lines: Option<usize>,
) -> AnyResult<String> {
    let (path, at) = (rel.as_str(), tree.sha().short());
    let text = match text_at(tree, path)? {
        Text::Shown(text) => text,
        Text::Withheld(why) => return Ok(why),
    };
    let (start, want) = match span {
        Some((a, b)) => (a, b + 1 - a),
        None => (1, lines.unwrap_or(DEFAULT_LINES)),
    };
    let want = want.clamp(1, SNIPPET_CAP);
    let cut = excerpt(path, &text, start, start + want - 1)?;
    let mut out = cut.header + &cut.lines;
    if cut.end < cut.total {
        out.push_str(&format!(
            "… {path} continues to line {}; recall {path}:{}-{} at {at} reads on\n",
            cut.total,
            cut.end + 1,
            (cut.end + want).min(cut.total)
        ));
    }
    out.push_str(&format!(
        "snippet {path}:{start}-{} reads the same lines of the file as it is now\n",
        cut.end
    ));
    Ok(out)
}

/// A symbol's declaration as it was: today's key, its file at the tree's
/// commit parsed by the installed preprocessors, and the lines they place it on.
fn symbol_lines(
    plane: &PlaneHandle<'_>,
    tree: &GitTree,
    parsers: &dyn Parsers,
    req: &RecallReq,
) -> AnyResult<String> {
    let at = tree.sha().short();
    let node = match compact::resolve(plane, &req.name)? {
        Resolved::One(node) => node,
        Resolved::Many(hits) => bail!("{}", compact::candidates(&req.name, &hits).trim_end()),
        Resolved::None => bail!(
            "`{}` is neither a path at {at} nor a symbol in plane `{}` — recall a directory \
             at {at} to see what the tree held",
            req.name,
            req.plane
        ),
    };
    let key = node.external_key.clone().unwrap_or_default();
    let Some(file) =
        str_prop(&node.properties, "file").or_else(|| str_prop(&node.properties, "path"))
    else {
        bail!("`{key}` records no file, so there is nothing of it to read at a revision");
    };
    let later = synced_commit(plane).map_or_else(|| "HEAD".to_string(), |s| s.as_str().to_string());
    let (file, note) = file_at(tree, &file, &later)?;
    let parsed = parsers.parse(tree, &file)?;
    let Some(fact) = parsed.nodes.iter().find(|n| n.key == key) else {
        return Err(not_declared(&key, &file, at, &parsed));
    };
    let Some(line) = int_prop(&fact.props, "line") else {
        bail!("the parser placed `{key}` in {file} at {at} but recorded no line for it");
    };
    let declared = int_prop(&fact.props, "end_line").filter(|end| *end >= line);
    let end = match req.lines {
        Some(n) => line + n.clamp(1, SNIPPET_CAP) - 1,
        None => declared
            .unwrap_or(line + DEFAULT_LINES - 1)
            .min(line + SNIPPET_CAP - 1),
    };
    let text = match text_at(tree, &file)? {
        Text::Shown(text) => text,
        Text::Withheld(why) => return Ok(why),
    };
    let cut = excerpt(&file, &text, line, end)?;
    let mut out = format!("{}in {key}\n{note}{}", cut.header, cut.lines);
    if let Some(last) = declared.filter(|last| *last > cut.end) {
        out.push_str(&format!(
            "… `{key}` continues to line {last}; recall {file}:{}-{last} at {at} reads on\n",
            cut.end + 1
        ));
    }
    out.push_str(&format!(
        "snippet {key} reads it as it is now; context {key} — callers, callees and every \
         other edge\n"
    ));
    Ok(out)
}

/// Where today's `file` was at the tree's commit, with a note when it had another name.
fn file_at(tree: &GitTree, file: &str, later: &str) -> AnyResult<(String, String)> {
    let rel = RelPath::parse(file)?;
    if matches!(tree.kind(&rel)?, Some(ObjKind::File | ObjKind::Link)) {
        return Ok((file.to_string(), String::new()));
    }
    let at = tree.sha().short();
    match tree.renamed_from(later, &rel)? {
        Some(old) => {
            let note = format!("note: {file} was {old} at {at}\n");
            Ok((old, note))
        }
        None => bail!(
            "{file} does not exist at {at}, and git sees no earlier name for it — the symbol \
             was added after that commit"
        ),
    }
}

/// Why a symbol is absent from its file's parse at a commit, with what was there by that name.
fn not_declared(key: &str, file: &str, at: &str, parsed: &Preprocessed) -> anyhow::Error {
    let tail = |k: &str| k.rsplit([':', '.']).next().unwrap_or(k).to_string();
    let name = tail(key);
    let same: Vec<&str> = parsed
        .nodes
        .iter()
        .map(|n| n.key.as_str())
        .filter(|k| tail(k) == name)
        .take(SAME_NAME_CAP)
        .collect();
    let mut msg = format!(
        "`{key}` is not declared in {file} at {at} ({} symbols parsed there) — added later, \
         renamed or moved?",
        parsed.nodes.len()
    );
    if !same.is_empty() {
        msg.push_str(&format!("\nsame name there: {}", same.join(", ")));
    }
    msg.push_str(&format!("\nrecall {file} at {at} reads the whole file"));
    anyhow!(msg)
}

/// A file's text at a commit, or the line saying why it is not shown.
enum Text {
    Shown(String),
    Withheld(String),
}

fn text_at(tree: &GitTree, path: &str) -> AnyResult<Text> {
    let at = tree.sha().short();
    let bytes = tree.read(path)?;
    if bytes.iter().take(BINARY_PROBE).any(|b| *b == 0) {
        let size = human(bytes.len() as u64);
        return Ok(Text::Withheld(format!(
            "{path} is binary ({size}) at {at} — not shown\n"
        )));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text.lines().next().is_none() {
        return Ok(Text::Withheld(format!("{path} is empty at {at}\n")));
    }
    Ok(Text::Shown(text))
}

/// Numbered lines cut from a file, under a `path:a-b (n lines of total)` header.
struct Excerpt {
    header: String,
    lines: String,
    end: usize,
    total: usize,
}

fn excerpt(path: &str, text: &str, start: usize, end: usize) -> AnyResult<Excerpt> {
    let total = text.lines().count();
    if start > total {
        bail!("{path} has {total} lines; line {start} is past its end");
    }
    let end = end.min(total);
    Ok(Excerpt {
        header: format!(
            "{path}:{start}-{end} ({} lines of {total})\n",
            end + 1 - start
        ),
        lines: numbered(start, text.lines().skip(start - 1).take(end + 1 - start)),
        end,
        total,
    })
}

/// A directory's entries at `at`, directories first, one line each.
fn listing(dir: &RelPath, entries: &[Entry], at: &str) -> String {
    let base = match dir.as_str() {
        "" => String::new(),
        d => format!("{d}/"),
    };
    let count = |kinds: &[ObjKind]| entries.iter().filter(|e| kinds.contains(&e.kind)).count();
    let mut out = format!(
        "{} at {at} — {} dir(s), {} file(s)",
        if base.is_empty() { "./" } else { &base },
        count(&[ObjKind::Dir]),
        count(&[ObjKind::File, ObjKind::Link])
    );
    match count(&[ObjKind::Submodule]) {
        0 => out.push('\n'),
        n => out.push_str(&format!(", {n} submodule(s)\n")),
    }
    let mut sorted: Vec<&Entry> = entries.iter().collect();
    sorted.sort_by_key(|e| (e.kind != ObjKind::Dir, e.name.as_str()));
    let rows: Vec<(String, String)> = sorted
        .iter()
        .take(ENTRY_CAP)
        .map(|e| match e.kind {
            ObjKind::Dir => (format!("{}/", e.name), "dir".into()),
            ObjKind::File => (e.name.clone(), human(e.size.unwrap_or(0))),
            ObjKind::Link => (e.name.clone(), "link".into()),
            ObjKind::Submodule => (e.name.clone(), "submodule".into()),
        })
        .collect();
    let width = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
    for (name, what) in &rows {
        out.push_str(&format!("  {name:<width$}  {what}\n"));
    }
    if entries.len() > ENTRY_CAP {
        out.push_str(&format!("  … and {} more\n", entries.len() - ENTRY_CAP));
    }
    out.push_str(&format!("recall {base}<entry> at {at} reads one\n"));
    out
}

/// A byte count as a reader sizes it: `812 B`, `1.2 KiB`, `3.4 MiB`.
fn human(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

fn str_prop(props: &Properties, key: &str) -> Option<String> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::Str(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn int_prop(props: &Properties, key: &str) -> Option<usize> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::Int(n)) if *n > 0 => usize::try_from(*n).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use dr_strange_core::PropDesc;
    use dr_strange_llm::preprocess::Input;
    use dr_strange_llm::{DigestNode, Manifest, Preprocessor};
    use serde_json::{from_value, json};

    use super::*;

    /// Run git in `dir`, isolated from the user's configuration, committing as Ada at `date`.
    fn git(dir: &Path, date: &str, args: &[&str]) -> String {
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
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn write(dir: &Path, rel: &str, body: &[u8]) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn commit(dir: &Path, message: &str, date: &str) -> String {
        git(dir, date, &["add", "-A"]);
        git(dir, date, &["commit", "-q", "-m", message]);
        git(dir, date, &["rev-parse", "HEAD"]).trim().to_string()
    }

    fn props(pairs: Vec<(&str, PropValue)>) -> Properties {
        let mut out = Properties::new();
        for (k, v) in pairs {
            out.insert(k.into(), PropDesc::new(v));
        }
        out
    }

    /// A code-shaped handler: claims `.rs`; each `fn NAME` line opens a symbol
    /// `m::NAME` that runs to the line before the next one.
    struct LineLang;

    impl Preprocessor for LineLang {
        fn manifest(&self) -> Manifest {
            Manifest {
                name: "line".into(),
                version: "1".into(),
                extensions: vec!["rs".into()],
                logo: None,
                build: None,
                source: None,
            }
        }

        fn preprocess(&self, input: &Input<'_>, host: &dyn Host) -> AnyResult<Preprocessed> {
            let Input::Files { paths } = input else {
                bail!("LineLang routes files");
            };
            let mut out = Preprocessed::default();
            for path in *paths {
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
                        props: props(vec![
                            ("file", PropValue::Str(path.clone())),
                            ("line", PropValue::Int(i as i64 + 1)),
                            ("end_line", PropValue::Int(end as i64)),
                        ]),
                        ..Default::default()
                    });
                }
            }
            Ok(out)
        }
    }

    /// A checkout with two commits (`one` tagged v1, then `two` on main) and a
    /// working tree that has moved on from both; its code plane `repo`, and a
    /// `repo_git` history plane describing it when `history` is set.
    struct Fixture {
        dir: tempfile::TempDir,
        db: Database,
        one: String,
        two: String,
    }

    fn fixture(history: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, "2026-08-01T10:00:00Z", &["init", "-q", "-b", "main"]);
        write(root, "src/lib.rs", b"fn one() {}\n");
        write(root, "src/sym.rs", b"fn alpha\n  a1\nfn beta\n  b1\n");
        write(root, "src/old.rs", b"fn moved\n  m1\n");
        write(root, "docs/a.md", b"# a\n");
        let one = commit(root, "one", "2026-08-01T10:00:00Z");
        git(root, "2026-08-01T10:00:00Z", &["tag", "v1"]);
        let body: String = (1..=60).map(|i| format!("line {i}\n")).collect();
        write(root, "src/lib.rs", body.as_bytes());
        write(
            root,
            "src/sym.rs",
            b"fn beta\n  b2\n  b2 more\nfn gamma\n  g\n",
        );
        std::fs::remove_file(root.join("src/old.rs")).unwrap();
        write(root, "src/new.rs", b"fn moved\n  m1\n");
        write(root, "blob.bin", &[0, 1, 2]);
        let two = commit(root, "two", "2026-08-02T10:00:00Z");
        write(root, "src/lib.rs", b"fn working_tree() {}\n");

        let db = Database::in_memory().unwrap();
        code_plane(&db, root, &two);
        if history {
            history_plane(&db, &one, &two);
        }
        Fixture { dir, db, one, two }
    }

    /// The code plane as a digest of `two` would leave it.
    fn code_plane(db: &Database, root: &Path, two: &str) {
        let code = props(vec![
            ("synced_root", PropValue::Str(root.display().to_string())),
            ("synced_commit", PropValue::Str(two.into())),
        ]);
        let p = db.create_plane("repo", code).unwrap();
        let mut txn = p.write().unwrap();
        for (key, file) in [
            ("m::beta", "src/sym.rs"),
            ("m::gamma", "src/sym.rs"),
            ("m::moved", "src/new.rs"),
        ] {
            let node = props(vec![("file", PropValue::Str(file.into()))]);
            txn.create_node_with_key(key, &["Function"], node).unwrap();
        }
        txn.commit().unwrap();
    }

    fn history_plane(db: &Database, one: &str, two: &str) {
        let p = db.create_plane("repo_git", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut ids = Vec::new();
        for (sha, summary, day, ts) in [
            (one, "one", "01", 1_785_578_400),
            (two, "two", "02", 1_785_664_800),
        ] {
            let node = props(vec![
                ("sha", PropValue::Str(sha.into())),
                ("summary", PropValue::Str(summary.into())),
                ("author_name", PropValue::Str("Ada".into())),
                (
                    "committed_at",
                    PropValue::Str(format!("2026-08-{day}T10:00:00+00:00")),
                ),
                ("committed_ts", PropValue::Int(ts)),
            ]);
            let key = format!("commit:{sha}");
            ids.push(txn.create_node_with_key(&key, &["Commit"], node).unwrap());
        }
        let order = props(vec![("order", PropValue::Int(1))]);
        txn.create_edge(ids[1], ids[0], "PARENT", order).unwrap();
        let main = props(vec![
            ("name", PropValue::Str("main".into())),
            ("is_head", PropValue::Bool(true)),
            ("remote", PropValue::Bool(false)),
            ("tip", PropValue::Str(two.into())),
        ]);
        txn.create_node_with_key("branch:main", &["Branch"], main)
            .unwrap();
        let tag = props(vec![
            ("name", PropValue::Str("v1".into())),
            ("target", PropValue::Str(one.into())),
        ]);
        txn.create_node_with_key("tag:v1", &["Tag"], tag).unwrap();
        txn.commit().unwrap();
    }

    fn recall(db: &Database, req: serde_json::Value) -> AnyResult<String> {
        let parsers = Plugins::from_handlers(vec![Box::new(LineLang)]);
        let out = recall_logic(db, None, &parsers, from_value(req).unwrap())?;
        Ok(out.as_str().unwrap().to_string())
    }

    #[test]
    fn a_file_reads_as_it_was_at_a_tag_not_as_it_is() {
        let f = fixture(true);
        let out = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "v1"}),
        )
        .unwrap();
        let header = format!("at {}  2026-08-01  Ada  one  (via v1)\n", &f.one[..12]);
        assert!(out.starts_with(&header), "{out}");
        assert!(
            out.contains("src/lib.rs:1-1 (1 lines of 1)\n    1 | fn one() {}\n"),
            "{out}"
        );
        assert!(!out.contains("working_tree"), "{out}");
        assert!(
            out.ends_with("snippet src/lib.rs:1-1 reads the same lines of the file as it is now\n"),
            "{out}"
        );
    }

    #[test]
    fn a_long_file_stops_and_says_how_to_read_on() {
        let f = fixture(true);
        let two = &f.two[..12];
        let out = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "main"}),
        )
        .unwrap();
        assert!(out.contains("src/lib.rs:1-40 (40 lines of 60)"), "{out}");
        assert!(
            out.contains(&format!("recall src/lib.rs:41-60 at {two} reads on")),
            "{out}"
        );
        let range = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs:10-12", "at": &f.two[..8]}),
        )
        .unwrap();
        assert!(
            !range.lines().next().unwrap().contains("via"),
            "a sha needs no gloss: {range}"
        );
        assert!(
            range.contains("   10 | line 10\n   11 | line 11\n   12 | line 12\n"),
            "{range}"
        );
        assert!(!range.contains("line 13"), "{range}");
        let short = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "HEAD", "lines": 5}),
        )
        .unwrap();
        assert!(short.contains("recall src/lib.rs:6-10 at"), "{short}");
    }

    #[test]
    fn a_directory_lists_what_it_held_then() {
        let f = fixture(true);
        let top = recall(&f.db, json!({"plane": "repo", "name": "", "at": "HEAD"})).unwrap();
        assert!(top.contains("./ at "), "{top}");
        assert!(top.contains("2 dir(s), 1 file(s)"), "{top}");
        assert!(
            top.contains("  docs/     dir\n  src/      dir\n  blob.bin  3 B\n"),
            "{top}"
        );
        let src = recall(&f.db, json!({"plane": "repo", "name": "src", "at": "v1"})).unwrap();
        assert!(src.contains("  lib.rs  12 B\n"), "{src}");
        assert!(
            src.contains(&format!("recall src/<entry> at {} reads one", &f.one[..12])),
            "{src}"
        );
    }

    #[test]
    fn what_cannot_be_shown_says_why() {
        let f = fixture(true);
        let bin = recall(
            &f.db,
            json!({"plane": "repo", "name": "blob.bin", "at": "main"}),
        )
        .unwrap();
        assert!(bin.contains("blob.bin is binary (3 B)"), "{bin}");
        let gone = recall(
            &f.db,
            json!({"plane": "repo", "name": "blob.bin:1-2", "at": "v1"}),
        )
        .unwrap_err();
        assert!(
            gone.to_string().contains("`blob.bin` does not exist at"),
            "{gone}"
        );
        let past = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs:5-6", "at": "v1"}),
        )
        .unwrap_err();
        assert!(past.to_string().contains("has 1 lines"), "{past}");
        let escape = recall(
            &f.db,
            json!({"plane": "repo", "name": "../etc/passwd", "at": "v1"}),
        )
        .unwrap_err();
        assert!(escape.to_string().contains("climbs out"), "{escape}");
    }

    #[test]
    fn an_unknown_revision_names_the_way_forward() {
        let f = fixture(true);
        let err = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "nope"}),
        )
        .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("no branch or tag is named `nope`"), "{err}");
        assert!(err.contains("`history repo` lists"), "{err}");
    }

    #[test]
    fn without_a_history_plane_git_resolves_the_revision() {
        let f = fixture(false);
        let out = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "v1"}),
        )
        .unwrap();
        let header = format!("at {}  2026-08-01  Ada  one  (via v1)\n", &f.one[..12]);
        assert!(out.starts_with(&header), "{out}");
        assert!(out.contains("note: no `repo_git` plane"), "{out}");
        let dated = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "2026-08-01"}),
        )
        .unwrap();
        assert!(dated.contains("fn one() {}"), "{dated}");
        let walked = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "main@{2026-08-01}"}),
        );
        assert!(
            walked
                .unwrap_err()
                .to_string()
                .contains("digest this checkout")
        );
        let err = recall(
            &f.db,
            json!({"plane": "repo", "name": "src/lib.rs", "at": "nope"}),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("git knows no commit `nope`"),
            "{err}"
        );
        drop(f.dir);
    }

    #[test]
    fn a_plane_without_a_tree_says_so() {
        let db = Database::in_memory().unwrap();
        db.create_plane("bare", Properties::new()).unwrap();
        let err = recall(&db, json!({"plane": "bare", "name": "a.rs", "at": "HEAD"})).unwrap_err();
        assert!(
            err.to_string().contains("no source tree for plane `bare`"),
            "{err}"
        );
    }

    #[test]
    fn a_symbol_reads_as_its_file_declared_it_then() {
        let f = fixture(true);
        let then = recall(
            &f.db,
            json!({"plane": "repo", "name": "m::beta", "at": "v1"}),
        )
        .unwrap();
        assert!(
            then.contains(
                "src/sym.rs:3-4 (2 lines of 4)\nin m::beta\n    3 | fn beta\n    4 |   b1\n"
            ),
            "{then}"
        );
        assert!(!then.contains("b2"), "{then}");
        assert!(
            then.ends_with("context m::beta — callers, callees and every other edge\n"),
            "{then}"
        );
        let now = recall(
            &f.db,
            json!({"plane": "repo", "name": "beta", "at": "main"}),
        )
        .unwrap();
        assert!(now.contains("src/sym.rs:1-3 (3 lines of 5)"), "{now}");
        assert!(
            !now.contains("continues"),
            "a whole declaration is whole: {now}"
        );
        let cut = recall(
            &f.db,
            json!({"plane": "repo", "name": "m::beta", "at": "main", "lines": 1}),
        )
        .unwrap();
        assert!(
            cut.contains(&format!(
                "… `m::beta` continues to line 3; recall src/sym.rs:2-3 at {} reads on",
                &f.two[..12]
            )),
            "{cut}"
        );
    }

    #[test]
    fn a_symbol_absent_then_says_what_its_file_held() {
        let f = fixture(true);
        let err = recall(
            &f.db,
            json!({"plane": "repo", "name": "m::gamma", "at": "v1"}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`m::gamma` is not declared in src/sym.rs at"),
            "{err}"
        );
        assert!(err.contains("(2 symbols parsed there)"), "{err}");
        assert!(err.contains("recall src/sym.rs at"), "{err}");
    }

    #[test]
    fn a_renamed_file_is_followed_back_to_its_old_name() {
        let f = fixture(true);
        let out = recall(
            &f.db,
            json!({"plane": "repo", "name": "m::moved", "at": "v1"}),
        )
        .unwrap();
        assert!(
            out.contains(&format!(
                "note: src/new.rs was src/old.rs at {}",
                &f.one[..12]
            )),
            "{out}"
        );
        assert!(out.contains("src/old.rs:1-2 (2 lines of 2)"), "{out}");
    }

    #[test]
    fn a_name_that_is_neither_path_nor_single_symbol_says_so() {
        let f = fixture(true);
        let many = recall(&f.db, json!({"plane": "repo", "name": "m::", "at": "v1"}))
            .unwrap_err()
            .to_string();
        assert!(
            many.contains("m::beta") && many.contains("m::gamma"),
            "{many}"
        );
        let none = recall(&f.db, json!({"plane": "repo", "name": "zzz", "at": "v1"}))
            .unwrap_err()
            .to_string();
        assert!(none.contains("`zzz` is neither a path at"), "{none}");
    }
}
