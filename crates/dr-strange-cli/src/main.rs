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

use std::io::{self, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new database.
    Init,
    /// Plane lifecycle.
    #[command(subcommand)]
    Plane(PlaneCmd),
    /// Import JSONL nodes/edges into a plane.
    Import {
        file: PathBuf,
        #[arg(long, default_value = "startup")]
        plane: String,
        /// What to do when a node's external key already exists in the plane:
        /// `error` (default, writes nothing), `skip`, or `update`.
        #[arg(long, value_enum, default_value = "error")]
        on_conflict: commands::OnConflict,
    },
    /// Export a plane as JSONL.
    Export {
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Fetch one node by id or `@external-key`.
    Get {
        node: String,
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Run a serialized query plan (JSON).
    Query {
        /// The plan as JSON, or `-` to read it from stdin.
        plan: String,
        #[arg(long, default_value = "startup")]
        plane: String,
    },
    /// Run a query in the openCypher-subset language (compiled to a plan).
    Cypher {
        /// The query text, or `-` to read it from stdin.
        query: String,
        #[arg(long, default_value = "startup")]
        plane: String,
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
    /// Print the soft-schema catalog (a plane's, or the whole database's).
    Catalog {
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
        #[arg(long, default_value = "startup")]
        plane: String,
        #[arg(long)]
        label: Option<String>,
        /// Embedding property to enable the vector channel (needs `digest`).
        #[arg(long)]
        vector: Option<String>,
        /// String property to enable the BM25 keyword channel.
        #[arg(long)]
        keyword: Option<String>,
        #[arg(long, value_enum, default_value_t = MetricArg::Cosine)]
        metric: MetricArg,
        /// Enable the graph-proximity channel with this many hops.
        #[arg(long)]
        graph_hops: Option<u32>,
        #[arg(long, default_value_t = 0.5)]
        graph_decay: f32,
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Embedding provider for the vector channel (preset or base URL).
        #[arg(long, default_value = "openai")]
        embed: String,
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
    },
    /// Ask a natural-language question; an LLM turns it into a read-only plan
    /// and runs it (ROADMAP §3).
    #[cfg(feature = "digest")]
    Ask {
        /// The question, in plain language.
        question: String,
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
    #[cfg(feature = "digest")]
    #[command(alias = "vec")]
    Vectorize {
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Embedding provider: preset (openai/deepseek/qwen/ollama) or a base URL.
        #[arg(long, default_value = "openai")]
        embed: String,
        /// Embedding model override (default: the provider's).
        #[arg(long)]
        embed_model: Option<String>,
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
        /// Target plane. **Omitted**: the directory's own name, `startup`
        /// as the fallback.
        #[arg(long)]
        plane: Option<String>,
        /// Rebuild the plane from the whole tree before serving: drop it,
        /// re-create it, and fold every file through the installed plugins.
        /// Facts only — embeddings return on the next `drsg digest`.
        #[arg(long)]
        force: bool,
    },
}

#[cfg(feature = "digest")]
#[derive(Subcommand)]
enum PluginCmd {
    /// Install a plugin from a local `.wasm` file or an `http(s)://` URL.
    ///
    /// The artifact is validated as a component, asked to describe itself, and
    /// its SHA-256 pinned; every later load re-checks the hash, so a file that
    /// changes on disk is refused rather than silently run. Installing a name
    /// again is the upgrade path.
    Install {
        /// Path to a `.wasm` component, or a URL to download one from.
        /// **Omitted**: an interactive chooser lists the official plugins —
        /// pick by number (`0` = all of them), paste a path/URL, or `q` to
        /// cancel.
        source: Option<String>,
    },
    /// List installed plugins as a table: name, version, extensions, hash,
    /// source.
    List {
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
        #[arg(long, default_value = "startup")]
        plane: String,
        /// Restrict to nodes carrying this label.
        #[arg(long)]
        label: Option<String>,
        /// How many top-ranked nodes to print.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long, default_value_t = 0.85)]
        damping: f64,
        #[arg(long, default_value_t = 20)]
        max_iters: u32,
    },
    /// Weakly connected components (representative = smallest id).
    Components {
        #[arg(long, default_value = "startup")]
        plane: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
    /// Weighted shortest path between two node ids.
    ShortestPath {
        src: u64,
        dst: u64,
        #[arg(long, default_value = "startup")]
        plane: String,
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
        #[arg(long, default_value = "startup")]
        plane: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value_t = 50)]
        top: usize,
    },
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
    /// Declare (and build) a vector index on a `(label, property)`.
    Ensure {
        label: String,
        property: String,
        #[arg(long, default_value = "startup")]
        plane: String,
        #[arg(long, value_enum, default_value_t = MetricArg::Cosine)]
        metric: MetricArg,
    },
    /// Declare (and build) a BM25 keyword index on a `(label, property)`.
    Keyword {
        label: String,
        property: String,
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

fn run(cli: Cli, cfg: &config::Config, out: &mut dyn Write) -> Result<()> {
    match cli.command {
        Command::Init => commands::init(&cli.db, out),
        Command::Plane(cmd) => {
            let db = commands::open(&cli.db)?;
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
            let db = commands::open(&cli.db)?;
            let reader = BufReader::new(std::fs::File::open(&file)?);
            commands::import(&db, &plane, reader, on_conflict, out)
        }
        Command::Export { plane } => {
            let db = commands::open(&cli.db)?;
            commands::export(&db, &plane, out)
        }
        Command::Get { node, plane } => {
            let db = commands::open(&cli.db)?;
            commands::get(&db, &plane, &node, out)
        }
        Command::Query { plan, plane } => {
            let db = commands::open(&cli.db)?;
            let plan = if plan == "-" {
                io::read_to_string(io::stdin())?
            } else {
                plan
            };
            commands::query(&db, &plane, &plan, out)
        }
        Command::Cypher {
            query,
            plane,
            embed,
            param,
        } => {
            let db = commands::open(&cli.db)?;
            let query = if query == "-" {
                io::read_to_string(io::stdin())?
            } else {
                query
            };
            commands::cypher(&db, &plane, &query, embed.as_deref(), &param, out)
        }
        Command::Catalog { plane } => {
            let db = commands::open(&cli.db)?;
            commands::catalog(&db, plane.as_deref(), out)
        }
        Command::Algo(cmd) => {
            let db = commands::open(&cli.db)?;
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
            }
        }
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
            let db = commands::open(&cli.db)?;
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
            label,
            property,
            plane,
            metric,
        }) => {
            let db = commands::open(&cli.db)?;
            commands::index_ensure(&db, &plane, &label, &property, metric.into(), out)
        }
        Command::Index(IndexCmd::Keyword {
            label,
            property,
            plane,
            lang,
        }) => {
            let db = commands::open(&cli.db)?;
            let language = lang.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
            commands::keyword_index_ensure(&db, &plane, &label, &property, language, out)
        }
        Command::Stats => {
            let db = commands::open(&cli.db)?;
            commands::stats(&db, out)
        }
        Command::Check => {
            let db = commands::open(&cli.db)?;
            commands::check(&db, out)
        }
        Command::Snapshot { out: path } => {
            let db = commands::open(&cli.db)?;
            commands::snapshot(&db, &path, out)
        }
        Command::Restore { input } => {
            let db = commands::open(&cli.db)?;
            commands::restore(&db, &input, out)
        }
        Command::Serve { addr, mode } => {
            let db = commands::open(&cli.db)?;
            #[allow(unused_mut)]
            let mut opts = config::serve_options(cfg, addr);
            #[cfg(feature = "digest")]
            if let Some(ServeMode::Watch { dir, plane, force }) = mode {
                let plane =
                    plane.unwrap_or_else(|| commands::default_plane(&dir.display().to_string()));
                let plugin_config = config::plugin_config(cfg)?;
                opts.on_start = Some(Box::new(move |db| {
                    commands::watch(db, dir, plane, plugin_config, force)
                }));
            }
            #[cfg(not(feature = "digest"))]
            let _ = mode;
            // Hands off to the web crate, which owns its own async runtime and
            // blocks until a shutdown signal; `out` is unused (the server logs
            // itself).
            dr_strange_web::serve(db, Some(cli.db.clone()), opts)
        }
        #[cfg(feature = "digest")]
        Command::Vectorize {
            plane,
            embed,
            embed_model,
        } => {
            let db = commands::open(&cli.db)?;
            let embedder =
                dr_strange_llm::build_provider(&embed, embed_model.as_deref(), None, None, true)?;
            commands::vectorize(&db, &plane, &embedder, out)
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
            let db = commands::open(&cli.db)?;
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
        #[cfg(feature = "digest")]
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
            match cmd {
                PluginCmd::Install { source } => {
                    commands::plugin_install(&plugin_config, &allow, source.as_deref(), out)
                }
                PluginCmd::List { json } => commands::plugin_list(&plugin_config, json, out),
                PluginCmd::Remove { name } => commands::plugin_remove(&plugin_config, &name, out),
            }
        }
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
        } => {
            let db = commands::open(&cli.db)?;
            // The `[plugins]` section, with the legacy flag folded in on top.
            let mut plugin_config = config::plugin_config(cfg)?;
            if plugin_source {
                plugin_config
                    .options
                    .entry("rust".to_string())
                    .or_default()
                    .push(("include_source".to_string(), "true".to_string()));
            }
            let args = commands::DigestArgs {
                source: &source,
                topic: topic.as_deref(),
                pages,
                depth,
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
            };
            commands::digest(&db, &args, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
