//! `affected`: the tests a change reaches.
//!
//! [`compact::affected`] walks the graph and knows nothing of git or of build
//! systems, which is the right split — but a change is a diff, and the
//! package that holds a file is written in a manifest. Both are read here and
//! handed down as plain keys and path prefixes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result as AnyResult, anyhow, bail};
use dr_strange_core::compact::{self, Affected, quoted};
use dr_strange_core::{Database, PlaneHandle};
use dr_strange_llm::git::Git;

use crate::recall::{DirDeclarations, Parsers, int_prop, str_prop};
use crate::{TreeAccess, default_plane, recorded_root};

/// Hops of incoming edges walked when the caller names no depth. A test
/// usually sits one hop from what it tests and two from what that calls;
/// three is the first depth that reaches a helper's helper without the walk
/// becoming the whole plane.
const DEFAULT_DEPTH: usize = 3;

/// The files that mark a directory as a package, in the ecosystems the
/// plugins parse. First one found walking up from a changed file wins, so a
/// crate inside a workspace answers with the crate.
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "package.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "pyproject.toml",
    "setup.py",
    "composer.json",
    "Gemfile",
    "*.csproj",
];

/// `affected`'s request: a change, and how cautious to be about it.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AffectedReq {
    /// The code plane. The tests come from it, so it must be the plane this
    /// working tree was parsed into.
    #[serde(default = "default_plane")]
    pub plane: String,
    /// An older revision to compare `HEAD` against. Omitted, the change is
    /// the working tree's own: what is uncommitted right now, which is the
    /// question asked before a commit rather than after one.
    #[serde(default)]
    pub vs: Option<String>,
    /// Add every test under the packages the change touched, reached or not.
    /// The answer to a walk that is a lower bound: slower to run, and it
    /// cannot miss a test the parse could not see the edge to.
    #[serde(default)]
    pub safe: bool,
    /// Hops of incoming structural edges to walk, clamped to 1..=6.
    #[serde(default)]
    pub depth: Option<usize>,
}

/// What a change is, before the graph is asked anything: the declarations it
/// touched, the files they are in, and anything the reading of it could not
/// settle.
struct Change {
    seeds: Vec<String>,
    files: Vec<String>,
    note: String,
}

/// What a caller wants back. Not a field of [`AffectedReq`]: the MCP tool
/// answers an agent, which wants the compact text and nothing else, and these
/// two exist for a CI template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The compact answer — one fact per line.
    Compact,
    /// Markdown for a pull request, the change's own note quoted above it.
    Report,
    /// The changed declarations' keys, one per line and nothing else, so a
    /// template can ask another verb about each in turn.
    Seeds,
}

/// `affected`'s body. `fallback_root` is the tree the process was started
/// with, used only when the plane does not record one of its own.
pub fn affected_logic(
    db: &Database,
    fallback_root: Option<&Path>,
    parsers: &dyn Parsers,
    req: AffectedReq,
    out: Output,
) -> AnyResult<String> {
    affected_logic_in(db, TreeAccess::local(fallback_root), parsers, req, out)
}

/// [`affected_logic`] under an explicit tree policy — what the served `/mcp`
/// uses, where a plane's own root is not automatically readable.
pub fn affected_logic_in(
    db: &Database,
    access: TreeAccess<'_>,
    parsers: &dyn Parsers,
    req: AffectedReq,
    out: Output,
) -> AnyResult<String> {
    let plane = db.plane(&req.plane)?;
    let Some(root) = access.plane_root(recorded_root(&plane).as_deref())? else {
        bail!(
            "no source tree for plane `{}` — `affected` reads what changed from git; digest \
             from a checkout, or attach the tree (`serve watch` does)",
            req.plane
        );
    };
    let git = Git::new(root.clone());
    let change = match &req.vs {
        Some(older) => from_range(&git, parsers, &root, older)?,
        None => from_worktree(&git, &plane)?,
    };
    if change.seeds.is_empty() {
        // A report of nothing is not a report: a template tests for an empty
        // answer and leaves the pull request alone.
        return match out {
            Output::Compact => Ok(format!(
                "{}no changed declaration to start from — nothing to run on this change's \
                 account\n",
                change.note
            )),
            Output::Report | Output::Seeds => Ok(String::new()),
        };
    }
    if out == Output::Seeds {
        let mut keys = change.seeds.join("\n");
        keys.push('\n');
        return Ok(keys);
    }
    let widen = match req.safe {
        true => packages(&root, &change.files),
        false => Vec::new(),
    };
    let ask = Affected {
        plane: &req.plane,
        seeds: &change.seeds,
        depth: req.depth.unwrap_or(DEFAULT_DEPTH),
        widen: &widen,
        instance: None,
    };
    match out {
        Output::Report => Ok(format!(
            "{}\n{}",
            quoted(&change.note),
            compact::affected_report(db, ask)?
        )),
        _ => Ok(format!("{}{}", change.note, compact::affected(db, ask)?)),
    }
}

/// The working tree against `HEAD`: what is uncommitted.
///
/// Read from the plane rather than by parsing the tree again, because the
/// plane already holds the working tree — `serve watch` keeps it there — and
/// a second parse would be a second opinion about the same bytes.
fn from_worktree(git: &Git, plane: &PlaneHandle<'_>) -> AnyResult<Change> {
    let diff = text(git.run(&["diff", "HEAD", "--unified=0", "--no-color"])?);
    let mut spans = hunks(&diff);
    let status = text(git.run(&["status", "--porcelain", "--untracked-files=all"])?);
    let mut untracked = 0usize;
    for line in status.lines() {
        if let Some(path) = line.strip_prefix("?? ") {
            untracked += 1;
            // Every declaration in it: the whole file is new.
            spans.insert(path.trim().to_string(), vec![(1, usize::MAX)]);
        }
    }
    let files: Vec<String> = spans.keys().cloned().collect();
    let seeds = declared_in(plane, &spans)?;
    let mut note = format!(
        "change: the working tree against HEAD — {} file(s), {} declaration(s)\n",
        files.len(),
        seeds.len()
    );
    if untracked > 0 {
        note.push_str(&format!(
            "note: {untracked} untracked file(s) are read whole — git has no older side to \
             compare them against\n"
        ));
    }
    note.push_str(&behind(git, plane, files.len())?);
    Ok(Change { seeds, files, note })
}

/// Whether the plane is behind the tree the diff was read from, and what
/// that costs.
///
/// A changed line is turned into a declaration by the extents the *plane*
/// recorded, and those were true of the tree as it stood when it was parsed.
/// A plane that has not seen the edit still holds the old line numbers, so an
/// edit that moved code down the file can name the declaration that used to
/// be there. Nothing in the answer would look wrong, which is why this is
/// said rather than left to be noticed.
///
/// Two facts settle it, each conclusive on its own: a `synced_commit` that is
/// not HEAD, and more changed files than the plane recorded as differing from
/// its commit.
fn behind(git: &Git, plane: &PlaneHandle<'_>, changed: usize) -> AnyResult<String> {
    let props = plane.properties()?;
    let head = text(git.run(&["rev-parse", "HEAD"])?).trim().to_string();
    let at = str_prop(&props, compact::SYNC_COMMIT_PROP);
    let stale = match &at {
        Some(at) => !head.starts_with(at.as_str()) && !at.starts_with(&head),
        None => false,
    };
    let dirty = int_prop(&props, "synced_modified").unwrap_or(0);
    if !stale && changed <= dirty.max(usize::from(changed == 0)) {
        return Ok(String::new());
    }
    let why = match stale {
        true => format!(
            "it was parsed at {}, and HEAD is {}",
            at.as_deref().unwrap_or("an unrecorded commit"),
            &head[..head.len().min(12)]
        ),
        false => format!("{changed} file(s) differ from HEAD and it recorded {dirty}"),
    };
    Ok(format!(
        "warning: the plane is behind this working tree — {why}. A changed line is matched to a \
         declaration by the extents the plane holds, so where an edit moved code the seed can be \
         the declaration that used to be at that line. Re-digest, or let `serve watch` catch up, \
         before trusting this set.\n"
    ))
}

/// Two revisions, compared by parsing both — [`DirDeclarations`], the same
/// reading `history --exact` does.
fn from_range(git: &Git, parsers: &dyn Parsers, root: &Path, older: &str) -> AnyResult<Change> {
    let newer = sha(git, "HEAD")?;
    let old = sha(git, older)?;
    let files: Vec<String> = text(git.run(&["diff", "--name-only", &old, &newer])?)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if files.is_empty() {
        return Ok(Change {
            seeds: Vec::new(),
            files,
            note: format!("change: {old:.12}..{newer:.12} — no file differs\n"),
        });
    }
    let decls = DirDeclarations::new(root, parsers);
    let moved = decls.moved(&files, &newer, Some(&old));
    let mut seeds: Vec<String> = Vec::new();
    let mut removed = 0usize;
    // Why, then which: a reason repeated over nine files is one fact, and
    // "no parser claims it" — a changelog, a lockfile — is not a failure.
    let mut unsettled: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, moves) in &moved {
        if let Some(why) = moves.why() {
            unsettled
                .entry(generalize(why, path))
                .or_default()
                .push(path.clone());
            continue;
        }
        seeds.extend(moves.present().cloned());
        removed += moves.gone();
    }
    let mut note = format!(
        "change: {old:.12}..{newer:.12} — {} file(s), {} declaration(s)\n",
        files.len(),
        seeds.len()
    );
    if removed > 0 {
        note.push_str(&format!(
            "note: {removed} declaration(s) the range removed are not seeds — they are not in \
             the tree the tests run against, and whatever called them changed too\n"
        ));
    }
    for (why, paths) in &unsettled {
        note.push_str(&format!(
            "note: {why} — {} file(s), so what they declare is not in this set: {}\n",
            paths.len(),
            shown(paths)
        ));
    }
    if !text(git.run(&["status", "--porcelain"])?).trim().is_empty() {
        note.push_str(
            "note: this working tree has uncommitted changes, and a range does not see them — \
             drop `vs` to ask about those instead\n",
        );
    }
    Ok(Change { seeds, files, note })
}

/// Files an answer names before it counts the rest instead.
const NAMED: usize = 6;

/// One file's reason, with that file taken out of it, so the same reason
/// about nine files is one line rather than nine.
fn generalize(why: &str, path: &str) -> String {
    why.replace(path, "it").replace("  ", " ")
}

fn shown(paths: &[String]) -> String {
    match paths.len() > NAMED {
        true => format!(
            "{}, and {} more",
            paths[..NAMED].join(", "),
            paths.len() - NAMED
        ),
        false => paths.join(", "),
    }
}

/// The declarations the plane holds at those lines, innermost only.
fn declared_in(
    plane: &PlaneHandle<'_>,
    spans: &BTreeMap<String, Vec<(usize, usize)>>,
) -> AnyResult<Vec<String>> {
    let mut hit: BTreeMap<String, Vec<(String, usize, usize)>> = BTreeMap::new();
    for n in plane.query().scan_all().nodes()? {
        let Some(key) = n.external_key.clone() else {
            continue;
        };
        let Some(file) =
            str_prop(&n.properties, "file").or_else(|| str_prop(&n.properties, "path"))
        else {
            continue;
        };
        let Some(ranges) = spans.get(&file) else {
            continue;
        };
        let Some(line) = int_prop(&n.properties, "line") else {
            continue;
        };
        let end = int_prop(&n.properties, "end_line")
            .filter(|e| *e >= line)
            .unwrap_or(line);
        if ranges.iter().any(|(s, e)| line <= *e && end >= *s) {
            hit.entry(file).or_default().push((key, line, end));
        }
    }
    let mut out = Vec::new();
    for decls in hit.values() {
        // A declaration that strictly holds another the change also touched
        // is reached through that one. Seeding both would walk the outer
        // twice and say the change is wider than it is.
        let inner: Vec<&String> = decls
            .iter()
            .filter(|(_, l, e)| {
                !decls
                    .iter()
                    .any(|(_, l2, e2)| l <= l2 && e2 <= e && (l2, e2) != (l, e))
            })
            .map(|(key, _, _)| key)
            .collect();
        match inner.is_empty() {
            true => out.extend(decls.iter().map(|(key, _, _)| key.clone())),
            false => out.extend(inner.into_iter().cloned()),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The nearest ancestor of each changed file that a manifest marks a package,
/// as a path prefix the graph can match a file against.
fn packages(root: &Path, files: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for file in files {
        let mut dir = Path::new(file).parent();
        while let Some(d) = dir {
            if MANIFESTS.iter().any(|m| holds(root, d, m)) {
                out.insert(match d.as_os_str().is_empty() {
                    true => String::new(),
                    false => format!("{}/", d.display()),
                });
                break;
            }
            dir = d.parent();
        }
    }
    out.into_iter().collect()
}

/// Whether `dir` holds a file of that name — a glob only in the one shape the
/// list uses, a leading `*`.
///
/// A cargo workspace root is the exception: its `Cargo.toml` declares members
/// and no package of its own, so taking it would make the package of a file
/// beside it the whole repository — and `--safe` would mean "run everything"
/// for a change to a `.gitignore`.
fn holds(root: &Path, dir: &Path, manifest: &str) -> bool {
    if manifest == "Cargo.toml" {
        return std::fs::read_to_string(root.join(dir).join(manifest)).is_ok_and(|toml| {
            toml.lines()
                .any(|l| l.trim_start().starts_with("[package]"))
        });
    }
    let Some(suffix) = manifest.strip_prefix('*') else {
        return root.join(dir).join(manifest).exists();
    };
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(suffix))
    })
}

/// The new-side line ranges of a unified diff, per file.
fn hunks(diff: &str) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut out: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // A file the change deletes has no new side. What called it
            // changed too, and that is where the tests are found.
            path = match rest.trim() {
                "/dev/null" => None,
                rest => Some(rest.strip_prefix("b/").unwrap_or(rest).to_string()),
            };
            continue;
        }
        let (Some(file), Some(rest)) = (path.as_ref(), line.strip_prefix("@@ ")) else {
            continue;
        };
        if let Some((start, len)) = new_side(rest) {
            // A hunk with no new lines is a deletion, and the line it was
            // taken from is what the change reaches.
            let start = start.max(1);
            out.entry(file.clone())
                .or_default()
                .push((start, start + len.saturating_sub(1)));
        }
    }
    out
}

/// `@@ -12,3 +12,5 @@` — the `+` side, start and length.
fn new_side(header: &str) -> Option<(usize, usize)> {
    let plus = header.split_whitespace().find(|t| t.starts_with('+'))?;
    let mut parts = plus[1..].split(',');
    let start = parts.next()?.parse().ok()?;
    let len = match parts.next() {
        Some(len) => len.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

/// A revision as the full object name the declaration readers require.
fn sha(git: &Git, rev: &str) -> AnyResult<String> {
    let out = text(git.run(&["rev-parse", rev])?);
    let sha = out.trim().to_string();
    match sha.len() >= 40 {
        true => Ok(sha),
        false => Err(anyhow!("`{rev}` does not name a commit in this repository")),
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests;
