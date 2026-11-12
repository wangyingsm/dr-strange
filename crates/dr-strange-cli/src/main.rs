//! drsg — command-line tools for dr-strange (arch/05). A thin wrapper over
//! `dr-strange-core`; contains no database logic. The `digest` subcommand
//! (LLM-powered ingestion) is intentionally absent pending its own design
//! session (arch/05 §3, arch/07).

/// mimalloc as the process allocator: the storage paths are allocation-heavy
/// (owned values per read, per-key buffers, record encode scratch), where it
/// measurably beats the platform malloc — see the drsg-bench comparison in
/// the commit that introduced it. Chosen in each BINARY, never in the core
/// library: the allocator is a process-global decision that belongs to the
/// embedding application.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod commands;
mod config;
mod update;

use std::io::{self, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use dr_strange_core::compact;
use dr_strange_core::{Dir, Metric};

#[derive(Parser)]
#[command(name = "drsg", version, about = "dr-strange graph database CLI")]
struct Cli {
    /// Path to the database file.
    #[arg(long, short, global = true, default_value = "graph.drsg")]
    db: PathBuf,

    /// Path to a `config.toml` (server / logging / LLM keys). Defaults to
    /// `$DRSG_CONFIG`, then `./drsg.toml` if present; environment variables
    /// override file values.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Say more about which files were read. `-v` counts them, `-vv` adds the
    /// ones an ignore rule withheld and the rule, `-vvv` names every file and
    /// what read it. Silent by default; a tree that is mostly withheld warns
    /// at any level (issue #36).
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap this repository for agent MCP access: digest it, spawn
    /// `serve watch` detached in the background, and write the connection
    /// details to `.mcp.json`.
    ///
    /// Safe to re-run — it is the "make sure drsg is up here" command.
    /// A server that is still answering is left alone; one that died is
    /// restarted on the address and token agents already hold, without
    /// re-parsing the tree.
    #[cfg(feature = "digest")]
    Init {
        /// Repository to digest and watch.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Which of a project's ignore files decide what is source:
        /// `gitignore`, `dockerignore`, or both, comma-separated. Empty
        /// honours neither. **Omitted**: `[digest] ignore_files`, then
        /// `gitignore` alone — `.dockerignore` answers a different question
        /// and is not read unless asked for.
        #[arg(long)]
        ignore_files: Option<String>,
        /// Read only what git tracks, leaving untracked files out of the
        /// graph. A tracked file with uncommitted edits is still read as it
        /// stands; the plane's sync point says when the two differ.
        #[arg(long)]
        tracked_only: bool,
        /// Read only files an installed handler claims, leaving Markdown, PDF,
        /// spreadsheets and unclaimed extensions out. A graph of a codebase,
        /// not of the documents beside it.
        #[arg(long)]
        code_only: bool,
        /// Target plane. **Omitted**: the directory's own name, `startup`
        /// as the fallback.
        #[arg(long)]
        plane: Option<String>,
        /// Address for the spawned server to listen on. **Omitted**:
        /// `drsg.toml`'s `[server] addr`, then the address a previous run
        /// recorded in `.mcp.json`, then an OS-assigned free port on
        /// 127.0.0.1.
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// Bearer token for the spawned server. **Omitted**:
        /// `DRSG_TOKEN`/`drsg.toml`'s `[server] token`, then the token a
        /// previous run recorded in `.mcp.json`, then a fresh random one.
        #[arg(long)]
        token: Option<String>,
        /// Drop the plane and fold the whole tree again, instead of resuming
        /// from where it left off.
        ///
        /// What to reach for after upgrading a language plugin: the parser
        /// decides what the tree means, so a new one only shows up in a plane
        /// that has been read again. A server already running for this
        /// repository is stopped first and comes back on the same address and
        /// token, so agents' configs keep working.
        #[arg(long)]
        rebuild: bool,
    },
    /// Print the bearer token a previous `init` recorded, masked.
    ///
    /// The clients that read the bearer from their environment rather than
    /// from a config file need it in a shell, which is what `--unhide` is
    /// for: `export DRSG_TOKEN=$(drsg token --unhide)`, the line `init`
    /// prints instead of the token so the secret stays out of its output.
    #[cfg(feature = "digest")]
    Token {
        /// Repository whose `.mcp.json` holds the token.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Print the token itself rather than a masked form of it. For
        /// command substitution, where nothing reaches the screen.
        #[arg(long)]
        unhide: bool,
    },
    /// Create a new database.
    #[cfg(not(feature = "digest"))]
    Init,
    /// Plane lifecycle.
    #[command(subcommand)]
    Plane(PlaneCmd),
    /// Import JSONL nodes/edges into a plane.
    Import {
        /// The JSONL file to read nodes and edges from.
        file: PathBuf,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// What to do when a node's external key already exists in the plane:
        /// `error` (default, writes nothing), `skip`, or `update`.
        #[arg(long, value_enum, default_value = "error")]
        on_conflict: commands::OnConflict,
    },
    /// Export a plane as JSONL.
    Export {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Fetch one node by id or `@external-key`.
    Get {
        /// A node id, or `@external-key` for one keyed by name.
        node: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Run a serialized query plan (JSON).
    Query {
        /// The plan as JSON, or `-` to read it from stdin.
        plan: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Run a query in the openCypher-subset language (compiled to a plan).
    Cypher {
        /// The query text, or `-` to read it from stdin. Omit it when
        /// `--history` names one to run again.
        query: Option<String>,
        /// Run a query from the history instead, by its id (see `drsg
        /// history`). **Omitted**: run the query given on the command line.
        #[arg(long, value_name = "ID")]
        history: Option<u64>,
        /// **Omitted**: `startup`, or — with `--history` — the plane the
        /// saved query ran against.
        #[arg(long)]
        plane: Option<String>,
        /// Embedding provider (preset or base URL) for a text
        /// `SEARCH … NEAR "…"`; the key comes from the environment. Omit for
        /// MATCH / literal-vector queries. Requires the `digest` build feature.
        #[arg(long)]
        embed: Option<String>,
        /// A `$name` parameter as `name=<json>` (repeatable), e.g.
        /// `--param min=18 --param who='"Alice"'`.
        #[arg(long = "param", value_name = "NAME=JSON")]
        param: Vec<String>,
    },
    /// The Cypher queries that have run, newest first — id, when, plane and
    /// text. (`drsg history` is the other one: a repository's commits.)
    ///
    /// With an id, print that one query's text alone, which is what to pipe
    /// into an editor. `drsg cypher --history <id>` runs it again.
    Queries {
        /// **Omitted**: list them.
        id: Option<u64>,
        /// How many to list. **Omitted**: all of them, which is capped
        /// anyway.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// One symbol's whole neighborhood in one call: definition, signature,
    /// doc, fields, callers with call sites, callees, containment and every
    /// other edge, plus what holds it or it holds by span where no edge
    /// relates them — the primary agent verb. Fuzzy names allowed; ambiguity
    /// returns candidates.
    Context {
        /// The symbol to ask about (fuzzy: an exact key, a `::name`/`.name`
        /// suffix, or a substring).
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Semantic lookup when no identifier is known: embeds the query, runs
    /// cosine top-k over the plane's `embedding` vectors, expands the best
    /// hit. Requires the plane to be vectorized.
    Search {
        /// What to look for, in plain words; it is embedded and compared.
        query: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Embedding provider: preset (openai/deepseek/qwen/ollama) or a base URL.
        #[arg(long, default_value = "openai")]
        embed: String,
        /// Embedding model override; the provider's own default otherwise.
        #[arg(long)]
        embed_model: Option<String>,
        /// How many hits to return (default 8).
        #[arg(long, default_value_t = 8)]
        k: u64,
    },
    /// One symbol's content as `prop: value` lines (vectors elided) — the
    /// lightweight node-only view; `context` adds the edges and the spans.
    Describe {
        /// The symbol to read (fuzzy, as `context` takes it).
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// How one symbol reaches another: the shortest recorded CALLS path,
    /// one hop per line; the reverse direction is tried and said when the
    /// forward holds nothing.
    Trace {
        /// The symbol the path starts at (fuzzy).
        from: String,
        /// The symbol the path should reach (fuzzy).
        to: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Blast radius: everything reaching this symbol through incoming
    /// structural edges, grouped by distance with exact counts.
    ///
    /// With `--across` it also says which other repositories the radius
    /// reaches, by the two keys they already state: a build coordinate this
    /// plane's manifests publish, and a symbol their source references.
    Impact {
        /// The symbol whose radius to measure (fuzzy, as `context` takes it).
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Hops to walk (max 6).
        #[arg(long, default_value_t = 3)]
        depth: usize,
        /// Also answer in other graphs: the planes this one attaches, or
        /// `all` — every plane this instance holds.
        #[arg(long, value_name = "SCOPE", num_args = 0..=1, default_missing_value = "attached")]
        across: Option<AcrossScope>,
        /// Markdown for a pull request instead of the compact answer, and
        /// nothing at all where nothing reaches the symbol (`ci/README.md`).
        #[arg(long)]
        report: bool,
    },
    /// Record a plane this one may ask — a plane on this instance, or one
    /// served by another drsg.
    ///
    /// An attachment carries no question of its own: it is the one fact that
    /// lets a verb reach a second graph, and every verb that crosses graphs
    /// reads it.
    Attach {
        /// The name to know it by. Also the plane it names there, unless
        /// `--as` says otherwise.
        name: String,
        /// The plane the attachment is recorded on.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Where the other drsg is served. Omit for a plane on this instance.
        #[arg(long)]
        url: Option<String>,
        /// What the plane is called on the far side, when it is not `name`.
        #[arg(long = "as")]
        as_plane: Option<String>,
        /// Also record the way back, so either plane can ask the other.
        /// Only for a plane on this instance.
        #[arg(long)]
        both: bool,
    },
    /// Serve the planes of another database on a running drsg, now.
    ///
    /// For the machine holding many repositories, each digested into its own
    /// store: one process then answers for all of them, routed by plane,
    /// where serving each would cost a process and a port. The store must
    /// already exist and sit under the server's `[server] mount_root`, and
    /// no other process may hold it — stop that server first.
    ///
    /// Acts on a *running* server over its RPC, not on a database file: the
    /// server holds the store's lock, so there is nothing here to open.
    Mount {
        /// The database directory to serve.
        path: PathBuf,
        /// Serve its planes as `<as>/<plane>`, for a repository whose plane
        /// name this server already serves.
        #[arg(long = "as")]
        as_prefix: Option<String>,
        /// Where the server is. Defaults to `[server] addr`, else the
        /// built-in serve address.
        #[arg(long)]
        url: Option<String>,
    },
    /// Stop serving a mounted database. An absent name is reported, not an
    /// error; nothing is deleted.
    Unmount {
        /// The mount to stop serving, by the name it was mounted under.
        name: String,
        /// The running drsg to ask, as `http://host:port`; the local `--db`
        /// otherwise.
        #[arg(long)]
        url: Option<String>,
    },
    /// What a running drsg serves: each mounted database with its directory
    /// and planes, and every plane with the source tree it records.
    Mounts {
        /// The running drsg to ask, as `http://host:port`; the local `--db`
        /// otherwise.
        #[arg(long)]
        url: Option<String>,
    },
    /// Forget an attachment. An absent name is reported, not an error.
    Detach {
        /// The name it was attached under.
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// What a plane attaches, and whether each is there to be asked.
    Attachments {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Ask each attachment served elsewhere whether it answers now,
        /// rather than listing it as recorded.
        #[arg(long)]
        probe: bool,
    },
    /// What serves an HTTP path, and what asks for it — matched across every
    /// language in the plane, computed now and never stored.
    ///
    /// Give a path (`/bridge/check`) for both sides of it, a handler to learn
    /// who calls it, or the function making a call to learn what serves it.
    Wire {
        /// A path, a handler, or the function making a call.
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Narrow a path to one method — `GET`, `post`.
        #[arg(long)]
        verb: Option<String>,
        /// Markdown for a pull request instead of the compact answer, and
        /// nothing at all where this states no route (`ci/README.md`).
        #[arg(long)]
        report: bool,
    },
    /// Source text: a symbol read to its end, or a range of a file.
    ///
    /// A symbol gives its body from the declaration down — exactly the
    /// declaration where the parser recorded where it stops, else a bounded
    /// guess — and `path:line` / `path:start-end` gives a range of any file
    /// in the tree the plane was parsed from, naming the symbol it opens in.
    Snippet {
        /// A symbol (fuzzy), or `path:line` / `path:start-end`.
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Lines from the declaration down. Default: exactly the declaration.
        #[arg(long)]
        lines: Option<usize>,
        /// The tree to read from, when the plane records none of its own.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Text search over the tree a plane was parsed from — each hit names the
    /// symbol it falls inside, so the next call is `context` or `snippet` on
    /// that key rather than opening the file.
    Grep {
        /// What to find. Literal text unless `--regex`.
        pattern: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Treat the pattern as a Rust regular expression.
        #[arg(long)]
        regex: bool,
        /// Case-insensitive matching.
        #[arg(long)]
        ignore_case: bool,
        /// Only under this directory or file, or with this extension (`.rs`).
        #[arg(long)]
        path: Option<String>,
        /// Lines of surrounding source per hit (max 10).
        #[arg(long)]
        context: Option<usize>,
        /// Max matching lines (default 50, max 200).
        #[arg(long)]
        max_results: Option<usize>,
        /// Search what `.gitignore` excludes too — build output, vendored
        /// trees. Off by default, so the search covers what the graph was
        /// built from.
        #[arg(long)]
        all_files: bool,
        /// The tree to search, when the plane records none of its own.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Walk edges from one node: the neighbours a hop (or several) away.
    Traverse {
        /// Where to start — an external key.
        from: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// `out` (default), `in`, or `both`.
        #[arg(long)]
        direction: Option<String>,
        /// Restrict to one edge type.
        #[arg(long)]
        edge_type: Option<String>,
        /// Minimum hops (default 1).
        #[arg(long)]
        min: Option<u32>,
        /// Maximum hops (default 1) — above 1 walks multi-hop.
        #[arg(long)]
        max: Option<u32>,
    },
    /// Read one region closely: what is within a few hops of this symbol, by
    /// label and edge type, with the hubs that hold it together.
    Fathom {
        /// The symbol at the centre of the region (fuzzy, as `context` takes it).
        name: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Hops to walk, out and in (max 6).
        #[arg(long, default_value_t = 2)]
        depth: usize,
    },
    /// A repository's history at a glance: where HEAD is, what the branches
    /// and tags point at, what was rebased, and the newest commits. Reads the
    /// `_git` plane a digest of a git checkout writes — naming the code plane
    /// finds it. Name a symbol or a path for the commits that changed it.
    History(HistoryArgs),
    /// What commit a running thing was built from, and what runs a commit.
    /// Joins the `_ops` plane — the workloads, images and infrastructure a
    /// digest of manifests records — to the `_git` plane beside it, through
    /// the revision an image's tag carries.
    Deployed(DeployedArgs),
    /// Code as it was at a git revision: a file, a range, a directory or a
    /// symbol read at `--at`; a search of that tree (`--pattern`); or what
    /// changed since an older revision (`--vs`).
    Recall(RecallArgs),
    /// The tests a change reaches — run these rather than the whole suite.
    /// The uncommitted working tree by default, `<rev>..HEAD` with `--vs`.
    /// A lower bound, and it says so: `--safe` adds every test under the
    /// packages the change touched.
    Affected(AffectedArgs),
    /// Print the soft-schema catalog (a plane's, or the whole database's).
    Catalog {
        /// One plane's catalog; every plane in the database otherwise.
        #[arg(long)]
        plane: Option<String>,
    },
    /// Graph algorithms over a plane (ROADMAP §1): read-only, transient results.
    #[command(subcommand)]
    Algo(AlgoCmd),
    /// Hybrid retrieval (ROADMAP §2): fuse vector + keyword + graph-proximity.
    Hybrid {
        /// The query text (embedded for the vector channel, tokenized for keyword).
        query: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to one label; the keyword channel requires it.
        #[arg(long)]
        label: Option<String>,
        /// Embedding property to enable the vector channel (needs `digest`).
        #[arg(long)]
        vector: Option<String>,
        /// String property to enable the BM25 keyword channel.
        #[arg(long)]
        keyword: Option<String>,
        /// How vector similarity is measured (default cosine).
        #[arg(long, value_enum, default_value_t = MetricArg::Cosine)]
        metric: MetricArg,
        /// Enable the graph-proximity channel with this many hops.
        #[arg(long)]
        graph_hops: Option<u32>,
        /// Per-hop decay for the graph channel (default 0.5).
        #[arg(long, default_value_t = 0.5)]
        graph_decay: f32,
        /// Rows returned after fusing the channels (default 10).
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Embedding provider for the vector channel (preset or base URL).
        #[arg(long, default_value = "openai")]
        embed: String,
        /// Embedding model override; the provider's own default otherwise.
        #[arg(long)]
        embed_model: Option<String>,
    },
    /// Vector index management.
    #[command(subcommand)]
    Index(IndexCmd),
    /// Summary counts across the database.
    Stats,
    /// Integrity check: scan every plane, report readability.
    Check,
    /// Rewrite storage runs left in an older on-disk format, so every block
    /// carries the checksum that turns corruption into an error.
    Upgrade,
    /// Write a consistent whole-database snapshot bundle (arch §6).
    Snapshot {
        /// Output file for the snapshot bundle.
        out: PathBuf,
    },
    /// Restore a snapshot bundle into the `--db` database, which must be empty.
    Restore {
        /// The snapshot bundle to restore.
        input: PathBuf,
    },
    /// Serve the web dashboard + JSON-RPC 2.0 API (arch/08).
    Serve {
        /// Address to listen on. Overrides `config.toml`'s `[server].addr`;
        /// defaults to 127.0.0.1:7700 when neither is set.
        #[arg(long)]
        addr: Option<SocketAddr>,
        #[command(subcommand)]
        mode: Option<ServeMode>,
        /// Run as a read-only replica of another `drsg serve` (arch/01 §9):
        /// every write RPC is refused, and this database (`--db`) mirrors
        /// the master's, bootstrapping from its `/snapshot` then tailing its
        /// `/ws/wal`. The master's address, e.g. `ws://host:7700` or
        /// `wss://host:7700`. Mutually exclusive with `mode` (checked at
        /// startup, not by clap — a subcommand can't be a `conflicts_with`
        /// target) — a replica mirrors its master rather than running its
        /// own ingestion.
        #[arg(long, value_name = "URL")]
        follow: Option<String>,
        /// Bearer token presented to the `--follow` master. Falls back to
        /// `DRSG_FOLLOW_TOKEN`. Distinct from this replica's own `DRSG_TOKEN`,
        /// which still gates *its* downstream clients.
        #[arg(long, requires = "follow")]
        follow_token: Option<String>,
    },
    /// Ask a natural-language question; an LLM turns it into a read-only plan
    /// and runs it (ROADMAP §3).
    #[cfg(feature = "digest")]
    Ask {
        /// The question, in plain language.
        question: String,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Show the generated plan without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Total model turns including tool calls and repairs.
        #[arg(long, default_value_t = 20)]
        max_attempts: u32,
        /// Safety row cap appended when the plan declares none.
        #[arg(long, default_value_t = 100)]
        limit: u64,
        /// Chat provider: preset (openai/deepseek/qwen/ollama) or a base URL.
        #[arg(long, default_value = "openai")]
        chat: String,
        /// Chat model override (default: the provider's).
        #[arg(long)]
        model: Option<String>,
        /// Embedding provider for the find_edge/find_entity grounding tools
        /// (should match how the plane was embedded). Omit to disable them.
        #[arg(long)]
        embed: Option<String>,
        /// Embedding model override (default: the provider's).
        #[arg(long)]
        embed_model: Option<String>,
    },
    /// Embed every node in a plane for similarity search (alias: `vec`).
    /// Incremental: nodes whose text is unchanged since the last run are
    /// skipped. Parser facts embed a stable projection (no positional
    /// properties); document-extracted nodes embed all their content.
    /// Finishes by ensuring a cosine vector index on `embedding` for every
    /// label that carries it — the plane is searchable when this returns.
    #[cfg(feature = "digest")]
    #[command(alias = "vec")]
    Vectorize {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Embedding provider: preset (openai/deepseek/qwen/ollama) or a base URL.
        #[arg(long, default_value = "openai")]
        embed: String,
        /// Embedding model override (default: the provider's).
        #[arg(long)]
        embed_model: Option<String>,
        /// Distance metric for the ensured indexes (`l2` is euclidean).
        #[arg(long, value_enum, default_value_t = MetricArg::Cosine)]
        metric: MetricArg,
    },
    /// Update drsg in place: check GitHub for a newer release and, if there is
    /// one, hand this process over to the installer.
    ///
    /// Nothing happens when this build is already the latest — or is *newer*
    /// than the latest release, which a build from source usually is. The
    /// installer is the same `curl … | sh` a first install runs, and it is
    /// pointed at the directory this binary is running from, so an upgrade
    /// replaces the copy on your `PATH` rather than adding a second one. A
    /// `drsg-mcp` installed beside it is updated with it.
    Update {
        /// Which binary to update: `drsg`, `drsg-mcp`, or `all`. Unset, it
        /// is `drsg` — and `all` when a `drsg-mcp` sits beside it, so the
        /// two never drift apart.
        #[arg(long)]
        bin: Option<String>,
        /// Install into this directory instead of the one this binary is
        /// running from — for a `drsg` in a location you cannot write to.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Install the archive even when its `.sha256` sidecar is missing or
        /// does not match. Insecure, as the name says: only for a mirror
        /// that publishes no checksums. `DRSG_INSECURE_SKIP_CHECKSUM=1` is
        /// the same.
        #[arg(long)]
        insecure_skip_checksum: bool,
    },
    /// Manage preprocessor plugins (ROADMAP §11): sandboxed wasm components
    /// that turn source files into graph facts before any model reads them.
    #[cfg(feature = "digest")]
    #[command(subcommand)]
    Plugin(PluginCmd),
    /// Digest a document into a plane via an LLM (arch/07). Dry-run by default.
    #[cfg(feature = "digest")]
    Digest {
        /// What to digest: a file, a **directory**, or an `http(s)://` URL.
        /// A URL is fetched, converted to Markdown, and its links followed
        /// under `--pages`/`--depth` (ROADMAP §9). A directory is walked and
        /// routed per file, so a project's code becomes parsed facts and its
        /// documents become prose (ROADMAP §11). **Omitted**: the current
        /// directory — `drsg digest` alone digests the repository you are
        /// standing in, into `graph.drsg`.
        #[arg(default_value = ".")]
        source: String,
        /// Target plane. **Omitted**: the source directory's own name, so a
        /// repo lands in a plane named after itself; `startup` when the
        /// source is not a directory.
        #[arg(long)]
        plane: Option<String>,
        /// Write the result (default is a dry-run preview).
        #[arg(long)]
        apply: bool,
        /// Chat provider: preset (openai/deepseek/qwen/ollama) or a base URL.
        #[arg(long, default_value = "openai")]
        chat: String,
        /// Embedding provider: preset or a base URL (e.g. `qwen` for DeepSeek chat).
        #[arg(long, default_value = "openai")]
        embed: String,
        /// Chat model override (default: the chat provider's).
        #[arg(long)]
        model: Option<String>,
        /// Embedding model override (default: the embed provider's).
        #[arg(long)]
        embed_model: Option<String>,
        /// Chat base-URL override.
        #[arg(long)]
        chat_url: Option<String>,
        /// Embedding base-URL override.
        #[arg(long)]
        embed_url: Option<String>,
        /// Env var for the chat API key (default: the provider's).
        #[arg(long)]
        chat_key_env: Option<String>,
        /// Env var for the embedding API key (default: the provider's).
        #[arg(long)]
        embed_key_env: Option<String>,
        /// Target chunk size in characters.
        #[arg(long, default_value_t = 4000)]
        chunk_chars: usize,
        /// Per-chunk extraction requests to run concurrently (1 = sequential).
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Skip embedding generation.
        #[arg(long)]
        no_embed: bool,
        /// Don't link extracted entities to existing plane nodes (propose every
        /// entity as new; skips the per-chunk vector retrieval).
        #[arg(long)]
        no_link: bool,
        /// How thoroughly to clean up the extraction (ROADMAP §8), trading
        /// cost for precision: `coarse` reconciles the label and edge-type
        /// vocabularies; `fine` also merges entities that name the same thing;
        /// `super` also re-reads every entity against all the passages
        /// mentioning it — the most accurate, and by far the most expensive
        /// (~15× input token usage).
        #[arg(long, default_value = "fine")]
        mode: String,
        /// URL only: what the crawl should count as relevant, beyond what the
        /// page itself is about. Sharpens which links are worth following.
        #[arg(long)]
        topic: Option<String>,
        /// URL only: ceiling on pages kept, the root included.
        #[arg(long, default_value_t = 10)]
        pages: usize,
        /// Force a preprocessor by name (e.g. `rust`) instead of routing by
        /// file extension. A router that guesses is worse than one that asks.
        #[arg(long)]
        handler: Option<String>,
        /// Store each parsed function's own source on its node, for retrieval.
        /// Off by default: it is roughly a copy of the codebase in the graph,
        /// and properties share one record, so every read of that node decodes
        /// the body too.
        #[arg(long)]
        plugin_source: bool,
        /// URL only: how far to follow links. 0 reads just the page named.
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Don't read the repository's history. By default, digesting a
        /// directory that is a git checkout also reads its commits, branches,
        /// tags and rebases into a plane of its own — facts only, never a
        /// model call — when the `git` plugin is installed.
        #[arg(long)]
        no_git: bool,
        /// Where that history lands. **Omitted**: `<plane>_git`, beside the
        /// code plane. History and code are kept apart because they answer
        /// different questions and have different lifetimes.
        #[arg(long)]
        git_plane: Option<String>,
        /// Which of a project's ignore files decide what is source:
        /// `gitignore`, `dockerignore`, or both, comma-separated. Empty
        /// honours neither. **Omitted**: `[digest] ignore_files`, then
        /// `gitignore` alone — `.dockerignore` answers a different question
        /// and is not read unless asked for.
        #[arg(long)]
        ignore_files: Option<String>,
        /// Read only what git tracks, leaving untracked files out of the
        /// graph. A tracked file with uncommitted edits is still read as it
        /// stands; the plane's sync point says when the two differ.
        #[arg(long)]
        tracked_only: bool,
        /// Read only files an installed handler claims, leaving Markdown, PDF,
        /// spreadsheets and unclaimed extensions out. A graph of a codebase,
        /// not of the documents beside it.
        #[arg(long)]
        code_only: bool,
    },
}

#[derive(Subcommand)]
enum ServeMode {
    /// Serve as usual, and watch a git repository beside it: every commit's
    /// changed files run through the installed plugins and the plane is
    /// updated in place — new symbols created, gone ones deleted, edges
    /// rewritten. Facts only; no model is ever called.
    #[cfg(feature = "digest")]
    Watch {
        /// Repository to watch.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Which of a project's ignore files decide what is source:
        /// `gitignore`, `dockerignore`, or both, comma-separated. Empty
        /// honours neither. **Omitted**: `[digest] ignore_files`, then
        /// `gitignore` alone — `.dockerignore` answers a different question
        /// and is not read unless asked for.
        #[arg(long)]
        ignore_files: Option<String>,
        /// Read only what git tracks, leaving untracked files out of the
        /// graph. A tracked file with uncommitted edits is still read as it
        /// stands; the plane's sync point says when the two differ.
        #[arg(long)]
        tracked_only: bool,
        /// Read only files an installed handler claims, leaving Markdown, PDF,
        /// spreadsheets and unclaimed extensions out. A graph of a codebase,
        /// not of the documents beside it.
        #[arg(long)]
        code_only: bool,
        /// Target plane. **Omitted**: the directory's own name, `startup`
        /// as the fallback.
        #[arg(long)]
        plane: Option<String>,
        /// Rebuild the plane from the whole tree before serving: drop it,
        /// re-create it, and fold every file through the installed plugins.
        /// Facts only — embeddings return on the next `drsg digest`.
        #[arg(long)]
        force: bool,
        /// Don't keep the repository's history plane (`<plane>_git`) current.
        /// By default every commit that folds into the code plane also
        /// refreshes history — commits, branches, tags and rebases — when the
        /// `git` plugin is installed.
        #[arg(long)]
        no_git: bool,
    },
}

#[cfg(feature = "digest")]
#[derive(Subcommand)]
enum PluginCmd {
    /// Install a plugin: an official plugin's name, a local `.wasm` file, or
    /// an `http(s)://` URL.
    ///
    /// A name is looked up in the official catalog, which lives in the
    /// extensions repository rather than in this binary — so a plugin release
    /// needs no drsg release — and pins each artifact's SHA-256, checked on
    /// download. The artifact is then validated as a component, asked to
    /// describe itself, and its hash pinned in the store; every later load
    /// re-checks it, so a file that changes on disk is refused rather than
    /// silently run. Installing a name again is the upgrade path.
    Install {
        /// An official plugin's name (`rust`, `go`, `git`, …), a path to a
        /// `.wasm` component, or a URL to download one from.
        /// **Omitted**: an interactive chooser lists the official catalog —
        /// pick by number (`0` = all of them), type a name, paste a path/URL,
        /// or `q` to cancel.
        source: Option<String>,
    },
    /// List installed plugins as a table: name, version, extensions, hash,
    /// source.
    List {
        /// List the official catalog instead of what is installed, each entry
        /// tagged `[installed]`/`[upgradable]` against this store. Needs the
        /// network; falls back to the cached catalog and says how old it is.
        #[arg(long)]
        available: bool,
        /// Print machine-readable JSON instead of the table — the same
        /// records `plugin.list` returns over RPC, so an agent can read
        /// them from either surface.
        #[arg(long)]
        json: bool,
    },
    /// Remove an installed plugin by name.
    Remove { name: String },
}

#[derive(Subcommand)]
enum PlaneCmd {
    List,
    Create { name: String },
    Drop { name: String },
    Show { name: String },
}

#[derive(Subcommand)]
enum AlgoCmd {
    /// PageRank importance, most-important first.
    Pagerank {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// How many top-ranked nodes to print.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Probability a walk follows an edge rather than restarting
        /// (default 0.85).
        #[arg(long, default_value_t = 0.85)]
        damping: f64,
        /// Most iterations to run before stopping (default 20).
        #[arg(long, default_value_t = 20)]
        max_iters: u32,
    },
    /// Weakly connected components (representative = smallest id).
    Components {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// How many of the largest components to print (default 50).
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
    /// Weighted shortest path between two node ids.
    ShortestPath {
        /// Node id the path starts at.
        src: u64,
        /// Node id the path should reach.
        dst: u64,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// Edge direction to follow.
        #[arg(long, value_enum, default_value_t = DirArg::Out)]
        dir: DirArg,
        /// Numeric edge property to use as weight (missing ⇒ unit weight).
        #[arg(long)]
        weight: Option<String>,
    },
    /// Louvain community detection.
    Louvain {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// How many of the largest communities to print (default 50).
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
    /// Dependency order; a node's generation is its depth, so one generation
    /// can be processed in parallel.
    Toposort {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// The one edge type that claims the order. Left unset, every type
        /// counts, and an unrelated edge can deny the order altogether.
        #[arg(long)]
        edge_type: Option<String>,
        /// Edge direction: `out` when an edge means "feeds", `in` when it
        /// means "depends on". `both` orders nothing and is refused.
        #[arg(long, value_enum, default_value_t = DirArg::Out)]
        dir: DirArg,
        /// How many ordered nodes to print (default 50).
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
    /// Cycles, each as the node list that closes it. No output rows means the
    /// scope is a DAG.
    Cycles {
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// The one edge type to look for cycles in. Left unset, two edges that
        /// mean different things can close a loop between them.
        #[arg(long)]
        edge_type: Option<String>,
        /// Stop after this many cycles; `1` asks only whether there is one.
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
}

/// How far beyond its own plane a verb looks.
#[derive(Copy, Clone, ValueEnum)]
enum AcrossScope {
    /// The planes this one attaches.
    Attached,
    /// Every plane this instance holds, and the attachments another drsg
    /// serves.
    All,
}

impl From<AcrossScope> for compact::Scope {
    fn from(s: AcrossScope) -> Self {
        match s {
            AcrossScope::Attached => compact::Scope::Attached,
            AcrossScope::All => compact::Scope::All,
        }
    }
}

#[derive(Copy, Clone, ValueEnum)]
enum DirArg {
    Out,
    In,
    Both,
}

impl From<DirArg> for Dir {
    fn from(d: DirArg) -> Self {
        match d {
            DirArg::Out => Dir::Out,
            DirArg::In => Dir::In,
            DirArg::Both => Dir::Both,
        }
    }
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Declare (and build) a vector index. Two arguments (`LABEL PROPERTY`)
    /// index one label; one argument (`PROPERTY`) indexes **every label in
    /// the plane that carries the property**; no arguments defaults the
    /// property to `embedding` — so `drsg index ensure --plane <p>` is the
    /// whole follow-up to `drsg vectorize`.
    Ensure {
        /// The label to index, or the property when given alone.
        #[arg(value_name = "LABEL|PROPERTY", default_value = "embedding")]
        first: String,
        /// The property to index, when a label was given first.
        #[arg(value_name = "PROPERTY")]
        second: Option<String>,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// How similarity is measured at query time (default cosine).
        #[arg(long, value_enum, default_value_t = MetricArg::Cosine)]
        metric: MetricArg,
    },
    /// Declare (and build) a BM25 keyword index. Two arguments
    /// (`LABEL PROPERTY`) index one label; one argument (`PROPERTY`) indexes
    /// every label in the plane that carries the property.
    Keyword {
        /// The label to index, or the property when given alone.
        #[arg(value_name = "LABEL|PROPERTY")]
        first: String,
        /// The property to index, when a label was given first.
        #[arg(value_name = "PROPERTY")]
        second: Option<String>,
        /// Plane to read. Defaults to `startup`, which is empty on a normal
        /// install — name the plane this tree was digested into.
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Analyzer language (name or code, e.g. `english`/`en`, `french`/`fr`).
        #[arg(long, default_value = "english")]
        lang: String,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum MetricArg {
    Cosine,
    Dot,
    L2,
}

impl From<MetricArg> for Metric {
    fn from(m: MetricArg) -> Self {
        match m {
            MetricArg::Cosine => Metric::Cosine,
            MetricArg::Dot => Metric::Dot,
            MetricArg::L2 => Metric::L2,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Load config and fold its env-backed values (token, log dir, LLM keys)
    // into the environment BEFORE logging starts or the tokio runtime spawns
    // threads — `apply_env` relies on the process being single-threaded here.
    let cfg = config::load(cli.config.as_deref())?;
    config::apply_env(&cfg);
    // Hold the guard for the whole run so the file writer flushes on exit.
    let _log = dr_strange_log::init("drsg");
    let out = io::stdout();
    let mut out = out.lock();
    run(cli, &cfg, &mut out)?;
    out.flush()?;
    Ok(())
}

/// `drsg history`'s arguments: which repository, what of it, and how much.
#[derive(clap::Args)]
struct HistoryArgs {
    /// A symbol or a repository-relative path: the commits that changed the
    /// file it lives in, newest first. **Omitted**: the whole repository at
    /// a glance.
    name: Option<String>,
    /// The history plane, or the code plane beside it (`<plane>_git` is
    /// tried when the one named holds no commits).
    #[arg(long, default_value = "startup")]
    plane: String,
    /// Read declarations at each listed commit and at its first parent: with
    /// a symbol, whether the commit moved it or only the file around it; with
    /// a path, which of the file's declarations moved. Two parses of one file
    /// per commit, so it is asked for rather than assumed.
    #[arg(long)]
    exact: bool,
    /// How many commits to list, newest first.
    #[arg(long, default_value_t = 15)]
    limit: usize,
}

/// `drsg deployed`'s arguments: what to ask about, and where to look.
#[derive(clap::Args)]
struct DeployedArgs {
    /// A workload, container, resource or image key, or a revision.
    /// **Omitted**: every image, with what joined and what did not.
    target: Option<String>,
    /// The code plane, or the ops plane beside it — naming `myrepo` finds
    /// `myrepo_ops` and `myrepo_git`.
    #[arg(long, default_value = "startup")]
    plane: String,
    /// The ops plane, when it is not the one the convention names.
    #[arg(long)]
    ops: Option<String>,
    /// The history plane, when it is not the one the convention names.
    #[arg(long)]
    history: Option<String>,
}

/// `drsg deployed`: core's verb over flags, printed as its text.
fn run_deployed(
    args: DeployedArgs,
    db_path: &Path,
    cfg: &config::Config,
    out: &mut dyn Write,
) -> Result<()> {
    let db = commands::open(db_path, config::retain_commits(cfg))?;
    let asked = dr_strange_core::ops::Asked {
        plane: &args.plane,
        ops: args.ops.as_deref(),
        history: args.history.as_deref(),
    };
    commands::deployed(&db, &asked, args.target.as_deref(), out)
}

/// `drsg recall`'s arguments: the MCP tool's request, as flags.
#[derive(clap::Args)]
struct RecallArgs {
    /// A path, `path:start-end`, a directory, or a symbol (fuzzy). Omit it with `--pattern`.
    name: Option<String>,
    /// The revision: a sha, a branch, a tag, HEAD, a date (YYYY-MM-DD or
    /// RFC-3339) or `<rev>@{<date>}`, each optionally followed by `~n` / `^n`.
    #[arg(long)]
    at: String,
    /// Plane to read. Defaults to `startup`, which is empty on a normal
    /// install — name the plane this tree was digested into.
    #[arg(long, default_value = "startup")]
    plane: String,
    /// An older revision: print what changed in the name since then.
    #[arg(long)]
    vs: Option<String>,
    /// Lines returned (a file 40, a symbol its extent, a diff 200; max 400).
    #[arg(long)]
    lines: Option<usize>,
    /// Search the tree at `--at` for this text instead of reading a name.
    #[arg(long)]
    pattern: Option<String>,
    /// Treat the pattern as a POSIX extended regex (git's).
    #[arg(long)]
    regex: bool,
    /// Case-insensitive matching.
    #[arg(long)]
    ignore_case: bool,
    /// Only under this directory or file, or with this extension (`.rs`).
    #[arg(long)]
    path: Option<String>,
    /// Max matching lines (default 50, max 200).
    #[arg(long)]
    max_results: Option<usize>,
    /// The checkout to read, when the plane records none of its own.
    #[arg(long)]
    root: Option<PathBuf>,
}

impl RecallArgs {
    /// The tool's request these flags spell, and the fallback checkout.
    fn into_req(self) -> (dr_strange_mcp::RecallReq, Option<PathBuf>) {
        let req = dr_strange_mcp::RecallReq {
            plane: self.plane,
            name: self.name,
            at: self.at,
            vs: self.vs,
            lines: self.lines,
            pattern: self.pattern,
            regex: self.regex.then_some(true),
            ignore_case: self.ignore_case.then_some(true),
            path: self.path,
            max_results: self.max_results,
        };
        (req, self.root)
    }
}

/// `drsg affected`'s arguments: the MCP tool's request, as flags.
#[derive(clap::Args)]
struct AffectedArgs {
    /// An older revision: the change is `<rev>..HEAD` rather than what is
    /// uncommitted in the working tree.
    #[arg(long)]
    vs: Option<String>,
    /// Plane to read. Defaults to `startup`, which is empty on a normal
    /// install — name the plane this tree was digested into.
    #[arg(long, default_value = "startup")]
    plane: String,
    /// Add every test under the packages the change touched, reached or not.
    #[arg(long)]
    safe: bool,
    /// Hops of incoming edges to walk (1..=6, default 3).
    #[arg(long)]
    depth: Option<usize>,
    /// Markdown for a pull request instead of the compact answer, and nothing
    /// at all when the change names no declaration — so a CI template can test
    /// the output for emptiness (`ci/README.md`).
    #[arg(long, conflicts_with = "seeds")]
    report: bool,
    /// Print only the keys of the declarations the change touched, one per
    /// line: what to ask `impact` and `wire` about, one at a time.
    #[arg(long)]
    seeds: bool,
    /// The checkout to read, when the plane records none of its own.
    #[arg(long)]
    root: Option<PathBuf>,
}

impl AffectedArgs {
    /// The tool's request these flags spell, the output asked for, and the
    /// fallback checkout.
    fn into_req(
        self,
    ) -> (
        dr_strange_mcp::AffectedReq,
        dr_strange_mcp::AffectedOutput,
        Option<PathBuf>,
    ) {
        let out = match (self.report, self.seeds) {
            (_, true) => dr_strange_mcp::AffectedOutput::Seeds,
            (true, _) => dr_strange_mcp::AffectedOutput::Report,
            _ => dr_strange_mcp::AffectedOutput::Compact,
        };
        let req = dr_strange_mcp::AffectedReq {
            plane: self.plane,
            vs: self.vs,
            safe: self.safe,
            depth: self.depth,
        };
        (req, out, self.root)
    }
}

/// `drsg affected`: the MCP tool's body over flags, printed as its text.
fn run_affected(
    args: AffectedArgs,
    db_path: &Path,
    cfg: &config::Config,
    out: &mut dyn Write,
) -> Result<()> {
    let db = commands::open(db_path, config::retain_commits(cfg))?;
    let (req, want, root) = args.into_req();
    let parsers = recall_parsers(cfg)?;
    write!(
        out,
        "{}",
        dr_strange_mcp::affected_logic(&db, root.as_deref(), &*parsers, req, want)?
    )?;
    Ok(())
}

/// The preprocessors `recall` locates a symbol with: the configured plugin store.
#[cfg(feature = "digest")]
fn recall_parsers(cfg: &config::Config) -> Result<std::sync::Arc<dyn dr_strange_mcp::Parsers>> {
    let live = dr_strange_llm::LivePlugins::new(config::plugin_config(cfg)?);
    Ok(std::sync::Arc::new(std::sync::Mutex::new(live)))
}

/// The preprocessors `recall` locates a symbol with: the default plugin store,
/// since this build carries no `[plugins]` configuration of its own.
#[cfg(not(feature = "digest"))]
fn recall_parsers(_cfg: &config::Config) -> Result<std::sync::Arc<dyn dr_strange_mcp::Parsers>> {
    Ok(dr_strange_mcp::default_parsers())
}

/// `drsg history` — the parsers come along because `--exact` reads a
/// declaration at each commit, the same way `recall` reads one at a revision.
fn run_history(
    args: HistoryArgs,
    db_path: &Path,
    cfg: &config::Config,
    out: &mut dyn Write,
) -> Result<()> {
    let db = commands::open(db_path, config::retain_commits(cfg))?;
    let parsers = recall_parsers(cfg)?;
    let asked = dr_strange_mcp::HistoryReq {
        plane: args.plane,
        name: args.name,
        exact: Some(args.exact),
        limit: Some(args.limit),
    };
    commands::history(&db, asked, &*parsers, out)
}

/// `drsg recall`: the MCP tool's body over flags, printed as its text.
fn run_recall(
    args: RecallArgs,
    db_path: &Path,
    cfg: &config::Config,
    out: &mut dyn Write,
) -> Result<()> {
    let db = commands::open(db_path, config::retain_commits(cfg))?;
    let (req, root) = args.into_req();
    let parsers = recall_parsers(cfg)?;
    let rendered = dr_strange_mcp::recall_logic(&db, root.as_deref(), &*parsers, req)?;
    write!(out, "{}", as_text(&rendered))?;
    Ok(())
}

/// The tree a plane was parsed from, as it recorded at digest time.
///
/// `grep` searches a directory, and the right one is the plane's own — the
/// same `synced_root` `snippet` consults. Read here rather than reusing the
/// digest-side reader, which is behind that feature and this is not.
fn plane_root(db: &dr_strange_core::Database, plane: &str) -> Option<PathBuf> {
    let plane = db.plane(plane).ok()?;
    let props = plane.properties().ok()?;
    match &props.get("synced_root")?.value {
        dr_strange_core::PropValue::Str(root) => Some(PathBuf::from(root)),
        _ => None,
    }
}

/// The text a reader's verb answers with. `snippet` and `grep` return a JSON
/// string; anything else is rendered as it stands.
fn as_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The command surface, dispatched in groups.
///
/// One `match` over thirty-odd variants is a body nobody re-reads before
/// changing it, so the arms are grouped by what they are for and each group
/// hands what it does not recognise to the next. The chain is exhaustive by
/// construction: the last group has no `other` arm to fall through.
fn run(cli: Cli, cfg: &config::Config, out: &mut dyn Write) -> Result<()> {
    let db = cli.db.clone();
    let verbose = commands::Verbosity::new(cli.verbose);
    run_bootstrap(cli.command, &db, cfg, verbose, out)
}

/// Bootstrapping a repository, plane lifecycle, and data in and out.
fn run_bootstrap(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        #[cfg(feature = "digest")]
        Command::Init {
            dir,
            plane,
            addr,
            token,
            rebuild,
            ignore_files,
            tracked_only,
            code_only,
        } => {
            // Both fall back to `drsg.toml`'s `[server]`, the same way
            // `serve` reads them — pinning an address and token there is what
            // keeps a repo's MCP endpoint stable across restarts.
            let addr = addr.or(cfg.server.addr);
            let token = token.or_else(|| cfg.server.token.clone());
            // The plugin store, so `init` can say whether this repository's
            // history will be read — the answer depends on what is installed,
            // and a promise it cannot keep would be worse than silence.
            let plugin_config = config::plugin_config(cfg)?;
            commands::init_bootstrap(
                commands::InitArgs {
                    db_path,
                    verbose,
                    tracked_only,
                    code_only: config::code_only(cfg, code_only),
                    dir,
                    plane,
                    addr,
                    token,
                    rebuild,
                    ignore_files,
                },
                &plugin_config,
                out,
            )
        }
        #[cfg(feature = "digest")]
        // No database: the token lives in `.mcp.json`, and a server already
        // holding the lock must not be collided with just to read it.
        Command::Token { dir, unhide } => commands::print_token(&dir, unhide, out),
        #[cfg(not(feature = "digest"))]
        Command::Init => commands::init(db_path, out),
        Command::Plane(cmd) => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            match cmd {
                PlaneCmd::List => commands::plane_list(&db, out),
                PlaneCmd::Create { name } => commands::plane_create(&db, &name, out),
                PlaneCmd::Drop { name } => commands::plane_drop(&db, &name, out),
                PlaneCmd::Show { name } => commands::plane_show(&db, &name, out),
            }
        }
        Command::Import {
            file,
            plane,
            on_conflict,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let reader = BufReader::new(std::fs::File::open(&file)?);
            commands::import(&db, &plane, reader, on_conflict, out)
        }
        Command::Export { plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::export(&db, &plane, out)
        }
        Command::Get { node, plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::get(&db, &plane, &node, out)
        }
        other => run_query(other, db_path, cfg, verbose, out),
    }
}

/// Running a query, and the record of what has run.
fn run_query(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Query { plan, plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let plan = if plan == "-" {
                io::read_to_string(io::stdin())?
            } else {
                plan
            };
            commands::query(&db, &plane, &plan, out)
        }
        Command::Cypher {
            query,
            history,
            plane,
            embed,
            param,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // A saved query brings its own plane with it: it was written
            // against one, and re-running it against `startup` would be
            // running a different query. An explicit `--plane` still wins.
            let (query, ran_against) = match (history, query) {
                (Some(_), Some(_)) => bail!("give a query or `--history`, not both"),
                (None, None) => {
                    bail!("give a query, `-` to read one from stdin, or `--history <id>`")
                }
                (Some(id), None) => {
                    let Some(saved) = db.recorded_query(id)? else {
                        bail!("no query #{id} in the history — `drsg history` lists them");
                    };
                    (saved.query, Some(saved.plane))
                }
                (None, Some(q)) if q == "-" => (io::read_to_string(io::stdin())?, None),
                (None, Some(q)) => (q, None),
            };
            let plane = plane
                .or(ran_against)
                .unwrap_or_else(|| "startup".to_string());
            commands::cypher(&db, &plane, &query, embed.as_deref(), &param, out)
        }
        Command::Queries { id, limit } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::queries(&db, id, limit, out)
        }
        other => run_agent_verbs(other, db_path, cfg, verbose, out),
    }
}

/// The agent verbs over a digested code plane (chapter 10's eight),
/// answering about symbols rather than about rows.
fn run_agent_verbs(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Context { name, plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::compact(&db, &plane, &name, dr_strange_core::compact::context, out)
        }
        #[cfg(feature = "digest")]
        Command::Search {
            query,
            plane,
            embed,
            embed_model,
            k,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let embedder =
                dr_strange_llm::build_provider(&embed, embed_model.as_deref(), None, None, true)?;
            write!(
                out,
                "{}",
                dr_strange_llm::semantic_search(&db, &plane, &query, &embedder, k)?
            )?;
            Ok(())
        }
        #[cfg(not(feature = "digest"))]
        Command::Search { .. } => anyhow::bail!(
            "`drsg search` embeds the query and needs the digest feature; \
             rebuild with default features"
        ),
        Command::Describe { name, plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::compact(&db, &plane, &name, dr_strange_core::compact::describe, out)
        }
        Command::Trace { from, to, plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let p = db.plane(&plane)?;
            write!(out, "{}", dr_strange_core::compact::trace(&p, &from, &to)?)?;
            Ok(())
        }
        Command::Impact {
            name,
            plane,
            depth,
            across,
            report,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // Reading a graph the operator attached themselves is proxied
            // like their other commands, as `wire` reads one.
            let remote = dr_strange_web::attach::Reader::from_env(config::network(cfg)?);
            let ask = compact::Impact {
                plane: &plane,
                name: &name,
                depth,
                across: across.map(Into::into),
                instance: None,
                remote: Some(&remote),
            };
            let answer = match report {
                true => compact::impact_report(&db, ask)?,
                false => compact::impact(&db, ask)?,
            };
            write!(out, "{answer}")?;
            Ok(())
        }
        at @ (Command::Attach { .. } | Command::Detach { .. } | Command::Attachments { .. }) => {
            run_attach_verbs(at, db_path, cfg, out)
        }
        m @ (Command::Mount { .. } | Command::Unmount { .. } | Command::Mounts { .. }) => {
            run_mount_verbs(m, cfg, out)
        }
        Command::Wire {
            name,
            plane,
            verb,
            report,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // The operator attached these planes themselves, so reading one is
            // proxied like their other commands — the same reading
            // `serve --follow` takes of the primary it was pointed at.
            let remote = dr_strange_web::attach::Reader::from_env(config::network(cfg)?);
            let ask = dr_strange_core::compact::Wire {
                plane: &plane,
                name: &name,
                verb: verb.as_deref(),
                instance: None,
                remote: Some(&remote),
            };
            let answer = match report {
                true => dr_strange_core::compact::wire_report(&db, ask)?,
                false => dr_strange_core::compact::wire(&db, ask)?,
            };
            write!(out, "{answer}")?;
            Ok(())
        }
        // `snippet`, `grep` and `traverse` are the MCP surface's, called here
        // rather than reimplemented: two copies of a reader's verb would
        // answer differently the first time one of them was fixed.
        other => run_tree_verbs(other, db_path, cfg, verbose, out),
    }
}

/// How `serve watch` watches a mounted repository: the rules the operator
/// gave this server, applied to whatever tree each mounted plane records.
///
/// One of these is built once and shared by every mount, because the rules
/// are the server's, not the repository's — an operator sets `code_only` and
/// the ignore policy for the machine, and a mount arriving three hours later
/// is watched under the same ones.
#[cfg(feature = "digest")]
struct WatchMounts {
    code_only: bool,
    policy: dr_strange_llm::IgnorePolicy,
    plugin_config: dr_strange_llm::PluginConfig,
    embed: Option<(String, Option<String>, Option<String>)>,
    git: bool,
    /// The directories a mounted tree may lie under to be watched: the
    /// watched tree itself, and the one mounts come from.
    ///
    /// A plane's `synced_root` is data, so the same rule the file-reading
    /// verbs apply holds here — more so, since a watcher *writes* what it
    /// reads back into the graph.
    readable: Vec<PathBuf>,
}

#[cfg(feature = "digest")]
impl WatchMounts {
    /// Start a watcher per plane this mount serves that records a tree this
    /// server may read.
    fn watch(&self, mount: &dr_strange_core::Mount) {
        for served in &mount.planes {
            // The name the mount's own database knows it by, which is what a
            // fold writes against.
            let within = served
                .rsplit_once(dr_strange_core::instance::PREFIX_SEP)
                .map_or(served.as_str(), |(_, p)| p);
            let Some(dir) = self.tree_of(mount, within) else {
                continue;
            };
            let tree = commands::WatchTree {
                dir,
                code_only: self.code_only,
                policy: self.policy.clone(),
            };
            // Borrowed, not held: the mount table owns this database, and
            // unmounting it must free its store rather than wait for the
            // watcher below to notice.
            let db = commands::WatchedDb::Borrowed(std::sync::Arc::downgrade(mount.database()));
            let plane = within.to_string();
            let (plugins, embed, git) = (self.plugin_config.clone(), self.embed.clone(), self.git);
            // A plain thread, as the host's watcher is: the work is blocking
            // (git, parsing, write transactions) and must not sit on the
            // request executor.
            std::thread::spawn(move || {
                // Never `force` for a mount: that re-routes the whole tree
                // before dropping the plane, and nobody asked for a rebuild
                // of a repository they merely mounted.
                commands::watch(db, tree, plane, plugins, embed, false, git);
            });
        }
    }

    /// The tree `plane` records, if this server may watch it — else `None`,
    /// said once per mount rather than per fold.
    fn tree_of(&self, mount: &dr_strange_core::Mount, plane: &str) -> Option<PathBuf> {
        let recorded = mount
            .database()
            .plane(plane)
            .ok()?
            .properties()
            .ok()?
            .get("synced_root")
            .map(|d| d.value.clone())
            .and_then(|v| match v {
                dr_strange_core::PropValue::Str(s) if !s.is_empty() => Some(PathBuf::from(s)),
                _ => None,
            })?;
        if !self
            .readable
            .iter()
            .any(|r| dr_strange_core::instance::dir_within(r, &recorded))
        {
            tracing::warn!(
                mount = %mount.name,
                plane,
                tree = %recorded.display(),
                "not watching: the tree this plane records is outside every \
                 directory this server reads"
            );
            return None;
        }
        tracing::info!(mount = %mount.name, plane, tree = %recorded.display(), "watching a mounted repository");
        Some(recorded)
    }
}

/// The verbs that change what a *running* server serves (issue #44).
///
/// These alone among the CLI's verbs open no database: the server holds its
/// store's lock, so acting on a file here would either fail or edit a copy
/// nothing is serving. They speak its RPC instead, which is also what makes
/// them work against a server on another machine.
fn run_mount_verbs(cmd: Command, cfg: &config::Config, out: &mut dyn Write) -> Result<()> {
    let (method, params, url) = match cmd {
        Command::Mount {
            path,
            as_prefix,
            url,
        } => (
            "db.mount",
            serde_json::json!({ "path": path.display().to_string(), "as": as_prefix }),
            url,
        ),
        Command::Unmount { name, url } => ("db.unmount", serde_json::json!({ "name": name }), url),
        Command::Mounts { url } => ("db.mounts", serde_json::json!({}), url),
        _ => unreachable!("every Command variant belongs to a dispatch group"),
    };
    let base = url.unwrap_or_else(|| config::server_url(cfg));
    let token = std::env::var("DRSG_TOKEN").ok().filter(|t| !t.is_empty());
    let answer = dr_strange_web::attach::call(
        &config::network(cfg)?,
        &base,
        token.as_deref(),
        method,
        params,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    write!(out, "{}", render_mounts(method, &answer))?;
    Ok(())
}

/// What the mount verbs print: one fact per line, as the other verbs do,
/// rather than the JSON the RPC answered with.
fn render_mounts(method: &str, answer: &serde_json::Value) -> String {
    let text = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let list = |v: &serde_json::Value, k: &str| -> Vec<String> {
        v.get(k)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut s = String::new();
    match method {
        "db.mount" => {
            s.push_str(&format!(
                "mounted '{}' from {}\n",
                text(answer, "name"),
                text(answer, "path")
            ));
            for p in list(answer, "planes") {
                s.push_str(&format!("  serves  {p}\n"));
            }
            let held = list(answer, "skipped");
            if !held.is_empty() {
                s.push_str(&format!("  held, not served: {}\n", held.join(", ")));
            }
        }
        "db.unmount" => match answer.get("unmounted").and_then(serde_json::Value::as_bool) {
            Some(true) => {
                s.push_str(&format!(
                    "unmounted '{}' at {}\n",
                    text(answer, "name"),
                    text(answer, "path")
                ));
                // Said only when it is not: an operator unmounting in order to
                // open the store elsewhere would otherwise meet its lock.
                if answer.get("freed").and_then(serde_json::Value::as_bool) == Some(false) {
                    s.push_str(
                        "  still locked: a fold is running against it; \
                         the lock frees when it finishes\n",
                    );
                }
            }
            _ => s.push_str(&format!("nothing mounted as '{}'\n", text(answer, "name"))),
        },
        _ => {
            let mounts = answer
                .get("mounts")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            match mounts.is_empty() {
                true => s.push_str("this server mounts nothing\n"),
                false => {
                    for m in &mounts {
                        s.push_str(&format!(
                            "{}  {}  ({} plane(s))\n",
                            text(m, "name"),
                            text(m, "path"),
                            list(m, "planes").len()
                        ));
                    }
                }
            }
            for p in answer
                .get("planes")
                .and_then(serde_json::Value::as_array)
                .unwrap_or(&Vec::new())
            {
                let root = text(p, "synced_root");
                s.push_str(&format!(
                    "  {}{}{}\n",
                    text(p, "plane"),
                    match p.get("mount").and_then(serde_json::Value::as_str) {
                        Some(m) => format!("  in {m}"),
                        None => "  here".into(),
                    },
                    match root.is_empty() {
                        true => String::new(),
                        false => format!("  {root}"),
                    }
                ));
            }
            if let Some(root) = answer.get("mount_root").and_then(serde_json::Value::as_str) {
                s.push_str(&format!("mounts may come from {root}\n"));
            }
        }
    }
    s
}

/// The verbs that record and read what a plane attaches, rather than reading
/// the graph itself.
fn run_attach_verbs(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    out: &mut dyn Write,
) -> Result<()> {
    use dr_strange_core::attach;
    let db = commands::open(db_path, config::retain_commits(cfg))?;
    let said = match cmd {
        Command::Attach {
            name,
            plane,
            url,
            as_plane,
            both,
        } => attach::attach(
            &db,
            &plane,
            attach::Attach {
                at: attach::Attachment::new(name, as_plane, url),
                both,
            },
        )?,
        Command::Detach { name, plane } => attach::detach(&db, &plane, &name)?,
        Command::Attachments { plane, probe } => {
            let reader = dr_strange_web::attach::Reader::from_env(config::network(cfg)?);
            attach::attachments(&db, &plane, probe.then_some(&reader as &dyn attach::Remote))?
        }
        _ => unreachable!("every Command variant belongs to a dispatch group"),
    };
    write!(out, "{said}")?;
    Ok(())
}

/// The verbs that read the source tree a plane was parsed from, rather
/// than the graph alone. Called into the MCP surface rather than
/// reimplemented: two copies of a reader's verb would answer differently
/// the first time one of them was fixed.
fn run_tree_verbs(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Snippet {
            name,
            plane,
            lines,
            root,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // `--root` is only the fallback: a plane that records the tree it
            // was parsed from already knows better.
            let rendered = dr_strange_mcp::snippet_logic(
                &db,
                root.as_deref(),
                dr_strange_mcp::SnippetReq { plane, name, lines },
            )?;
            write!(out, "{}", as_text(&rendered))?;
            Ok(())
        }
        Command::Grep {
            pattern,
            plane,
            regex,
            ignore_case,
            path,
            context,
            max_results,
            all_files,
            root,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // The tree a plane was parsed from is the one to search; `--root`
            // overrides, and the cwd is the last resort.
            let root = match root {
                Some(r) => r,
                None => plane_root(&db, &plane).unwrap_or_else(|| PathBuf::from(".")),
            };
            let rendered = dr_strange_mcp::grep_logic(
                &db,
                &root,
                dr_strange_mcp::GrepReq {
                    pattern,
                    regex: Some(regex),
                    ignore_case: Some(ignore_case),
                    path,
                    context,
                    max_results,
                    all_files: Some(all_files),
                    plane: Some(plane),
                },
            )?;
            write!(out, "{}", as_text(&rendered))?;
            Ok(())
        }
        Command::Traverse {
            from,
            plane,
            direction,
            edge_type,
            min,
            max,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let plane_name = plane.clone();
            let rendered = dr_strange_mcp::traverse_logic(
                &db,
                dr_strange_mcp::Traverse {
                    plane,
                    from_id: None,
                    from_key: Some(from),
                    direction,
                    edge_type,
                    min,
                    max,
                },
            )?;
            // The walk answers with node ids, which is the right primitive
            // for a program and nothing at all for a reader. Rendered as the
            // nodes they are, the way `cypher` renders what it returns.
            let plane = db.plane(&plane_name)?;
            let mut rows = Vec::new();
            for id in rendered.as_array().into_iter().flatten() {
                let Some(id) = id.as_u64() else { continue };
                if let Some(node) = plane.node(dr_strange_core::NodeId(id))? {
                    rows.push((node, None));
                }
            }
            write!(out, "{}", dr_strange_core::compact::records(&plane, &rows)?)?;
            Ok(())
        }
        Command::Fathom { name, plane, depth } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let p = db.plane(&plane)?;
            write!(
                out,
                "{}",
                dr_strange_core::compact::fathom(&p, &name, depth)?
            )?;
            Ok(())
        }
        Command::History(args) => run_history(args, db_path, cfg, out),
        Command::Deployed(args) => run_deployed(args, db_path, cfg, out),
        Command::Recall(args) => run_recall(args, db_path, cfg, out),
        Command::Affected(args) => run_affected(args, db_path, cfg, out),
        other => run_analytics(other, db_path, cfg, verbose, out),
    }
}

/// The soft schema, the graph algorithms, and the retrieval surface.
fn run_analytics(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Catalog { plane } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::catalog(&db, plane.as_deref(), out)
        }
        Command::Algo(cmd) => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            match cmd {
                AlgoCmd::Pagerank {
                    plane,
                    label,
                    top,
                    damping,
                    max_iters,
                } => commands::algo_pagerank(
                    &db,
                    &plane,
                    label.as_deref(),
                    top,
                    damping,
                    max_iters,
                    out,
                ),
                AlgoCmd::Components { plane, label, top } => {
                    commands::algo_components(&db, &plane, label.as_deref(), top, out)
                }
                AlgoCmd::ShortestPath {
                    src,
                    dst,
                    plane,
                    label,
                    dir,
                    weight,
                } => commands::algo_shortest_path(
                    &db,
                    &plane,
                    label.as_deref(),
                    src,
                    dst,
                    dir.into(),
                    weight,
                    out,
                ),
                AlgoCmd::Louvain { plane, label, top } => {
                    commands::algo_louvain(&db, &plane, label.as_deref(), top, out)
                }
                AlgoCmd::Toposort {
                    plane,
                    label,
                    edge_type,
                    dir,
                    top,
                } => commands::algo_toposort(
                    &db,
                    &commands::ToposortArgs {
                        plane: &plane,
                        label: label.as_deref(),
                        edge_type: edge_type.as_deref(),
                        dir: dir.into(),
                        top,
                    },
                    out,
                ),
                AlgoCmd::Cycles {
                    plane,
                    label,
                    edge_type,
                    top,
                } => commands::algo_cycles(
                    &db,
                    &plane,
                    label.as_deref(),
                    edge_type.as_deref(),
                    top,
                    out,
                ),
            }
        }
        other => run_retrieval(other, db_path, cfg, verbose, out),
    }
}

/// Fused retrieval and the indexes that make it possible.
fn run_retrieval(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Hybrid {
            query,
            plane,
            label,
            vector,
            keyword,
            metric,
            graph_hops,
            graph_decay,
            k,
            embed,
            embed_model,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::hybrid(
                &db,
                &plane,
                &query,
                label.as_deref(),
                vector.as_deref(),
                keyword.as_deref(),
                metric.into(),
                graph_hops.map(|h| (h, graph_decay)),
                k,
                &embed,
                embed_model.as_deref(),
                out,
            )
        }
        Command::Index(IndexCmd::Ensure {
            first,
            second,
            plane,
            metric,
        }) => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            match second {
                Some(property) => {
                    commands::index_ensure(&db, &plane, &first, &property, metric.into(), out)
                }
                None => commands::index_ensure_all(&db, &plane, &first, metric.into(), out),
            }
        }
        Command::Index(IndexCmd::Keyword {
            first,
            second,
            plane,
            lang,
        }) => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let language = lang.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            match second {
                Some(property) => {
                    commands::keyword_index_ensure(&db, &plane, &first, &property, language, out)
                }
                None => commands::keyword_index_ensure_all(&db, &plane, &first, language, out),
            }
        }
        other => run_maintenance(other, db_path, cfg, verbose, out),
    }
}

/// Counting, checking, backing up and updating — the operator's verbs.
fn run_maintenance(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Stats => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::stats(&db, out)
        }
        Command::Check => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::check(&db, out)
        }
        Command::Upgrade => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::upgrade(&db, out)
        }
        Command::Snapshot { out: path } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::snapshot(&db, &path, out)
        }
        Command::Restore { input } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::restore(&db, &input, out)
        }
        other => run_services(other, db_path, cfg, verbose, out),
    }
}

/// The long-running and model-backed commands: the server, embedding,
/// natural language, plugins, and ingestion.
fn run_services(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        Command::Serve {
            addr,
            mode,
            follow,
            follow_token,
        } => {
            if follow.is_some() && mode.is_some() {
                bail!("--follow and a serve subcommand (e.g. `watch`) are mutually exclusive");
            }
            // The web crate refuses this bind too, but only once it runs —
            // after a follower has wiped its directory or a watch has begun.
            // `apply_env` has already folded `[server] token` into the
            // environment, so the variable is the one reading.
            let token_configured = std::env::var("DRSG_TOKEN").is_ok_and(|t| !t.is_empty());
            config::check_serve_bind(cfg, addr, token_configured)?;
            // Said once at startup rather than leaving an operator to wonder
            // why one fetch is proxied and another is not. A proxy carries the
            // requests this binary makes on their behalf; a URL a caller hands
            // the API is address-guarded instead, and through a proxy that
            // guard would have no address to judge (issue #37).
            if !config::network(cfg)?.is_direct() {
                tracing::info!(
                    "a proxy is configured: plugin, update and provider requests use it, \
                     while URL fetches asked for through the API go direct under the \
                     address guard"
                );
            }
            if let Some(upstream) = follow {
                let follow_opts = dr_strange_web::FollowOptions {
                    upstream,
                    token: follow_token.or_else(|| std::env::var("DRSG_FOLLOW_TOKEN").ok()),
                    net: config::network(cfg)?,
                };
                // Every (re)connect is a full resync from scratch (arch/01
                // §9): each loop iteration wipes `db_path` and reopens a fresh,
                // empty, read-only engine before `serve` bootstraps it from
                // the master's `/snapshot`.
                loop {
                    commands::prepare_follower_dir(db_path)?;
                    let db = dr_strange_core::Database::open_read_only(db_path)
                        .with_context(|| format!("opening replica at {}", db_path.display()))?;
                    let mut opts = config::serve_options(cfg, addr);
                    opts.follow = Some(follow_opts.clone());
                    opts.recall_parsers = Some(recall_parsers(cfg)?);
                    match dr_strange_web::serve(db, Some(db_path.to_path_buf()), opts)? {
                        dr_strange_web::ServeOutcome::Stopped => break Ok(()),
                        dr_strange_web::ServeOutcome::ResyncNeeded => {
                            tracing::warn!(
                                "resyncing from master after losing the replication stream"
                            );
                            std::thread::sleep(std::time::Duration::from_secs(2));
                        }
                    }
                }
            } else {
                let db = commands::open(db_path, config::retain_commits(cfg))?;
                #[allow(unused_mut)]
                let mut opts = config::serve_options(cfg, addr);
                opts.recall_parsers = Some(recall_parsers(cfg)?);
                #[cfg(feature = "digest")]
                if let Some(ServeMode::Watch {
                    dir,
                    plane,
                    force,
                    no_git,
                    ignore_files,
                    tracked_only,
                    code_only,
                }) = mode
                {
                    let plane = plane
                        .unwrap_or_else(|| commands::default_plane(&dir.display().to_string()));
                    let plugin_config = config::plugin_config(cfg)?;
                    // The same embed config the server's search uses keeps the
                    // watched plane's vectors current after each fold.
                    let embed = opts.embed_provider.clone();
                    // The watched tree is the graph's source — attach it, so the
                    // MCP `grep` tool answers literal-text questions beside the
                    // graph's structural ones.
                    opts.source_root = Some(dir.clone());
                    let tree = commands::WatchTree {
                        dir: dir.clone(),
                        code_only: config::code_only(cfg, code_only),
                        policy: config::ignore_policy(cfg, ignore_files.as_deref(), tracked_only)?,
                    };
                    // Every mounted repository gets a watcher of its own, on
                    // the tree its own plane records (issue #44) — which is
                    // what lets one `serve watch` replace one process per
                    // repository. The host's `--dir` keeps `on_start`.
                    let mounted = WatchMounts {
                        code_only: config::code_only(cfg, code_only),
                        policy: config::ignore_policy(cfg, ignore_files.as_deref(), tracked_only)?,
                        plugin_config: config::plugin_config(cfg)?,
                        embed: opts.embed_provider.clone(),
                        git: !no_git,
                        readable: [Some(dir.clone()), opts.mount_root.clone()]
                            .into_iter()
                            .flatten()
                            .collect(),
                    };
                    opts.on_mount = Some(std::sync::Arc::new(move |m| mounted.watch(m)));
                    opts.on_start = Some(Box::new(move |db| {
                        commands::watch(
                            commands::WatchedDb::Held(db),
                            tree,
                            plane,
                            plugin_config,
                            embed,
                            force,
                            !no_git,
                        )
                    }));
                }
                #[cfg(not(feature = "digest"))]
                let _ = mode;
                // Hands off to the web crate, which owns its own async runtime
                // and blocks until a shutdown signal; `out` is unused (the
                // server logs itself). Never `--follow`, so always `Stopped`.
                dr_strange_web::serve(db, Some(db_path.to_path_buf()), opts).map(|_| ())
            }
        }
        other => run_model_backed(other, db_path, cfg, verbose, out),
    }
}

/// The model-backed commands: embedding a plane, and asking in words.
fn run_model_backed(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        #[cfg(feature = "digest")]
        Command::Vectorize {
            plane,
            embed,
            embed_model,
            metric,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            let embedder =
                dr_strange_llm::build_provider(&embed, embed_model.as_deref(), None, None, true)?;
            commands::vectorize(&db, &plane, &embedder, metric.into(), out)
        }
        #[cfg(feature = "digest")]
        Command::Ask {
            question,
            plane,
            dry_run,
            max_attempts,
            limit,
            chat,
            model,
            embed,
            embed_model,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            commands::ask(
                &db,
                &plane,
                &question,
                dry_run,
                max_attempts,
                limit,
                &chat,
                model.as_deref(),
                embed.as_deref(),
                embed_model.as_deref(),
                out,
            )
        }
        Command::Update {
            bin,
            dir,
            insecure_skip_checksum,
        } => {
            let allow: Vec<dr_strange_web::fetch::Prefix> = cfg
                .fetch
                .allow_private
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|s| dr_strange_web::fetch::Prefix::parse(s))
                .collect::<Result<_>>()?;
            let net = config::network(cfg)?;
            update::update(
                dr_strange_web::fetch::Route {
                    net: &net,
                    allow: &allow,
                },
                bin.as_deref(),
                dir.as_deref(),
                insecure_skip_checksum || update::skip_checksum_from_env(),
                out,
            )
        }
        other => run_plugins_and_ingest(other, db_path, cfg, verbose, out),
    }
}

/// Preprocessor plugins, and the ingestion that runs them.
// Every arm here is a `digest` command, so without that feature the
// group is empty and only the chain's own contract is left.
#[cfg_attr(not(feature = "digest"), allow(unused_variables))]
fn run_plugins_and_ingest(
    cmd: Command,
    db_path: &Path,
    cfg: &config::Config,
    verbose: commands::Verbosity,
    out: &mut dyn Write,
) -> Result<()> {
    match cmd {
        #[cfg(feature = "digest")]
        Command::Plugin(cmd) => {
            let plugin_config = config::plugin_config(cfg)?;
            let allow: Vec<dr_strange_web::fetch::Prefix> = cfg
                .fetch
                .allow_private
                .clone()
                .unwrap_or_default()
                .iter()
                .map(|s| dr_strange_web::fetch::Prefix::parse(s))
                .collect::<Result<_>>()?;
            let net = config::network(cfg)?;
            let route = dr_strange_web::fetch::Route {
                net: &net,
                allow: &allow,
            };
            match cmd {
                PluginCmd::Install { source } => {
                    commands::plugin_install(&plugin_config, route, source.as_deref(), out)
                }
                PluginCmd::List { available, json } => {
                    commands::plugin_list(&plugin_config, route, available, json, out)
                }
                PluginCmd::Remove { name } => commands::plugin_remove(&plugin_config, &name, out),
            }
        }
        #[cfg(feature = "digest")]
        Command::Digest {
            source,
            plane,
            apply,
            chat,
            embed,
            model,
            embed_model,
            chat_url,
            embed_url,
            chat_key_env,
            embed_key_env,
            chunk_chars,
            concurrency,
            no_embed,
            no_link,
            mode,
            topic,
            pages,
            depth,
            handler,
            plugin_source,
            no_git,
            git_plane,
            ignore_files,
            tracked_only,
            code_only,
        } => {
            let db = commands::open(db_path, config::retain_commits(cfg))?;
            // The `[plugins]` section, with the legacy flag folded in on top.
            let mut plugin_config = config::plugin_config(cfg)?;
            if plugin_source {
                plugin_config
                    .options
                    .entry("rust".to_string())
                    .or_default()
                    .push(("include_source".to_string(), "true".to_string()));
            }
            let net = config::network(cfg)?;
            let ignore = config::ignore_policy(cfg, ignore_files.as_deref(), tracked_only)?;
            let args = commands::DigestArgs {
                source: &source,
                topic: topic.as_deref(),
                pages,
                depth,
                ignore: &ignore,
                verbose,
                code_only: config::code_only(cfg, code_only),
                net: &net,
                plane: &plane.unwrap_or_else(|| commands::default_plane(&source)),
                apply,
                chunk_chars,
                concurrency,
                embed: !no_embed,
                link: !no_link,
                mode: &mode,
                chat_provider: &chat,
                embed_provider: &embed,
                model: model.as_deref(),
                embed_model: embed_model.as_deref(),
                chat_url: chat_url.as_deref(),
                embed_url: embed_url.as_deref(),
                chat_key_env: chat_key_env.as_deref(),
                embed_key_env: embed_key_env.as_deref(),
                handler: handler.as_deref(),
                plugin_config,
                git: !no_git,
                git_plane: git_plane.as_deref(),
            };
            commands::digest(&db, &args, out)
        }
        // The chain above is exhaustive over `Command`; anything reaching
        // here was handled by an earlier group and cannot arrive twice.
        _ => unreachable!("every Command variant belongs to a dispatch group"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database path nothing else in this run will use. Built from the pid
    /// and the clock, as `commands`' own tests do, because the CLI has no
    /// `tempfile` dependency and a test is not a reason to add one.
    fn unique_db(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("drsg-run-{name}-{}-{nanos}", std::process::id()))
    }

    /// The database and the sidecars that sit *beside* it, not inside.
    fn remove_db(db: &Path) {
        let _ = std::fs::remove_dir_all(db);
        let _ = std::fs::remove_file(db);
        for ext in ["hnsw", "bm25"] {
            let _ = std::fs::remove_file(format!("{}.{ext}", db.display()));
        }
    }

    /// Dispatch one invocation against `db` and hand back what it printed.
    ///
    /// Through `run`, not the group functions, because the routing is the part
    /// under test: every group hands what it does not recognise to the next,
    /// and a verb landing in the wrong group is the failure this catches.
    fn drsg(db: &Path, args: &[&str]) -> Result<String> {
        let argv: Vec<String> = ["drsg", "--db", &db.display().to_string()]
            .iter()
            .map(|s| s.to_string())
            .chain(args.iter().map(|s| s.to_string()))
            .collect();
        let cli = Cli::try_parse_from(&argv).expect("the test's own invocation parses");
        let mut out: Vec<u8> = Vec::new();
        run(cli, &config::Config::default(), &mut out)?;
        Ok(String::from_utf8(out).expect("verbs print UTF-8"))
    }

    /// A session over one database: the read verbs, in the order a reader
    /// would reach for them, each against what the one before it wrote.
    ///
    /// One test rather than a dozen because they share a database, and setting
    /// one up per verb would be the slow half of the work.
    #[test]
    fn the_read_verbs_dispatch_and_answer_over_a_real_database() {
        let db = unique_db("read");
        let jsonl = unique_db("read-in").with_extension("jsonl");
        std::fs::write(
            &jsonl,
            "{\"id\":1,\"external_key\":\"m::caller\",\"labels\":[\"Fn\"],\
             \"properties\":{\"name\":\"caller\"}}\n\
             {\"id\":2,\"external_key\":\"m::callee\",\"labels\":[\"Fn\"],\
             \"properties\":{\"name\":\"callee\"}}\n\
             {\"id\":1,\"src\":1,\"dst\":2,\"type\":\"CALLS\",\"properties\":{\"_confidence\":0.8}}\n",
        )
        .unwrap();

        let got = drsg(&db, &["import", &jsonl.display().to_string()]).expect("import");
        assert!(got.contains('2') && got.contains("node"), "{got}");

        // `get` by id, and the node carries what was imported.
        let got = drsg(&db, &["get", "1"]).expect("get");
        assert!(got.contains("caller"), "{got}");

        // Cypher, including the relationship variable §15 added — the whole
        // chain from this binary's argument down to the edge map.
        let got = drsg(
            &db,
            &[
                "cypher",
                "MATCH (a:Fn)-[r:CALLS]->(b:Fn) RETURN b.name, r._confidence",
            ],
        )
        .expect("cypher");
        assert!(got.contains("callee") && got.contains("0.8"), "{got}");

        // The catalog observes what the import declared.
        let got = drsg(&db, &["catalog"]).expect("catalog");
        assert!(got.contains("Fn"), "{got}");

        // Counts, integrity, and the plane listing all answer.
        assert!(drsg(&db, &["stats"]).expect("stats").contains('2'));
        assert!(!drsg(&db, &["check"]).expect("check").is_empty());
        assert!(
            drsg(&db, &["plane", "list"])
                .expect("plane list")
                .contains("startup")
        );

        // A traversal from a node. `traverse` addresses it by external key —
        // the stable identifier, not the id a reimport may renumber.
        let got = drsg(&db, &["traverse", "m::caller"]).expect("traverse");
        assert!(got.contains("callee") || got.contains("m::callee"), "{got}");
        let got = drsg(&db, &["algo", "pagerank"]).expect("pagerank");
        assert!(!got.is_empty(), "pagerank printed nothing");

        // And the queries this session ran are recorded.
        let got = drsg(&db, &["queries"]).expect("queries");
        assert!(got.contains("MATCH") || got.contains("startup"), "{got}");

        // Export round-trips what import read.
        let got = drsg(&db, &["export"]).expect("export");
        assert!(got.contains("caller") && got.contains("CALLS"), "{got}");

        remove_db(&db);
        let _ = std::fs::remove_file(&jsonl);
    }

    /// What a verb says when it cannot do the thing. These are the paths a
    /// reader actually meets, and an unclear one costs more than a missing
    /// feature: the message has to name what was wrong, not merely fail.
    #[test]
    fn a_verb_that_cannot_answer_says_why() {
        let db = unique_db("errs");
        drsg(&db, &["stats"]).expect("an empty database still answers");

        // A node that is not there.
        let said = format!(
            "{:#}",
            drsg(&db, &["get", "9999"]).expect_err("no such node")
        );
        assert!(
            said.contains("9999") || said.to_lowercase().contains("not found"),
            "{said}"
        );

        // A query the subset refuses: the error names the shape, which is the
        // whole point of refusing rather than misreading it.
        let said = format!(
            "{:#}",
            drsg(&db, &["cypher", "MATCH (a)-->(b), (a)-->(c) RETURN a"])
                .expect_err("several paths are refused")
        );
        assert!(said.to_lowercase().contains("path"), "{said}");

        // And a plan that is not a plan.
        let said = format!(
            "{:#}",
            drsg(&db, &["query", "{not json"]).expect_err("a malformed plan is refused")
        );
        assert!(!said.is_empty(), "the parse failure should say something");

        remove_db(&db);
    }

    /// The zero-argument invocations are the contract: `drsg digest` alone
    /// must mean "this repository, into graph.drsg's startup plane".
    #[cfg(feature = "digest")]
    #[test]
    fn bare_digest_means_cwd_into_a_plane_named_after_it() {
        let cli = Cli::try_parse_from(["drsg", "digest"]).unwrap();
        assert_eq!(cli.db, PathBuf::from("graph.drsg"));
        match cli.command {
            Command::Digest {
                source,
                plane,
                apply,
                ..
            } => {
                assert_eq!(source, ".");
                // Resolved at dispatch: the source directory's own name.
                assert_eq!(plane, None);
                assert_eq!(commands::default_plane(&source), "dr-strange-cli");
                assert!(!apply, "a bare digest must stay a dry run");
            }
            _ => panic!("digest should parse as Digest"),
        }
    }

    /// `drsg serve watch` with nothing else watches $CWD into a plane named
    /// after it, beside the normally-served dashboard.
    #[cfg(feature = "digest")]
    #[test]
    fn bare_serve_watch_takes_cwd_and_no_explicit_plane() {
        let cli = Cli::try_parse_from(["drsg", "serve", "watch"]).unwrap();
        match cli.command {
            Command::Serve {
                mode: Some(ServeMode::Watch { dir, plane, .. }),
                ..
            } => {
                assert_eq!(dir, PathBuf::from("."));
                assert_eq!(plane, None);
                let forced = Cli::try_parse_from(["drsg", "serve", "watch", "--force"]).unwrap();
                assert!(matches!(
                    forced.command,
                    Command::Serve {
                        mode: Some(ServeMode::Watch { force: true, .. }),
                        ..
                    }
                ));
            }
            _ => panic!("should parse as Serve(Watch)"),
        }
    }

    /// `recall` spells the tool's request as flags; the answering half is the
    /// MCP crate's, and is tested there.
    #[test]
    fn recall_parses_into_the_request_its_tool_takes() {
        let read = Cli::try_parse_from([
            "drsg",
            "recall",
            "src/lib.rs:3-9",
            "--at",
            "v1",
            "--plane",
            "p",
        ])
        .unwrap();
        let Command::Recall(args) = read.command else {
            panic!("should parse as Recall")
        };
        let (req, root) = args.into_req();
        assert_eq!(req.name.as_deref(), Some("src/lib.rs:3-9"));
        assert_eq!((req.at.as_str(), req.plane.as_str()), ("v1", "p"));
        assert_eq!((req.vs, req.pattern, root), (None, None, None));
        assert_eq!(
            (req.regex, req.ignore_case),
            (None, None),
            "an unset flag stays unset, so reading a name is not refused"
        );

        let search = Cli::try_parse_from([
            "drsg",
            "recall",
            "--pattern",
            "fn (a|b)",
            "--at",
            "HEAD~2",
            "--regex",
            "--path",
            ".rs",
        ])
        .unwrap();
        let Command::Recall(args) = search.command else {
            panic!("should parse as Recall")
        };
        let (req, _) = args.into_req();
        assert_eq!(req.name, None);
        assert_eq!(req.pattern.as_deref(), Some("fn (a|b)"));
        assert_eq!((req.regex, req.ignore_case), (Some(true), None));
        assert_eq!(req.path.as_deref(), Some(".rs"));

        let diff =
            Cli::try_parse_from(["drsg", "recall", "k::go", "--at", "main", "--vs", "v2.0.0"])
                .unwrap();
        let Command::Recall(args) = diff.command else {
            panic!("should parse as Recall")
        };
        assert_eq!(args.into_req().0.vs.as_deref(), Some("v2.0.0"));
        assert!(
            Cli::try_parse_from(["drsg", "recall", "src/lib.rs"]).is_err(),
            "`--at` is required"
        );
    }

    /// The two flags a CI template drives `affected` with, and the one
    /// combination that has no meaning: a stream of keys cannot also be a
    /// Markdown report, and clap should say so rather than pick one.
    #[test]
    fn a_report_and_a_seed_list_are_each_asked_for_once() {
        let plain = Cli::try_parse_from(["drsg", "affected"]).unwrap();
        let Command::Affected(args) = plain.command else {
            panic!("should parse as Affected")
        };
        assert_eq!(args.into_req().1, dr_strange_mcp::AffectedOutput::Compact);

        for (argv, want) in [
            (
                ["drsg", "affected", "--report"],
                dr_strange_mcp::AffectedOutput::Report,
            ),
            (
                ["drsg", "affected", "--seeds"],
                dr_strange_mcp::AffectedOutput::Seeds,
            ),
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            let Command::Affected(args) = cli.command else {
                panic!("should parse as Affected")
            };
            assert_eq!(args.into_req().1, want, "{argv:?}");
        }

        assert!(
            Cli::try_parse_from(["drsg", "affected", "--report", "--seeds"]).is_err(),
            "a report and a seed list are different answers"
        );
    }

    /// `--report` is the pull-request rendering of the two verbs a template
    /// asks about each changed declaration.
    #[test]
    fn impact_and_wire_each_take_a_report_flag() {
        let radius = Cli::try_parse_from(["drsg", "impact", "k::go", "--report"]).unwrap();
        assert!(
            matches!(radius.command, Command::Impact { report: true, .. }),
            "impact takes --report"
        );
        let routes = Cli::try_parse_from(["drsg", "wire", "/api", "--report"]).unwrap();
        assert!(
            matches!(routes.command, Command::Wire { report: true, .. }),
            "wire takes --report"
        );
        let compact = Cli::try_parse_from(["drsg", "impact", "k::go"]).unwrap();
        assert!(
            matches!(compact.command, Command::Impact { report: false, .. }),
            "and neither reports unasked"
        );
    }

    /// The reader's verbs the MCP surface had and the CLI did not. Parsing is
    /// the CLI's own half of them — the answering half is shared with MCP, so
    /// it is tested there.
    #[test]
    fn the_readers_verbs_parse_the_way_their_tools_do() {
        let cli = Cli::try_parse_from(["drsg", "snippet", "k::go", "--plane", "p"]).unwrap();
        match cli.command {
            Command::Snippet {
                name,
                plane,
                lines,
                root,
            } => {
                assert_eq!(name, "k::go");
                assert_eq!(plane, "p");
                // Unset: the declaration's own extent, not a fixed guess.
                assert_eq!(lines, None);
                assert_eq!(root, None);
            }
            _ => panic!("should parse as Snippet"),
        }

        // A file range is the same argument, as it is in the tool.
        let range = Cli::try_parse_from(["drsg", "snippet", "src/lib.rs:3-9"]).unwrap();
        assert!(
            matches!(range.command, Command::Snippet { ref name, .. } if name == "src/lib.rs:3-9")
        );

        let g = Cli::try_parse_from([
            "drsg",
            "grep",
            "fn run",
            "--regex",
            "--ignore-case",
            "--path",
            ".rs",
            "--context",
            "2",
        ])
        .unwrap();
        match g.command {
            Command::Grep {
                pattern,
                regex,
                ignore_case,
                path,
                context,
                max_results,
                ..
            } => {
                assert_eq!(pattern, "fn run");
                assert!(regex && ignore_case);
                assert_eq!(path.as_deref(), Some(".rs"));
                assert_eq!(context, Some(2));
                assert_eq!(max_results, None);
            }
            _ => panic!("should parse as Grep"),
        }
        // The flags are opt-in: a plain grep is a literal, case-sensitive one.
        let plain = Cli::try_parse_from(["drsg", "grep", "TODO"]).unwrap();
        assert!(matches!(
            plain.command,
            Command::Grep {
                regex: false,
                ignore_case: false,
                ..
            }
        ));

        let tr = Cli::try_parse_from([
            "drsg",
            "traverse",
            "k::go",
            "--edge-type",
            "CALLS",
            "--max",
            "3",
        ])
        .unwrap();
        match tr.command {
            Command::Traverse {
                from,
                edge_type,
                min,
                max,
                direction,
                ..
            } => {
                assert_eq!(from, "k::go");
                assert_eq!(edge_type.as_deref(), Some("CALLS"));
                assert_eq!(max, Some(3));
                // Defaulted by the shared logic, not by the parser.
                assert_eq!(min, None);
                assert_eq!(direction, None);
            }
            _ => panic!("should parse as Traverse"),
        }
    }

    /// One positional = property, sweeping all labels; two = label, property.
    #[test]
    fn bare_index_ensure_defaults_to_the_embedding_property() {
        let none = Cli::try_parse_from(["drsg", "index", "ensure"]).unwrap();
        match none.command {
            Command::Index(IndexCmd::Ensure { first, second, .. }) => {
                assert_eq!(first, "embedding");
                assert_eq!(second, None);
            }
            _ => panic!("should parse as Index(Ensure)"),
        }
    }

    #[test]
    fn index_ensure_takes_one_or_two_positionals() {
        let one = Cli::try_parse_from(["drsg", "index", "ensure", "embedding"]).unwrap();
        match one.command {
            Command::Index(IndexCmd::Ensure { first, second, .. }) => {
                assert_eq!(first, "embedding");
                assert_eq!(second, None);
            }
            _ => panic!("should parse as Index(Ensure)"),
        }
        let two =
            Cli::try_parse_from(["drsg", "index", "ensure", "Function", "embedding"]).unwrap();
        match two.command {
            Command::Index(IndexCmd::Ensure { first, second, .. }) => {
                assert_eq!(first, "Function");
                assert_eq!(second.as_deref(), Some("embedding"));
            }
            _ => panic!("should parse as Index(Ensure)"),
        }
    }

    /// Sources that don't name a directory keep the `startup` fallback.
    #[cfg(feature = "digest")]
    #[test]
    fn non_directory_sources_default_to_startup() {
        assert_eq!(commands::default_plane("https://example.com/x"), "startup");
        assert_eq!(commands::default_plane("Cargo.toml"), "startup");
        assert_eq!(commands::default_plane("/"), "startup");
    }

    /// `drsg plugin install` with nothing after it must parse — the missing
    /// source is what routes to the interactive chooser.
    #[cfg(feature = "digest")]
    #[test]
    fn bare_plugin_install_parses_without_a_source() {
        let cli = Cli::try_parse_from(["drsg", "plugin", "install"]).unwrap();
        match cli.command {
            Command::Plugin(PluginCmd::Install { source }) => assert!(source.is_none()),
            _ => panic!("should parse as Plugin(Install)"),
        }
    }

    /// clap's own audit of the whole surface: duplicate long or short flags,
    /// a `default_value` that its own type rejects, a positional after an
    /// optional one. Thirty-odd variants is past the size where reading the
    /// definition catches these, and the panic names the offender.
    #[test]
    fn the_command_surface_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// Every verb parses from the shortest invocation a reader would type, and
    /// lands on the variant that dispatch expects.
    ///
    /// The point is the *mapping*, not each field: `run` routes on the variant,
    /// so a verb that parses into the wrong one is a verb that silently does
    /// something else. Fields with defaults worth defending get their own
    /// assertions below.
    #[test]
    fn every_verb_parses_into_its_own_variant() {
        /// An invocation and the variant it must land on.
        type Case = (Vec<&'static str>, fn(&Command) -> bool);
        let cases: Vec<Case> = vec![
            #[cfg(feature = "digest")]
            (vec!["token"], |c| matches!(c, Command::Token { .. })),
            (vec!["import", "in.jsonl"], |c| {
                matches!(c, Command::Import { .. })
            }),
            (vec!["export"], |c| matches!(c, Command::Export { .. })),
            (vec!["get", "7"], |c| matches!(c, Command::Get { .. })),
            (vec!["query", "{}"], |c| matches!(c, Command::Query { .. })),
            (vec!["cypher", "MATCH (n) RETURN n"], |c| {
                matches!(c, Command::Cypher { .. })
            }),
            (vec!["queries"], |c| matches!(c, Command::Queries { .. })),
            (vec!["attach", "other"], |c| {
                matches!(c, Command::Attach { .. })
            }),
            (vec!["mount", "/srv/other.drsg"], |c| {
                matches!(c, Command::Mount { .. })
            }),
            (vec!["unmount", "other"], |c| {
                matches!(c, Command::Unmount { .. })
            }),
            (vec!["mounts"], |c| matches!(c, Command::Mounts { .. })),
            (vec!["detach", "other"], |c| {
                matches!(c, Command::Detach { .. })
            }),
            (vec!["attachments"], |c| {
                matches!(c, Command::Attachments { .. })
            }),
            (vec!["wire", "/v1/health"], |c| {
                matches!(c, Command::Wire { .. })
            }),
            (vec!["traverse", "7"], |c| {
                matches!(c, Command::Traverse { .. })
            }),
            (vec!["fathom", "run"], |c| {
                matches!(c, Command::Fathom { .. })
            }),
            (vec!["history"], |c| matches!(c, Command::History { .. })),
            (vec!["catalog"], |c| matches!(c, Command::Catalog { .. })),
            (vec!["stats"], |c| matches!(c, Command::Stats)),
            (vec!["check"], |c| matches!(c, Command::Check)),
            (vec!["upgrade"], |c| matches!(c, Command::Upgrade)),
            (vec!["snapshot", "out.drsg"], |c| {
                matches!(c, Command::Snapshot { .. })
            }),
            (vec!["restore", "in.drsg"], |c| {
                matches!(c, Command::Restore { .. })
            }),
            (vec!["serve"], |c| matches!(c, Command::Serve { .. })),
            (vec!["update"], |c| matches!(c, Command::Update { .. })),
            (vec!["plane", "list"], |c| matches!(c, Command::Plane(_))),
            (vec!["algo", "pagerank"], |c| matches!(c, Command::Algo(_))),
            (vec!["index", "keyword", "body"], |c| {
                matches!(c, Command::Index(_))
            }),
            (vec!["hybrid", "ferns"], |c| {
                matches!(c, Command::Hybrid { .. })
            }),
        ];
        for (args, want) in cases {
            let argv: Vec<&str> = std::iter::once("drsg")
                .chain(args.iter().copied())
                .collect();
            let cli = Cli::try_parse_from(&argv)
                .unwrap_or_else(|e| panic!("`drsg {}` should parse: {e}", args.join(" ")));
            assert!(
                want(&cli.command),
                "`drsg {}` parsed into the wrong variant",
                args.join(" ")
            );
        }
    }

    /// The defaults a reader relies on without typing them. Each of these is a
    /// promise: `--db` naming the file in this directory, the empty `startup`
    /// plane, and a `serve` that listens on the configured address rather than
    /// a watcher.
    #[test]
    fn the_defaults_a_bare_invocation_promises() {
        let cli = Cli::try_parse_from(["drsg", "stats"]).unwrap();
        assert_eq!(cli.db, PathBuf::from("graph.drsg"));
        assert_eq!(cli.verbose, 0, "quiet unless asked");
        assert!(cli.config.is_none());

        // `-v` stacks: the count is what `Verbosity` reads.
        let loud = Cli::try_parse_from(["drsg", "-vv", "stats"]).unwrap();
        assert_eq!(loud.verbose, 2);

        // A plane-scoped read defaults to `startup`, which is empty on a
        // normal install — the fallback that makes the misses explainable.
        let cli = Cli::try_parse_from(["drsg", "export"]).unwrap();
        match cli.command {
            Command::Export { plane, .. } => assert_eq!(plane, "startup"),
            _ => panic!("should parse as Export"),
        }

        // `serve` with no mode is the server itself, not `serve watch`.
        let cli = Cli::try_parse_from(["drsg", "serve"]).unwrap();
        match cli.command {
            Command::Serve { mode: None, .. } => {}
            _ => panic!("bare `serve` must not imply a mode"),
        }
    }

    /// The argument enums exist to keep clap's vocabulary out of the core
    /// types; each spelling has to arrive as the value it names.
    #[test]
    fn the_value_enums_convert_to_the_core_types() {
        assert!(matches!(Dir::from(DirArg::Out), Dir::Out));
        assert!(matches!(Dir::from(DirArg::In), Dir::In));
        assert!(matches!(Dir::from(DirArg::Both), Dir::Both));
        assert!(matches!(Metric::from(MetricArg::Cosine), Metric::Cosine));
        assert!(matches!(Metric::from(MetricArg::Dot), Metric::Dot));
        assert!(matches!(Metric::from(MetricArg::L2), Metric::L2));

        // And they parse from the spellings a reader types. `toposort --dir in`
        // is the one that matters: `in` is what an edge meaning "depends on"
        // needs, and getting it backwards reverses the whole answer.
        let cli = Cli::try_parse_from(["drsg", "algo", "toposort", "--dir", "in"]).unwrap();
        match cli.command {
            Command::Algo(AlgoCmd::Toposort { dir, top, .. }) => {
                assert!(matches!(Dir::from(dir), Dir::In));
                assert_eq!(top, 50, "the printed-rows default");
            }
            _ => panic!("should parse as Algo(Toposort)"),
        }
        // Unset, it is `out`.
        let cli = Cli::try_parse_from(["drsg", "algo", "toposort"]).unwrap();
        match cli.command {
            Command::Algo(AlgoCmd::Toposort { dir, .. }) => {
                assert!(matches!(Dir::from(dir), Dir::Out))
            }
            _ => panic!("should parse as Algo(Toposort)"),
        }
    }

    /// A reader's verb answers with JSON; a string answer is its text, and
    /// anything else is printed as it stands rather than quoted.
    #[test]
    fn as_text_unwraps_a_string_answer_only() {
        assert_eq!(as_text(&serde_json::json!("fn run() {}")), "fn run() {}");
        assert_eq!(as_text(&serde_json::json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(as_text(&serde_json::json!(7)), "7");
        assert_eq!(as_text(&serde_json::Value::Null), "null");
    }

    /// The mount verbs print facts, one per line. These answers come off the
    /// wire, so every field is optional in practice and a missing one must not
    /// panic or print `null`.
    #[test]
    fn render_mounts_prints_one_fact_per_line() {
        let got = render_mounts(
            "db.mount",
            &serde_json::json!({
                "name": "other", "path": "/srv/other.drsg",
                "planes": ["other::code"], "skipped": ["other::ops"],
            }),
        );
        assert_eq!(
            got,
            "mounted 'other' from /srv/other.drsg\n  serves  other::code\n  \
             held, not served: other::ops\n"
        );

        // Nothing held: the line is absent rather than empty.
        let got = render_mounts(
            "db.mount",
            &serde_json::json!({"name": "o", "path": "/p", "planes": []}),
        );
        assert_eq!(got, "mounted 'o' from /p\n");

        // An unmount that could not free the store says so — an operator
        // unmounting in order to open it elsewhere would otherwise meet a lock
        // with no explanation.
        let got = render_mounts(
            "db.unmount",
            &serde_json::json!({"unmounted": true, "name": "o", "path": "/p", "freed": false}),
        );
        assert!(got.starts_with("unmounted 'o' at /p\n"), "{got}");
        assert!(got.contains("still locked"), "{got}");

        // Freed, or silent about it: no second line.
        let got = render_mounts(
            "db.unmount",
            &serde_json::json!({"unmounted": true, "name": "o", "path": "/p", "freed": true}),
        );
        assert_eq!(got, "unmounted 'o' at /p\n");

        // Absent is reported, not an error.
        let got = render_mounts(
            "db.unmount",
            &serde_json::json!({"unmounted": false, "name": "o"}),
        );
        assert_eq!(got, "nothing mounted as 'o'\n");

        // The listing: mounts, then every plane with where it lives.
        let got = render_mounts(
            "db.mounts",
            &serde_json::json!({
                "mounts": [{"name": "other", "path": "/srv/o.drsg", "planes": ["a", "b"]}],
                "planes": [
                    {"plane": "here", "synced_root": "/home/me/repo"},
                    {"plane": "a", "mount": "other"},
                ],
                "mount_root": "/home/me",
            }),
        );
        assert_eq!(
            got,
            "other  /srv/o.drsg  (2 plane(s))\n  here  here  /home/me/repo\n  \
             a  in other\nmounts may come from /home/me\n"
        );

        // An empty server says so rather than printing a blank line.
        let got = render_mounts("db.mounts", &serde_json::json!({"mounts": []}));
        assert_eq!(got, "this server mounts nothing\n");
        // And a malformed answer is still survivable.
        assert_eq!(
            render_mounts("db.mounts", &serde_json::json!({})),
            "this server mounts nothing\n"
        );
    }
}
