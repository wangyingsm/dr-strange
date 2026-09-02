//! drsg-mcp — the [`dr_strange_mcp`] tool set served over stdio (arch/06).
//!
//! The **fallback** way to reach a graph, not the usual one. A repository
//! prepared by `drsg init` runs `drsg serve … watch` and every host shares
//! that one instance over its `/mcp` endpoint (ROADMAP §10), which is what
//! keeps the plane synced to the repository's commits. A database may be
//! opened directly by one process at a time, so this binary is for the case
//! where no such server runs: a host that speaks only stdio, or a database
//! nothing is watching.
//!
//! So it looks for that server before opening anything. When the nearest
//! `.mcp.json` declares one and it answers, this process **relays** the
//! host's session to it ([`dr_strange_mcp::relay`]) instead of contending for
//! the database — the host ends up talking to the process that already holds
//! it. Naming a database (`--db`, `$DRSG_DB`) skips the search: a caller who
//! says which graph they want is not asking to be sent to another one.
//!
//! Failing that it embeds the database, talking to nothing — point it at a
//! path and it works — but it will not *create* that path. A server that
//! silently conjures an empty database answers every question with "nothing
//! found", which reads exactly like a graph whose digest went wrong.

/// mimalloc as the process allocator (see the drsg binary for the rationale;
/// binaries choose the allocator, the core library never does).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;

use dr_strange_core::Database;
use dr_strange_mcp::{DrStrange, relay};
use rmcp::ServiceExt;

/// The database served when nothing names one — the path `drsg digest` and
/// `drsg init` write by default, so standing in a prepared repository is
/// enough.
const DEFAULT_DB: &str = "graph.drsg";

const USAGE: &str = "\
drsg-mcp — the dr-strange MCP tool set, served over stdio

Usage: drsg-mcp [--db <path>] [<path>]

Options:
  --db <path>    The database to serve; also accepted as a bare argument.
                 Naming one skips the search described below.
                 Defaults to $DRSG_DB, else ./graph.drsg.
  -h, --help     Print this message.
  -V, --version  Print the version.

With no database named, the nearest .mcp.json is read (walking up, as git
finds its own directory). If it declares a drsg server and that server
answers, this process relays the session to it — so a repository prepared by
`drsg init` is reached through the `drsg serve … watch` already holding its
database, whose plane is synced to the repository's commits.

Otherwise the database is opened here, and it must already exist: this server
never creates one. Build it with `drsg digest <dir> --apply --db <path>`, or
with `drsg init` in the repository.
";

/// What the command line asked for.
#[derive(Debug, PartialEq)]
enum Args {
    Serve {
        db: PathBuf,
        /// Whether to look for a running server first. False once a caller
        /// names a database: they are asking for *that* graph, and serving
        /// another one because a nearby `.mcp.json` mentions it would be the
        /// worst kind of helpful.
        discover: bool,
    },
    Help,
    Version,
}

/// Parse the arguments after the program name, with `$DRSG_DB` as the
/// fallback the flags override.
///
/// Every unrecognised dash-led word is an error rather than a path, which is
/// the whole reason this exists: with the first argument taken as the
/// database, `drsg-mcp --version` opened a database *named* `--version`, and
/// created it.
fn parse<I: IntoIterator<Item = String>>(args: I, env_db: Option<String>) -> Result<Args, String> {
    let mut db: Option<String> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Args::Help),
            "-V" | "--version" => return Ok(Args::Version),
            "--db" => {
                let path = args
                    .next()
                    .ok_or_else(|| format!("`--db` needs a path\n\n{USAGE}"))?;
                take(&mut db, path)?;
            }
            _ => match arg.strip_prefix("--db=") {
                Some(path) => take(&mut db, path.to_string())?,
                // A dash-led word this binary doesn't know is a mistake to
                // report, never a filename to open.
                None if arg.starts_with('-') => {
                    return Err(format!("unknown option `{arg}`\n\n{USAGE}"));
                }
                None => take(&mut db, arg)?,
            },
        }
    }
    let named = db.or(env_db);
    Ok(Args::Serve {
        discover: named.is_none(),
        db: PathBuf::from(named.unwrap_or_else(|| DEFAULT_DB.to_string())),
    })
}

/// Record the database, refusing a second one: two paths mean the caller
/// expects something this process cannot do, and serving whichever came last
/// would hide that.
fn take(slot: &mut Option<String>, path: String) -> Result<(), String> {
    match slot {
        Some(first) => Err(format!(
            "two databases named (`{first}` and `{path}`); this serves one\n\n{USAGE}"
        )),
        None => {
            *slot = Some(path);
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Argument handling comes before logging, so `--help` and `--version`
    // print their one thing and nothing else.
    let (path, discover) = match parse(std::env::args().skip(1), std::env::var("DRSG_DB").ok()) {
        Ok(Args::Help) => {
            print!("{USAGE}");
            return Ok(());
        }
        Ok(Args::Version) => {
            println!("drsg-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(Args::Serve { db, discover }) => (db, discover),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // Logs go to stderr + a rolling file — never stdout, which carries the
    // stdio JSON-RPC protocol. Hold the guard so the writer flushes on exit.
    let _log = dr_strange_log::init("drsg-mcp");

    // Before opening anything: this repository may already be running the
    // server that holds this database, and joining it is strictly better than
    // losing a race for the file it has.
    if discover && let Some(upstream) = declared_server().await {
        tracing::info!(
            server = %upstream.name,
            url = %upstream.url,
            declared_by = %upstream.source.display(),
            "relaying to the running drsg server",
        );
        return relay::relay(&upstream).await;
    }

    // An absent database is a mistake to report, not one to paper over by
    // creating it: an empty graph answers every question with "nothing
    // found", which is indistinguishable from a digest that went wrong.
    if !path.exists() {
        anyhow::bail!(
            "no database at `{}` — this server never creates one. Build it with \
             `drsg digest <dir> --apply --db {0}`, or run `drsg init` in the repository \
             (which also starts the shared server this binary is the fallback for).",
            path.display()
        );
    }
    let db = Arc::new(Database::open(&path)?);
    tracing::info!(db = %path.display(), "drsg-mcp: database opened; serving MCP over stdio");

    // Local files are allowed here and nowhere else: this process runs on the
    // agent's own machine, as that agent's user, so `digest { path }` reads
    // exactly what the agent could already read for itself. The served `/mcp`
    // leaves the flag off, where the same param would be arbitrary file read.
    let server = DrStrange::new(db).with_local_files(true);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// The drsg server this directory's `.mcp.json` declares, if it is answering.
///
/// Both halves are best-effort by design: no `.mcp.json`, no drsg entry in
/// one, or nothing listening all mean "open the database yourself", which is
/// what this binary did before it could relay at all.
async fn declared_server() -> Option<relay::Upstream> {
    let cwd = std::env::current_dir().ok()?;
    let upstream = relay::discover(&cwd)?;
    match relay::alive(&upstream, relay::PROBE_TIMEOUT).await {
        true => Some(upstream),
        false => {
            tracing::debug!(
                url = %upstream.url,
                "the declared drsg server is not answering; opening the database here",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()), None)
    }

    #[test]
    fn a_flag_is_never_a_database() {
        // The bug this parser exists for: both of these used to name a
        // database, and opening one created it.
        assert_eq!(parse_args(&["--help"]), Ok(Args::Help));
        assert_eq!(parse_args(&["-h"]), Ok(Args::Help));
        assert_eq!(parse_args(&["--version"]), Ok(Args::Version));
        assert_eq!(parse_args(&["-V"]), Ok(Args::Version));
        let err = parse_args(&["--plane", "code"]).unwrap_err();
        assert!(err.contains("unknown option `--plane`"), "{err}");
        // …and the message says what the binary does take.
        assert!(err.contains("--db <path>"), "{err}");
    }

    #[test]
    fn the_database_is_named_by_flag_or_position() {
        let expect = |args: &[&str]| match parse_args(args) {
            // Naming a database also turns the search off.
            Ok(Args::Serve {
                db,
                discover: false,
            }) => db,
            other => panic!("{args:?} → {other:?}"),
        };
        assert_eq!(expect(&["graph.drsg"]), PathBuf::from("graph.drsg"));
        assert_eq!(expect(&["--db", "a.drsg"]), PathBuf::from("a.drsg"));
        assert_eq!(expect(&["--db=a.drsg"]), PathBuf::from("a.drsg"));
        // A path may look like anything as long as it isn't dash-led.
        assert_eq!(expect(&["/tmp/x y.drsg"]), PathBuf::from("/tmp/x y.drsg"));

        let err = parse_args(&["--db"]).unwrap_err();
        assert!(err.contains("`--db` needs a path"), "{err}");
        let err = parse_args(&["a.drsg", "b.drsg"]).unwrap_err();
        assert!(err.contains("two databases named"), "{err}");
        let err = parse_args(&["--db", "a.drsg", "--db", "b.drsg"]).unwrap_err();
        assert!(err.contains("two databases named"), "{err}");
    }

    #[test]
    fn the_environment_fills_in_and_the_flags_override_it() {
        let env = || Some("env.drsg".to_string());
        let serve = |db: &str, discover| {
            Ok(Args::Serve {
                db: PathBuf::from(db),
                discover,
            })
        };
        // The environment names a database as surely as the flag does, so it
        // switches the search off too.
        assert_eq!(parse(std::iter::empty(), env()), serve("env.drsg", false));
        assert_eq!(
            parse(["--db".to_string(), "flag.drsg".to_string()], env()),
            serve("flag.drsg", false)
        );
        // Neither: the default path, and a look around first.
        assert_eq!(parse(std::iter::empty(), None), serve(DEFAULT_DB, true));
    }
}
