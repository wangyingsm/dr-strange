//! drsg — command-line tools for dr-strange (arch/05). A thin wrapper over
//! `dr-strange-core`; contains no database logic. The `digest` subcommand
//! (LLM-powered ingestion) is intentionally absent pending its own design
//! session (arch/05 §3, arch/07).

mod commands;

use std::io::{self, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use dr_strange_core::Metric;

#[derive(Parser)]
#[command(name = "drsg", version, about = "dr-strange graph database CLI")]
struct Cli {
    /// Path to the database file.
    #[arg(long, short, global = true, default_value = "graph.drsg")]
    db: PathBuf,

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
    /// Print the soft-schema catalog (a plane's, or the whole database's).
    Catalog {
        #[arg(long)]
        plane: Option<String>,
    },
    /// Vector index management.
    #[command(subcommand)]
    Index(IndexCmd),
    /// Summary counts across the database.
    Stats,
    /// Integrity check: scan every plane, report readability.
    Check,
    /// Serve the web dashboard + JSON-RPC 2.0 API (arch/08).
    Serve {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:7700")]
        addr: SocketAddr,
    },
    /// Digest a document into a plane via an LLM (arch/07). Dry-run by default.
    #[cfg(feature = "digest")]
    Digest {
        /// Document to digest (text / markdown).
        file: PathBuf,
        #[arg(long, default_value = "startup")]
        plane: String,
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
        /// Skip embedding generation.
        #[arg(long)]
        no_embed: bool,
        /// Don't link extracted entities to existing plane nodes (propose every
        /// entity as new; skips the per-chunk vector retrieval).
        #[arg(long)]
        no_link: bool,
    },
}

#[derive(Subcommand)]
enum PlaneCmd {
    List,
    Create { name: String },
    Drop { name: String },
    Show { name: String },
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
    // Hold the guard for the whole run so the file writer flushes on exit.
    let _log = dr_strange_log::init("drsg");
    let out = io::stdout();
    let mut out = out.lock();
    run(cli, &mut out)?;
    out.flush()?;
    Ok(())
}

fn run(cli: Cli, out: &mut dyn Write) -> Result<()> {
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
        Command::Import { file, plane } => {
            let db = commands::open(&cli.db)?;
            let reader = BufReader::new(std::fs::File::open(&file)?);
            commands::import(&db, &plane, reader, out)
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
        Command::Catalog { plane } => {
            let db = commands::open(&cli.db)?;
            commands::catalog(&db, plane.as_deref(), out)
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
        Command::Stats => {
            let db = commands::open(&cli.db)?;
            commands::stats(&db, out)
        }
        Command::Check => {
            let db = commands::open(&cli.db)?;
            commands::check(&db, out)
        }
        Command::Serve { addr } => {
            let db = commands::open(&cli.db)?;
            // Hands off to the web crate, which owns its own async runtime and
            // blocks until Ctrl-C; `out` is unused (the server logs itself).
            dr_strange_web::serve(db, Some(cli.db.clone()), addr)
        }
        #[cfg(feature = "digest")]
        Command::Digest {
            file,
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
            no_embed,
            no_link,
        } => {
            let db = commands::open(&cli.db)?;
            let args = commands::DigestArgs {
                file: &file,
                plane: &plane,
                apply,
                chunk_chars,
                embed: !no_embed,
                link: !no_link,
                chat_provider: &chat,
                embed_provider: &embed,
                model: model.as_deref(),
                embed_model: embed_model.as_deref(),
                chat_url: chat_url.as_deref(),
                embed_url: embed_url.as_deref(),
                chat_key_env: chat_key_env.as_deref(),
                embed_key_env: embed_key_env.as_deref(),
            };
            commands::digest(&db, &args, out)
        }
    }
}
