//! A commit's tree read from git's object store through the `git` CLI, served
//! as a read-only [`Host`] so a preprocessor parses code as it was.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail};
use dr_strange_core::rev::Sha;

use crate::preprocess::{Host, IGNORED_DIRS};

/// Largest blob read into memory, in bytes.
pub const BLOB_CAP: u64 = 4 << 20;

/// The `git` CLI run in one directory, never through a shell.
#[derive(Debug, Clone)]
pub struct Git {
    root: PathBuf,
    program: OsString,
}

impl Git {
    /// Git run from `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            program: "git".into(),
        }
    }

    /// Run `git -C <root> <args>`; a non-zero exit is an error carrying git's message.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Vec<u8>> {
        Ok(self.run_allow(args, &[])?.1)
    }

    /// Run git, accepting the listed exit codes beside 0; returns the code and stdout.
    pub fn run_allow<S: AsRef<OsStr>>(&self, args: &[S], ok: &[i32]) -> Result<(i32, Vec<u8>)> {
        // A caller's GIT_DIR (set inside git hooks) would redirect every read to another repository.
        let out = Command::new(&self.program)
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .map_err(|e| match e.kind() {
                ErrorKind::NotFound => {
                    anyhow::anyhow!("git not found on PATH — reading history needs the git CLI")
                }
                _ => anyhow::Error::new(e).context("running git"),
            })?;
        let code = out.status.code().unwrap_or(-1);
        if code != 0 && !ok.contains(&code) {
            let verb = args
                .first()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .unwrap_or_default();
            bail!(
                "git {verb} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok((code, out.stdout))
    }
}

/// A path inside a tree that can neither climb out of it nor pass for a git option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelPath(String);

impl RelPath {
    /// `s` normalized (`.` segments, doubled and trailing `/` dropped); `""` names the root.
    pub fn parse(s: &str) -> Result<Self> {
        if s.starts_with('/') || s.contains(['\0', '\\']) {
            bail!("`{s}` is not a relative path inside the tree");
        }
        let parts: Vec<&str> = s
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        if parts.contains(&"..") {
            bail!("`{s}` climbs out of the tree");
        }
        let path = parts.join("/");
        if path.starts_with(['-', ':']) {
            bail!("`{s}` would read as a git option or pathspec");
        }
        Ok(Self(path))
    }

    /// The normalized path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One commit's files under a root directory, read from git's object store.
pub struct GitTree {
    git: Git,
    sha: Sha,
    prefix: String,
    label: Option<String>,
    listing: Mutex<Option<Vec<String>>>,
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl GitTree {
    /// The tree at `sha` as seen from `root`, which may be any directory of a checkout.
    pub fn open(root: &Path, sha: Sha) -> Result<Self> {
        let git = Git::new(root);
        let prefix = String::from_utf8(git.run(&["rev-parse", "--show-prefix"])?)
            .context("the directory's path inside the repository is not UTF-8")?
            .trim_end_matches('\n')
            .to_string();
        git.run(&["cat-file", "-e", &format!("{}^{{commit}}", sha.as_str())])
            .with_context(|| {
                format!("commit {} is not in this clone's object store", sha.short())
            })?;
        Ok(Self {
            git,
            sha,
            prefix,
            label: None,
            listing: Mutex::default(),
            blobs: Mutex::default(),
        })
    }

    /// The same tree, naming itself `label` when its files do not say.
    pub fn with_label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }

    /// The commit this tree is read at.
    pub fn sha(&self) -> &Sha {
        &self.sha
    }

    /// `path`'s object name at this commit.
    fn spec(&self, path: &RelPath) -> String {
        format!("{}:{}{}", self.sha.as_str(), self.prefix, path.as_str())
    }

    /// Every tracked blob under the root that `LocalFiles` would serve, in its walk order.
    fn files(&self) -> Result<Vec<String>> {
        let mut cached = self.listing.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(files) = cached.as_ref() {
            return Ok(files.clone());
        }
        let raw = self.git.run(&[
            "ls-tree",
            "--full-tree",
            "-r",
            "-z",
            &self.spec(&RelPath(String::new())),
        ])?;
        let mut files: Vec<String> = raw
            .split(|b| *b == 0)
            .filter_map(|entry| {
                let (meta, path) = std::str::from_utf8(entry).ok()?.split_once('\t')?;
                let mut meta = meta.split(' ');
                let (mode, kind) = (meta.next()?, meta.next()?);
                (kind == "blob" && mode != "120000" && servable(path)).then(|| path.to_string())
            })
            .collect();
        files.sort_by(|a, b| a.split('/').cmp(b.split('/')));
        *cached = Some(files.clone());
        Ok(files)
    }
}

/// Whether `LocalFiles`' default policy serves `path`: nothing hidden, no build-output directory.
fn servable(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = &parts[..parts.len() - 1];
    parts.iter().all(|p| !p.starts_with('.')) && dirs.iter().all(|d| !IGNORED_DIRS.contains(d))
}

impl Host for GitTree {
    fn list(&self, suffix: &str) -> Result<Vec<String>> {
        Ok(self
            .files()?
            .into_iter()
            .filter(|p| p.ends_with(suffix))
            .collect())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let rel = RelPath::parse(path)?;
        let cached = self
            .blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(rel.as_str())
            .cloned();
        if let Some(hit) = cached {
            return Ok(hit);
        }
        let spec = self.spec(&rel);
        let at = self.sha.short();
        let size = self
            .git
            .run(&["cat-file", "-s", &spec])
            .with_context(|| format!("{path} is not in the tree at {at}"))?;
        let size: u64 = String::from_utf8_lossy(&size)
            .trim()
            .parse()
            .context("git reported no size")?;
        if size > BLOB_CAP {
            bail!("{path} is {size} bytes at {at} — over the {BLOB_CAP}-byte read cap");
        }
        let bytes = self.git.run(&["cat-file", "blob", &spec])?;
        self.blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(rel.0, bytes.clone());
        Ok(bytes)
    }

    fn label(&self) -> Option<String> {
        self.label.clone()
    }
}

/// Scratch repositories for tests that need real git history.
#[cfg(test)]
pub(crate) mod scratch {
    use super::*;

    /// A git repository in a temporary directory, removed on drop.
    pub(crate) struct Repo(pub(crate) PathBuf);

    impl Repo {
        /// An empty repository on branch `main`.
        pub(crate) fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("drsg-git-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let repo = Self(dir);
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }

        /// Run git here, isolated from the user's configuration, with a fixed identity and clock.
        pub(crate) fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Ada")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .env("GIT_AUTHOR_DATE", "2026-08-01T10:00:00Z")
                .env("GIT_COMMITTER_DATE", "2026-08-01T10:00:00Z")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        }

        /// Write a file relative to the repository root.
        pub(crate) fn write(&self, rel: &str, body: &str) -> &Self {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
            self
        }

        /// Commit everything in the working tree and return the new commit.
        pub(crate) fn commit(&self, message: &str) -> Sha {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", message]);
            Sha::parse(self.git(&["rev-parse", "HEAD"]).trim()).unwrap()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scratch::Repo;
    use super::*;
    use crate::preprocess::LocalFiles;

    fn text(tree: &GitTree, path: &str) -> String {
        String::from_utf8(tree.read(path).unwrap()).unwrap()
    }

    #[test]
    fn an_old_commit_reads_as_it_was_not_as_the_tree_is() {
        let repo = Repo::new("reads");
        let one = repo.write("src/lib.rs", "fn one() {}\n").commit("one");
        let two = repo.write("src/lib.rs", "fn two() {}\n").commit("two");
        repo.write("src/lib.rs", "fn three() {}\n");
        let at_one = GitTree::open(&repo.0, one.clone()).unwrap();
        assert_eq!(text(&at_one, "src/lib.rs"), "fn one() {}\n");
        assert_eq!(text(&at_one, "./src//lib.rs"), "fn one() {}\n");
        assert_eq!(at_one.sha(), &one);
        let at_two = GitTree::open(&repo.0, two).unwrap();
        assert_eq!(text(&at_two, "src/lib.rs"), "fn two() {}\n");
    }

    #[test]
    fn the_listing_is_what_a_directory_walk_of_that_commit_would_serve() {
        let repo = Repo::new("lists");
        for path in [
            "a/b.rs",
            "a.rs",
            "src/lib.rs",
            "notes/target",
            ".env",
            ".hidden/x.rs",
            "target/y.rs",
            "sub/node_modules/z.js",
        ] {
            repo.write(path, "x\n");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.rs", repo.0.join("link.rs")).unwrap();
        let sha = repo.commit("tree");
        let tree = GitTree::open(&repo.0, sha).unwrap();
        let listed = tree.list("").unwrap();
        assert_eq!(listed, ["a/b.rs", "a.rs", "notes/target", "src/lib.rs"]);
        assert_eq!(listed, LocalFiles::new(&repo.0).unwrap().list("").unwrap());
        assert_eq!(tree.list(".rs").unwrap(), ["a/b.rs", "a.rs", "src/lib.rs"]);
        repo.write("new.rs", "untracked\n");
        assert!(!tree.list("").unwrap().contains(&"new.rs".to_string()));
    }

    #[test]
    fn a_subdirectory_root_reads_relative_to_itself() {
        let repo = Repo::new("subdir");
        let sha = repo
            .write("crates/x/src/lib.rs", "pub fn x() {}\n")
            .write("top.rs", "\n")
            .commit("nested");
        let tree = GitTree::open(&repo.0.join("crates/x"), sha).unwrap();
        assert_eq!(tree.list("").unwrap(), ["src/lib.rs"]);
        assert_eq!(text(&tree, "src/lib.rs"), "pub fn x() {}\n");
        assert!(
            tree.read("top.rs").is_err(),
            "outside the root is not served"
        );
    }

    #[test]
    fn a_path_that_could_escape_or_pass_as_an_option_is_refused() {
        for bad in [
            "../x",
            "a/../../b",
            "/etc/passwd",
            "-rf",
            ":(glob)*",
            "a\\b",
            "a\0b",
        ] {
            assert!(RelPath::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            RelPath::parse("./src//lib.rs/").unwrap().as_str(),
            "src/lib.rs"
        );
        assert_eq!(RelPath::parse("").unwrap().as_str(), "");
    }

    #[test]
    fn what_cannot_be_read_says_why() {
        let repo = Repo::new("errors");
        let sha = repo.write("a.rs", "x\n").commit("one");
        let absent = Sha::parse(&"0".repeat(40)).unwrap();
        let err = GitTree::open(&repo.0, absent).err().unwrap();
        assert!(
            format!("{err:#}").contains("not in this clone's object store"),
            "{err:#}"
        );
        let tree = GitTree::open(&repo.0, sha).unwrap();
        let err = tree.read("b.rs").unwrap_err();
        assert!(
            format!("{err:#}").contains("b.rs is not in the tree at"),
            "{err:#}"
        );
        let missing = Git {
            root: repo.0.clone(),
            program: "drsg-no-such-git".into(),
        };
        let err = missing.run(&["version"]).unwrap_err();
        assert!(err.to_string().contains("git not found on PATH"), "{err}");
    }

    #[test]
    fn a_blob_over_the_cap_is_refused() {
        let repo = Repo::new("cap");
        let big = "x".repeat(BLOB_CAP as usize + 1);
        let sha = repo.write("big.txt", &big).commit("big");
        let err = GitTree::open(&repo.0, sha)
            .unwrap()
            .read("big.txt")
            .unwrap_err();
        assert!(err.to_string().contains("read cap"), "{err}");
    }
}
