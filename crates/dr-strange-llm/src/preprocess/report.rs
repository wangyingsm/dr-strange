//! What a walk admitted, what it withheld, and which rule withheld it
//! (issue #36).
//!
//! A repository whose `.dockerignore` excluded `src/` digested nine nodes
//! instead of six thousand, and said nothing. The walk knew; nothing carried
//! what it knew back to the operator. These types do.
//!
//! The `ignore` crate's walker cannot say *which* rule matched — it yields
//! admitted entries and drops the rest silently — so attribution is a second
//! pass: build a matcher per candidate ignore file and ask each one. That is
//! only paid when a report is asked for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::IGNORED_DIRS;

/// Why one file the tree holds is not in the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// A pattern in one of the project's own ignore files. Both are named,
    /// because "something ignored it" is not an answer an operator can act on.
    Rule {
        /// The ignore file, relative to the walk root.
        file: PathBuf,
        /// The pattern in it, as written.
        pattern: String,
    },
    /// Admitted by the policy, but no installed plugin claims the extension.
    /// Not a problem — a README is not missing from the graph, it is not code.
    NoPlugin,
}

impl SkipReason {
    /// Whether this is the project's own doing, and so the kind of exclusion
    /// worth warning about. A plugin gap is nobody's mistake.
    pub fn is_declared(&self) -> bool {
        matches!(self, Self::Rule { .. })
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule { file, pattern } => write!(f, "`{pattern}` in {}", file.display()),
            Self::NoPlugin => write!(f, "no plugin claims it"),
        }
    }
}

/// One withheld file and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Root-relative.
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// What one walk of a tree came to.
///
/// `total` deliberately excludes [`IGNORED_DIRS`] and hidden files: they are
/// drsg's own floor, not the project's statement, and counting a
/// `node_modules/` of 80,000 files would make every percentage meaningless and
/// the warning fire on every healthy repository.
#[derive(Debug, Default, Clone)]
pub struct WalkReport {
    /// Files under the root once drsg's own floor is applied.
    pub total: usize,
    /// What the project's ignore files left, root-relative.
    pub admitted: Vec<PathBuf>,
    /// What was withheld, and by what.
    pub skipped: Vec<Skipped>,
}

impl WalkReport {
    /// Files withheld by the project's own ignore rules.
    pub fn declared(&self) -> impl Iterator<Item = &Skipped> {
        self.skipped.iter().filter(|s| s.reason.is_declared())
    }

    /// The share of `total` the project's own rules withheld, 0.0 to 1.0.
    /// Zero when the tree is empty, so a caller need not special-case it.
    pub fn declared_share(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.declared().count() as f64 / self.total as f64
    }

    /// The ignore files responsible, each with how many files it withheld,
    /// most first. What a warning names.
    pub fn culprits(&self) -> Vec<(PathBuf, usize)> {
        let mut by_file: BTreeMap<PathBuf, usize> = BTreeMap::new();
        for s in self.declared() {
            if let SkipReason::Rule { file, .. } = &s.reason {
                *by_file.entry(file.clone()).or_default() += 1;
            }
        }
        let mut out: Vec<(PathBuf, usize)> = by_file.into_iter().collect();
        // Most withheld first; the path breaks ties so the order is stable.
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }
}

/// The ignore files a policy consults, in the order the `ignore` crate gives
/// them precedence — later wins, which is why a `.dockerignore` outranked the
/// `.gitignore` beside it before this was made optional.
pub(crate) fn candidate_ignore_files(gitignore: bool, dockerignore: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    if gitignore {
        out.push(".gitignore");
    }
    if gitignore {
        out.push(".ignore");
    }
    if dockerignore {
        out.push(".dockerignore");
    }
    out
}

/// A matcher per ignore file found anywhere under `root`, nearest-first within
/// a directory, so a withheld path can be asked what withheld it.
///
/// Built by reading the files directly rather than through the walker: the
/// walker is what withheld them, and it does not say.
pub(crate) struct Attribution {
    /// Directory the file sits in (root-relative), its root-relative path, and
    /// the matcher built from it.
    matchers: Vec<(PathBuf, PathBuf, Gitignore)>,
}

impl Attribution {
    /// Collect every `names` file under `root`, skipping drsg's own floor so a
    /// vendored `node_modules/**/.gitignore` is not read.
    pub(crate) fn build(root: &Path, names: &[&str]) -> Self {
        let mut matchers = Vec::new();
        let mut walker = ignore::WalkBuilder::new(root);
        walker
            .standard_filters(false)
            .hidden(false)
            .filter_entry(|e| {
                !(e.file_type().is_some_and(|t| t.is_dir())
                    && IGNORED_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
            });
        for entry in walker.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !names.contains(&name.as_str()) {
                continue;
            }
            let Some(dir) = entry.path().parent() else {
                continue;
            };
            let mut b = GitignoreBuilder::new(dir);
            // A malformed line is skipped by the builder; a file that will not
            // build at all simply cannot be blamed, which is better than
            // blaming it wrongly.
            if b.add(entry.path()).is_some() {
                continue;
            }
            let Ok(gi) = b.build() else { continue };
            let rel_dir = dir.strip_prefix(root).unwrap_or(dir).to_path_buf();
            let rel_file = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_path_buf();
            matchers.push((rel_dir, rel_file, gi));
        }
        // Deeper directories first: the nearest ignore file to a path is the
        // one an operator will look at, and the crate gives it precedence too.
        matchers.sort_by(|a, b| {
            b.0.components()
                .count()
                .cmp(&a.0.components().count())
                .then_with(|| a.1.cmp(&b.1))
        });
        Self { matchers }
    }

    /// Which rule withheld `rel` (root-relative), if one of these files did.
    ///
    /// `is_dir` matters: a gitignore pattern ending in `/` matches only a
    /// directory, and the ancestor test is how `src/` withholds `src/a/b.ts`.
    pub(crate) fn blame(&self, rel: &Path, is_dir: bool) -> Option<SkipReason> {
        for (dir, file, gi) in &self.matchers {
            // A file only answers for paths beneath it.
            if !rel.starts_with(dir) {
                continue;
            }
            let m = gi.matched_path_or_any_parents(rel, is_dir);
            if let ignore::Match::Ignore(glob) = m {
                return Some(SkipReason::Rule {
                    file: file.clone(),
                    pattern: glob.original().to_string(),
                });
            }
        }
        None
    }
}
