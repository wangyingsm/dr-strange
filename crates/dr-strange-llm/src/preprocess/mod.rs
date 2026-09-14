//! Preprocessors — domain structure before the model (ROADMAP §11).
//!
//! Every source of truth carries structure of its own, and that structure is
//! knowledge the model should not have to rediscover from prose. A preprocessor
//! turns a format-specific input into two things:
//!
//! - **facts** — nodes and edges it is *certain* about, in the shapes
//!   [`DigestNode`] / [`DigestEdge`] already use, so they are writable through
//!   the path `digest.write` has always taken;
//! - **prose** — the residue that still needs understanding.
//!
//! Three wins, and only the first is the obvious one. *Tokens*: an
//! interface-level view of a source file is a fraction of the file, and no body
//! text reaches the model. *Precision*: an AST does not infer that `parse()`
//! calls `lex()`, it knows — handing that to a model as prose so it can
//! re-derive the edge spends tokens for a worse answer. *Vocabulary*: a
//! plugin's labels are constants rather than inventions, so the vocabulary
//! fragmentation §8 reconciles never arises for that part of the graph.
//!
//! An input that yields only facts is a digest with **no model call at all**.
//!
//! ## Where this sits
//!
//! [`route_document`] and [`route_tree`] are the single dispatch point for
//! everything the digest pipeline ingests — an upload, a fetched URL body, a
//! file, a whole project directory. The built-in document reader
//! ([`crate::document::to_markdown`]) is the fallback every unclaimed input
//! lands on, returning prose and no facts, so a default install works with
//! nothing configured.
//!
//! ## Parallel, and still deterministic
//!
//! Preprocessing is CPU-bound — parsing, not waiting — so files are handled in
//! parallel through rayon. That is a different problem from the digest's own
//! concurrency ([`crate::digest`] uses `std::thread::scope` with an operator-set
//! `concurrency`), because those are network calls against a rate limit while
//! these are pure compute with no provider to throttle.
//!
//! Results are collected **in input order**, never as they complete. Ordered
//! collection is what lets the work fan out without giving up the property the
//! sorted walk exists for: re-ingesting a repository yields the same graph.

#[cfg(feature = "plugins")]
mod catalog;
mod ground;
mod ledger;
#[cfg(feature = "plugins")]
mod registry;
mod repo;
mod sync;
#[cfg(feature = "plugins")]
mod wasm;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ahash::AHashSet;
use anyhow::{Context, Result, bail};
use dr_strange_core::{PropDesc, PropValue};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use rayon::prelude::*;

use crate::digest::{DigestEdge, DigestNode, SOURCE_MARKER};

#[cfg(feature = "plugins")]
pub use catalog::{
    CATALOG_DOWNLOAD_CAP, CATALOG_URL, CONTRACT_VERSION, Catalog, Compat, Fetched, HOST_VERSION,
    OfficialPlugin, Pick, Source as CatalogSource, load_catalog, load_catalog_within,
    read_cache as cached_catalog, refresh_cache,
};
pub use ground::{FactsAndPlane, fold, stamp_run};
pub use ledger::{LEDGER_PROP, record_ledger};

/// Bytes the loaded wasm plugins hold right now, process-wide: every compiled
/// plugin image plus the linear memory of every instance mid-call. Zero with
/// the runtime compiled out — nothing is loaded, so nothing is held.
pub fn plugin_memory_bytes() -> usize {
    #[cfg(feature = "plugins")]
    {
        wasm::held_bytes()
    }
    #[cfg(not(feature = "plugins"))]
    {
        0
    }
}
#[cfg(feature = "plugins")]
pub use registry::{InstalledPlugin, PluginStore, StoreStamp};
pub use repo::{
    GitDir, PLANE_SUFFIX, REPO_PLUGIN, WriteStats, git_dir, plane_name, route_repository,
    write_history,
};
pub use sync::{CommitDelta, SyncStats, resync, sync_paths};
#[cfg(feature = "plugins")]
pub use wasm::{Limits, WasmPlugin};

/// What a preprocessor produces from one input.
#[derive(Debug, Default)]
pub struct Preprocessed {
    /// Facts the preprocessor is certain about.
    pub nodes: Vec<DigestNode>,
    pub edges: Vec<DigestEdge>,
    /// The residue that still needs a model. Empty means no model call.
    pub prose: String,
    pub report: PreprocessReport,
}

impl Preprocessed {
    /// Prose and nothing else — what the built-in document reader returns, and
    /// the shape any text-only handler takes.
    pub fn prose_only(handler: impl Into<String>, prose: String) -> Self {
        let prose_chars = prose.chars().count();
        Preprocessed {
            prose,
            report: PreprocessReport {
                handlers: vec![(handler.into(), 0)],
                prose_chars,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Whether this still needs the model.
    pub fn needs_model(&self) -> bool {
        !self.prose.trim().is_empty()
    }
}

/// What ran, what it produced, and what it left behind.
///
/// Skips and collisions are *counted and named* rather than dropped silently: a
/// thin graph should be explained by its report, not investigated by re-running
/// the ingest with different arguments.
///
/// `Clone` because the CLI drains the notes as it prints them and the plane's
/// ledger needs the same account after that — printing and recording are two
/// readers of one report, not one consuming it from the other.
#[derive(Debug, Default, Clone)]
pub struct PreprocessReport {
    /// `(name@version, facts emitted)`, in the order the handlers ran.
    pub handlers: Vec<(String, usize)>,
    pub prose_chars: usize,
    /// Files no handler claimed, that carried nothing readable, or that the
    /// handler which claimed them could not get through.
    pub skipped: usize,
    /// Keys two handlers both produced — a plugin bug, kept visible.
    pub collisions: Vec<String>,
    /// Extensions no installed plugin claimed, as `.ext (n)` — the files that
    /// were read as prose instead of parsed. Structured beside the note that
    /// says the same thing in a sentence, so a reader later can act on it.
    pub unclaimed: Vec<String>,
    /// Anything else a reader would want to know, such as a stated limit.
    pub notes: Vec<String>,
}

/// What a preprocessor says it is and what it handles.
///
/// Owned rather than `&'static str`: a plugin loaded from a file at runtime
/// learns its own name by asking the component, and there is no static string
/// to borrow from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// Extensions this handles, lowercase and without the dot.
    pub extensions: Vec<String>,
    /// An inline SVG for UIs to show beside the name; `None` means the UI's
    /// default mark. Rendered without script execution.
    pub logo: Option<String>,
    /// Which *build* of the plugin this is: the short SHA-256 of the artifact,
    /// filled in by the host from the store record rather than by the
    /// component — a plugin cannot know its own hash, and one that claimed to
    /// would be claiming something unverifiable.
    ///
    /// `None` for the built-in reader and anything else with no artifact
    /// behind it.
    pub build: Option<String>,
    /// Where that artifact came from — a release URL or a local path, as the
    /// store recorded it at install. Carried so a plane's ledger can turn a
    /// build hash back into a release without the store, which is mutable and
    /// may by then describe a different build entirely.
    pub source: Option<String>,
}

impl Manifest {
    /// The value stamped into `_generated_by`.
    ///
    /// `version` is the *fact-format* version — the shape of what the plugin
    /// emits — and it changes almost never, so on its own it cannot tell a
    /// plane parsed by go 1.4 from one parsed by go 1.5. That difference is
    /// exactly what a reader needs to know before concluding that an absence
    /// means absence: a plane with no `Channel` nodes may hold no channels, or
    /// may predate the parser that could see them. The build says which.
    fn stamp(&self) -> String {
        match &self.build {
            Some(build) => format!("{}@{}+{build}", self.name, self.version),
            None => format!("{}@{}", self.name, self.version),
        }
    }

    /// Whether this handler claims a file with the given extension.
    fn claims(&self, ext: &str) -> bool {
        self.extensions.iter().any(|e| e == ext)
    }
}

/// One unit of work handed to a preprocessor.
pub enum Input<'a> {
    /// A single document — an upload, a fetched body, one file.
    Document { name: &'a str, bytes: &'a [u8] },
    /// The subset of a tree the router assigned to this handler. A handler may
    /// still pull more through the [`Host`]; following imports is where a call
    /// graph lives.
    Files { paths: &'a [String] },
}

/// The host's answer to a preprocessor's requests.
///
/// Input arrives by **pull, not push** (§11): a repository pushed into a
/// plugin's memory is a needless copy with a 4 GiB ceiling, and pulling is what
/// lets a code plugin follow an import across files. It is also the capability
/// grant itself — *what the host will answer* is the boundary, rather than a
/// policy document beside it that can drift out of step.
///
/// `Sync` because handlers read files in parallel.
pub trait Host: Sync {
    /// Readable paths whose name ends with `suffix` (`""` for all), relative to
    /// the root, **sorted**.
    ///
    /// Sorted is part of the contract, not a convenience: unsorted directory
    /// order would vary fact and prose order between runs, and re-ingesting a
    /// repository is supposed to yield the same graph.
    fn list(&self, suffix: &str) -> Result<Vec<String>>;

    /// One file's bytes — and only a file [`list`](Self::list) would name.
    ///
    /// The two answer the same question. A path outside the root is refused,
    /// and so is a path inside it that the host's ignore policy hides: a
    /// `.env`, a `.gitignore`d credentials file, a build directory. Those are
    /// exactly what a project's own ignore files exist to keep out of a
    /// reader's hands, and a plugin that could `read` what `list` withheld
    /// would make the listing a suggestion rather than the grant.
    fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// What to call the thing being read, when its own contents do not say.
    ///
    /// A Rust crate normally names itself in the manifest — but pointed at
    /// `…/crates/foo/src`, the manifest is one level up and outside the grant.
    /// Without a name every crate's items key themselves `crate::…`, so
    /// ingesting two of them into one plane silently merges two `api::Database`
    /// into one node. A name is not file access, so answering it costs the
    /// boundary nothing.
    fn label(&self) -> Option<String> {
        None
    }
}

/// What the built-in document reader owns outright — kept in step with
/// `document::to_markdown`, which is the authority. Files with these
/// extensions reaching the fallback are the system working as designed, not a
/// missing plugin.
const DOCUMENT_EXTS: &[&str] = &[
    "md", "markdown", "txt", "text", // already the target format
    "doc", "docx", "odt", "rtf", "epub", "pdf", // converted by anydoc
    "ppt", "pptx", "xls", "xlsx", "ods", "odp", "csv",
];

/// Directories skipped even when a project declares no ignore rules of its own.
///
/// A `target/` can outweigh the source it was built from by orders of
/// magnitude, and a repository checked out without its `.gitignore` is still
/// not a request to ingest build output.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "vendor",
];

/// Which files the host will answer for.
///
/// A project's own ignore files are the best available statement of what is
/// source and what is derived — better than a list this crate guesses at — so
/// they are honoured by default. They are *not* obeyed unconditionally:
/// generated code a build ignores is sometimes exactly what a reader wants in
/// the graph, so every rule here can be turned off.
#[derive(Debug, Clone)]
pub struct IgnorePolicy {
    /// Honour `.gitignore`, `.git/info/exclude` and the global gitignore.
    pub gitignore: bool,
    /// Honour `.dockerignore`.
    pub dockerignore: bool,
    /// Skip dotfiles and dot-directories.
    pub hidden: bool,
    /// Skip [`IGNORED_DIRS`] regardless of what the project declares.
    pub builtin_dirs: bool,
    /// Extra gitignore-syntax patterns from configuration.
    pub extra: Vec<String>,
}

impl Default for IgnorePolicy {
    fn default() -> Self {
        Self {
            gitignore: true,
            dockerignore: true,
            hidden: true,
            builtin_dirs: true,
            extra: Vec::new(),
        }
    }
}

impl IgnorePolicy {
    /// Whether this policy withholds anything at all. One that does not —
    /// the history reader's, rooted at a `.git` directory — makes every
    /// regular file under the root readable, and the walk that would say so
    /// is not worth taking.
    fn filters(&self) -> bool {
        self.gitignore
            || self.dockerignore
            || self.hidden
            || self.builtin_dirs
            || !self.extra.is_empty()
    }
}

/// A [`Host`] over one directory on disk, refusing to answer for anything
/// outside it — or anything inside it the ignore policy hides.
pub struct LocalFiles {
    root: PathBuf,
    policy: IgnorePolicy,
    /// The files the policy admits, root-relative, as the most recent walk
    /// saw the tree. `read` checks against this rather than re-deriving the
    /// policy per path: the `ignore` crate's precedence between nested
    /// ignore files, negations, overrides and hidden ancestors is exactly
    /// what a hand-rolled per-path check gets subtly wrong, and the walk is
    /// the one implementation of it this crate has. Filled by every `list`
    /// — routing lists before any plugin reads, so the common case costs
    /// nothing extra — and by the first `read` otherwise. `None` until then.
    readable: Mutex<Option<Arc<AHashSet<PathBuf>>>>,
}

impl LocalFiles {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_policy(root, IgnorePolicy::default())
    }

    pub fn with_policy(root: impl AsRef<Path>, policy: IgnorePolicy) -> Result<Self> {
        let root = root.as_ref();
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving {}", root.display()))?;
        Ok(Self {
            root,
            policy,
            readable: Mutex::new(None),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every regular file under the root the policy admits, root-relative,
    /// in the walker's sorted order — the one walk both `list` and `read`
    /// are answered from.
    fn walk(&self) -> Result<Vec<PathBuf>> {
        let p = &self.policy;
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .git_ignore(p.gitignore)
            .git_global(p.gitignore)
            .git_exclude(p.gitignore)
            .parents(p.gitignore)
            .hidden(p.hidden)
            .require_git(false)
            // Deterministic order is part of the `Host` contract: this walk
            // decides fact and prose order, and re-ingesting a repository is
            // supposed to produce the same graph.
            .sort_by_file_name(std::ffi::OsStr::cmp);
        if p.dockerignore {
            builder.add_custom_ignore_filename(".dockerignore");
        }
        if !p.extra.is_empty() {
            let mut ov = OverrideBuilder::new(&self.root);
            for pat in &p.extra {
                // Leading `!` because an override without one is an *allow*
                // list, which would exclude everything the caller did not name.
                ov.add(&format!("!{pat}"))
                    .with_context(|| format!("bad ignore pattern `{pat}`"))?;
            }
            builder.overrides(ov.build()?);
        }
        if p.builtin_dirs {
            builder.filter_entry(|e| {
                !(e.file_type().is_some_and(|t| t.is_dir())
                    && IGNORED_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
            });
        }

        let mut out = Vec::new();
        for entry in builder.build() {
            let entry = entry?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                out.push(rel.to_path_buf());
            }
        }
        Ok(out)
    }

    /// Remember what the walk admitted, for `read` to check against.
    fn remember(&self, files: &[PathBuf]) -> Arc<AHashSet<PathBuf>> {
        let set = Arc::new(files.iter().cloned().collect::<AHashSet<_>>());
        *self.readable.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&set));
        set
    }

    /// The admitted set, walking for it if no `list` has yet.
    fn readable(&self) -> Result<Arc<AHashSet<PathBuf>>> {
        let known = self
            .readable
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match known {
            Some(set) => Ok(set),
            None => Ok(self.remember(&self.walk()?)),
        }
    }

    /// Resolve `rel` inside the root, or refuse.
    ///
    /// The check is on the *resolved* path rather than the string, because `..`
    /// segments and symlinks both walk straight through a textual one. This is
    /// the line between "the plugin reads the repository it was pointed at" and
    /// "the plugin reads the filesystem".
    fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let resolved = self
            .root
            .join(rel)
            .canonicalize()
            .with_context(|| format!("resolving {rel}"))?;
        if !resolved.starts_with(&self.root) {
            bail!("{rel} is outside the directory this preprocessor was given");
        }
        Ok(resolved)
    }
}

impl Host for LocalFiles {
    /// The directory's own name — or its parent's, when it is the `src` of
    /// something, since `src` names nothing and the directory holding it does.
    fn label(&self) -> Option<String> {
        let name = |p: &Path| p.file_name()?.to_str().map(str::to_string);
        match name(&self.root).as_deref() {
            Some("src") => name(self.root.parent()?).or_else(|| name(&self.root)),
            _ => name(&self.root),
        }
    }

    fn list(&self, suffix: &str) -> Result<Vec<String>> {
        let files = self.walk()?;
        // Every `list` refreshes what `read` may answer: a watch fold lists
        // the tree before it routes, so a host that outlives one fold still
        // reads each fold's tree and not the first one's.
        if self.policy.filters() {
            self.remember(&files);
        }
        Ok(files
            .iter()
            .map(|rel| rel.to_string_lossy().into_owned())
            .filter(|rel| suffix.is_empty() || rel.ends_with(suffix))
            .collect())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        let resolved = self.resolve(path)?;
        // A regular file or nothing: a FIFO would block the read — and the
        // sandbox with it, since no deadline reaches a host call — and a
        // directory or device is not a file a plugin was promised.
        let meta = std::fs::metadata(&resolved).with_context(|| format!("reading {path}"))?;
        if !meta.is_file() {
            bail!("{path} is not a regular file");
        }
        // Checked on the resolved path, like the root itself: `./a/../.env`
        // and a symlink to an ignored file both resolve to what they name.
        if self.policy.filters() {
            let rel = resolved.strip_prefix(&self.root).unwrap_or(&resolved);
            if !self.readable()?.contains(rel) {
                bail!(
                    "{path} is not a file this preprocessor may read — the ignore policy \
                     hides it, so `list` never named it"
                );
            }
        }
        std::fs::read(&resolved).with_context(|| format!("reading {path}"))
    }
}

/// A format-specific reader: input in, facts and prose out.
pub trait Preprocessor: Sync {
    fn manifest(&self) -> Manifest;

    fn preprocess(&self, input: &Input<'_>, host: &dyn Host) -> Result<Preprocessed>;
}

/// How the caller wants plugins loaded and configured.
///
/// Every plugin's own settings pass through as key/value pairs — the host does
/// not interpret them, because what a plugin can be configured to do is the
/// plugin's business, not the database's.
#[derive(Debug, Clone, Default)]
pub struct PluginConfig {
    /// `plugin name → its settings`, from `[plugins.<name>]` in the config.
    pub options: BTreeMap<String, Vec<(String, String)>>,
    /// An explicit store directory; `None` means the per-user default
    /// (`$XDG_DATA_HOME/drsg/plugins`).
    pub store_dir: Option<PathBuf>,
    /// Instruction budget per sandbox call; `None` keeps the default, and
    /// `Some(0)` disables the check for a trusted plugin on an input big
    /// enough to make the ceiling a nuisance rather than a safeguard.
    pub fuel: Option<u64>,
    /// Linear-memory bound per sandbox call, in bytes; `None` keeps the
    /// default. No value can lift the 4 GiB ceiling wasm32 itself imposes.
    pub memory_bytes: Option<usize>,
    /// Wall-clock deadline per sandbox call, in whole seconds; `None` keeps
    /// the default and `Some(0)` switches it off. The config file's spelling
    /// of [`ENV_PLUGIN_DEADLINE_SECS`], which still overrides it.
    pub deadline_secs: Option<u64>,
    /// Linear memory every sandbox call in the process may hold together, in
    /// MiB; `None` or `Some(0)` keeps the default. The config file's spelling
    /// of [`ENV_PLUGIN_TOTAL_MEMORY_MB`], which still overrides it.
    pub total_memory_mb: Option<u64>,
}

/// The wall-clock deadline per sandbox call, in whole seconds; `0` disables
/// it. Read by [`Plugins::load`] on top of the config file.
pub const ENV_PLUGIN_DEADLINE_SECS: &str = "DRSG_PLUGINS_DEADLINE_SECS";
/// The linear memory every sandbox call in the process may hold together,
/// in MiB. Read by [`Plugins::load`] on top of the config file.
pub const ENV_PLUGIN_TOTAL_MEMORY_MB: &str = "DRSG_PLUGINS_TOTAL_MEMORY_MB";

/// Apply the two environment knobs to `limits`.
///
/// The environment rather than [`PluginConfig`] fields: these are the net
/// under the budgets the config file already names, and an embedder that
/// wants them exactly sets them on [`Limits`] directly. A value that is not
/// a number is an error naming the variable, not a silently kept default —
/// an operator who typed it meant it.
#[cfg(feature = "plugins")]
fn apply_env_limits(limits: &mut Limits) -> Result<()> {
    fn read(name: &str) -> Result<Option<u64>> {
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|e| anyhow::anyhow!("{name}={v:?} is not a whole number: {e}")),
            _ => Ok(None),
        }
    }
    if let Some(secs) = read(ENV_PLUGIN_DEADLINE_SECS)? {
        limits.deadline = (secs > 0).then(|| std::time::Duration::from_secs(secs));
    }
    if let Some(mb) = read(ENV_PLUGIN_TOTAL_MEMORY_MB)? {
        // A zero budget would let no store run; treat it as "the default".
        limits.total_memory_bytes = (mb > 0).then_some((mb as usize) << 20);
    }
    Ok(())
}

/// The handlers a routing call can dispatch to, resolved once by the caller.
///
/// A handle rather than a per-call lookup because loading a wasm plugin
/// *compiles* it — work worth doing once per command, not once per routed
/// input.
///
/// The built-in document reader is not in here: it is the fallback every
/// unclaimed input lands on, and giving it an entry would mean giving it
/// extensions to claim.
pub struct Plugins {
    handlers: Vec<Box<dyn Preprocessor>>,
}

impl Plugins {
    /// The built-in handlers only — what a test or a tool that must not touch
    /// the operator's plugin store uses.
    pub fn builtin() -> Self {
        Self::with_options(&BTreeMap::new())
    }

    /// Built-ins, configured. Empty since the Rust parser moved out to the
    /// extensions repository — every code parser is an installed plugin now,
    /// and the built-in document reader was never in the registry.
    pub fn with_options(_options: &BTreeMap<String, Vec<(String, String)>>) -> Self {
        Plugins {
            handlers: Vec::new(),
        }
    }

    /// A registry from explicit handlers — how an embedder brings its own, and
    /// how the router's tests probe it without a wasm artifact.
    pub fn from_handlers(handlers: Vec<Box<dyn Preprocessor>>) -> Self {
        Plugins { handlers }
    }

    /// Built-ins plus every installed plugin, each verified against the hash
    /// pinned at install.
    ///
    /// A plugin that fails to load is an **error naming it**, never a silent
    /// skip: the operator installed it, and a digest quietly running without
    /// it would be the worst of the options.
    #[cfg(feature = "plugins")]
    pub fn load(config: &PluginConfig) -> Result<Self> {
        let mut plugins = Self::with_options(&config.options);
        let store = Self::store(config)?;
        let limits = Self::limits_for(config)?;
        for plugin in store.load_all(&config.options, &limits)? {
            plugins.handlers.push(Box::new(plugin));
        }
        Ok(plugins)
    }

    /// The sandbox limits `config` asks for: the file's budgets over the
    /// defaults, then the environment knobs over the file — an operator at
    /// the shell overrides what `drsg.toml` says, the same precedence every
    /// other `DRSG_*` variable has.
    #[cfg(feature = "plugins")]
    fn limits_for(config: &PluginConfig) -> Result<Limits> {
        let mut limits = Limits::default();
        match config.fuel {
            Some(0) => limits.fuel = None,
            Some(n) => limits.fuel = Some(n),
            None => {}
        }
        if let Some(bytes) = config.memory_bytes {
            limits.memory_bytes = bytes;
        }
        if let Some(secs) = config.deadline_secs {
            limits.deadline = (secs > 0).then(|| std::time::Duration::from_secs(secs));
        }
        if let Some(mb) = config.total_memory_mb {
            // Same reading as the environment's: a zero budget would let no
            // store run, so it means "the default".
            limits.total_memory_bytes = (mb > 0).then_some((mb as usize) << 20);
        }
        apply_env_limits(&mut limits)?;
        Ok(limits)
    }

    /// The store `config` names, or the per-user default.
    #[cfg(feature = "plugins")]
    fn store(config: &PluginConfig) -> Result<PluginStore> {
        match &config.store_dir {
            Some(dir) => PluginStore::open(dir.clone()),
            None => PluginStore::open_default(),
        }
    }

    /// What is available, for `--handler` errors and for `plugin list`.
    pub fn manifests(&self) -> Vec<Manifest> {
        self.handlers.iter().map(|p| p.manifest()).collect()
    }
}

/// The plugins a long-lived process keeps loaded, reloaded only when the
/// store changes underneath them.
///
/// `serve watch` used to call [`Plugins::load`] on every commit so that a
/// `drsg plugin install` between commits was picked up without a restart —
/// at the price of a full load per commit, which compiled every plugin
/// before compiled artifacts existed and still reads and verifies each one
/// now. This keeps the property and drops the price: [`current`] compares
/// the store's [stamp](PluginStore::stamp) with the one the last load saw,
/// and loads again only when it moved.
///
/// [`current`]: Self::current
#[cfg(feature = "plugins")]
pub struct LivePlugins {
    config: PluginConfig,
    loaded: Option<(StoreStamp, Plugins)>,
    loads: usize,
}

#[cfg(feature = "plugins")]
impl LivePlugins {
    /// Nothing loaded yet; the first [`current`](Self::current) loads.
    pub fn new(config: PluginConfig) -> Self {
        Self {
            config,
            loaded: None,
            loads: 0,
        }
    }

    /// The plugins as the store stands now: the ones already loaded when
    /// nothing changed since, a fresh load otherwise. A load that fails
    /// leaves the previous plugins in place, so a half-written store is an
    /// error for this call rather than a watcher left with nothing.
    pub fn current(&mut self) -> Result<&Plugins> {
        let store = Plugins::store(&self.config)?;
        let stamp = store.stamp()?;
        let fresh = matches!(&self.loaded, Some((seen, _)) if *seen == stamp);
        if !fresh {
            let plugins = Plugins::load(&self.config)?;
            // Stamped after the load: a load may itself rewrite the registry
            // (recording a compiled artifact), and that is not a change worth
            // loading again for.
            let stamp = store.stamp()?;
            self.loaded = Some((stamp, plugins));
            self.loads += 1;
        }
        Ok(&self
            .loaded
            .as_ref()
            .expect("loaded just above, or already")
            .1)
    }

    /// How many times the store has been loaded — for a caller (or a test)
    /// that wants to know a call was answered from memory.
    pub fn loads(&self) -> usize {
        self.loads
    }
}

/// Stamp `_generated_by` onto everything a handler produced, so a later reader
/// can always tell a parsed fact from a model's guess.
///
/// Name and version travel in one value because they are never useful apart.
/// `_`-prefixed properties are already hidden from the schema summary the model
/// reads and filtered out of LLM context, so this costs the read paths nothing —
/// the mechanism §8 established for `_label_as_written`.
fn stamp_provenance(out: &mut Preprocessed, mark: &str) {
    let desc = "preprocessor that produced this, rather than a model";
    for n in &mut out.nodes {
        n.props.insert(
            "_generated_by".into(),
            PropDesc::described(desc, PropValue::Str(mark.to_string())),
        );
    }
    for e in &mut out.edges {
        e.props.insert(
            "_generated_by".into(),
            PropDesc::described(desc, PropValue::Str(mark.to_string())),
        );
    }
}

/// The manifest files, and the plugin name that reads each.
///
/// A build manifest is named, not extensioned: `package.json` is not "every
/// `.json`", and claiming the extension would take every fixture and
/// `tsconfig` in the tree away from the reader that handles them. The WIT
/// `manifest` record cannot carry filenames either — a record's fields are
/// its ABI, so adding one would stop every installed component from loading
/// until all of them were rebuilt.
///
/// So the host dispatches on a shape it can see, to a plugin installed under
/// a reserved name — the same statement `.git` → [`REPO_PLUGIN`] already
/// makes, and the same one `--handler` makes by hand. A **list**, because one
/// repository may run two build systems: a plugin registered here reads the
/// files beside it, another registered later reads its own, and a name with
/// nothing installed under it leaves those files exactly where they are
/// today — read as prose, and said so in the notes. Nothing is guessed.
const MANIFEST_PLUGINS: &[(&str, &[&str])] = &[(
    "deps",
    &[
        "package.json",
        "go.mod",
        "requirements.txt",
        "pyproject.toml",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
    ],
)];

/// The plugin that claims this file by name, when one is installed.
fn manifest_handler(registry: &[Box<dyn Preprocessor>], path: &str) -> Option<usize> {
    let base = Path::new(path).file_name()?.to_str()?.to_ascii_lowercase();
    let (name, _) = MANIFEST_PLUGINS
        .iter()
        .find(|(_, files)| files.contains(&base.as_str()))?;
    registry.iter().position(|p| p.manifest().name == *name)
}

/// Extension of `name`, lowercased, without the dot. `""` when there is none.
fn extension_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Resolution is declared, never guessed (§11 — *a router that guesses is worse
/// than one that asks*): an explicit name wins, then a declared extension, then
/// nothing, which means the built-in document reader.
fn index_for(
    registry: &[Box<dyn Preprocessor>],
    ext: &str,
    handler: Option<&str>,
) -> Option<usize> {
    match handler {
        Some(want) => registry.iter().position(|p| p.manifest().name == want),
        None => registry.iter().position(|p| p.manifest().claims(ext)),
    }
}

fn no_such_handler(registry: &[Box<dyn Preprocessor>], want: &str) -> anyhow::Error {
    let known: Vec<String> = registry.iter().map(|p| p.manifest().name).collect();
    anyhow::anyhow!(
        "no preprocessor named `{want}` (known: {})",
        known.join(", ")
    )
}

/// Route one document to a handler and run it.
pub fn route_document(
    name: &str,
    bytes: &[u8],
    handler: Option<&str>,
    host: &dyn Host,
    plugins: &Plugins,
) -> Result<Preprocessed> {
    let registry = &plugins.handlers;
    let idx = index_for(registry, &extension_of(name), handler);
    if let (Some(want), None) = (handler, idx) {
        return Err(no_such_handler(registry, want));
    }
    match idx {
        Some(i) => {
            let mark = registry[i].manifest().stamp();
            let mut out = registry[i].preprocess(&Input::Document { name, bytes }, host)?;
            stamp_provenance(&mut out, &mark);
            if out.report.handlers.is_empty() {
                let facts = out.nodes.len() + out.edges.len();
                out.report.handlers.push((mark, facts));
            }
            out.report.prose_chars = out.prose.chars().count();
            Ok(out)
        }
        None => Ok(Preprocessed::prose_only(
            "document",
            crate::document::to_markdown(name, bytes)?,
        )),
    }
}

/// Route a whole directory: bucket its files by handler, run each once over its
/// own subset, and merge.
///
/// A project directory is not a document with a type — it is Rust *and* Go
/// *and* markdown *and* a lockfile — so a single dispatch would be the wrong
/// shape. The router groups; handlers do not filter.
pub fn route_tree(
    host: &dyn Host,
    handler: Option<&str>,
    plugins: &Plugins,
) -> Result<Preprocessed> {
    let paths = host.list("")?;
    route_paths(host, paths, handler, plugins)
}

/// Route an explicit set of paths — the same bucketing as [`route_tree`],
/// over a list the caller chose. This is what an incremental sync runs: the
/// files one commit touched, not the tree. Handlers may still pull *other*
/// files through the host — that is where cross-file resolution comes from —
/// but facts are only expected for the paths given.
pub fn route_paths(
    host: &dyn Host,
    paths: Vec<String>,
    handler: Option<&str>,
    plugins: &Plugins,
) -> Result<Preprocessed> {
    let registry = &plugins.handlers;
    if let Some(want) = handler
        && !registry.iter().any(|p| p.manifest().name == want)
    {
        return Err(no_such_handler(registry, want));
    }

    // Bucket by handler index; `None` is the built-in reader's pile.
    let mut buckets: BTreeMap<Option<usize>, Vec<String>> = BTreeMap::new();
    for path in paths {
        // A named manifest first: `pyproject.toml` is a dependency
        // declaration before it is a `.toml`, and only the plugin that knows
        // the format can say so. An explicit `--handler` still outranks
        // everything, as it does for extensions.
        let idx = match handler {
            None => manifest_handler(registry, &path)
                .or_else(|| index_for(registry, &extension_of(&path), handler)),
            Some(_) => index_for(registry, &extension_of(&path), handler),
        };
        buckets.entry(idx).or_default().push(path);
    }

    let mut merged = Preprocessed::default();

    // Say when a whole class of source had no handler. Once a parser is a
    // plugin rather than a built-in, a tree full of `.rs` with nothing
    // installed would otherwise be read as plain text and quietly sent to the
    // model — a behaviour change that deserves a sentence, not a guess. The
    // formats the document reader genuinely owns are not worth a warning.
    if let Some(unclaimed) = buckets.get(&None) {
        let mut by_ext: BTreeMap<String, usize> = BTreeMap::new();
        for path in unclaimed {
            let ext = extension_of(path);
            if !DOCUMENT_EXTS.contains(&ext.as_str()) && !ext.is_empty() {
                *by_ext.entry(ext).or_default() += 1;
            }
        }
        if !by_ext.is_empty() {
            let listed: Vec<String> = by_ext
                .iter()
                .map(|(ext, n)| format!(".{ext} ({n})"))
                .collect();
            merged.report.unclaimed = listed.clone();
            merged.report.notes.push(format!(
                "no installed plugin claims {} — these files were read as plain \
                 text; `drsg plugin list` shows what is installed",
                listed.join(", ")
            ));
        }
    }
    let mut owners: BTreeMap<String, String> = BTreeMap::new();

    for (idx, paths) in buckets {
        match idx {
            Some(i) => {
                let mark = registry[i].manifest().stamp();
                let mut out = registry[i].preprocess(&Input::Files { paths: &paths }, host)?;
                stamp_provenance(&mut out, &mark);
                merge(&mut merged, out, &mark, &mut owners);
            }
            None => read_unclaimed(host, &paths, &mut merged, &mut owners),
        }
    }

    merged.report.prose_chars = merged.prose.chars().count();
    Ok(merged)
}

/// Hand every unclaimed file to the built-in reader.
///
/// Conversion is CPU-bound, so the files run in parallel — but the results are
/// collected in *path* order and merged sequentially, because merge order is
/// what the final prose and node order depend on.
///
/// A file that cannot be read is counted rather than fatal: a repository is
/// full of PNGs and lockfiles, and refusing to ingest a project because it
/// contains an icon would be absurd.
fn read_unclaimed(
    host: &dyn Host,
    paths: &[String],
    merged: &mut Preprocessed,
    owners: &mut BTreeMap<String, String>,
) {
    let converted: Vec<Result<String, Option<String>>> =
        paths.par_iter().map(|path| read_one(host, path)).collect();

    for text in converted {
        match text {
            Ok(text) => {
                let out = Preprocessed::prose_only("document", text);
                merge(merged, out, "document", owners);
            }
            // `Err(None)` is a file not worth a line in the report — a binary,
            // or one that held nothing.
            Err(note) => {
                merged.report.skipped += 1;
                merged.report.notes.extend(note);
            }
        }
    }
}

/// A file large enough that reading it as prose would cost more than it says.
///
/// Aimed at the minified bundle and the generated fixture, which are text and
/// are not writing. Named in the report rather than dropped quietly, because a
/// file a reader expected to see should be explained.
const MAX_TEXT_BYTES: usize = 256 * 1024;

/// One unclaimed file as prose, or the reason there is none.
fn read_one(host: &dyn Host, path: &str) -> Result<String, Option<String>> {
    let bytes = host.read(path).map_err(|_| None)?;

    let text = match crate::document::to_markdown(path, &bytes) {
        Ok(text) => text,
        // Not a format anydoc converts — but a `.toml`, a `.yaml`, a `.sql` or
        // a source file in a language with no plugin yet is still readable
        // text, and refusing all of them would leave a repository's own
        // configuration out of its graph. `to_markdown` is right to be strict
        // about a *single upload*, where an unreadable file is a user error;
        // here the file was found rather than chosen.
        Err(_) => {
            // Text first, then size: a JPEG is not a large text file, and
            // saying so in the report sends a reader looking for the wrong
            // thing. An image with no handler is simply not prose.
            let text = std::str::from_utf8(&bytes).map_err(|_| None)?;
            if bytes.len() > MAX_TEXT_BYTES {
                let kb = bytes.len() / 1024;
                return Err(Some(format!("{path}: skipped, {kb} KiB of plain text")));
            }
            let lang = Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            // Fenced, so a model reads it as the file it is rather than as
            // prose that happens to contain colons.
            format!("```{lang}\n{}\n```", text.trim_end())
        }
    };

    if text.trim().is_empty() {
        return Err(None);
    }
    // Name the source, so the chunker keeps this file's prose in its own chunks
    // rather than gluing two documents together.
    Ok(format!("{SOURCE_MARKER} {path} -->\n\n{text}"))
}

/// Fold one handler's output into the running result.
fn merge(
    into: &mut Preprocessed,
    from: Preprocessed,
    who: &str,
    owners: &mut BTreeMap<String, String>,
) {
    let mut kept = 0usize;
    for node in from.nodes {
        // Plugin-beats-model has an authority ordering; plugin-versus-plugin
        // has none, and a well-formed key carries its file path — so a
        // collision here means a plugin bug. Keep the first and say so:
        // failing an entire ingest over it would be worse than reporting it.
        if let Some(owner) = owners.get(&node.key) {
            // One exception, and it is what makes reading a build manifest
            // worth anything: `External` is an *assertion* that the key names
            // something outside the tree, so two handlers naming one foreign
            // key are agreeing, not conflicting. The ts parser writes
            // `express` because a file imports it; the manifest reader writes
            // `express` because `package.json` declares it. That is one
            // package, and the whole point is that they meet.
            //
            // Anything else is still a plugin bug: two parsers claiming one
            // *declaration* have no authority ordering between them, and a
            // key that carries its file path should not collide at all.
            let foreign = |n: &DigestNode| n.extra_labels.iter().any(|l| l == "External");
            if foreign(&node)
                && let Some(prev) = into.nodes.iter_mut().find(|n| n.key == node.key)
            {
                if foreign(prev) {
                    // Two stand-ins: keep the first, and take whatever the
                    // second knew that the first did not.
                    for (k, v) in node.props {
                        prev.props.entry(k).or_insert(v);
                    }
                } else {
                    // A stand-in met the tree's own declaration of the same
                    // key. The declaration outranks it and keeps its label;
                    // the stand-in has nothing to add but its assertion.
                    let add: Vec<String> = node
                        .extra_labels
                        .into_iter()
                        .filter(|l| l != "External" && !prev.extra_labels.contains(l))
                        .collect();
                    prev.extra_labels.extend(add);
                }
                continue;
            }
            into.report
                .collisions
                .push(format!("{} (kept {owner}'s, dropped {who}'s)", node.key));
            continue;
        }
        owners.insert(node.key.clone(), who.to_string());
        into.nodes.push(node);
        kept += 1;
    }
    kept += from.edges.len();
    into.edges.extend(from.edges);

    if !from.prose.trim().is_empty() {
        if !into.prose.is_empty() {
            into.prose.push_str("\n\n");
        }
        into.prose.push_str(from.prose.trim());
    }

    into.report.skipped += from.report.skipped;
    into.report.notes.extend(from.report.notes);
    into.report.collisions.extend(from.report.collisions);
    into.report.unclaimed.extend(from.report.unclaimed);
    match into.report.handlers.iter_mut().find(|(n, _)| n == who) {
        Some((_, facts)) => *facts += kept,
        None => into.report.handlers.push((who.to_string(), kept)),
    }
}

#[cfg(test)]
mod tests;
