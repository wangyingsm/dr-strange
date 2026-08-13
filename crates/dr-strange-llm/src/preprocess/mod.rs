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

mod ground;
#[cfg(feature = "plugins")]
mod registry;
mod rust_code;
#[cfg(feature = "plugins")]
mod wasm;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use dr_strange_core::{PropDesc, PropValue};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use rayon::prelude::*;

use crate::digest::{DigestEdge, DigestNode, SOURCE_MARKER};

pub use ground::{FactsAndPlane, fold, stamp_run};
#[cfg(feature = "plugins")]
pub use registry::{InstalledPlugin, PluginStore};
pub use rust_code::RustCode;
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
#[derive(Debug, Default)]
pub struct PreprocessReport {
    /// `(name@version, facts emitted)`, in the order the handlers ran.
    pub handlers: Vec<(String, usize)>,
    pub prose_chars: usize,
    /// Files no handler claimed, or that carried nothing readable.
    pub skipped: usize,
    /// Keys two handlers both produced — a plugin bug, kept visible.
    pub collisions: Vec<String>,
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
}

impl Manifest {
    /// The value stamped into `_generated_by`.
    fn stamp(&self) -> String {
        format!("{}@{}", self.name, self.version)
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

/// A [`Host`] over one directory on disk, refusing to answer for anything
/// outside it.
pub struct LocalFiles {
    root: PathBuf,
    policy: IgnorePolicy,
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
        Ok(Self { root, policy })
    }

    pub fn root(&self) -> &Path {
        &self.root
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
            let Ok(rel) = entry.path().strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().into_owned();
            if suffix.is_empty() || rel.ends_with(suffix) {
                out.push(rel);
            }
        }
        Ok(out)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>> {
        std::fs::read(self.resolve(path)?).with_context(|| format!("reading {path}"))
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

    /// Built-ins, configured. `[plugins.rust] include_source = "true"` is how
    /// the native parser's one switch arrives now that it is a plugin setting
    /// rather than a field on a host struct.
    pub fn with_options(options: &BTreeMap<String, Vec<(String, String)>>) -> Self {
        let rust_include_source = options
            .get("rust")
            .is_some_and(|kv| kv.iter().any(|(k, v)| k == "include_source" && v == "true"));
        Plugins {
            handlers: vec![Box::new(RustCode {
                include_source: rust_include_source,
            })],
        }
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
        let store = match &config.store_dir {
            Some(dir) => PluginStore::open(dir.clone())?,
            None => PluginStore::open_default()?,
        };
        let mut limits = Limits::default();
        match config.fuel {
            Some(0) => limits.fuel = None,
            Some(n) => limits.fuel = Some(n),
            None => {}
        }
        if let Some(bytes) = config.memory_bytes {
            limits.memory_bytes = bytes;
        }
        for plugin in store.load_all(&config.options, &limits)? {
            plugins.handlers.push(Box::new(plugin));
        }
        Ok(plugins)
    }

    /// What is available, for `--handler` errors and for `plugin list`.
    pub fn manifests(&self) -> Vec<Manifest> {
        self.handlers.iter().map(|p| p.manifest()).collect()
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
    let registry = &plugins.handlers;
    if let Some(want) = handler
        && !registry.iter().any(|p| p.manifest().name == want)
    {
        return Err(no_such_handler(registry, want));
    }

    // Bucket by handler index; `None` is the built-in reader's pile.
    let mut buckets: BTreeMap<Option<usize>, Vec<String>> = BTreeMap::new();
    for path in host.list("")? {
        let idx = index_for(registry, &extension_of(&path), handler);
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
    match into.report.handlers.iter_mut().find(|(n, _)| n == who) {
        Some((_, facts)) => *facts += kept,
        None => into.report.handlers.push((who.to_string(), kept)),
    }
}

#[cfg(test)]
mod tests;
