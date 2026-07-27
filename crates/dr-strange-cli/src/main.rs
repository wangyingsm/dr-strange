//! drsg — command-line tools for dr-strange (arch/05). A thin wrapper over
//! `dr-strange-core`; contains no database logic. The `digest` subcommand
//! (LLM-powered ingestion) is intentionally absent pending its own design
//! session (arch/05 §3, arch/07).

mod commands;
mod jsonio;

use std::io::{self, BufReader, Write};
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
    }
}
