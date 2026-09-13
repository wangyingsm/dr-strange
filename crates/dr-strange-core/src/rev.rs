//! Git revisions resolved over a repository's history plane (`<name>_git`):
//! which commit `main`, `v2.0.0`, `e34039f`, `HEAD~2` or `2026-08-01` names.

use crate::api::PlaneHandle;
use crate::compact::{day, prop_bool, prop_int, prop_str};
use crate::error::Result;
use crate::time::{date_end_of_day, rfc3339_to_epoch_ms};
use crate::types::{Dir, NodeRecord, PropValue};

/// Shortest hex prefix accepted as an abbreviated sha, git's own minimum.
const MIN_PREFIX: usize = 4;
/// Most commits a walk down first parents visits.
const WALK_CAP: usize = 100_000;
/// Most candidates an ambiguous answer lists.
const CANDIDATE_CAP: usize = 20;
/// Every form a revision may take, for answers that could not read one.
const FORMS: &str = "a revision is a sha (4+ hex digits), a branch, a tag, HEAD, a date \
                     (YYYY-MM-DD or RFC-3339) or `<rev>@{<date>}`, each optionally followed by \
                     `~n` / `^n`";

/// A full git object name: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha(String);

impl Sha {
    /// `s` as a full object name, or `None` if it is not one.
    pub fn parse(s: &str) -> Option<Sha> {
        let hex = s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        (matches!(s.len(), 40 | 64) && hex).then(|| Sha(s.to_string()))
    }

    /// The full name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The 12-digit abbreviation answers print.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

/// One step back from a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `~n`: n first parents back.
    Back(u32),
    /// `^n`: the n-th parent; `^0` is the commit itself.
    Parent(u32),
}

/// A parsed revision expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevExpr {
    /// The checked-out commit.
    Head,
    /// A full sha, a ref or a sha prefix, tried in that order.
    Name(String),
    /// The newest commit at or before an instant (epoch ms) on a revision's first-parent line.
    AtDate(Box<RevExpr>, i64),
    /// A step back from a revision.
    Ancestor(Box<RevExpr>, Step),
}

/// Parse a revision: `HEAD`, a sha or prefix, a ref, a date or `<rev>@{<date>}`, then `~n`/`^n` steps.
pub fn parse_rev(s: &str) -> std::result::Result<RevExpr, String> {
    let s = s.trim();
    let (base, mut rest) = s.split_at(s.find(['~', '^']).unwrap_or(s.len()));
    let mut expr = parse_base(base)?;
    while let Some(op) = rest.chars().next() {
        let digits: String = rest[1..].chars().take_while(char::is_ascii_digit).collect();
        let n = match digits.as_str() {
            "" => 1,
            d => d
                .parse()
                .map_err(|_| format!("`{d}` is too large a step"))?,
        };
        let step = if op == '~' {
            Step::Back(n)
        } else {
            Step::Parent(n)
        };
        expr = RevExpr::Ancestor(Box::new(expr), step);
        rest = &rest[1 + digits.len()..];
        if !rest.is_empty() && !rest.starts_with(['~', '^']) {
            return Err(format!(
                "`{rest}` after a step is not `~n` or `^n` — {FORMS}"
            ));
        }
    }
    Ok(expr)
}

/// The part of a revision before its steps.
fn parse_base(base: &str) -> std::result::Result<RevExpr, String> {
    if base.is_empty() {
        return Err(format!("an empty revision — {FORMS}"));
    }
    if let Some((name, when)) = base.strip_suffix('}').and_then(|b| b.split_once("@{")) {
        let at = instant(when)
            .ok_or_else(|| format!("`{when}` is not a date (YYYY-MM-DD or RFC-3339)"))?;
        let from = if name.is_empty() {
            RevExpr::Head
        } else {
            parse_base(name)?
        };
        return Ok(RevExpr::AtDate(Box::new(from), at));
    }
    if base == "HEAD" || base == "@" {
        return Ok(RevExpr::Head);
    }
    if let Some(at) = instant(base) {
        return Ok(RevExpr::AtDate(Box::new(RevExpr::Head), at));
    }
    Ok(RevExpr::Name(base.to_string()))
}

/// Epoch ms of a `YYYY-MM-DD` day's end or of an RFC-3339 instant.
fn instant(s: &str) -> Option<i64> {
    date_end_of_day(s).or_else(|| rfc3339_to_epoch_ms(s))
}

/// The commit a revision named, with what an answer prints about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRef {
    /// Its full object name.
    pub sha: Sha,
    /// The day it was committed.
    pub date: String,
    /// Who wrote it.
    pub author: String,
    /// Its message's first line.
    pub summary: String,
    /// `false` when a rewrite left it behind, so gc may prune it.
    pub reachable: bool,
}

/// How resolving a revision ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevAnswer {
    /// Exactly one commit.
    One(CommitRef),
    /// Several commits fit a sha prefix, newest first.
    Many(Vec<CommitRef>),
    /// Nothing fits; the text says why.
    None(String),
}

/// Resolve `rev` over a history plane; `head_hint` stands in for HEAD when no local branch is checked out.
pub fn resolve_rev(
    history: &PlaneHandle<'_>,
    head_hint: Option<&Sha>,
    rev: &str,
) -> Result<RevAnswer> {
    let expr = match parse_rev(rev) {
        Ok(expr) => expr,
        Err(why) => return Ok(RevAnswer::None(why)),
    };
    let walker = Walker {
        plane: history,
        head_hint,
    };
    Ok(match walker.eval(&expr)? {
        Found::One(node) => RevAnswer::One(commit_ref(&node)),
        Found::Many(nodes) => RevAnswer::Many(nodes.iter().map(commit_ref).collect()),
        Found::None(why) => RevAnswer::None(why),
    })
}

/// The line an answer about `commit` opens with; `rev` is shown unless it was the sha itself.
pub fn commit_header(commit: &CommitRef, rev: &str) -> String {
    let mut out = format!(
        "at {}  {}  {}  {}",
        commit.sha.short(),
        commit.date,
        commit.author,
        commit.summary
    );
    if !commit.sha.as_str().starts_with(&rev.to_ascii_lowercase()) {
        out.push_str(&format!("  (via {rev})"));
    }
    out.push('\n');
    if !commit.reachable {
        out.push_str(
            "note: reachable: false — a rewrite left this commit behind, and gc may prune it\n",
        );
    }
    out
}

/// The commits an ambiguous sha prefix matched, and how to narrow them.
pub fn ambiguous(rev: &str, commits: &[CommitRef]) -> String {
    let mut out = format!(
        "`{rev}` is ambiguous — {} commits start with it; call again with more of the sha:\n",
        commits.len()
    );
    for c in commits.iter().take(CANDIDATE_CAP) {
        out.push_str(&format!("  {}  {}  {}\n", c.sha.short(), c.date, c.summary));
    }
    if commits.len() > CANDIDATE_CAP {
        out.push_str(&format!("  … and {} more\n", commits.len() - CANDIDATE_CAP));
    }
    out
}

/// A resolution step's outcome, before it is rendered as a [`RevAnswer`].
enum Found {
    One(NodeRecord),
    Many(Vec<NodeRecord>),
    None(String),
}

/// Evaluates revision expressions against one history plane.
struct Walker<'p, 'db> {
    plane: &'p PlaneHandle<'db>,
    head_hint: Option<&'p Sha>,
}

impl Walker<'_, '_> {
    fn eval(&self, expr: &RevExpr) -> Result<Found> {
        match expr {
            RevExpr::Head => self.head(),
            RevExpr::Name(name) => self.name(name),
            RevExpr::AtDate(from, at) => match self.eval(from)? {
                Found::One(start) => self.before(start, *at),
                other => Ok(other),
            },
            RevExpr::Ancestor(from, step) => match self.eval(from)? {
                Found::One(start) => self.step(start, *step),
                other => Ok(other),
            },
        }
    }

    /// The checked-out local branch's tip, else the hint.
    fn head(&self) -> Result<Found> {
        let branches = self.plane.query().scan_label("Branch").nodes()?;
        let tip = branches
            .iter()
            .find(|b| prop_bool(&b.properties, "is_head") && !prop_bool(&b.properties, "remote"))
            .and_then(|b| prop_str(&b.properties, "tip"))
            .map(str::to_string)
            .or_else(|| self.head_hint.map(|s| s.as_str().to_string()));
        match tip {
            Some(sha) => self.commit(&sha),
            None => Ok(Found::None(
                "HEAD is detached and the plane records no synced commit — name a sha, \
                 branch or tag"
                    .into(),
            )),
        }
    }

    fn commit(&self, sha: &str) -> Result<Found> {
        Ok(match self.plane.node_by_key(&format!("commit:{sha}"))? {
            Some(node) => Found::One(node),
            None => Found::None(format!(
                "commit {} is not in this history plane — it may be newer than the last digest",
                &sha[..sha.len().min(12)]
            )),
        })
    }

    /// A full sha, then a ref, then a sha prefix — a ref outranks a prefix, as in git.
    fn name(&self, name: &str) -> Result<Found> {
        if let Some(sha) = Sha::parse(name) {
            return self.commit(sha.as_str());
        }
        if let Some(target) = self.ref_target(name)? {
            return self.commit(&target);
        }
        let hex = name.to_ascii_lowercase();
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Found::None(format!(
                "no branch or tag is named `{name}` — {FORMS}"
            )));
        }
        if hex.len() < MIN_PREFIX {
            return Ok(Found::None(format!(
                "`{name}` is too short to name a commit — give at least {MIN_PREFIX} hex digits"
            )));
        }
        let mut hits: Vec<NodeRecord> = self
            .plane
            .query()
            .scan_label("Commit")
            .nodes()?
            .into_iter()
            .filter(|c| prop_str(&c.properties, "sha").is_some_and(|s| s.starts_with(&hex)))
            .collect();
        hits.sort_by_key(|c| std::cmp::Reverse(prop_int(&c.properties, "committed_ts")));
        Ok(match hits.len() {
            0 => Found::None(format!("no commit starts with `{name}` — {FORMS}")),
            1 => Found::One(hits.remove(0)),
            _ => Found::Many(hits),
        })
    }

    /// The commit a ref (short or full refname) points at: tag, then local branch, then remote.
    fn ref_target(&self, name: &str) -> Result<Option<String>> {
        let named = |n: &&NodeRecord| {
            prop_str(&n.properties, "name") == Some(name)
                || prop_str(&n.properties, "ref") == Some(name)
        };
        let tags = self.plane.query().scan_label("Tag").nodes()?;
        if let Some(tag) = tags.iter().find(named) {
            return Ok(prop_str(&tag.properties, "target").map(str::to_string));
        }
        let branches = self.plane.query().scan_label("Branch").nodes()?;
        let (remote, local): (Vec<&NodeRecord>, Vec<&NodeRecord>) = branches
            .iter()
            .partition(|b| prop_bool(&b.properties, "remote"));
        Ok(local
            .into_iter()
            .find(named)
            .or_else(|| remote.into_iter().find(named))
            .and_then(|b| prop_str(&b.properties, "tip"))
            .map(str::to_string))
    }

    /// The newest commit at or before `at` (epoch ms) on `start`'s first-parent line.
    fn before(&self, start: NodeRecord, at: i64) -> Result<Found> {
        let limit = at.div_euclid(1000);
        let mut node = start;
        for _ in 0..WALK_CAP {
            if prop_int(&node.properties, "committed_ts").is_some_and(|ts| ts <= limit) {
                return Ok(Found::One(node));
            }
            match self.parent(&node, 1)? {
                Some(parent) => node = parent,
                None => {
                    return Ok(Found::None(format!(
                        "nothing on this line of history is that old — it begins at {} ({})",
                        short_of(&node),
                        day(&node.properties, "committed_at")
                    )));
                }
            }
        }
        Ok(Found::None(format!(
            "gave up after {WALK_CAP} commits looking for one that old"
        )))
    }

    fn step(&self, start: NodeRecord, step: Step) -> Result<Found> {
        let (hops, order) = match step {
            Step::Back(n) => (n, 1),
            Step::Parent(0) => return Ok(Found::One(start)),
            Step::Parent(n) => (1, n),
        };
        let mut node = start;
        for taken in 0..hops {
            match self.parent(&node, order)? {
                Some(parent) => node = parent,
                None => {
                    return Ok(Found::None(format!(
                        "{} has no parent #{order} — the walk stopped {taken} step(s) back",
                        short_of(&node)
                    )));
                }
            }
        }
        Ok(Found::One(node))
    }

    /// `node`'s parent number `order`, counting from 1, when the plane holds it.
    fn parent(&self, node: &NodeRecord, order: u32) -> Result<Option<NodeRecord>> {
        for nb in self.plane.neighbors(node.id, Dir::Out, Some("PARENT"))? {
            let edge = self.plane.edge(nb.edge)?;
            if edge.is_some_and(|e| prop_int(&e.properties, "order") == Some(i64::from(order))) {
                return self.plane.node(nb.node);
            }
        }
        Ok(None)
    }
}

fn commit_ref(node: &NodeRecord) -> CommitRef {
    let p = &node.properties;
    CommitRef {
        sha: Sha(prop_str(p, "sha").unwrap_or_default().to_string()),
        date: day(p, "committed_at"),
        author: prop_str(p, "author_name").unwrap_or_default().to_string(),
        summary: prop_str(p, "summary").unwrap_or_default().to_string(),
        reachable: !matches!(
            p.get("reachable").map(|d| &d.value),
            Some(PropValue::Bool(false))
        ),
    }
}

fn short_of(node: &NodeRecord) -> String {
    prop_str(&node.properties, "sha")
        .unwrap_or("?")
        .chars()
        .take(12)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Database;
    use crate::types::{PropDesc, Properties};

    const ROOT: &str = "1111111111111111111111111111111111111111";
    const MID: &str = "abcd100000000000000000000000000000000000";
    const TOP: &str = "abcd200000000000000000000000000000000000";
    const SIDE: &str = "5555555555555555555555555555555555555555";
    const MERGE: &str = "9999999999999999999999999999999999999999";
    const LOST: &str = "7777777777777777777777777777777777777777";

    fn props(pairs: Vec<(&str, PropValue)>) -> Properties {
        let mut out = Properties::new();
        for (k, v) in pairs {
            out.insert(k.into(), PropDesc::new(v));
        }
        out
    }

    fn s(v: &str) -> PropValue {
        PropValue::Str(v.into())
    }

    /// root ← mid ← top ← merge (main); side (off mid) is the merge's second
    /// parent; lost (off root) was rewritten away. v1 tags mid, origin/main
    /// points at top, and a branch named `1111` points at the merge.
    fn repo(head: bool) -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("repo_git", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut id = std::collections::HashMap::new();
        for (sha, ts, summary, reachable) in [
            (ROOT, 1000, "root", true),
            (MID, 2000, "mid", true),
            (TOP, 3000, "top", true),
            (SIDE, 2500, "side", true),
            (MERGE, 4000, "merge", true),
            (LOST, 3500, "lost", false),
        ] {
            let node = props(vec![
                ("sha", s(sha)),
                ("summary", s(summary)),
                ("author_name", s("Ada")),
                (
                    "committed_at",
                    s(&format!("2026-08-0{}T10:00:00+00:00", ts / 1000)),
                ),
                ("committed_ts", PropValue::Int(ts)),
                ("reachable", PropValue::Bool(reachable)),
            ]);
            let key = format!("commit:{sha}");
            id.insert(
                sha,
                txn.create_node_with_key(&key, &["Commit"], node).unwrap(),
            );
        }
        for (child, parent, order) in [
            (MID, ROOT, 1),
            (TOP, MID, 1),
            (SIDE, MID, 1),
            (MERGE, TOP, 1),
            (MERGE, SIDE, 2),
            (LOST, ROOT, 1),
        ] {
            let edge = props(vec![("order", PropValue::Int(order))]);
            txn.create_edge(id[child], id[parent], "PARENT", edge)
                .unwrap();
        }
        for (key, labels, name, is_head, remote, tip) in [
            ("branch:main", &["Branch"][..], "main", head, false, MERGE),
            ("branch:1111", &["Branch"][..], "1111", false, false, MERGE),
            (
                "branch:origin/main",
                &["Branch", "Remote"][..],
                "origin/main",
                false,
                true,
                TOP,
            ),
        ] {
            let prefix = if remote {
                "refs/remotes/"
            } else {
                "refs/heads/"
            };
            let node = props(vec![
                ("name", s(name)),
                ("ref", s(&format!("{prefix}{name}"))),
                ("is_head", PropValue::Bool(is_head)),
                ("remote", PropValue::Bool(remote)),
                ("tip", s(tip)),
            ]);
            txn.create_node_with_key(key, labels, node).unwrap();
        }
        let tag = props(vec![
            ("name", s("v1")),
            ("ref", s("refs/tags/v1")),
            ("target", s(MID)),
        ]);
        txn.create_node_with_key("tag:v1", &["Tag"], tag).unwrap();
        txn.commit().unwrap();
        db
    }

    fn answer(db: &Database, hint: Option<&Sha>, rev: &str) -> RevAnswer {
        resolve_rev(&db.plane("repo_git").unwrap(), hint, rev).unwrap()
    }

    fn sha_of(db: &Database, rev: &str) -> String {
        match answer(db, None, rev) {
            RevAnswer::One(c) => c.sha.as_str().to_string(),
            other => panic!("{rev}: {other:?}"),
        }
    }

    fn why_not(db: &Database, rev: &str) -> String {
        match answer(db, None, rev) {
            RevAnswer::None(why) => why,
            other => panic!("{rev}: {other:?}"),
        }
    }

    #[test]
    fn a_revision_parses_into_its_base_and_steps() {
        use RevExpr::*;
        let head = || Box::new(Head);
        assert_eq!(
            parse_rev("HEAD~2^2"),
            Ok(Ancestor(
                Box::new(Ancestor(head(), Step::Back(2))),
                Step::Parent(2)
            ))
        );
        assert_eq!(parse_rev("@^"), Ok(Ancestor(head(), Step::Parent(1))));
        assert_eq!(
            parse_rev("v1~"),
            Ok(Ancestor(Box::new(Name("v1".into())), Step::Back(1)))
        );
        assert_eq!(
            parse_rev("main@{2026-08-01}"),
            Ok(AtDate(
                Box::new(Name("main".into())),
                date_end_of_day("2026-08-01").unwrap()
            ))
        );
        assert_eq!(parse_rev("1970-01-01T00:00:01Z"), Ok(AtDate(head(), 1000)));
        for bad in ["", "HEAD~x", "main@{soon}", "HEAD~99999999999"] {
            assert!(parse_rev(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn head_and_its_steps_follow_the_checked_out_branch() {
        let db = repo(true);
        assert_eq!(sha_of(&db, "HEAD"), MERGE);
        assert_eq!(sha_of(&db, "HEAD^0"), MERGE);
        assert_eq!(sha_of(&db, "HEAD~1"), TOP);
        assert_eq!(sha_of(&db, "HEAD^2"), SIDE);
        assert_eq!(sha_of(&db, "HEAD^2~1"), MID);
        assert_eq!(sha_of(&db, "HEAD~3"), ROOT);
        assert!(why_not(&db, "HEAD~4").contains("no parent #1"));
        assert!(why_not(&db, "HEAD^3").contains("no parent #3"));
    }

    #[test]
    fn a_detached_head_falls_back_to_the_hint() {
        let db = repo(false);
        let hint = Sha::parse(TOP).unwrap();
        match answer(&db, Some(&hint), "HEAD") {
            RevAnswer::One(c) => assert_eq!(c.sha, hint),
            other => panic!("{other:?}"),
        }
        assert!(why_not(&db, "HEAD").contains("detached"));
    }

    #[test]
    fn a_sha_resolves_whole_or_by_a_long_enough_prefix() {
        let db = repo(true);
        assert_eq!(sha_of(&db, MID), MID);
        assert_eq!(sha_of(&db, "5555"), SIDE);
        assert_eq!(sha_of(&db, "ABCD1"), MID);
        match answer(&db, None, "abcd") {
            RevAnswer::Many(hits) => {
                let shas: Vec<&str> = hits.iter().map(|c| c.sha.as_str()).collect();
                assert_eq!(shas, [TOP, MID], "newest first");
                let out = ambiguous("abcd", &hits);
                assert!(out.contains("2 commits start with it"), "{out}");
                assert!(out.contains("abcd20000000  2026-08-03  top"), "{out}");
            }
            other => panic!("{other:?}"),
        }
        assert!(why_not(&db, "abc").contains("at least 4 hex digits"));
        assert!(why_not(&db, "0000").contains("no commit starts with"));
        let absent = "0".repeat(40);
        assert!(why_not(&db, &absent).contains("not in this history plane"));
    }

    #[test]
    fn a_ref_resolves_tag_then_branch_then_remote_and_outranks_a_prefix() {
        let db = repo(true);
        assert_eq!(sha_of(&db, "v1"), MID);
        assert_eq!(sha_of(&db, "refs/tags/v1"), MID);
        assert_eq!(sha_of(&db, "main"), MERGE);
        assert_eq!(sha_of(&db, "origin/main"), TOP);
        assert_eq!(sha_of(&db, "refs/remotes/origin/main"), TOP);
        assert_eq!(sha_of(&db, "1111"), MERGE, "a ref beats root's sha prefix");
        assert!(why_not(&db, "nope").contains("no branch or tag is named `nope`"));
    }

    #[test]
    fn a_date_walks_first_parents_to_the_newest_commit_that_old() {
        let db = repo(true);
        assert_eq!(sha_of(&db, "1970-01-01T00:58:20Z"), TOP);
        assert_eq!(
            sha_of(&db, "1970-01-01T00:43:20Z"),
            MID,
            "side is off the line"
        );
        assert_eq!(sha_of(&db, "1970-01-01"), MERGE);
        assert_eq!(sha_of(&db, "origin/main@{1970-01-01T00:43:20Z}"), MID);
        assert!(why_not(&db, "1970-01-01T00:10:00Z").contains("begins at 111111111111"));
    }

    #[test]
    fn the_header_names_the_commit_and_what_led_to_it() {
        let db = repo(true);
        let RevAnswer::One(merge) = answer(&db, None, "main") else {
            panic!()
        };
        assert_eq!(
            commit_header(&merge, "main"),
            "at 999999999999  2026-08-04  Ada  merge  (via main)\n"
        );
        assert!(!commit_header(&merge, "9999").contains("via"));
        let RevAnswer::One(lost) = answer(&db, None, LOST) else {
            panic!()
        };
        assert!(!lost.reachable);
        assert!(commit_header(&lost, LOST).contains("reachable: false"));
    }
}
