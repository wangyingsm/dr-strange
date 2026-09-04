//! Command handlers for `drsg` (arch/05). Each takes an open `Database` (or a
//! path) and writes to a `&mut dyn Write`, so they are unit-testable without
//! spawning a process.

use std::io::{BufRead, Write};
use std::path::Path;
#[cfg(feature = "digest")]
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use dr_strange_core::{
    BulkEdgeById, BulkNode, Database, Dir, Language, LogicalPlan, LouvainOptions, Metric, NodeId,
    PageRankOptions, PlaneHandle, Properties, ShortestPathOptions,
};
// Only `vectorize` and the digest pipeline write property *descriptions*; the
// rest of the CLI reads and writes plain values.
#[cfg(feature = "digest")]
use dr_strange_core::{PropDesc, PropValue};
use serde_json::{Value, json};

use dr_strange_core::json as jsonio;

/// Opens (creating if needed) the database at `path`.
pub fn open(path: &Path) -> Result<Database> {
    Database::open(path).with_context(|| format!("opening database at {}", path.display()))
}

/// Marker file left in a `serve --follow` replica's directory (arch/01 §9)
/// after a successful resync, so the next resync knows it's safe to wipe.
const FOLLOWER_MARKER: &str = ".drsg-follower";

/// Prepares `path` for a fresh `serve --follow` resync: wipes it if empty,
/// absent, or already marked as a prior replica copy, then recreates it with
/// the marker in place. Refuses to touch a directory that holds something
/// else — a replica resyncs by deleting everything it has, so silently
/// wiping a real database would be exactly the wrong kind of convenient.
pub fn prepare_follower_dir(path: &Path) -> Result<()> {
    if path.exists() {
        let has_content = std::fs::read_dir(path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if has_content && !path.join(FOLLOWER_MARKER).exists() {
            bail!(
                "{} is not empty and not a previous `serve --follow` replica; \
                 refusing to wipe it. Point --db at an empty or dedicated \
                 directory for --follow.",
                path.display()
            );
        }
        std::fs::remove_dir_all(path)
            .with_context(|| format!("clearing the replica directory {}", path.display()))?;
    }
    std::fs::create_dir_all(path)?;
    std::fs::write(path.join(FOLLOWER_MARKER), b"")?;
    Ok(())
}

fn plane<'db>(db: &'db Database, name: &str) -> Result<PlaneHandle<'db>> {
    db.plane(name)
        .with_context(|| format!("no such plane '{name}'"))
}

/// Pin a plane handle to the snapshot a query's `AS OF` names (ROADMAP §4).
#[cfg(feature = "native-backend")]
fn pin(p: PlaneHandle<'_>, at: Option<dr_strange_parser::AsOfSpec>) -> Result<PlaneHandle<'_>> {
    use dr_strange_core::AsOf;
    use dr_strange_parser::AsOfSpec;
    Ok(match at {
        None => p,
        Some(AsOfSpec::Seq(seq)) => p.as_of(AsOf::Seq(seq))?,
        Some(AsOfSpec::Time(ms)) => p.as_of(AsOf::Time(ms))?,
    })
}

/// Other backends keep no history, so an `AS OF` query is refused outright
/// rather than silently reading the present.
#[cfg(not(feature = "native-backend"))]
fn pin(p: PlaneHandle<'_>, at: Option<dr_strange_parser::AsOfSpec>) -> Result<PlaneHandle<'_>> {
    if at.is_some() {
        bail!("AS OF (time-travel) requires the native backend");
    }
    Ok(p)
}

#[cfg(not(feature = "digest"))]
pub fn init(path: &Path, out: &mut dyn Write) -> Result<()> {
    open(path)?;
    writeln!(out, "initialized dr-strange database at {}", path.display())?;
    Ok(())
}

// ---- init bootstrap (drsg init) -------------------------------------------

#[cfg(feature = "digest")]
const INIT_TOKEN_LEN: usize = 40;

#[cfg(feature = "digest")]
const INIT_HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The name every agent config gives the entry `init` writes. One constant
/// because `recorded_endpoint` reads back what `write_mcp_json_entry` wrote:
/// a restart that kept the old address and token depends on the two agreeing.
#[cfg(feature = "digest")]
const MCP_SERVER_NAME: &str = "drsg-watch";

#[cfg(feature = "digest")]
const GITIGNORE_PATTERNS: &[&str] = &[
    "*.drsg",
    "*.drsg.jsonl",
    "*.drsg.hnsw",
    "*.drsg.bm25",
    "logs/",
    ".mcp.json",
];

/// Bootstraps `dir` for agent MCP access: ensures `.gitignore` covers the
/// artifacts this leaves behind, spawns `drsg serve watch` detached in the
/// background, waits for it to come up, and writes the connection details to
/// `dir`'s `.mcp.json`. Never blocks past the health check — the spawned
/// server keeps running after this returns, the same way this repo's own
/// `serve watch` instances do.
///
/// Safe to run again at any time, which is the point: the spawned server is
/// nobody's child (no client relaunches an MCP `http` entry, and nothing
/// survives a reboot), so "is drsg up for this repo?" has to be a question
/// `init` itself can answer. It reads the endpoint a previous run recorded
/// and probes it:
///
/// * **reachable** — nothing to do. The configs are rewritten (idempotent,
///   and a newly-present agent gets its file now) and the database is never
///   opened, so this cannot collide with the running server's lock.
/// * **recorded but dead** — respawn on the *same* address and token, so
///   every agent's config stays valid, and without `--force`, so the plane
///   resumes from its sync point instead of re-parsing the tree.
/// * **nothing recorded** — a first run: pick a free port and a fresh token,
///   and build the plane from scratch.
#[cfg(feature = "digest")]
#[allow(clippy::too_many_arguments)]
pub fn init_bootstrap(
    db_path: &Path,
    dir: PathBuf,
    plane: Option<String>,
    addr: Option<std::net::SocketAddr>,
    token: Option<String>,
    plugin_config: &dr_strange_llm::PluginConfig,
    out: &mut dyn Write,
) -> Result<()> {
    use rand::distr::{Alphanumeric, SampleString};

    let db_path = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        dir.join(db_path)
    };
    ensure_gitignore_patterns(&dir)?;

    // What a previous run left behind, and whether it is still answering.
    let recorded = recorded_endpoint(&dir);
    let live = recorded
        .as_ref()
        .filter(|(recorded_addr, _)| health_ok(*recorded_addr, INIT_HEALTH_CHECK_TIMEOUT));

    if let Some((live_addr, live_token)) = live {
        // An explicit address asks for something this cannot deliver: the
        // running server holds the database, so a second one cannot bind.
        // Say that instead of pretending the flag was honoured.
        if let Some(wanted) = addr
            && wanted != *live_addr
        {
            bail!(
                "drsg is already serving {} at http://{live_addr}/mcp, and one process at a \
                 time may open {}. Stop that server before re-running with --addr {wanted}.",
                dir.display(),
                db_path.display()
            );
        }
        let (live_addr, live_token) = (*live_addr, live_token.clone());
        writeln!(
            out,
            "drsg is already serving {} at http://{live_addr}/mcp — reusing it, the plane is \
             untouched",
            dir.display()
        )?;
        write_agent_configs(&dir, &live_addr, &live_token, out)?;
        return Ok(());
    }

    open(&db_path)?;

    // A recorded endpoint that stopped answering is the one worth restoring
    // verbatim: agents already hold that URL and token. An explicit flag
    // still wins over it.
    let (addr, token, force) = match recorded {
        Some((recorded_addr, recorded_token)) => (
            match addr {
                Some(explicit) => explicit,
                // The recorded port was an arbitrary one the OS handed out,
                // and after a reboot it may belong to something else — which
                // the health probe already ruled out as being drsg. Moving is
                // then the only way to come up at all; agents pick the new
                // address up from the rewritten configs.
                None if addr_bindable(recorded_addr) => recorded_addr,
                None => {
                    let moved = pick_free_port()?;
                    writeln!(
                        out,
                        "note: {recorded_addr} is taken by another process — moving to {moved}"
                    )?;
                    moved
                }
            },
            token.unwrap_or(recorded_token),
            // The plane already exists and records where it left off; `serve
            // watch` catches it up from there. Re-parsing the whole tree here
            // would make every restart cost a full digest.
            false,
        ),
        None => (
            match addr {
                Some(addr) => addr,
                None => pick_free_port()?,
            },
            token.unwrap_or_else(|| Alphanumeric.sample_string(&mut rand::rng(), INIT_TOKEN_LEN)),
            true,
        ),
    };
    let plane_name = plane.unwrap_or_else(|| default_plane(&dir.display().to_string()));

    let exe = std::env::current_exe().context("resolving the running drsg binary's path")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.current_dir(&dir)
        .arg("--db")
        .arg(&db_path)
        .arg("serve")
        .arg("--addr")
        .arg(addr.to_string())
        .arg("watch")
        .arg("--dir")
        .arg(&dir)
        .arg("--plane")
        .arg(&plane_name)
        .env("DRSG_TOKEN", &token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if force {
        cmd.arg("--force");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid()` is async-signal-safe and touches only the
        // child's own process state; this runs in the forked child before
        // exec, per `pre_exec`'s contract.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "spawning `{} serve watch` for {}",
            exe.display(),
            dir.display()
        )
    })?;
    let pid = child.id();

    if !wait_for_listener(addr, &mut child, INIT_HEALTH_CHECK_TIMEOUT) {
        let log_tail =
            tail_recent_log(&dir).unwrap_or_else(|| "(no log file found under logs/)".to_string());
        let _ = child.kill();
        bail!("`drsg serve watch` (pid {pid}) never started listening on {addr}\n{log_tail}");
    }

    let what = if force { "bootstrapped" } else { "restarted" };
    writeln!(
        out,
        "plane '{plane_name}' {what} — serve watch pid {pid}, http://{addr}/mcp"
    )?;
    say_history(&dir, &plane_name, plugin_config, out)?;
    write_agent_configs(&dir, &addr, &token, out)
}

/// Say what will become of this repository's history — one line, and only
/// where there is something to say.
///
/// A checkout whose history is being read should say so, because a second
/// plane appearing unannounced is exactly the kind of thing a reader later
/// finds by accident. A checkout whose history is *not* being read should
/// say that instead: the reason is always the same one, and it is fixable
/// in one command.
#[cfg(feature = "digest")]
fn say_history(
    dir: &Path,
    plane_name: &str,
    plugin_config: &dr_strange_llm::PluginConfig,
    out: &mut dyn Write,
) -> Result<()> {
    if !matches!(
        dr_strange_llm::git_dir(dir),
        dr_strange_llm::GitDir::Here(_)
    ) {
        return Ok(());
    }
    // The registry file, not the components: this asks which plugins are
    // installed, and compiling one to answer that would be absurd.
    let installed = plugin_store(plugin_config)
        .and_then(|s| s.list())
        .unwrap_or_default();
    let history = dr_strange_llm::git_plane_name(plane_name);
    if installed
        .iter()
        .any(|p| p.name == dr_strange_llm::REPO_PLUGIN)
    {
        writeln!(
            out,
            "  history → plane '{history}', current with every commit — \
             `drsg history --plane {plane_name}`"
        )?;
    } else {
        writeln!(
            out,
            "  note: no `{}` plugin installed, so this repository's history \
             (commits, branches, rebases) is not read — `drsg plugin install` \
             offers one",
            dr_strange_llm::REPO_PLUGIN
        )?;
    }
    Ok(())
}

/// Point every agent config in `dir` at `addr`, and say which files that
/// touched. Idempotent: rewriting the same endpoint changes nothing, so this
/// runs on a reused server too — a repo that grew a `.cursor/` since the last
/// `init` gets its file on the next one.
#[cfg(feature = "digest")]
fn write_agent_configs(
    dir: &Path,
    addr: &std::net::SocketAddr,
    token: &str,
    out: &mut dyn Write,
) -> Result<()> {
    write_mcp_json_entry(dir, addr, token)?;
    writeln!(out, "  + wrote {}", dir.join(".mcp.json").display())?;
    if probe_and_write_claude_hooks(dir, &default_hooks_dir()?, user_has_claude_code())? {
        writeln!(
            out,
            "  + Claude Code: hooks in {} — a shell search or read on code is \
             redirected to the drsg tools (DRSG_RAW=1 <command> runs it anyway)",
            dir.join(".claude/settings.local.json").display()
        )?;
    }

    // Beyond Claude Code's `.mcp.json`, only add a file for an agent whose
    // own marker (a directory it creates, or a config file it already owns)
    // is already present — writing one for a tool nobody here uses would
    // just be repo clutter.
    if probe_and_write_cursor(dir, addr, token)? {
        writeln!(
            out,
            "  + Cursor: wrote {}",
            dir.join(".cursor/mcp.json").display()
        )?;
    }
    if probe_and_write_opencode(dir, addr, token)? {
        writeln!(
            out,
            "  + OpenCode: wrote {}",
            dir.join(".opencode.json").display()
        )?;
    }
    if probe_and_write_gemini(dir, addr, token)? {
        writeln!(
            out,
            "  + Gemini CLI: wrote {}",
            dir.join(".gemini/settings.json").display()
        )?;
    }
    if probe_and_write_codex(dir, addr)? {
        writeln!(
            out,
            "  + Codex CLI: wrote {} (no token inside it — Codex reads the bearer from its \
             own process environment; export {CODEX_TOKEN_ENV_VAR}={token} before launching \
             `codex` here, and mark this project trusted, or its project-scoped MCP config is \
             ignored)",
            dir.join(".codex/config.toml").display()
        )?;
    }
    Ok(())
}

/// The address and token a previous `init` wrote into `dir`'s `.mcp.json`,
/// if one is still there and still parses. This is the only record of them —
/// the token is generated once and never stored anywhere else — so recovering
/// it here is what lets a restart keep every agent's config valid.
#[cfg(feature = "digest")]
fn recorded_endpoint(dir: &Path) -> Option<(std::net::SocketAddr, String)> {
    let raw = std::fs::read_to_string(dir.join(".mcp.json")).ok()?;
    let entry = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("mcpServers")?
        .get(MCP_SERVER_NAME)?
        .clone();
    let url = entry.get("url")?.as_str()?;
    // `http://host:port/mcp` — take the authority, which is what a socket
    // address is. Anything else was not written by `init`.
    let addr = url
        .strip_prefix("http://")?
        .split('/')
        .next()?
        .parse()
        .ok()?;
    let token = entry
        .get("headers")?
        .get("Authorization")?
        .as_str()?
        .strip_prefix("Bearer ")?
        .to_string();
    Some((addr, token))
}

/// Whether a drsg server is answering on `addr` — `GET /health`, which the
/// server leaves unauthenticated precisely for probes like this one.
///
/// An HTTP round trip rather than a bare TCP connect, because the recorded
/// port is an arbitrary one the OS handed out: after a reboot some unrelated
/// process may well hold it, and accepting a connection from *that* would
/// make `init` report a healthy drsg that does not exist.
#[cfg(feature = "digest")]
fn health_ok(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write as _};

    let probe = || -> std::io::Result<String> {
        let mut sock = std::net::TcpStream::connect_timeout(&addr, timeout)?;
        sock.set_read_timeout(Some(timeout))?;
        sock.set_write_timeout(Some(timeout))?;
        write!(
            sock,
            "GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )?;
        let mut body = Vec::new();
        // Bounded: a health response is a couple of hundred bytes, and this
        // must not hang on a socket that answers with a firehose.
        sock.take(4096).read_to_end(&mut body)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    };
    match probe() {
        Ok(resp) => resp.starts_with("HTTP/1.1 200") && resp.contains("\"status\":\"ok\""),
        Err(_) => false,
    }
}

/// Whether `addr` is free for the spawned server to bind. Racy by nature —
/// something could take it in between — but it is the difference between
/// reusing a recorded port and failing to start on one that is gone.
#[cfg(feature = "digest")]
fn addr_bindable(addr: std::net::SocketAddr) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

#[cfg(feature = "digest")]
fn pick_free_port() -> Result<std::net::SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("picking a free port")?;
    listener.local_addr().context("reading the picked port")
}

/// Polls `addr` until something accepts a TCP connection, the child exits
/// first, or `timeout` elapses.
#[cfg(feature = "digest")]
fn wait_for_listener(
    addr: std::net::SocketAddr,
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The last few lines of the most recently modified file under `dir/logs`
/// (the same rolling log `dr_strange_log::init` already writes) — the best
/// available diagnostic when the spawned server never comes up.
#[cfg(feature = "digest")]
fn tail_recent_log(dir: &Path) -> Option<String> {
    let logs_dir = dir.join("logs");
    let newest = std::fs::read_dir(&logs_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())?;
    let contents = std::fs::read_to_string(newest.path()).ok()?;
    let tail: Vec<&str> = contents.lines().rev().take(20).collect();
    Some(format!(
        "--- tail of {} ---\n{}",
        newest.path().display(),
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    ))
}

/// Appends whichever of `GITIGNORE_PATTERNS` are missing from `dir`'s
/// `.gitignore` (creating it if absent) — idempotent, never duplicates, and
/// never touches unrelated lines.
#[cfg(feature = "digest")]
fn ensure_gitignore_patterns(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let have: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&str> = GITIGNORE_PATTERNS
        .iter()
        .copied()
        .filter(|p| !have.contains(p))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() {
        content.push('\n');
    }
    content.push_str(
        "# drsg — local database, logs, and the MCP config carrying a live bearer token\n",
    );
    for p in missing {
        content.push_str(p);
        content.push('\n');
    }
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))
}

/// Upserts `entry` under `path`'s JSON `top_key.entry_name`, preserving
/// every other key untouched. Creates `path` fresh (as `{top_key: {}}`) if
/// it doesn't exist yet.
#[cfg(feature = "digest")]
fn upsert_json_mcp_entry(path: &Path, top_key: &str, entry_name: &str, entry: Value) -> Result<()> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("{} exists but is not valid JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} does not contain a JSON object", path.display()))?;
    let servers = obj.entry(top_key).or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}'s '{top_key}' key is not an object", path.display()))?;
    servers.insert(entry_name.to_string(), entry);
    let pretty = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, pretty + "\n").with_context(|| format!("writing {}", path.display()))
}

/// Upserts the `"drsg-watch"` entry under `dir`'s `.mcp.json`'s
/// `mcpServers` — Claude Code's own convention, also read as-is by GitHub
/// Copilot CLI (which walks from cwd up to the repo root looking for this
/// exact file).
#[cfg(feature = "digest")]
fn write_mcp_json_entry(dir: &Path, addr: &std::net::SocketAddr, token: &str) -> Result<()> {
    upsert_json_mcp_entry(
        &dir.join(".mcp.json"),
        "mcpServers",
        MCP_SERVER_NAME,
        json!({
            "type": "http",
            "url": format!("http://{addr}/mcp"),
            "headers": { "Authorization": format!("Bearer {token}") },
        }),
    )
}

/// Cursor reads the identical shape from its own path instead of
/// `.mcp.json`. Only written when `.cursor/` already exists — that's
/// Cursor's own marker, created the first time someone opens this repo in
/// it, regardless of MCP use.
#[cfg(feature = "digest")]
fn probe_and_write_cursor(dir: &Path, addr: &std::net::SocketAddr, token: &str) -> Result<bool> {
    if !dir.join(".cursor").is_dir() {
        return Ok(false);
    }
    upsert_json_mcp_entry(
        &dir.join(".cursor").join("mcp.json"),
        "mcpServers",
        MCP_SERVER_NAME,
        json!({
            "type": "http",
            "url": format!("http://{addr}/mcp"),
            "headers": { "Authorization": format!("Bearer {token}") },
        }),
    )?;
    Ok(true)
}

/// OpenCode has no directory marker of its own (it's terminal-first, no
/// rules/config dir it creates unprompted) — the only honest signal that
/// this repo's contributors already use it is a pre-existing
/// `.opencode.json`, so that's the marker, not something created fresh.
#[cfg(feature = "digest")]
fn probe_and_write_opencode(dir: &Path, addr: &std::net::SocketAddr, token: &str) -> Result<bool> {
    if !dir.join(".opencode.json").is_file() {
        return Ok(false);
    }
    upsert_json_mcp_entry(
        &dir.join(".opencode.json"),
        "mcp",
        MCP_SERVER_NAME,
        json!({
            "type": "remote",
            "url": format!("http://{addr}/mcp"),
            "headers": { "Authorization": format!("Bearer {token}") },
            "enabled": true,
        }),
    )?;
    Ok(true)
}

/// Gemini CLI shares Claude Code's `mcpServers` key but names the URL field
/// `httpUrl` instead of `url`, and has no `type` discriminator. Written
/// only when `.gemini/` already exists.
#[cfg(feature = "digest")]
fn probe_and_write_gemini(dir: &Path, addr: &std::net::SocketAddr, token: &str) -> Result<bool> {
    if !dir.join(".gemini").is_dir() {
        return Ok(false);
    }
    upsert_json_mcp_entry(
        &dir.join(".gemini").join("settings.json"),
        "mcpServers",
        MCP_SERVER_NAME,
        json!({
            "httpUrl": format!("http://{addr}/mcp"),
            "headers": { "Authorization": format!("Bearer {token}") },
        }),
    )?;
    Ok(true)
}

/// The env var Codex CLI reads its bearer token from at its *own* launch
/// time — Codex's schema takes `bearer_token_env_var` (a variable name),
/// never a literal token, so the secret never lands in `.codex/config.toml`
/// itself. The caller still has to export it before running `codex` here.
#[cfg(feature = "digest")]
const CODEX_TOKEN_ENV_VAR: &str = "DRSG_TOKEN";

/// Codex CLI's project-scoped MCP config, `.codex/config.toml`, is TOML —
/// parsed and re-emitted as a generic table (like the JSON upserts above,
/// this preserves every other *key* but not hand-written comments or
/// formatting). Written only when `.codex/` already exists. Note this does
/// **not** set `trust_level = "trusted"`: Codex treats that as a deliberate
/// user decision and ignores project-scoped MCP config for untrusted
/// projects, so the caller still has to trust the project themselves.
#[cfg(feature = "digest")]
fn probe_and_write_codex(dir: &Path, addr: &std::net::SocketAddr) -> Result<bool> {
    let path = dir.join(".codex").join("config.toml");
    if !dir.join(".codex").is_dir() {
        return Ok(false);
    }
    let mut root: toml::Value = match std::fs::read_to_string(&path) {
        Ok(s) => s
            .parse()
            .with_context(|| format!("{} exists but is not valid TOML", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let root_table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("{} does not contain a TOML table", path.display()))?;
    let mcp_servers = root_table
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let mcp_servers = mcp_servers
        .as_table_mut()
        .ok_or_else(|| anyhow!("{}'s 'mcp_servers' key is not a table", path.display()))?;
    let mut entry = toml::value::Table::new();
    entry.insert(
        "url".to_string(),
        toml::Value::String(format!("http://{addr}/mcp")),
    );
    entry.insert(
        "bearer_token_env_var".to_string(),
        toml::Value::String(CODEX_TOKEN_ENV_VAR.to_string()),
    );
    mcp_servers.insert(MCP_SERVER_NAME.to_string(), toml::Value::Table(entry));
    let rendered = toml::to_string_pretty(&root)?;
    std::fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The hook scripts `init` installs for Claude Code, carried in the binary
/// and written afresh on every `init` — a newer drsg brings newer hooks —
/// into the per-user data directory, never into the repository.
///
/// The MCP config tells an agent where the graph is; these tell it *when*
/// to use it. `drsg-shell-guard` is a `PreToolUse` hook on `Bash` that meets
/// an `rg`/`grep`/`cat`/`sed -n` on code with the verb that answers instead
/// (`DRSG_RAW=1 <command>` runs it anyway); `drsg-session-brief` is a
/// `SessionStart` hook that puts the same rule in the agent's context, where
/// it survives a resume, a clear and a compaction. Claude Code is the host
/// with hooks; the other hosts get the rule from the server's own MCP
/// instructions, which every host places in the system prompt.
#[cfg(feature = "digest")]
const CLAUDE_HOOKS: &[(&str, &str)] = &[
    (
        "drsg-shell-guard",
        include_str!("../hooks/drsg-shell-guard"),
    ),
    (
        "drsg-session-brief",
        include_str!("../hooks/drsg-session-brief"),
    ),
];

/// `$XDG_DATA_HOME/drsg/hooks`, or `~/.local/share/drsg/hooks` — beside the
/// plugin store.
#[cfg(feature = "digest")]
fn default_hooks_dir() -> Result<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(std::path::PathBuf::from(xdg).join("drsg").join("hooks"));
    }
    let home = std::env::home_dir()
        .context("neither $XDG_DATA_HOME nor a home directory — set XDG_DATA_HOME")?;
    Ok(home.join(".local").join("share").join("drsg").join("hooks"))
}

/// Whether this user runs Claude Code at all: its per-user directory exists.
#[cfg(feature = "digest")]
fn user_has_claude_code() -> bool {
    std::env::home_dir().is_some_and(|h| h.join(".claude").is_dir())
}

/// Claude Code's hooks, in the repository's `.claude/settings.local.json` —
/// the per-user, uncommitted settings file, so nothing lands in the tree a
/// team shares. Written when Claude Code shows itself: a `.claude/` in the
/// repository, or (`user_has_claude`) the user's own `~/.claude`. Idempotent:
/// an entry already pointing at a script of ours is repointed, not repeated.
#[cfg(feature = "digest")]
fn probe_and_write_claude_hooks(
    dir: &Path,
    hooks_dir: &Path,
    user_has_claude: bool,
) -> Result<bool> {
    if !dir.join(".claude").is_dir() && !user_has_claude {
        return Ok(false);
    }
    std::fs::create_dir_all(hooks_dir)
        .with_context(|| format!("creating {}", hooks_dir.display()))?;
    let mut scripts = Vec::with_capacity(CLAUDE_HOOKS.len());
    for (name, body) in CLAUDE_HOOKS {
        let path = hooks_dir.join(name);
        write_executable(&path, body)?;
        scripts.push(path);
    }
    let settings_dir = dir.join(".claude");
    std::fs::create_dir_all(&settings_dir)
        .with_context(|| format!("creating {}", settings_dir.display()))?;
    let settings = settings_dir.join("settings.local.json");
    upsert_claude_hook(&settings, "PreToolUse", Some("Bash"), &scripts[0])?;
    upsert_claude_hook(&settings, "SessionStart", None, &scripts[1])?;
    Ok(true)
}

/// Write-then-rename, executable — a hook the host runs must never be seen
/// half-written.
#[cfg(feature = "digest")]
fn write_executable(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("marking {} executable", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("moving {} into place", path.display()))
}

/// Upsert one hook command under `hooks.<event>` of a Claude Code settings
/// file. An entry whose command already ends in this script's name is
/// repointed (the data directory may have moved); otherwise one is added,
/// with `matcher` when the event takes one. Every other key is untouched,
/// and a file that is not there yet is created.
#[cfg(feature = "digest")]
fn upsert_claude_hook(
    path: &Path,
    event: &str,
    matcher: Option<&str>,
    script: &Path,
) -> Result<()> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("{} exists but is not valid JSON", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} does not contain a JSON object", path.display()))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}'s 'hooks' key is not an object", path.display()))?;
    let entries = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("{}'s 'hooks.{event}' is not a list", path.display()))?;
    let name = script
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let command = script.display().to_string();
    let mut found = false;
    for entry in entries.iter_mut() {
        let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        for hook in list.iter_mut() {
            let ours = hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| c.ends_with(&name));
            if ours {
                hook["command"] = Value::String(command.clone());
                found = true;
            }
        }
    }
    if !found {
        let mut entry = json!({ "hooks": [{ "type": "command", "command": command }] });
        if let Some(m) = matcher {
            entry["matcher"] = Value::String(m.to_string());
        }
        entries.push(entry);
    }
    let pretty = serde_json::to_string_pretty(&root)?;
    std::fs::write(path, pretty + "\n").with_context(|| format!("writing {}", path.display()))
}

/// `drsg history` — a repository's history at a glance.
///
/// The plane argument is forgiving in one direction only: naming the **code**
/// plane finds the history plane beside it, because `myrepo` is what a reader
/// has in mind and `myrepo_git` is an implementation detail of where the
/// digest put it. The reverse is not guessed at — a plane that holds no
/// commits and has no `_git` beside it says so.
pub fn history(db: &Database, plane_name: &str, limit: usize, out: &mut dyn Write) -> Result<()> {
    let history_plane = dr_strange_core::compact::history_plane_name(plane_name);
    let (name, p) = match db.plane(&history_plane) {
        // A `_git` plane beside the one named: that is the history, whatever
        // the named plane happens to hold.
        Ok(p) if !plane_name.ends_with(dr_strange_core::compact::HISTORY_SUFFIX) => {
            (history_plane, p)
        }
        _ => (plane_name.to_string(), plane(db, plane_name)?),
    };
    writeln!(out, "plane '{name}':")?;
    write!(
        out,
        "{}",
        dr_strange_core::compact::history(&p, Some(limit))?
    )?;
    Ok(())
}

// ---- planes --------------------------------------------------------------

pub fn plane_list(db: &Database, out: &mut dyn Write) -> Result<()> {
    for (id, name) in db.planes()? {
        writeln!(out, "{}\t{}", id.0, name)?;
    }
    Ok(())
}

pub fn plane_create(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let handle = db.create_plane(name, Properties::new())?;
    writeln!(out, "created plane '{name}' (id {})", handle.id().0)?;
    Ok(())
}

pub fn plane_drop(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let id = plane(db, name)?.id();
    db.drop_plane(id)?;
    writeln!(out, "dropped plane '{name}'")?;
    Ok(())
}

pub fn plane_show(db: &Database, name: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, name)?;
    let props = p.properties()?;
    let cat = p.catalog()?;
    writeln!(
        out,
        "plane '{name}': {} nodes, {} edges",
        cat.node_count, cat.edge_count
    )?;
    if !props.is_empty() {
        writeln!(out, "  properties: {}", jsonio::properties_to_json(&props))?;
    }
    for (label, stats) in &cat.labels {
        writeln!(out, "  label {label}: {} nodes", stats.count)?;
    }
    for (ty, stats) in &cat.edge_types {
        writeln!(out, "  edge {ty}: {} edges", stats.count)?;
    }
    Ok(())
}

// ---- get / query / catalog -----------------------------------------------

/// Resolves a node reference: `@key` looks up an external key, otherwise a
/// numeric id.
fn resolve_node(p: &PlaneHandle, reference: &str) -> Result<Option<NodeId>> {
    if let Some(key) = reference.strip_prefix('@') {
        Ok(p.node_by_key(key)?.map(|n| n.id))
    } else {
        let id: u64 = reference
            .parse()
            .with_context(|| format!("'{reference}' is not a node id or @external-key"))?;
        Ok(Some(NodeId(id)))
    }
}

pub fn get(db: &Database, plane_name: &str, reference: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, plane_name)?;
    let Some(id) = resolve_node(&p, reference)? else {
        bail!("no node with external key {reference}");
    };
    match p.node(id)? {
        Some(node) => writeln!(out, "{}", jsonio::node_to_json(&node))?,
        None => bail!("no node with id {}", id.0),
    }
    Ok(())
}

pub fn query(db: &Database, plane_name: &str, plan_json: &str, out: &mut dyn Write) -> Result<()> {
    let plan: LogicalPlan =
        serde_json::from_str(plan_json).context("parsing the query plan JSON")?;
    run_plan(plane(db, plane_name)?, plan, out)
}

/// Run a statement written in the query language (arch/00 §5): a read compiles
/// to a `LogicalPlan` and runs like the JSON `query` path; a write (`CREATE`, …)
/// is applied to the plane and its change-counts are reported. `embed` names an
/// embedding provider for a text `SEARCH … NEAR "…"`.
pub fn cypher(
    db: &Database,
    plane_name: &str,
    query: &str,
    embed: Option<&str>,
    param: &[String],
    out: &mut dyn Write,
) -> Result<()> {
    let params = parse_params(param)?;
    match parse_stmt(query, embed, &params)? {
        dr_strange_parser::Statement::Read(read) => {
            run_plan(pin(plane(db, plane_name)?, read.as_of)?, read.plan, out)
        }
        dr_strange_parser::Statement::Write(w) => {
            let p = plane(db, plane_name)?;
            let summary = w.apply(&p).map_err(|e| anyhow!("{e}"))?;
            writeln!(out, "{}", write_summary_line(&summary))?;
            Ok(())
        }
    }
}

/// A human-readable one-liner of a write's effect — the non-zero counts.
fn write_summary_line(s: &dr_strange_parser::WriteSummary) -> String {
    let mut parts = Vec::new();
    for (n, label) in [
        (s.nodes_created, "nodes created"),
        (s.edges_created, "edges created"),
        (s.props_set, "props set"),
        (s.labels_set, "labels set"),
        (s.nodes_deleted, "nodes deleted"),
        (s.edges_deleted, "edges deleted"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }
    if parts.is_empty() {
        "no changes".to_string()
    } else {
        parts.join(", ")
    }
}

/// Build the `$param` map from `name=<json>` CLI args.
fn parse_params(param: &[String]) -> Result<dr_strange_parser::Params> {
    let mut params = dr_strange_parser::Params::new();
    for kv in param {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow!("--param must be NAME=<json>, got `{kv}`"))?;
        let json: Value = serde_json::from_str(v)
            .with_context(|| format!("--param `{k}`: value must be JSON"))?;
        let pv = jsonio::json_to_value(&json).map_err(|e| anyhow!("--param `{k}`: {e}"))?;
        params.insert(k.to_string(), pv);
    }
    Ok(params)
}

/// The plane a digest lands in when `--plane` is not given: the source
/// directory's own name, so `drsg digest` in a checkout writes a plane named
/// after the repo. Anything that doesn't yield one — a URL, a bare file, a
/// nameless path like `/` — stays `startup`.
#[cfg(feature = "digest")]
pub fn default_plane(source: &str) -> String {
    std::fs::canonicalize(source)
        .ok()
        .filter(|p| p.is_dir())
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "startup".to_string())
}

// ---- serve watch (ROADMAP §11) -------------------------------------------

/// How often the watcher asks the repository where HEAD is. Commits are
/// human-paced; two seconds is invisible latency and negligible cost.
#[cfg(feature = "digest")]
const WATCH_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// The run id stamped on facts parsed before the repository has a commit.
/// `_run` is free text, and saying "working-tree" is more honest than a
/// commit-shaped placeholder nothing could ever resolve.
#[cfg(feature = "digest")]
const UNBORN_RUN_ID: &str = "working-tree";

/// Plane properties recording what the graph reflects: the commit the last
/// digest or fold left it at, and the directory the facts were parsed from.
/// Together they answer "is the graph in sync with the repository?" — and
/// which basis its `file` props are relative to.
#[cfg(feature = "digest")]
pub const SYNC_COMMIT_PROP: &str = "synced_commit";
#[cfg(feature = "digest")]
pub const SYNC_ROOT_PROP: &str = "synced_root";

/// Stamp the plane with the commit and parse basis it now reflects. A quiet
/// no-op outside a git repository — there is no commit to speak of.
#[cfg(feature = "digest")]
fn record_sync_point(db: &Database, plane_name: &str, dir: &Path) -> Result<()> {
    let Ok(head) = git_head(dir) else {
        return Ok(());
    };
    let root = dir
        .canonicalize()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| dir.display().to_string());
    let plane = db.plane(plane_name)?;
    let mut props = plane.properties()?;
    props.insert(
        SYNC_COMMIT_PROP.into(),
        PropDesc::described("commit the plane reflects", PropValue::Str(head)),
    );
    props.insert(
        SYNC_ROOT_PROP.into(),
        PropDesc::described("directory the facts were parsed from", PropValue::Str(root)),
    );
    plane.set_properties(props)?;
    Ok(())
}

/// The recorded sync point, if any: `(commit, root)`.
#[cfg(feature = "digest")]
fn recorded_sync_point(db: &Database, plane_name: &str) -> (Option<String>, Option<String>) {
    let Ok(plane) = db.plane(plane_name) else {
        return (None, None);
    };
    let Ok(props) = plane.properties() else {
        return (None, None);
    };
    let get = |k: &str| match props.get(k).map(|d| &d.value) {
        Some(PropValue::Str(v)) => Some(v.clone()),
        _ => None,
    };
    (get(SYNC_COMMIT_PROP), get(SYNC_ROOT_PROP))
}

/// The entry `drsg serve watch` hands to the server's `on_start` hook: run
/// the loop forever, and if it stops, say why — the server stays up either
/// way, and a watcher that died silently would just look like a quiet repo.
#[cfg(feature = "digest")]
#[allow(clippy::too_many_arguments)]
pub fn watch(
    db: std::sync::Arc<Database>,
    dir: std::path::PathBuf,
    plane_name: String,
    plugin_config: dr_strange_llm::PluginConfig,
    embed: Option<(String, Option<String>, Option<String>)>,
    force: bool,
    git: bool,
) {
    if let Err(e) = watch_loop(&db, &dir, &plane_name, &plugin_config, embed, force, git) {
        tracing::error!(error = format!("{e:#}"), "repository watch stopped");
    }
}

#[cfg(feature = "digest")]
#[allow(clippy::too_many_arguments)]
fn watch_loop(
    db: &Database,
    dir: &Path,
    plane_name: &str,
    plugin_config: &dr_strange_llm::PluginConfig,
    embed: Option<(String, Option<String>, Option<String>)>,
    force: bool,
    git: bool,
) -> Result<()> {
    // The server's embed config, when it has one, keeps a watched plane
    // searchable: after a fold changes facts, the changed nodes re-embed
    // (`_embedded_from` makes that pass incremental). No config, or a
    // provider that cannot be built (missing key), degrades to facts-only —
    // said once here, not per fold.
    let embedder: Option<Box<dyn dr_strange_llm::Embedder>> = match &embed {
        Some((provider, model, key_env)) => {
            match dr_strange_llm::build_provider(
                provider,
                model.as_deref(),
                None,
                key_env.as_deref(),
                true,
            ) {
                Ok(e) => Some(Box::new(e) as Box<dyn dr_strange_llm::Embedder>),
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "embed provider unavailable — folds will update facts only"
                    );
                    None
                }
            }
        }
        None => None,
    };
    let revectorize = |why: &str| {
        let Some(e) = embedder.as_deref() else {
            return;
        };
        match dr_strange_llm::vectorize_plane(db, plane_name, e, dr_strange_core::Metric::Cosine) {
            Ok(v) if v.embedded > 0 => tracing::info!(
                embedded = v.embedded,
                current = v.current,
                tokens = v.tokens,
                why,
                "re-vectorized changed nodes"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "re-vectorizing after the fold failed; facts are current, vectors lag"
            ),
        }
    };
    let root = dir
        .canonicalize()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| dir.display().to_string());
    let source = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| dir.display().to_string());

    // The plugins, loaded once and kept: every fold below asks `live` for
    // them, and it reloads only when the store changed — so a `drsg plugin
    // install` between commits is still picked up without a restart, but a
    // commit no longer pays for a load.
    let mut live = dr_strange_llm::LivePlugins::new(plugin_config.clone());

    // Where to start folding from — and, when the repository cannot say,
    // what to do instead of giving up. A brand-new project has no commit to
    // anchor on: `git rev-parse HEAD` fails both in a repository whose first
    // commit is still unborn and in a directory that is not a repository at
    // all. Neither is a reason to leave the tree unparsed.
    let (mut head, bootstrapped) = match git_head(dir) {
        Ok(head) => (head, false),
        Err(e) => {
            match bootstrap_unborn(db, dir, plane_name, &mut live, &source, &e, &revectorize)? {
                Some(first) => (first, true),
                // Not a repository: the plane is built and current as of the
                // scan, but no commit will ever arrive to fold into it.
                None => return Ok(()),
            }
        }
    };

    if bootstrapped {
        // `bootstrap_unborn` already rebuilt the plane on this commit and
        // recorded the sync point — there is no staleness left for `--force`
        // to clear, and nothing to catch up on.
    } else if force {
        // Rebuild before serving anything stale: drop, re-create, fold the
        // whole tree as one delta. Facts only — embeddings return on the next
        // real digest.
        tracing::info!(
            plane = plane_name,
            "--force: rebuilding the plane from the tree"
        );
        let stats = rebuild_from_tree(db, dir, plane_name, live.current()?, &source, &head)?;
        record_sync_point(db, plane_name, dir)?;
        tracing::info!(
            commit = %&head[..12.min(head.len())],
            nodes_loaded = stats.nodes_loaded,
            edges_written = stats.edges_written,
            prose_skipped_chars = stats.prose_chars,
            "plane rebuilt"
        );
        revectorize("--force rebuild");
    } else {
        if db.plane(plane_name).is_err() {
            db.create_plane(plane_name, Properties::new())?;
            tracing::info!(plane = plane_name, "created plane");
        }
        // Where does the graph stand relative to the repository? The plane
        // says which commit it reflects; the answer decides how to start.
        let (rec_commit, rec_root) = recorded_sync_point(db, plane_name);
        if let Some(r) = &rec_root
            && *r != root
        {
            tracing::warn!(
                plane_root = %r,
                watch_root = %root,
                "the plane was parsed from a different directory — file \
                 attribution will not line up; `--force` (or a re-digest \
                 from this directory) puts them on one basis"
            );
        }
        match rec_commit {
            Some(rec) if rec == head => {
                tracing::info!(commit = %&rec[..12.min(rec.len())], "graph and repository are in sync");
            }
            Some(rec) if commit_known(dir, &rec) => {
                tracing::info!(
                    from = %&rec[..12.min(rec.len())],
                    to = %&head[..12.min(head.len())],
                    "graph is behind the repository — catching up"
                );
                // The ordinary fold covers the gap: start from the recorded
                // commit and let the first poll diff it against HEAD.
                head = rec;
            }
            Some(rec) => {
                tracing::warn!(
                    recorded = %&rec[..12.min(rec.len())],
                    "the plane's sync point is unknown to this repository \
                     (rewritten history, or another repo) — folding forward \
                     from the current HEAD; `--force` re-establishes exact sync"
                );
            }
            None => {
                tracing::warn!(
                    "the plane records no sync point, so graph and repository \
                     cannot be compared — folding forward from the current \
                     HEAD; a digest of this directory (or `--force`) \
                     establishes one"
                );
            }
        }
    }
    // History, once, before serving: a plane bootstrapped by `drsg init` has
    // never seen a digest, so this is where it first gains one.
    if git {
        match live.current() {
            Ok(plugins) => fold_history(db, dir, plane_name, plugins, "startup"),
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "loading plugins for the history plane failed"
            ),
        }
    }

    tracing::info!(
        dir = %dir.display(),
        plane = plane_name,
        head = %head,
        history = git,
        // Said once, because it is the bound of what follows: the watcher wakes
        // on HEAD, so a tag or a branch created without a commit reaches the
        // history plane on the next commit rather than immediately.
        "watching repository — each commit folds into the graph, and \
         into the history plane on the next HEAD move"
    );
    loop {
        std::thread::sleep(WATCH_POLL);
        let now = match git_head(dir) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "reading HEAD failed; will retry");
                continue;
            }
        };
        if now == head {
            continue;
        }
        // One diff covers however far HEAD moved — several commits, a rebase,
        // a branch switch. What matters is the file set between the states.
        let step = (|| -> Result<bool> {
            let delta = git_changes(dir, &head, &now)?;
            let touches_code = !(delta.changed.is_empty() && delta.deleted.is_empty());
            if !touches_code && !git {
                return Ok(false);
            }
            // From memory unless the store changed since the last load.
            let plugins = live.current()?;
            // History first, and regardless of the delta: a commit is a fact
            // about the repository even when it touched no file the code plane
            // holds — an empty commit, or one that moved only something
            // ignored, still moved a branch.
            if git {
                fold_history(db, dir, plane_name, plugins, "commit");
            }
            if !touches_code {
                return Ok(false);
            }
            let host = dr_strange_llm::LocalFiles::new(dir)?;
            let stats =
                dr_strange_llm::sync_paths(db, plane_name, &host, &delta, plugins, &source, &now)?;
            tracing::info!(
                commit = %&now[..12.min(now.len())],
                changed = delta.changed.len(),
                deleted = delta.deleted.len(),
                nodes_loaded = stats.nodes_loaded,
                nodes_patched = stats.nodes_patched,
                nodes_deleted = stats.nodes_deleted,
                edges_written = stats.edges_written,
                edges_deleted = stats.edges_deleted,
                edges_reattached = stats.edges_reattached,
                edges_dropped = stats.edges_dropped,
                prose_skipped_chars = stats.prose_chars,
                "commit folded into the graph"
            );
            for note in &stats.notes {
                tracing::info!(note, "sync note");
            }
            Ok(stats.nodes_loaded + stats.nodes_patched + stats.nodes_deleted > 0)
        })();
        match step {
            Ok(changed) => {
                // The plane now reflects `now`; say so durably, so the next
                // start knows where to catch up from.
                if let Err(e) = record_sync_point(db, plane_name, dir) {
                    tracing::warn!(error = format!("{e:#}"), "recording the sync point failed");
                }
                if changed {
                    revectorize("commit fold");
                }
                // Folds are commit-paced, so keeping the sidecars fresh here
                // is cheap — and a hard kill then costs the next boot nothing.
                db.save_sidecars();
            }
            Err(e) => {
                // The in-memory cursor advances so polling continues, but the
                // recorded point stays behind — a restart retries this gap.
                tracing::warn!(
                    error = format!("{e:#}"),
                    from = %head, to = %now,
                    "folding the commit failed; watching continues from the new HEAD"
                );
            }
        }
        head = now;
    }
}

/// Bring `<plane>_git` up to date with the repository — commits, branches,
/// tags and rebases, facts only.
///
/// Logged rather than returned, and never fatal: the code plane is what a
/// watcher exists to keep current, and a history read that failed is not a
/// reason to stop following commits. A repository with no `git` plugin
/// installed is silent — [`route_repository`](dr_strange_llm::route_repository)
/// says so by returning nothing, and a warning per commit would be noise
/// about a choice the operator already made.
#[cfg(feature = "digest")]
fn fold_history(
    db: &Database,
    dir: &Path,
    plane_name: &str,
    plugins: &dr_strange_llm::Plugins,
    why: &str,
) {
    let plane = dr_strange_llm::git_plane_name(plane_name);
    let done = (|| -> Result<Option<dr_strange_llm::GitWriteStats>> {
        let Some(facts) = dr_strange_llm::route_repository(dir, plugins)? else {
            return Ok(None);
        };
        if db.plane(&plane).is_err() {
            db.create_plane(&plane, Properties::new())?;
            tracing::info!(plane = %plane, "created the history plane");
        }
        Ok(Some(dr_strange_llm::write_history(db, &plane, &facts)?))
    })();
    match done {
        // Nothing written is the ordinary state between commits that add no
        // history — said at debug, not info, so a quiet repository stays quiet.
        Ok(Some(stats)) if stats.nodes_created + stats.nodes_patched == 0 => {
            tracing::debug!(plane = %plane, why, "history already current")
        }
        Ok(Some(stats)) => tracing::info!(
            plane = %plane,
            why,
            nodes_created = stats.nodes_created,
            nodes_patched = stats.nodes_patched,
            edges_written = stats.edges_created,
            edges_deleted = stats.edges_deleted,
            "history folded"
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!(
            error = format!("{e:#}"),
            plane = %plane,
            "reading the repository's history failed; the code plane is unaffected"
        ),
    }
}

/// Drop the plane and rebuild it from `dir`'s working tree as one delta,
/// stamping every fact with `run_id`. Facts only — embeddings are the
/// caller's to refresh.
#[cfg(feature = "digest")]
fn rebuild_from_tree(
    db: &Database,
    dir: &Path,
    plane_name: &str,
    plugins: &dr_strange_llm::Plugins,
    source: &str,
    run_id: &str,
) -> Result<dr_strange_llm::SyncStats> {
    let host = dr_strange_llm::LocalFiles::new(dir)?;
    dr_strange_llm::resync(db, plane_name, &host, plugins, source, run_id)
}

/// Start watching a directory that has no HEAD to anchor on.
///
/// The plane is built from the working tree straight away, so a brand-new
/// project is queryable the moment `drsg init` returns rather than only
/// after its first commit. What happens next depends on why HEAD was
/// missing: an unborn repository gets waited on, and the plane is rebuilt on
/// its first commit (the tree can have moved on in between, and only a real
/// commit can be recorded as a sync point) — that commit comes back as
/// `Some`. A directory that is not a repository at all has no second act:
/// `None` says so, and the caller stops watching.
#[cfg(feature = "digest")]
fn bootstrap_unborn(
    db: &Database,
    dir: &Path,
    plane_name: &str,
    live: &mut dr_strange_llm::LivePlugins,
    source: &str,
    why: &anyhow::Error,
    revectorize: &dyn Fn(&str),
) -> Result<Option<String>> {
    let is_repo = is_git_repo(dir);
    tracing::info!(
        dir = %dir.display(),
        plane = plane_name,
        reason = format!("{why:#}"),
        "no commit to anchor on — building the plane from the working tree"
    );
    let stats = rebuild_from_tree(db, dir, plane_name, live.current()?, source, UNBORN_RUN_ID)?;
    tracing::info!(
        nodes_loaded = stats.nodes_loaded,
        edges_written = stats.edges_written,
        prose_skipped_chars = stats.prose_chars,
        "plane built from the working tree — no sync point until a commit exists"
    );
    revectorize("working-tree scan");

    if !is_repo {
        tracing::warn!(
            dir = %dir.display(),
            "not a git repository — the plane reflects this scan and nothing will fold into it; `git init` here and restart `drsg serve watch` to follow commits"
        );
        return Ok(None);
    }

    tracing::info!("waiting for this repository's first commit");
    let first = loop {
        std::thread::sleep(WATCH_POLL);
        if let Ok(head) = git_head(dir) {
            break head;
        }
    };
    // Rebuild rather than fold: there is no earlier commit to diff the first
    // one against, and the tree may have changed since the scan above.
    let stats = rebuild_from_tree(db, dir, plane_name, live.current()?, source, &first)?;
    record_sync_point(db, plane_name, dir)?;
    tracing::info!(
        commit = %&first[..12.min(first.len())],
        nodes_loaded = stats.nodes_loaded,
        edges_written = stats.edges_written,
        prose_skipped_chars = stats.prose_chars,
        "first commit — plane rebuilt on it"
    );
    revectorize("first commit");
    Ok(Some(first))
}

#[cfg(feature = "digest")]
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git — `serve watch` needs it on PATH")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Whether `dir` is inside a git repository at all — the question that
/// separates "no commit *yet*" from "no commits ever": an unborn HEAD and a
/// plain directory both fail `rev-parse HEAD`, but only the first is worth
/// waiting on.
#[cfg(feature = "digest")]
fn is_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--git-dir"]).is_ok()
}

/// Whether `sha` names a commit this repository knows.
#[cfg(feature = "digest")]
fn commit_known(dir: &Path, sha: &str) -> bool {
    git(dir, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok()
}

#[cfg(feature = "digest")]
fn git_head(dir: &Path) -> Result<String> {
    let out = git(dir, &["rev-parse", "HEAD"])?;
    Ok(String::from_utf8(out)?.trim().to_string())
}

/// The files between two commits, rename-aware: `(changed, deleted)`, where
/// changed paths' current content should be believed and deleted ones no
/// longer exist (a rename contributes one of each).
#[cfg(feature = "digest")]
fn git_changes(dir: &Path, old: &str, new: &str) -> Result<dr_strange_llm::CommitDelta> {
    // `-z` because paths are data: NUL separators cannot collide with them.
    // `--relative` because the watched directory may be a subdirectory of the
    // repository: paths must be relative to what the host serves (and files
    // outside the watched directory are rightly excluded).
    let out = git(
        dir,
        &["diff", "--relative", "--name-status", "-M", "-z", old, new],
    )?;
    let (changed, deleted) = parse_name_status(&out);
    Ok(dr_strange_llm::CommitDelta { changed, deleted })
}

/// Parse `git diff --name-status -z` output. Statuses carry one path, except
/// renames/copies which carry two (source, then destination).
#[cfg(feature = "digest")]
fn parse_name_status(raw: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut fields = raw
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let (mut changed, mut deleted) = (Vec::new(), Vec::new());
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else { break };
        match status.chars().next() {
            Some('D') => deleted.push(path),
            Some('R') | Some('C') => {
                let Some(target) = fields.next() else { break };
                // The source of a copy still exists; a rename's does not.
                if status.starts_with('R') {
                    deleted.push(path);
                }
                changed.push(target);
            }
            // A/M/T and anything exotic: believe the file's current content.
            _ => changed.push(path),
        }
    }
    (changed, deleted)
}

/// Parse a statement, embedding a text `SEARCH … NEAR "…"` when an `embed`
/// provider is given, and resolving `$name` placeholders from `params`.
/// Embedding lives behind the `digest` feature (which pulls in dr-strange-llm);
/// everything else parses without it.
#[cfg(feature = "digest")]
fn parse_stmt(
    query: &str,
    embed: Option<&str>,
    params: &dr_strange_parser::Params,
) -> Result<dr_strange_parser::Statement> {
    // Adapt the LLM provider to the parser's embedder seam (key from the env).
    struct LlmEmbedder(Box<dyn dr_strange_llm::Embedder>);
    impl dr_strange_parser::Embedder for LlmEmbedder {
        fn embed(&self, text: &str) -> std::result::Result<Vec<f32>, String> {
            let reply = self
                .0
                .embed(&[text.to_string()])
                .map_err(|e| e.to_string())?;
            reply
                .vectors
                .into_iter()
                .next()
                .ok_or_else(|| "embedder returned no vector".to_string())
        }
    }
    let embedder = match embed {
        Some(provider) => Some(LlmEmbedder(Box::new(dr_strange_llm::build_provider(
            provider, None, None, None, true,
        )?))),
        None => None,
    };
    dr_strange_parser::parse_statement_full(
        query,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_parser::Embedder),
        params,
    )
    .map_err(|e| anyhow!("{e}"))
}

#[cfg(not(feature = "digest"))]
fn parse_stmt(
    query: &str,
    embed: Option<&str>,
    params: &dr_strange_parser::Params,
) -> Result<dr_strange_parser::Statement> {
    if embed.is_some() {
        bail!(
            "text SEARCH embedding needs the `digest` build feature \
             (this binary was built with --no-default-features)"
        );
    }
    dr_strange_parser::parse_statement_full(query, None, params).map_err(|e| anyhow!("{e}"))
}

/// Execute a `LogicalPlan` and print what it returns: each matched node as a
/// JSON line, tagged with its similarity score when the plan produced one, or
/// the whole table as one object when the plan projects.
///
/// One object rather than a line per row: a table's columns are part of its
/// answer, and a projection is a barrier, so nothing is left to stream.
fn run_plan(p: PlaneHandle<'_>, plan: LogicalPlan, out: &mut dyn Write) -> Result<()> {
    let q = p.query_from_plan(plan);
    if q.plan().project.is_some() {
        writeln!(out, "{}", jsonio::table_to_json(&q.table()?))?;
        return Ok(());
    }
    for (node, score) in q.scored_nodes()? {
        let mut obj = jsonio::node_to_json(&node);
        if let (Some(s), Value::Object(map)) = (score, &mut obj) {
            map.insert("score".into(), json!(s));
        }
        writeln!(out, "{obj}")?;
    }
    Ok(())
}

/// The compact agent verbs (`find`/`callers`/`callees`/`describe`): one
/// shared driver, the rendering shared with the MCP tools via
/// [`dr_strange_core::compact`].
pub fn compact(
    db: &Database,
    plane_name: &str,
    name: &str,
    render: fn(&PlaneHandle<'_>, &str) -> dr_strange_core::Result<String>,
    out: &mut dyn Write,
) -> Result<()> {
    let p = plane(db, plane_name)?;
    write!(out, "{}", render(&p, name)?)?;
    Ok(())
}

pub fn catalog(db: &Database, plane_name: Option<&str>, out: &mut dyn Write) -> Result<()> {
    let cat = match plane_name {
        Some(name) => plane(db, name)?.catalog()?,
        None => db.catalog()?,
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&cat)?)?;
    Ok(())
}

// ---- graph algorithms (ROADMAP §1) ---------------------------------------

/// Scope an algorithm run to the whole plane, or one label if given.
fn algo_scoped<'db>(
    db: &'db Database,
    plane_name: &str,
    label: Option<&str>,
) -> Result<dr_strange_core::AlgoBuilder<'db>> {
    let mut b = plane(db, plane_name)?.algo();
    if let Some(l) = label {
        b = b.label(l);
    }
    Ok(b)
}

#[allow(clippy::too_many_arguments)]
pub fn algo_pagerank(
    db: &Database,
    plane_name: &str,
    label: Option<&str>,
    top: usize,
    damping: f64,
    max_iters: u32,
    out: &mut dyn Write,
) -> Result<()> {
    let opts = PageRankOptions {
        damping,
        max_iters,
        ..Default::default()
    };
    let scored = algo_scoped(db, plane_name, label)?.pagerank(opts)?;
    writeln!(out, "pagerank: {} nodes (top {top})", scored.len())?;
    for (id, s) in scored.iter().take(top) {
        writeln!(out, "  {}\t{s:.6}", id.0)?;
    }
    Ok(())
}

pub fn algo_components(
    db: &Database,
    plane_name: &str,
    label: Option<&str>,
    top: usize,
    out: &mut dyn Write,
) -> Result<()> {
    let (rows, count) = algo_scoped(db, plane_name, label)?.connected_components()?;
    writeln!(out, "components: {count} across {} nodes", rows.len())?;
    for (id, rep) in rows.iter().take(top) {
        writeln!(out, "  {}\tcomponent {}", id.0, rep.0)?;
    }
    if rows.len() > top {
        writeln!(out, "  … and {} more", rows.len() - top)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn algo_shortest_path(
    db: &Database,
    plane_name: &str,
    label: Option<&str>,
    src: u64,
    dst: u64,
    dir: Dir,
    weight: Option<String>,
    out: &mut dyn Write,
) -> Result<()> {
    let opts = ShortestPathOptions { dir, weight };
    match algo_scoped(db, plane_name, label)?.shortest_path(NodeId(src), NodeId(dst), &opts)? {
        Some(p) => {
            let chain = p
                .nodes
                .iter()
                .map(|n| n.0.to_string())
                .collect::<Vec<_>>()
                .join(" -> ");
            writeln!(
                out,
                "path (cost {}, {} hops): {chain}",
                p.cost,
                p.edges.len()
            )?;
        }
        None => writeln!(out, "no path from {src} to {dst}")?,
    }
    Ok(())
}

pub fn algo_louvain(
    db: &Database,
    plane_name: &str,
    label: Option<&str>,
    top: usize,
    out: &mut dyn Write,
) -> Result<()> {
    let (rows, count) = algo_scoped(db, plane_name, label)?.louvain(LouvainOptions::default())?;
    writeln!(out, "communities: {count} across {} nodes", rows.len())?;
    for (id, rep) in rows.iter().take(top) {
        writeln!(out, "  {}\tcommunity {}", id.0, rep.0)?;
    }
    if rows.len() > top {
        writeln!(out, "  … and {} more", rows.len() - top)?;
    }
    Ok(())
}

pub fn index_ensure(
    db: &Database,
    plane_name: &str,
    label: &str,
    property: &str,
    metric: Metric,
    out: &mut dyn Write,
) -> Result<()> {
    plane(db, plane_name)?.ensure_vector_index(label, property, metric)?;
    writeln!(out, "ensured vector index on {label}.{property}")?;
    Ok(())
}

/// The labels of `plane_name` whose nodes actually carry `property` — what
/// "ensure an index for every label" should mean. A label without the
/// property would only gain an empty index and a misleading line of output.
fn labels_carrying(db: &Database, plane_name: &str, property: &str) -> Result<Vec<String>> {
    let cat = plane(db, plane_name)?.catalog()?;
    Ok(cat
        .labels
        .iter()
        .filter(|(_, st)| st.properties.contains_key(property))
        .map(|(l, _)| l.clone())
        .collect())
}

/// `drsg index ensure <property>` — one vector index per label that carries
/// the property, so a freshly vectorized plane becomes searchable in one
/// command instead of one per label.
pub fn index_ensure_all(
    db: &Database,
    plane_name: &str,
    property: &str,
    metric: Metric,
    out: &mut dyn Write,
) -> Result<()> {
    let labels = labels_carrying(db, plane_name, property)?;
    if labels.is_empty() {
        writeln!(
            out,
            "no label in plane '{plane_name}' carries `{property}` — nothing to index"
        )?;
        return Ok(());
    }
    let p = plane(db, plane_name)?;
    for label in &labels {
        p.ensure_vector_index(label, property, metric)?;
        writeln!(out, "ensured vector index on {label}.{property}")?;
    }
    writeln!(out, "{} label(s) indexed", labels.len())?;
    Ok(())
}

/// `drsg index keyword <property>` — the same sweep for BM25.
pub fn keyword_index_ensure_all(
    db: &Database,
    plane_name: &str,
    property: &str,
    language: Language,
    out: &mut dyn Write,
) -> Result<()> {
    let labels = labels_carrying(db, plane_name, property)?;
    if labels.is_empty() {
        writeln!(
            out,
            "no label in plane '{plane_name}' carries `{property}` — nothing to index"
        )?;
        return Ok(());
    }
    let p = plane(db, plane_name)?;
    for label in &labels {
        p.ensure_keyword_index(label, property, language)?;
        writeln!(
            out,
            "ensured keyword index on {label}.{property} ({language:?})"
        )?;
    }
    writeln!(out, "{} label(s) indexed", labels.len())?;
    Ok(())
}

pub fn keyword_index_ensure(
    db: &Database,
    plane_name: &str,
    label: &str,
    property: &str,
    language: Language,
    out: &mut dyn Write,
) -> Result<()> {
    plane(db, plane_name)?.ensure_keyword_index(label, property, language)?;
    writeln!(
        out,
        "ensured keyword index on {label}.{property} ({language:?})"
    )?;
    Ok(())
}

// ---- hybrid retrieval (ROADMAP §2) ---------------------------------------

fn fmt_channel(v: Option<f32>) -> String {
    v.map_or_else(|| "-".to_string(), |x| format!("{x:.3}"))
}

/// `drsg vectorize` — embed every node in a plane so it answers similarity
/// search, incrementally, then ensure the vector indexes. The engine lives
/// in [`dr_strange_llm::vectorize_plane`], shared with `plane.vectorize`
/// over RPC; this is its terminal voice.
#[cfg(feature = "digest")]
pub fn vectorize(
    db: &Database,
    plane_name: &str,
    embedder: &dyn dr_strange_llm::Embedder,
    metric: Metric,
    out: &mut dyn Write,
) -> Result<()> {
    let stats = dr_strange_llm::vectorize_plane(db, plane_name, embedder, metric)?;
    if stats.embedded == 0 {
        writeln!(
            out,
            "nothing to embed: {} node(s) already current, {} with no text",
            stats.current, stats.empty
        )?;
    } else {
        writeln!(
            out,
            "embedded {} node(s) ({} unique texts, {} tokens); {} already current, {} with no text",
            stats.embedded, stats.unique, stats.tokens, stats.current, stats.empty
        )?;
    }
    if !stats.labels.is_empty() {
        writeln!(
            out,
            "  vector indexes ensured ({metric:?}) on `embedding` for {} label(s): {}",
            stats.labels.len(),
            stats.labels.join(", ")
        )?;
    }
    Ok(())
}

/// Embed a query string for the vector channel. Needs the `digest` feature
/// (the LLM provider layer); otherwise a clear error.
#[cfg(feature = "digest")]
fn embed_query(query: &str, provider: &str, model: Option<&str>) -> Result<Vec<f32>> {
    use dr_strange_llm::Embedder;
    let embedder = dr_strange_llm::build_provider(provider, model, None, None, true)?;
    let reply = embedder.embed(std::slice::from_ref(&query.to_string()))?;
    reply
        .vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("embedder returned no vector"))
}

#[cfg(not(feature = "digest"))]
fn embed_query(_query: &str, _provider: &str, _model: Option<&str>) -> Result<Vec<f32>> {
    bail!("the vector channel needs the `digest` build feature (LLM embedding)")
}

#[allow(clippy::too_many_arguments)]
pub fn hybrid(
    db: &Database,
    plane_name: &str,
    query: &str,
    label: Option<&str>,
    vector_prop: Option<&str>,
    keyword_prop: Option<&str>,
    metric: Metric,
    graph: Option<(u32, f32)>,
    k: usize,
    embed_provider: &str,
    embed_model: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let p = plane(db, plane_name)?;
    let mut b = p.hybrid();
    if let Some(l) = label {
        b = b.label(l);
    }
    if let Some(prop) = vector_prop {
        let vec = embed_query(query, embed_provider, embed_model)?;
        b = b.vector(prop, vec, metric);
    }
    if let Some(prop) = keyword_prop {
        b = b.keyword(prop, query);
    }
    if let Some((hops, decay)) = graph {
        b = b.graph(hops, decay);
    }
    let hits = b.k(k).run()?;
    writeln!(out, "hybrid: {} results", hits.len())?;
    for h in &hits {
        let name = p
            .node(h.node)?
            .and_then(|n| n.external_key)
            .unwrap_or_else(|| format!("#{}", h.node.0));
        writeln!(
            out,
            "  {:.4}\t{name}\t[v={} k={} g={}]",
            h.score,
            fmt_channel(h.vector),
            fmt_channel(h.keyword),
            fmt_channel(h.graph),
        )?;
    }
    Ok(())
}

pub fn stats(db: &Database, out: &mut dyn Write) -> Result<()> {
    let planes = db.planes()?;
    // The maintained summary row, not the catalog scan — same numbers,
    // constant time (arch/03 §5).
    let counters = db.counters()?;
    writeln!(
        out,
        "{} planes, {} nodes, {} edges",
        planes.len(),
        counters.nodes,
        counters.edges
    )?;
    Ok(())
}

pub fn check(db: &Database, out: &mut dyn Write) -> Result<()> {
    // A full scan of every plane (via the catalog) exercises decode paths and
    // surfaces corruption as an error. arch/05 §2.
    let mut nodes = 0u64;
    for (_, name) in db.planes()? {
        nodes += db.plane(&name)?.catalog()?.node_count;
    }
    writeln!(out, "ok: {nodes} nodes readable across all planes")?;
    Ok(())
}

// ---- digest (LLM ingest, arch/07) ----------------------------------------

/// Flags for [`digest`]. Chat and embeddings are configured separately so a
/// document can be extracted by one provider and embedded by another (e.g.
/// `--chat deepseek --embed qwen`, since DeepSeek has no embeddings endpoint).
#[cfg(feature = "digest")]
pub struct DigestArgs<'a> {
    /// A filesystem path, or an `http(s)://` URL to fetch (ROADMAP §9). The
    /// scheme is required for a URL: a bare `example.com` is a valid filename,
    /// and guessing which one a reader meant is worse than asking.
    pub source: &'a str,
    /// URL only: sharpens what the crawl counts as relevant.
    pub topic: Option<&'a str>,
    /// URL only: ceiling on pages kept, the root included.
    pub pages: usize,
    /// URL only: link-following depth.
    pub depth: usize,
    pub plane: &'a str,
    pub apply: bool,
    pub chunk_chars: usize,
    /// Per-chunk extraction chat calls to run concurrently.
    pub concurrency: usize,
    pub embed: bool,
    /// Link extracted entities to existing plane nodes via vector retrieval.
    pub link: bool,
    /// How thoroughly to clean up the extraction: `coarse` / `fine` / `super`.
    pub mode: &'a str,
    /// Provider preset name (openai/deepseek/qwen/ollama) or a raw base URL.
    pub chat_provider: &'a str,
    pub embed_provider: &'a str,
    pub model: Option<&'a str>,
    pub embed_model: Option<&'a str>,
    pub chat_url: Option<&'a str>,
    pub embed_url: Option<&'a str>,
    pub chat_key_env: Option<&'a str>,
    pub embed_key_env: Option<&'a str>,
    /// Force a preprocessor by name instead of routing by extension
    /// (ROADMAP §11). A router that guesses is worse than one that asks.
    pub handler: Option<&'a str>,
    /// The `[plugins]` section, resolved: budgets, store, and each plugin's
    /// own settings.
    pub plugin_config: dr_strange_llm::PluginConfig,
    /// Also read the repository's history into its own plane, when the source
    /// is a git checkout and the `git` plugin is installed (ROADMAP §11).
    pub git: bool,
    /// Where that history lands. `None` means `<plane>_git`.
    pub git_plane: Option<&'a str>,
}

/// Natural-language query (ROADMAP §3): an LLM turns `question` into a
/// read-only plan grounded in the plane's schema, runs it (unless `dry_run`),
/// and prints the generated plan plus the matching nodes.
#[cfg(feature = "digest")]
#[allow(clippy::too_many_arguments)]
pub fn ask(
    db: &Database,
    plane_name: &str,
    question: &str,
    dry_run: bool,
    max_attempts: u32,
    limit: u64,
    chat_provider: &str,
    model: Option<&str>,
    embed_provider: Option<&str>,
    embed_model: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let p = plane(db, plane_name)?;
    let chat = dr_strange_llm::build_provider(chat_provider, model, None, None, false)?;
    // Grounding tools are enabled when an embed provider is given.
    let embedder = embed_provider
        .and_then(|ep| dr_strange_llm::build_provider(ep, embed_model, None, None, true).ok());
    let opts = dr_strange_llm::AskOptions {
        max_attempts,
        dry_run,
        limit,
    };
    let res = dr_strange_llm::ask(
        &chat,
        embedder
            .as_ref()
            .map(|e| e as &dyn dr_strange_llm::Embedder),
        &p,
        question,
        &opts,
    )?;
    let plural = if res.attempts == 1 { "" } else { "s" };
    writeln!(
        out,
        "{} plan(s) ({} turn{plural}):",
        res.plans.len(),
        res.attempts
    )?;
    writeln!(out, "{}", serde_json::to_string_pretty(&res.plans)?)?;
    if res.ran {
        writeln!(
            out,
            "subgraph: {} nodes, {} edges",
            res.nodes.len(),
            res.edges.len()
        )?;
        for n in &res.nodes {
            writeln!(out, "{}", jsonio::node_to_json(n))?;
        }
        for e in &res.edges {
            writeln!(out, "  {} --{}--> {}", e.src.0, e.ty, e.dst.0)?;
        }
    } else {
        writeln!(out, "(dry run — not executed)")?;
    }
    Ok(())
}

/// Read what is to be digested: a file, or — when the argument carries an
/// `http(s)` scheme — a page and the pages it links to (ROADMAP §9).
///
/// The CLI has nowhere to show a selection list, so it keeps what cleared the
/// relevance floor and *says* what it kept and what it dropped. A crawl that
/// quietly read less than the reader expected would be worse than one that
/// read nothing.
/// The routing handlers for this invocation: built-ins plus every installed
/// plugin, loaded once. Built only on the branches that route — a URL digest
/// never needs them, and must not fail because an installed plugin is broken.
#[cfg(feature = "digest")]
fn load_plugins(args: &DigestArgs) -> Result<dr_strange_llm::Plugins> {
    dr_strange_llm::Plugins::load(&args.plugin_config)
}

// ---- plugins (ROADMAP §11) -----------------------------------------------

/// A plugin artifact is code this process will execute, so the download cap is
/// not a courtesy: nothing legitimate is near it, and an endless body should
/// stop mattering early.
#[cfg(feature = "digest")]
const PLUGIN_DOWNLOAD_CAP: usize = 256 << 20;

/// The official catalog, fetched from the extensions repository and cached in
/// the plugin store — the same list the dashboard's `plugin.catalog` serves.
///
/// The fetch goes through the ordinary network policy, as a plugin download
/// does: this is a file from the internet deciding what this process will be
/// asked to execute, and it gets no shortcut for being small.
#[cfg(feature = "digest")]
fn official_catalog(
    store: &dr_strange_llm::PluginStore,
    allow_private: &[dr_strange_web::fetch::Prefix],
) -> Result<dr_strange_llm::Fetched> {
    dr_strange_llm::load_catalog(store, |url| {
        dr_strange_web::fetch::fetch_bytes(url, dr_strange_llm::CATALOG_DOWNLOAD_CAP, allow_private)
    })
}

/// What each installed plugin hashes to, so a catalog entry can be tagged
/// against the store without instantiating anything.
#[cfg(feature = "digest")]
fn installed_hashes(
    store: &dr_strange_llm::PluginStore,
) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(store
        .list()?
        .into_iter()
        .map(|p| (p.name, p.sha256))
        .collect())
}

/// One catalog entry's status against the local store: `[installed]` when
/// the stored hash matches the release artifact's, `[upgradable]` when a
/// plugin of that name is installed but its bytes differ (an older release,
/// or a local build), nothing when it is absent.
#[cfg(feature = "digest")]
fn official_status(
    installed: &std::collections::BTreeMap<String, String>,
    name: &str,
    release_sha: &str,
) -> &'static str {
    match installed.get(name) {
        Some(have) if have.eq_ignore_ascii_case(release_sha) => "[installed]",
        Some(_) => "[upgradable]",
        None => "",
    }
}

/// The catalog as a table, tagged against the store — shared by the
/// interactive chooser and `plugin list --available`, so the two cannot drift
/// into describing the same list differently.
///
/// `numbered` is what makes it a menu rather than a report. An entry this
/// build cannot run is printed either way, with the reason: hiding it would
/// leave an operator wondering why the plugin the README promises is not
/// there.
#[cfg(feature = "digest")]
fn print_catalog(
    picks: &[dr_strange_llm::Pick<'_>],
    installed: &std::collections::BTreeMap<String, String>,
    numbered: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let ver_w = picks
        .iter()
        .map(|p| p.plugin.version.len())
        .max()
        .unwrap_or(0);
    let claims_w = picks
        .iter()
        .map(|p| p.plugin.claims.len())
        .max()
        .unwrap_or(0);
    let name_w = picks.iter().map(|p| p.plugin.name.len()).max().unwrap_or(0);
    for (i, pick) in picks.iter().enumerate() {
        let p = pick.plugin;
        let lead = if numbered {
            format!("  {}) ", i + 1)
        } else {
            "  ".to_string()
        };
        let mut tail = official_status(installed, &p.name, &p.sha256).to_string();
        if let Some(why) = pick.compat.note() {
            if !tail.is_empty() {
                tail.push(' ');
            }
            tail.push_str(&format!("[unsupported: {why}]"));
        }
        // Trimmed, not padded to the last column: an entry with no tag would
        // otherwise carry trailing spaces into whatever reads this.
        let row = format!(
            "{lead}{:name_w$}  {:ver_w$}  {:claims_w$}  {tail}",
            p.name, p.version, p.claims
        );
        writeln!(out, "{}", row.trim_end())?;
    }
    Ok(())
}

/// Where a plugin about to be installed comes from.
///
/// The distinction is the hash. An operator's path or URL is trusted because
/// they typed it, and the store pins whatever arrives; a catalog entry carries
/// the hash the extensions repository published, so what arrives is checked
/// against what was promised before anything is stored.
#[cfg(feature = "digest")]
enum PluginSource {
    /// A local `.wasm` or an `http(s)://` URL, exactly as given.
    Given(String),
    /// An official plugin, with the catalog's pin.
    Official(dr_strange_llm::OfficialPlugin),
}

#[cfg(feature = "digest")]
impl PluginSource {
    fn location(&self) -> &str {
        match self {
            PluginSource::Given(s) => s,
            PluginSource::Official(p) => &p.url,
        }
    }
}

/// The interactive chooser behind bare `drsg plugin install`: the official
/// catalog by number, `0` for all of it, a plugin's name, a pasted path/URL,
/// `q` to walk away. Returns the sources to install.
#[cfg(feature = "digest")]
fn choose_plugins(
    store: &dr_strange_llm::PluginStore,
    allow_private: &[dr_strange_web::fetch::Prefix],
    out: &mut dyn Write,
) -> Result<Vec<PluginSource>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "no source given and stdin is not a terminal — pass a name, path or \
             URL, e.g. `drsg plugin install <name | file.wasm | url>`"
        );
    }
    let installed = installed_hashes(store)?;
    let fetched = official_catalog(store, allow_private)?;
    let picks = fetched.catalog.current();

    writeln!(out, "official plugins:")?;
    if let Some(note) = fetched.source.note() {
        writeln!(out, "  ({note})")?;
    }
    if fetched.catalog.from_the_future() {
        writeln!(
            out,
            "  (this catalog is schema {} — newer than this drsg reads; \
             entries may say more than is shown)",
            fetched.catalog.schema
        )?;
    }
    writeln!(out, "  0) all of the below")?;
    print_catalog(&picks, &installed, true, out)?;
    write!(out, "install [number, name, path/URL, or q to cancel]: ")?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit")
    {
        writeln!(out, "cancelled")?;
        return Ok(Vec::new());
    }
    if let Ok(n) = answer.parse::<usize>() {
        if n == 0 {
            // "All of the below" means all this host can run. An entry it
            // cannot is skipped *by name*, so the count never silently
            // disagrees with the list just printed.
            let mut sources = Vec::new();
            for pick in &picks {
                if pick.compat.is_ok() {
                    sources.push(PluginSource::Official(pick.plugin.clone()));
                } else {
                    writeln!(out, "skipping {} — unsupported here", pick.plugin.name)?;
                }
            }
            return Ok(sources);
        }
        return match picks.get(n - 1) {
            Some(pick) => Ok(vec![PluginSource::Official(pick.plugin.clone())]),
            None => anyhow::bail!("no option {n} — pick 0..={}", picks.len()),
        };
    }
    // A name from the list is the other thing an operator naturally types.
    if let Some(pick) = fetched.catalog.best(answer) {
        return Ok(vec![PluginSource::Official(pick.plugin.clone())]);
    }
    Ok(vec![PluginSource::Given(answer.to_string())])
}

/// Resolve one `drsg plugin install <arg>`: a path, a URL, or an official
/// plugin's name.
///
/// A bare word is looked up in the catalog rather than read as a filename,
/// because `drsg plugin install rust` is what an operator means by it — and a
/// name that is in neither the catalog nor the filesystem is worth an error
/// that lists what the catalog does have.
#[cfg(feature = "digest")]
fn resolve_source(
    store: &dr_strange_llm::PluginStore,
    allow_private: &[dr_strange_web::fetch::Prefix],
    arg: &str,
    out: &mut dyn Write,
) -> Result<PluginSource> {
    if arg.starts_with("http://") || arg.starts_with("https://") || Path::new(arg).exists() {
        return Ok(PluginSource::Given(arg.to_string()));
    }
    let fetched = official_catalog(store, allow_private)?;
    if let Some(note) = fetched.source.note() {
        writeln!(out, "{note}")?;
    }
    match fetched.catalog.best(arg) {
        Some(pick) => {
            if let Some(why) = pick.compat.note() {
                writeln!(
                    out,
                    "warning: {}@{} is unsupported here — {why}",
                    pick.plugin.name, pick.plugin.version
                )?;
            }
            Ok(PluginSource::Official(pick.plugin.clone()))
        }
        None => {
            let known: Vec<&str> = fetched
                .catalog
                .current()
                .iter()
                .map(|p| p.plugin.name.as_str())
                .collect();
            anyhow::bail!(
                "`{arg}` is not a file, not a URL, and not in the official \
                 catalog (which has: {})",
                known.join(", ")
            )
        }
    }
}

/// Installed plugins (other than `manifest`'s own name) that already claim
/// any of its extensions — the head-on collision `install` must not create
/// silently: the router routes each extension to exactly one handler.
#[cfg(feature = "digest")]
fn extension_conflicts(
    store: &dr_strange_llm::PluginStore,
    name: &str,
    extensions: &[String],
) -> Result<Vec<dr_strange_llm::InstalledPlugin>> {
    let mut out = Vec::new();
    for installed in store.list()? {
        if installed.name == name {
            continue; // same name is the upgrade path, not a conflict
        }
        if installed.extensions.iter().any(|e| extensions.contains(e)) {
            out.push(installed);
        }
    }
    Ok(out)
}

#[cfg(feature = "digest")]
fn plugin_store(cfg: &dr_strange_llm::PluginConfig) -> Result<dr_strange_llm::PluginStore> {
    match &cfg.store_dir {
        Some(dir) => dr_strange_llm::PluginStore::open(dir.clone()),
        None => dr_strange_llm::PluginStore::open_default(),
    }
}

/// Install a plugin from a local `.wasm` or a URL.
///
/// A URL goes through the same network policy as every other fetch (ROADMAP
/// §9): resolved-address checks, the private-range guard at every redirect
/// hop, a size cap. The artifact is then validated as a component, asked to
/// describe itself, hashed, and only then stored — nothing unloadable enters
/// the store to fail later at digest time.
#[cfg(feature = "digest")]
pub fn plugin_install(
    cfg: &dr_strange_llm::PluginConfig,
    allow_private: &[dr_strange_web::fetch::Prefix],
    source: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let store = plugin_store(cfg)?;
    let sources = match source {
        Some(s) => vec![resolve_source(&store, allow_private, s, out)?],
        None => choose_plugins(&store, allow_private, out)?,
    };
    for source in &sources {
        install_one(cfg, allow_private, source, out)?;
    }
    Ok(())
}

#[cfg(feature = "digest")]
fn install_one(
    cfg: &dr_strange_llm::PluginConfig,
    allow_private: &[dr_strange_web::fetch::Prefix],
    source: &PluginSource,
    out: &mut dyn Write,
) -> Result<()> {
    let location = source.location();
    let is_url = location.starts_with("http://") || location.starts_with("https://");
    let bytes = if is_url {
        writeln!(out, "downloading {location}")?;
        dr_strange_web::fetch::fetch_bytes(location, PLUGIN_DOWNLOAD_CAP, allow_private)?
    } else {
        std::fs::read(location).with_context(|| format!("reading {location}"))?
    };

    // Before the bytes are looked at as a component, and long before they are
    // stored: an official plugin has to be the artifact the catalog named.
    if let PluginSource::Official(entry) = source {
        entry.verify(&bytes)?;
    }

    let store = plugin_store(cfg)?;

    // The router routes each extension to exactly one handler, so an
    // install that would create a second claimant is a decision, not a
    // default: cancel, or remove the incumbent and continue.
    let manifest = {
        use dr_strange_llm::preprocess::Preprocessor as _;
        dr_strange_llm::WasmPlugin::from_bytes(
            &bytes,
            Vec::new(),
            dr_strange_llm::Limits::default(),
        )?
        .manifest()
    };
    let conflicts = extension_conflicts(&store, &manifest.name, &manifest.extensions)?;
    if !conflicts.is_empty() {
        use std::io::IsTerminal;
        let named = conflicts
            .iter()
            .map(|p| {
                format!(
                    "{}@{} ({})",
                    p.name,
                    p.version,
                    p.extensions
                        .iter()
                        .map(|e| format!(".{e}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "{}@{} claims extensions already handled by {named} — remove \
                 the incumbent first (`drsg plugin remove <name>`) or run \
                 interactively to choose",
                manifest.name,
                manifest.version
            );
        }
        writeln!(
            out,
            "{}@{} claims extensions already handled by {named}",
            manifest.name, manifest.version
        )?;
        write!(
            out,
            "  c) cancel installation\n  r) remove and continue\nchoice [c/r]: "
        )?;
        out.flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        match line.trim() {
            "r" | "R" => {
                for p in &conflicts {
                    let removed = store.remove(&p.name)?;
                    writeln!(out, "removed {}@{}", removed.name, removed.version)?;
                }
            }
            _ => {
                writeln!(out, "cancelled")?;
                return Ok(());
            }
        }
    }

    let (entry, replaced) = store.install(&bytes, location)?;
    match replaced {
        Some(old) if old != entry.version => writeln!(
            out,
            "installed {}@{} (replacing {old})  sha256:{}",
            entry.name,
            entry.version,
            &entry.sha256[..12]
        )?,
        Some(_) => writeln!(
            out,
            "reinstalled {}@{}  sha256:{}",
            entry.name,
            entry.version,
            &entry.sha256[..12]
        )?,
        None => writeln!(
            out,
            "installed {}@{}  sha256:{}",
            entry.name,
            entry.version,
            &entry.sha256[..12]
        )?,
    }
    // A plugin claiming no extension is not a broken one: the `git` plugin is
    // dispatched by the source being a repository rather than by any file's
    // name, and printing an empty list would read as a failed install.
    let handles = entry
        .extensions
        .iter()
        .map(|e| format!(".{e}"))
        .collect::<Vec<_>>();
    if handles.is_empty() {
        writeln!(
            out,
            "  claims no file extension — the host dispatches it by what the \
             source is, not by what a file is called"
        )?;
    } else {
        writeln!(out, "  handles: {}", handles.join(", "))?;
    }
    Ok(())
}

/// `plugin list --available`: the official catalog rather than the store —
/// the same table the interactive installer shows, without the prompt, so the
/// list is readable from a script and from a pipe.
#[cfg(feature = "digest")]
fn plugin_list_available(
    cfg: &dr_strange_llm::PluginConfig,
    allow_private: &[dr_strange_web::fetch::Prefix],
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let store = plugin_store(cfg)?;
    let fetched = official_catalog(&store, allow_private)?;
    let picks = fetched.catalog.current();
    if json {
        // Shaped like `plugin.catalog` over RPC, staleness included: a script
        // that reads this should be able to tell a live answer from a cached
        // one without asking a second question.
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": fetched.source,
                "stale": fetched.source.is_stale(),
                "schema": fetched.catalog.schema,
                "plugins": picks,
            }))?
        )?;
        return Ok(());
    }
    if let Some(note) = fetched.source.note() {
        writeln!(out, "{note}")?;
    }
    print_catalog(&picks, &installed_hashes(&store)?, false, out)
}

#[cfg(feature = "digest")]
pub fn plugin_list(
    cfg: &dr_strange_llm::PluginConfig,
    allow_private: &[dr_strange_web::fetch::Prefix],
    available: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    if available {
        return plugin_list_available(cfg, allow_private, json, out);
    }
    let store = plugin_store(cfg)?;
    let plugins = store.list()?;
    if json {
        // The same records `plugin.list` serves over RPC — one shape for
        // agents whichever surface they read.
        writeln!(out, "{}", serde_json::to_string_pretty(&plugins)?)?;
        return Ok(());
    }
    if plugins.is_empty() {
        writeln!(
            out,
            "no plugins installed — `drsg plugin install` offers the official \
             catalog, or `drsg plugin install <name | file.wasm | url>` adds one"
        )?;
        return Ok(());
    }
    // A terminal table: fixed columns sized to the content.
    let rows: Vec<[String; 5]> = plugins
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.version.clone(),
                p.extensions
                    .iter()
                    .map(|e| format!(".{e}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                p.sha256[..12].to_string(),
                p.source.clone(),
            ]
        })
        .collect();
    let header = ["NAME", "VERSION", "EXTENSIONS", "SHA256", "SOURCE"];
    let mut widths = header.map(str::len);
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.len());
        }
    }
    let print_row = |out: &mut dyn Write, cells: [&str; 5]| -> Result<()> {
        let mut line = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            line.push_str(&format!("{cell:<w$}"));
        }
        writeln!(out, "{}", line.trim_end())?;
        Ok(())
    };
    print_row(out, header)?;
    for row in &rows {
        print_row(out, [&row[0], &row[1], &row[2], &row[3], &row[4]])?;
    }
    Ok(())
}

#[cfg(feature = "digest")]
pub fn plugin_remove(
    cfg: &dr_strange_llm::PluginConfig,
    name: &str,
    out: &mut dyn Write,
) -> Result<()> {
    let store = plugin_store(cfg)?;
    let entry = store.remove(name)?;
    writeln!(out, "removed {}@{}", entry.name, entry.version)?;
    Ok(())
}

#[cfg(feature = "digest")]
fn read_source(
    args: &DigestArgs,
    plugins: &dr_strange_llm::Plugins,
    out: &mut dyn Write,
) -> Result<(dr_strange_llm::Preprocessed, String)> {
    let is_url = args.source.starts_with("http://") || args.source.starts_with("https://");
    if !is_url {
        let path = Path::new(args.source);
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        // A directory is a legal source since ROADMAP §11: a preprocessor pulls
        // the files it wants through the host, so "digest this project" needs
        // no file list from the caller.
        if path.is_dir() {
            let host = dr_strange_llm::LocalFiles::new(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let facts = dr_strange_llm::route_tree(&host, args.handler, plugins)?;
            return Ok((facts, name));
        }

        // Bytes, not `read_to_string`: a PDF or .docx is not UTF-8, and the old
        // read failed on one before the user learned whether it was supported.
        // Markdown and plain text pass straight through the converter.
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        // The host is the file's own directory: a preprocessor handed one
        // source file may still need to follow an import beside it.
        let host = dr_strange_llm::LocalFiles::new(path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("reading {}", path.display()))?;
        let facts = dr_strange_llm::route_document(&name, &bytes, args.handler, &host, plugins)
            .with_context(|| format!("reading {}", path.display()))?;
        return Ok((facts, name));
    }

    let opts = dr_strange_web::fetch::FetchOptions {
        topic: args.topic.map(str::to_string),
        max_pages: args.pages.max(1),
        max_depth: args.depth,
        ..Default::default()
    };
    // Progress goes to stderr so a piped `--dry-run` still yields clean stdout.
    let fetched = dr_strange_web::fetch::fetch_with_progress(args.source, &opts, &mut |p| {
        eprintln!("fetching {}/{} {}", p.done, p.total, p.url);
    })?;

    let kept = fetched.pages.iter().filter(|p| p.kept).count();
    writeln!(
        out,
        "fetched {} page(s) from {} — {kept} kept, {} dropped",
        fetched.pages.len(),
        args.source,
        fetched.pages.len() - kept + fetched.dropped.len()
    )?;
    for page in fetched.pages.iter().filter(|p| p.kept) {
        writeln!(
            out,
            "  {:.2}  {}  ({} chars){}",
            page.score,
            page.url,
            page.chars,
            if page.depth == 0 {
                "  [the page you named]"
            } else {
                ""
            }
        )?;
    }
    for d in &fetched.dropped {
        writeln!(out, "  ----  {}  — {}", d.url, d.reason)?;
    }
    for page in fetched.pages.iter().filter(|p| !p.kept) {
        writeln!(
            out,
            "  {:.2}  {}  — below the relevance floor",
            page.score, page.url
        )?;
    }
    writeln!(out)?;

    let doc = fetched.document();
    if doc.trim().is_empty() {
        bail!("{} yielded no readable text", args.source);
    }
    Ok((
        dr_strange_llm::Preprocessed::prose_only("fetch", doc),
        args.source.to_string(),
    ))
}

/// Digests a document into the plane: an LLM extracts entities/relations
/// (labels chosen purely from the document), they're embedded and stamped with
/// provenance, and — only with `apply` — written through the bulk path.
/// Dry-run by default (arch/07 §2: proposals, not mutations).
#[cfg(feature = "digest")]
pub fn digest(db: &Database, args: &DigestArgs, out: &mut dyn Write) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Mode is parsed before anything expensive: a typo should not cost a crawl.
    let mode = dr_strange_llm::DigestMode::parse(args.mode).ok_or_else(|| {
        anyhow!(
            "unknown digest mode `{}` — expected coarse, fine or super",
            args.mode
        )
    })?;
    // Loaded once and used twice: compiling the installed components is the
    // expensive part of a facts-only digest, and the history stage below runs
    // one of the same plugins.
    let plugins = load_plugins(args)?;
    let (mut facts, source) = read_source(args, &plugins, out)?;

    // Digest creates its target plane on demand: with the plane defaulting
    // to the source directory's name, `drsg digest` in a fresh checkout must
    // not fail over a plane nobody had a chance to create.
    let p = match db.plane(args.plane) {
        Ok(p) => p,
        Err(_) => {
            writeln!(out, "created plane '{}'", args.plane)?;
            db.create_plane(args.plane, Properties::new())?
        }
    };
    let run_id = format!(
        "{}-{}",
        source,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    dr_strange_llm::stamp_run(&mut facts, &source, &run_id);

    if !facts.report.handlers.is_empty() {
        let ran: Vec<String> = facts
            .report
            .handlers
            .iter()
            .map(|(n, c)| format!("{n} ({c} facts)"))
            .collect();
        writeln!(out, "preprocessed by {}", ran.join(", "))?;
    }
    // The preprocess notes print here — before any provider is built — because
    // the one that matters most ("no installed plugin claims `.rs`") must not
    // be lost behind a model-call failure that happens later. Drained, so the
    // folded report does not say everything twice.
    for note in facts.report.notes.drain(..) {
        writeln!(out, "  note: {note}")?;
    }

    // History first, and deliberately before anything that can reach for a
    // model: it needs none, so a digest that dies on a missing API key should
    // not take the repository's history down with it. Reported rather than
    // propagated, for the same reason in the other direction — an unreadable
    // repository is not a reason to throw away the code digest that was about
    // to succeed, and a line saying so is louder than a plane nobody looks at.
    if args.git
        && let Err(e) = digest_history(db, args, &plugins, out)
    {
        writeln!(out, "history: not read — {e:#}")?;
    }

    // The §11 headline: an input that yields only facts is digested with **no
    // model call at all** — no provider constructed, no key read, no request
    // made. Building the chat client eagerly would defeat it, since that is
    // where a missing API key turns into an error.
    let result = if facts.needs_model() {
        let chat = dr_strange_llm::build_provider(
            args.chat_provider,
            args.model,
            args.chat_url,
            args.chat_key_env,
            false,
        )?;
        let embedder = dr_strange_llm::build_provider(
            args.embed_provider,
            args.embed_model,
            args.embed_url,
            args.embed_key_env,
            args.embed,
        )?;
        let opts = dr_strange_llm::DigestOptions {
            source,
            model: chat.model().to_string(),
            run_id,
            chunk_chars: args.chunk_chars,
            embed: args.embed,
            concurrency: args.concurrency,
            mode,
            refine_max_entities: None,
            refine_max_context: None,
        };

        let cands = dr_strange_llm::PlaneCandidates::new(&p);
        let plane_source = args
            .link
            .then_some(&cands as &dyn dr_strange_llm::CandidateSource);
        // Grounded whether or not `--link` is on: without this the model is
        // told the facts this very run parsed are new, and proposes a second
        // `parse` beside the one the AST just established.
        let grounded = dr_strange_llm::FactsAndPlane::new(&facts, plane_source);
        let extracted =
            dr_strange_llm::digest(&facts.prose, &chat, &embedder, Some(&grounded), &opts)?;
        dr_strange_llm::fold(facts, extracted)
    } else {
        writeln!(out, "no prose left to read — digested without a model call")?;
        dr_strange_llm::fold(facts, dr_strange_llm::DigestResult::default())
    };

    let r = &result.report;
    writeln!(
        out,
        "digest: {} chunks → {} new entities ({} linked to existing), {} relations ({} dangling dropped)",
        r.chunks, r.entities, r.linked, r.relations, r.dropped_relations
    )?;
    writeln!(
        out,
        "  {} chat request(s); tokens {} in / {} out / {} embed",
        r.chat_requests, r.input_tokens, r.output_tokens, r.embed_tokens
    )?;
    for note in &r.notes {
        writeln!(out, "  note: {note}")?;
    }

    if args.apply {
        let mut txn = p.write()?;
        let stats = result.apply(&p, &mut txn)?;
        txn.commit()?;
        writeln!(
            out,
            "applied: wrote {} nodes, {} edges",
            stats.written.nodes, stats.written.edges
        )?;
        if !stats.skipped.is_empty() {
            writeln!(
                out,
                "  {} entit{} already in the plane, left untouched: {}",
                stats.skipped.len(),
                if stats.skipped.len() == 1 { "y" } else { "ies" },
                stats.skipped.join(", ")
            )?;
        }
        // Only when something was actually embedded: a facts-only digest calls
        // no provider at all, and pointing at an `embedding` property nothing
        // wrote would send a reader to build an index over empty vectors.
        if args.embed && r.embed_tokens > 0 {
            writeln!(
                out,
                "  embeddings stored as `embedding`; `drsg index ensure <label> embedding` for indexed search"
            )?;
        }
        // A directory inside a git repository gets its sync point stamped, so
        // `serve watch` can later say whether the graph is current and catch
        // up from exactly here.
        if std::fs::metadata(args.source)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            record_sync_point(db, args.plane, Path::new(args.source))?;
        }
    } else {
        for n in result.nodes.iter().take(12) {
            writeln!(out, "  [{}] {} ({} props)", n.label, n.key, n.props.len())?;
        }
        if result.nodes.len() > 12 {
            writeln!(out, "  … and {} more", result.nodes.len() - 12)?;
        }
        writeln!(out, "dry run — re-run with --apply to write")?;
    }
    Ok(())
}

/// The second half of digesting a checkout: its **history**, into its own
/// plane (ROADMAP §11).
///
/// Separate from the code digest in every way that matters — a different
/// plugin, a different grant (the git directory, not the working tree), a
/// different plane, and never a model call — so it is a stage of its own
/// rather than a branch inside one. Its caller reports a failure here instead
/// of propagating it: losing the code digest because a repository could not be
/// read would be the wrong trade.
#[cfg(feature = "digest")]
fn digest_history(
    db: &Database,
    args: &DigestArgs,
    plugins: &dr_strange_llm::Plugins,
    out: &mut dyn Write,
) -> Result<()> {
    let dir = Path::new(args.source);
    if !dir.is_dir() {
        return Ok(()); // a document or a URL has no repository behind it
    }
    match dr_strange_llm::git_dir(dir) {
        dr_strange_llm::GitDir::Here(_) => {}
        dr_strange_llm::GitDir::Elsewhere(why) => {
            writeln!(out, "history: {why}")?;
            return Ok(());
        }
        dr_strange_llm::GitDir::None => return Ok(()),
    }

    let facts = match dr_strange_llm::route_repository(dir, plugins)? {
        Some(facts) => facts,
        // A repository, but nothing installed that reads one. Said plainly and
        // once: the digest that just succeeded is not diminished by it.
        None => {
            writeln!(
                out,
                "history: {} is a git repository, but no `{}` plugin is installed — \
                 `drsg plugin install {}` adds one",
                dir.display(),
                dr_strange_llm::REPO_PLUGIN,
                dr_strange_llm::REPO_PLUGIN
            )?;
            return Ok(());
        }
    };

    let plane = args
        .git_plane
        .map(str::to_string)
        .unwrap_or_else(|| dr_strange_llm::git_plane_name(args.plane));
    writeln!(
        out,
        "history: {} node(s), {} edge(s) → plane '{plane}'",
        facts.nodes.len(),
        facts.edges.len()
    )?;
    for note in &facts.report.notes {
        writeln!(out, "  note: {note}")?;
    }

    if !args.apply {
        for n in facts.nodes.iter().take(6) {
            writeln!(out, "  [{}] {}", n.label, n.key)?;
        }
        if facts.nodes.len() > 6 {
            writeln!(out, "  … and {} more", facts.nodes.len() - 6)?;
        }
        writeln!(out, "  dry run — re-run with --apply to write")?;
        return Ok(());
    }

    if db.plane(&plane).is_err() {
        db.create_plane(&plane, Properties::new())?;
        writeln!(out, "  created plane '{plane}'")?;
    }
    let stats = dr_strange_llm::write_history(db, &plane, &facts)?;
    writeln!(
        out,
        "  applied: {} new, {} updated, {} edge(s) written",
        stats.nodes_created, stats.nodes_patched, stats.edges_created
    )?;
    // Every one of these is a silence worth breaking: a key this plugin does
    // not own, an edge that resolved to nothing, a stale edge removed.
    if stats.nodes_skipped > 0 {
        writeln!(
            out,
            "  {} key(s) left alone — the plane holds them on nodes this plugin \
             does not own",
            stats.nodes_skipped
        )?;
    }
    if stats.edges_deleted > 0 || stats.edges_dropped > 0 {
        writeln!(
            out,
            "  {} stale edge(s) replaced, {} dropped for an endpoint outside the plane",
            stats.edges_deleted, stats.edges_dropped
        )?;
    }
    Ok(())
}

// ---- import / export -----------------------------------------------------

/// An edge endpoint reference in the JSONL: `{prefix}_key` (external key) or
/// `{prefix}` (numeric node id, as `export` emits).
enum Ref {
    Key(String),
    Id(u64),
}

/// What [`import`] does when an incoming node's external key already exists in
/// the target plane.
///
/// This needs a policy at all because `bulk_load` is a trusting fast path: it
/// rejects duplicates *within* a batch but does not check keys already in the
/// plane. Unguarded, a re-import therefore wrote a second node under the same
/// key — reachable by scan, invisible to `key(n) = …` (which resolves through
/// the index to exactly one), and reported healthy by `drsg check`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OnConflict {
    /// Write nothing and report the offending keys. The default: a colliding
    /// key usually means the same file was imported twice.
    Error,
    /// Keep the existing node as it is and drop the incoming one. Edges in the
    /// file still resolve to the node already in the plane.
    Skip,
    /// Overwrite the existing node's properties from the incoming line, and
    /// its labels when the line carries a non-empty `labels`. Properties the
    /// line omits are left alone — soft schema, so absence is not a deletion.
    Update,
}

/// Imports JSONL: each line is a node `{"id"?, "labels":[…], "external_key"?,
/// "properties"?}` or an edge `{"src_key"|"src", "dst_key"|"dst", "type",
/// "properties"?}` (an edge line is one carrying `type`).
///
/// Uses the bulk-load fast path: the whole file is buffered, nodes are loaded
/// in one batch, then edge endpoints are resolved — by external key, or by
/// remapping the exported numeric `id` to the node's freshly-assigned one —
/// and edges are bulk-written. Endpoints must resolve within this batch or
/// already exist in the plane; keys are assumed fresh (as bulk load requires).
pub fn import(
    db: &Database,
    plane_name: &str,
    reader: impl BufRead,
    on_conflict: OnConflict,
    out: &mut dyn Write,
) -> Result<()> {
    let p = plane(db, plane_name)?;

    // Buffer the whole file (bulk load needs the batch up front).
    let mut old_ids: Vec<Option<u64>> = Vec::new();
    let mut keys: Vec<Option<String>> = Vec::new();
    let mut labels: Vec<Vec<String>> = Vec::new();
    let mut node_props: Vec<Properties> = Vec::new();
    let mut edges: Vec<(Ref, Ref, String, Properties)> = Vec::new();

    for (lineno, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ctx = || format!("line {}", lineno + 1);
        let value: Value =
            serde_json::from_str(&line).with_context(|| format!("{}: bad JSON", ctx()))?;
        let obj = value
            .as_object()
            .ok_or_else(|| anyhow!("{}: expected a JSON object", ctx()))?;

        if obj.contains_key("type") {
            let src = parse_ref(obj, "src").with_context(ctx)?;
            let dst = parse_ref(obj, "dst").with_context(ctx)?;
            let ty = obj
                .get("type")
                .and_then(|t| t.as_str())
                .with_context(|| format!("{}: edge missing `type`", ctx()))?
                .to_string();
            edges.push((src, dst, ty, edge_props(obj)?));
        } else {
            old_ids.push(obj.get("id").and_then(Value::as_u64));
            keys.push(
                obj.get("external_key")
                    .and_then(|k| k.as_str())
                    .map(str::to_string),
            );
            labels.push(
                obj.get("labels")
                    .and_then(|l| l.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            node_props.push(edge_props(obj)?);
        }
    }

    let mut txn = p.write()?;

    // Which incoming keys already exist. Done under the open write transaction
    // so no other writer can land between the check and the load.
    let mut conflicted: ahash::AHashMap<usize, NodeId> = ahash::AHashMap::new();
    for (i, key) in keys.iter().enumerate() {
        if let Some(key) = key
            && let Some(node) = p.node_by_key(key)?
        {
            conflicted.insert(i, node.id);
        }
    }
    if on_conflict == OnConflict::Error && !conflicted.is_empty() {
        // Name a few rather than all: a doubled file collides on every line,
        // and a thousand-key error message helps nobody.
        let mut names: Vec<&str> = conflicted
            .keys()
            .filter_map(|&i| keys[i].as_deref())
            .collect();
        names.sort_unstable();
        let shown = names.iter().take(5).copied().collect::<Vec<_>>().join(", ");
        let more = names.len().saturating_sub(5);
        let tail = if more > 0 {
            format!(" (and {more} more)")
        } else {
            String::new()
        };
        bail!(
            "{} external key(s) already exist in plane `{plane_name}`: {shown}{tail}. \
             Nothing was imported — re-run with `--on-conflict skip` to keep the \
             existing nodes, or `--on-conflict update` to overwrite them.",
            names.len()
        );
    }

    // Node phase (fast path): one batch, contiguous ids. Conflicting lines are
    // held back so `bulk_load` keeps its fresh-keys precondition.
    let label_refs: Vec<Vec<&str>> = labels
        .iter()
        .map(|ls| ls.iter().map(String::as_str).collect())
        .collect();
    let kept: Vec<usize> = (0..keys.len())
        .filter(|i| !conflicted.contains_key(i))
        .collect();
    let bnodes: Vec<BulkNode> = kept
        .iter()
        .map(|&i| BulkNode {
            external_key: keys[i].as_deref(),
            labels: &label_refs[i],
            props: std::mem::take(&mut node_props[i]),
        })
        .collect();
    let n_nodes = bnodes.len() as u64;
    let stats = txn.bulk_load(bnodes, Vec::new())?;

    // Maps from this batch's identifiers to the node ids edges must resolve to.
    let mut old_to_new = ahash::AHashMap::new();
    let mut key_to_new = ahash::AHashMap::new();
    for (n, &i) in kept.iter().enumerate() {
        let id = NodeId(stats.node_start + n as u64);
        if let Some(o) = old_ids[i] {
            old_to_new.insert(o, id);
        }
        if let Some(k) = &keys[i] {
            key_to_new.insert(k.clone(), id);
        }
    }
    // A skipped or updated line still names a real node, so edges in this file
    // resolve to the one already in the plane rather than failing.
    for (&i, &id) in &conflicted {
        if let Some(o) = old_ids[i] {
            old_to_new.insert(o, id);
        }
        if let Some(k) = &keys[i] {
            key_to_new.insert(k.clone(), id);
        }
    }
    if on_conflict == OnConflict::Update {
        for (&i, &id) in &conflicted {
            for (key, prop) in std::mem::take(&mut node_props[i]) {
                txn.set_prop(id, &key, prop)?;
            }
            if !labels[i].is_empty() {
                let ls: Vec<&str> = labels[i].iter().map(String::as_str).collect();
                txn.set_labels(id, &ls)?;
            }
        }
    }

    // Resolve + validate every endpoint, then bulk-write the edges by id.
    let mut bedges: Vec<BulkEdgeById> = Vec::with_capacity(edges.len());
    for (src, dst, ty, props) in &edges {
        bedges.push(BulkEdgeById {
            src: resolve(src, &key_to_new, &old_to_new, &p)?,
            dst: resolve(dst, &key_to_new, &old_to_new, &p)?,
            ty,
            props: props.clone(),
        });
    }
    let n_edges = txn.bulk_load_edges(bedges)?;

    txn.commit()?;
    // Report the collisions rather than folding them into the node count: a
    // silent "imported 2 nodes" after skipping both is how you end up trusting
    // an import that did nothing.
    let (verb, n_conflicted) = match on_conflict {
        OnConflict::Skip => ("skipped", conflicted.len()),
        OnConflict::Update => ("updated", conflicted.len()),
        OnConflict::Error => ("skipped", 0),
    };
    tracing::info!(
        plane = plane_name,
        nodes = n_nodes,
        edges = n_edges,
        existing = n_conflicted,
        "imported JSONL into plane",
    );
    let tail = if n_conflicted > 0 {
        format!(", {n_conflicted} existing {verb}")
    } else {
        String::new()
    };
    writeln!(out, "imported {n_nodes} nodes, {n_edges} edges{tail}")?;
    Ok(())
}

fn edge_props(obj: &serde_json::Map<String, Value>) -> Result<Properties> {
    Ok(obj
        .get("properties")
        .map(jsonio::json_to_properties)
        .transpose()?
        .unwrap_or_default())
}

fn parse_ref(obj: &serde_json::Map<String, Value>, prefix: &str) -> Result<Ref> {
    if let Some(key) = obj.get(&format!("{prefix}_key")).and_then(|v| v.as_str()) {
        Ok(Ref::Key(key.to_string()))
    } else if let Some(id) = obj.get(prefix).and_then(|v| v.as_u64()) {
        Ok(Ref::Id(id))
    } else {
        bail!("edge missing `{prefix}_key` or `{prefix}`")
    }
}

/// Resolves a reference to a node id, validating existence: a batch key/id
/// maps to the freshly-assigned id; otherwise it must already exist in the
/// plane (a committed key, or a live node id).
fn resolve(
    r: &Ref,
    key_to_new: &ahash::AHashMap<String, NodeId>,
    old_to_new: &ahash::AHashMap<u64, NodeId>,
    p: &PlaneHandle,
) -> Result<NodeId> {
    match r {
        Ref::Key(k) => {
            if let Some(&id) = key_to_new.get(k) {
                return Ok(id);
            }
            p.node_by_key(k)?
                .map(|n| n.id)
                .ok_or_else(|| anyhow!("edge references unknown key '{k}'"))
        }
        Ref::Id(o) => {
            if let Some(&id) = old_to_new.get(o) {
                return Ok(id);
            }
            let id = NodeId(*o);
            if p.node(id)?.is_some() {
                Ok(id)
            } else {
                bail!("edge references unknown node id {o}")
            }
        }
    }
}

/// Exports a plane as JSONL: node lines then edge lines (id-based).
pub fn export(db: &Database, plane_name: &str, out: &mut dyn Write) -> Result<()> {
    let p = plane(db, plane_name)?;
    for node in p.query().scan_all().nodes()? {
        writeln!(out, "{}", jsonio::node_to_json(&node))?;
    }
    // Edges: walk every node's out-adjacency, emit each edge once.
    for node in p.query().scan_all().nodes()? {
        for n in p.neighbors(node.id, Dir::Out, None)? {
            if let Some(edge) = p.edge(n.edge)? {
                writeln!(
                    out,
                    "{}",
                    json!({
                        "id": edge.id.0,
                        "src": edge.src.0,
                        "dst": edge.dst.0,
                        "type": edge.ty,
                        "properties": jsonio::properties_to_json(&edge.properties),
                    })
                )?;
            }
        }
    }
    Ok(())
}

/// `drsg snapshot <out>` — write a consistent, whole-database snapshot bundle
/// (ROADMAP §6) to a file. Restore it into a fresh database with `drsg restore`.
pub fn snapshot(db: &Database, out_path: &Path, out: &mut dyn Write) -> Result<()> {
    let file = std::fs::File::create(out_path)
        .with_context(|| format!("creating snapshot at {}", out_path.display()))?;
    let stats = db
        .snapshot(std::io::BufWriter::new(file))
        .context("writing snapshot")?;
    writeln!(
        out,
        "snapshot: {} planes · {} nodes · {} edges @ seq {} -> {}",
        stats.planes,
        stats.nodes,
        stats.edges,
        stats.seq,
        out_path.display()
    )?;
    Ok(())
}

/// `drsg restore <in>` — restore a snapshot bundle into the `--db` database,
/// which must be empty (ROADMAP §6). Preserves ids, the commit sequence, and
/// the built search indexes.
pub fn restore(db: &Database, in_path: &Path, out: &mut dyn Write) -> Result<()> {
    let file = std::fs::File::open(in_path)
        .with_context(|| format!("opening snapshot at {}", in_path.display()))?;
    let stats = db
        .restore(std::io::BufReader::new(file))
        .context("restoring snapshot")?;
    writeln!(
        out,
        "restored: {} planes · {} nodes · {} edges @ seq {}",
        stats.planes, stats.nodes, stats.edges, stats.seq
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a handler and returns its captured stdout as a String.
    fn cap(f: impl FnOnce(&mut dyn Write) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    const SAMPLE: &str = concat!(
        r#"{"labels":["Paper"],"external_key":"p1","properties":{"year":2020,"emb":{"$vector":[0.0,0.0]}}}"#,
        "\n",
        r#"{"labels":["Paper"],"external_key":"p2","properties":{"year":2021,"emb":{"$vector":[1.0,0.0]}}}"#,
        "\n",
        r#"{"src_key":"p1","dst_key":"p2","type":"CITES"}"#,
        "\n",
    );

    fn loaded() -> Database {
        let db = Database::in_memory().unwrap();
        cap(|out| import(&db, "startup", SAMPLE.as_bytes(), OnConflict::Error, out));
        db
    }

    /// Re-importing used to write a second node under the same external key:
    /// `bulk_load` rejects in-batch duplicates but does not check the plane, so
    /// the copy was reachable by scan, invisible to `key(n) = …` (which
    /// resolves through the index to exactly one node), and `drsg check` called
    /// the database healthy. Failing loudly is the default.
    #[test]
    fn re_import_fails_instead_of_duplicating() {
        let db = loaded();
        let err = import(
            &db,
            "startup",
            SAMPLE.as_bytes(),
            OnConflict::Error,
            &mut Vec::new(),
        )
        .expect_err("a colliding key must not be written");
        let msg = err.to_string();
        assert!(msg.contains("already exist"), "unhelpful: {msg}");
        assert!(msg.contains("p1"), "should name the key: {msg}");
        assert!(
            msg.contains("--on-conflict"),
            "should say how to proceed: {msg}"
        );
        // And nothing was written: the count is unchanged.
        assert!(cap(|o| stats(&db, o)).contains("2 nodes"));
    }

    #[test]
    fn skip_keeps_the_existing_node_and_still_resolves_edges() {
        let db = loaded();
        let changed = SAMPLE.replace("\"year\":2020", "\"year\":1999");
        let out = cap(|o| import(&db, "startup", changed.as_bytes(), OnConflict::Skip, o));
        assert!(out.contains("0 nodes"), "no node should be written: {out}");
        assert!(out.contains("2 existing skipped"), "{out}");

        // The existing node is untouched...
        assert!(cap(|o| get(&db, "startup", "@p1", o)).contains("\"year\":2020"));
        // ...and the file's edge line still resolved against it rather than
        // failing to find its endpoints. Edges carry no external key, so there
        // is no identity to skip on and the CITES edge is written again: the
        // node conflict policy deliberately does not imply edge dedup.
        assert!(cap(|o| stats(&db, o)).contains("1 planes, 2 nodes, 2 edges"));
    }

    #[test]
    fn update_overwrites_properties_of_the_existing_node() {
        let db = loaded();
        let changed = SAMPLE
            .replace("\"year\":2020", "\"year\":1999")
            .replace(r#""labels":["Paper"]"#, r#""labels":["Paper","Retracted"]"#);
        let out = cap(|o| import(&db, "startup", changed.as_bytes(), OnConflict::Update, o));
        assert!(out.contains("2 existing updated"), "{out}");

        let got = cap(|o| get(&db, "startup", "@p1", o));
        assert!(got.contains("\"year\":1999"), "property not updated: {got}");
        assert!(got.contains("Retracted"), "labels not updated: {got}");
        // Still one node per key — an update must not fork the identity.
        assert!(cap(|o| stats(&db, o)).contains("2 nodes"));
    }

    /// A line with no external key cannot collide, so it is always inserted —
    /// including on a re-import under the default policy.
    #[test]
    fn keyless_lines_never_conflict() {
        let db = Database::in_memory().unwrap();
        let keyless = concat!(r#"{"labels":["Note"],"properties":{"n":1}}"#, "\n");
        for _ in 0..2 {
            cap(|o| import(&db, "startup", keyless.as_bytes(), OnConflict::Error, o));
        }
        assert!(cap(|o| stats(&db, o)).contains("2 nodes"));
    }

    #[test]
    fn plane_lifecycle() {
        let db = Database::in_memory().unwrap();
        assert!(cap(|o| plane_create(&db, "scratch", o)).contains("created plane 'scratch'"));
        let list = cap(|o| plane_list(&db, o));
        assert!(list.contains("startup") && list.contains("scratch"));
        assert!(cap(|o| plane_drop(&db, "scratch", o)).contains("dropped"));
        assert!(db.plane("scratch").is_err());
    }

    #[test]
    fn import_then_get_and_stats() {
        let db = loaded();
        let got = cap(|o| get(&db, "startup", "@p1", o));
        assert!(got.contains("\"external_key\":\"p1\""));
        assert!(got.contains("\"year\":2020"));
        // get by numeric id works too
        assert!(cap(|o| get(&db, "startup", "1", o)).contains("\"id\":1"));
        assert!(cap(|o| stats(&db, o)).contains("1 planes, 2 nodes, 1 edges"));
        assert!(cap(|o| check(&db, o)).contains("ok: 2 nodes"));
    }

    #[test]
    fn algo_commands_report_over_the_loaded_graph() {
        let db = loaded(); // p1 (id 1) —CITES→ p2 (id 2)
        let pr = cap(|o| algo_pagerank(&db, "startup", None, 20, 0.85, 20, o));
        assert!(pr.contains("pagerank: 2 nodes"), "{pr}");

        let comp = cap(|o| algo_components(&db, "startup", None, 50, o));
        assert!(comp.contains("components: 1 across 2 nodes"), "{comp}");

        let sp = cap(|o| algo_shortest_path(&db, "startup", None, 1, 2, Dir::Out, None, o));
        assert!(sp.contains("cost 1") && sp.contains("1 -> 2"), "{sp}");

        // No forward path 2 -> 1 (edge is directed).
        let none = cap(|o| algo_shortest_path(&db, "startup", None, 2, 1, Dir::Out, None, o));
        assert!(none.contains("no path from 2 to 1"), "{none}");

        let lv = cap(|o| algo_louvain(&db, "startup", None, 50, o));
        assert!(lv.contains("communities: 1"), "{lv}");
    }

    #[test]
    fn hybrid_keyword_channel_ranks_and_declares_index() {
        use dr_strange_core::{PropDesc, PropValue};

        let db = Database::in_memory().unwrap();
        {
            let plane = db.plane("startup").unwrap();
            let mut txn = plane.write().unwrap();
            let mk = |b: &str| -> Properties {
                [("body".to_string(), PropDesc::new(PropValue::Str(b.into())))]
                    .into_iter()
                    .collect()
            };
            txn.create_node_with_key("d0", &["Doc"], mk("graph databases store data"))
                .unwrap();
            txn.create_node_with_key("d1", &["Doc"], mk("graph graph graph queries"))
                .unwrap();
            txn.commit().unwrap();
        }
        let declared =
            cap(|o| keyword_index_ensure(&db, "startup", "Doc", "body", Language::English, o));
        assert!(declared.contains("ensured keyword index on Doc.body"));

        // Keyword-only hybrid (no vector ⇒ no embedding needed).
        let out = cap(|o| {
            hybrid(
                &db,
                "startup",
                "graph",
                Some("Doc"),
                None,
                Some("body"),
                Metric::Cosine,
                None,
                10,
                "openai",
                None,
                o,
            )
        });
        assert!(out.contains("hybrid: 2 results"), "{out}");
        assert!(out.contains("d1"), "graph-dense doc present: {out}");
    }

    #[test]
    fn import_remaps_exported_numeric_edge_ids() {
        // The file's node ids (5, 6) don't match the fresh db's assignments;
        // the numeric edge (src:5 → dst:6) must still connect a → b.
        let jsonl = concat!(
            r#"{"id":5,"external_key":"a","labels":["N"]}"#,
            "\n",
            r#"{"id":6,"external_key":"b","labels":["N"]}"#,
            "\n",
            r#"{"src":5,"dst":6,"type":"E"}"#,
            "\n",
        );
        let db = Database::in_memory().unwrap();
        cap(|o| import(&db, "startup", jsonl.as_bytes(), OnConflict::Error, o));
        let p = db.plane("startup").unwrap();
        let a = p.node_by_key("a").unwrap().unwrap();
        let b = p.node_by_key("b").unwrap().unwrap();
        assert_ne!(a.id.0, 5, "ids are reassigned, not copied from the file");
        let ns = p.neighbors(a.id, Dir::Out, None).unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].node, b.id, "numeric edge remapped to the right node");
    }

    #[test]
    fn query_plan_json() {
        let db = loaded();
        // scan Paper, filter year >= 2021 -> only p2
        let plan = r#"{"source":{"ScanLabel":"Paper"},"steps":[
            {"Filter":{"Compare":{"op":"Ge","lhs":{"Property":"year"},"rhs":{"Literal":{"Int":2021}}}}}]}"#;
        let out = cap(|o| query(&db, "startup", plan, o));
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
    }

    #[test]
    fn cypher_query_over_the_graph() {
        let db = loaded();
        // WHERE pushdown: scan Paper, keep year >= 2021 → only p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (n:Paper) WHERE n.year >= 2021 RETURN n",
                None,
                &[],
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // Traversal over the CITES edge: p1 → p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (a:Paper)-[:CITES]->(b:Paper) RETURN b",
                None,
                &[],
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // A vector-literal SEARCH runs the top-k with no embedder: the fixture's
        // Papers carry an `emb` vector, so NEAR [1,0] (cosine) returns p2.
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "SEARCH (n:Paper) ON emb NEAR [1.0, 0.0] TOPK 1 RETURN n",
                None,
                &[],
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
        // An unsupported query surfaces the parser's error, not a panic.
        let mut sink = Vec::new();
        let err = cypher(&db, "startup", "MATCH (n)", None, &[], &mut sink).unwrap_err();
        assert!(err.to_string().contains("syntax error"), "{err}");
        // A text SEARCH with no --embed is a clear error, not a panic.
        let mut sink = Vec::new();
        let err = cypher(
            &db,
            "startup",
            "SEARCH (n:Paper) ON emb NEAR \"hi\" RETURN n",
            None,
            &[],
            &mut sink,
        )
        .unwrap_err();
        assert!(err.to_string().contains("embedding provider"), "{err}");
    }

    /// A projecting query prints the table, not the nodes: one JSON object
    /// carrying the columns beside the rows.
    #[test]
    fn cypher_projection_prints_one_table() {
        let db = loaded();
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (n:Paper) RETURN n.year AS year, count(*) AS papers ORDER BY year",
                None,
                &[],
                o,
            )
        });
        assert_eq!(out.lines().count(), 1, "one object, not a line per row");
        let table: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(table["columns"], json!(["year", "papers"]));
        assert_eq!(table["rows"], json!([[2020, 1], [2021, 1]]));
    }

    #[test]
    fn cypher_create_writes_and_summarizes() {
        let db = Database::in_memory().unwrap();
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                r#"CREATE (a:Person {key:"alice"})-[:KNOWS]->(b:Person {key:"bob"})"#,
                None,
                &[],
                o,
            )
        });
        assert!(out.contains("2 nodes created"), "{out}");
        assert!(out.contains("1 edges created"), "{out}");
        let p = db.plane("startup").unwrap();
        assert!(p.node_by_key("alice").unwrap().is_some());
        assert!(p.node_by_key("bob").unwrap().is_some());
    }

    #[test]
    fn cypher_with_params() {
        let db = loaded(); // Papers p1(2020), p2(2021)
        let out = cap(|o| {
            cypher(
                &db,
                "startup",
                "MATCH (n:Paper) WHERE n.year >= $min RETURN n",
                None,
                &["min=2021".to_string()],
                o,
            )
        });
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\"external_key\":\"p2\""));
    }

    #[test]
    fn vector_query_via_declared_index() {
        let db = loaded();
        cap(|o| index_ensure(&db, "startup", "Paper", "emb", Metric::L2, o));
        let plan = r#"{"source":{"VectorTopK":{"label":"Paper","property":"emb",
            "query":[0.0,0.0],"metric":"L2","k":1}},"steps":[]}"#;
        let out = cap(|o| query(&db, "startup", plan, o));
        assert!(out.contains("\"external_key\":\"p1\"")); // nearest [0,0]
        assert!(out.contains("\"score\":")); // score channel projected
    }

    #[test]
    fn catalog_and_show() {
        let db = loaded();
        let cat = cap(|o| catalog(&db, Some("startup"), o));
        assert!(cat.contains("\"Paper\""));
        assert!(cat.contains("\"node_count\": 2"));
        // whole-db roll-up too
        assert!(cap(|o| catalog(&db, None, o)).contains("\"node_count\": 2"));
        assert!(cap(|o| plane_show(&db, "startup", o)).contains("2 nodes, 1 edges"));
    }

    #[test]
    fn export_round_trips_into_a_second_db() {
        let db = loaded();
        let dumped = cap(|o| export(&db, "startup", o));
        // nodes carry keys; the re-import resolves the edge by src_key/dst_key
        let db2 = Database::in_memory().unwrap();
        cap(|o| import(&db2, "startup", dumped.as_bytes(), OnConflict::Error, o));
        assert!(cap(|o| stats(&db2, o)).contains("2 nodes, 1 edges"));
    }

    #[test]
    fn bad_plan_and_missing_node_error() {
        let db = loaded();
        assert!(query(&db, "startup", "not json", &mut Vec::new()).is_err());
        assert!(get(&db, "startup", "9999", &mut Vec::new()).is_err());
        assert!(get(&db, "startup", "@nope", &mut Vec::new()).is_err());
    }

    // ---- plugin management ------------------------------------------------

    /// The sandbox suite's committed fixture — a real component claiming
    /// `.fix` — so these tests exercise the actual validate/store path, not
    /// a mock of it.
    #[cfg(feature = "digest")]
    const FIXTURE_WASM: &[u8] = include_bytes!("../../dr-strange-llm/tests/fixtures/fixture.wasm");

    /// A throwaway store directory wired through `PluginConfig` — the same
    /// knob `[plugins] store_dir` sets, so nothing here touches the user's
    /// real per-user store.
    #[cfg(feature = "digest")]
    fn scratch_store(name: &str) -> (std::path::PathBuf, dr_strange_llm::PluginConfig) {
        let dir = std::env::temp_dir().join(format!("drsg-cli-plug-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dr_strange_llm::PluginConfig {
            store_dir: Some(dir.clone()),
            ..Default::default()
        };
        (dir, cfg)
    }

    #[cfg(feature = "digest")]
    #[test]
    fn plugin_list_reports_the_empty_store_in_both_shapes() {
        let (dir, cfg) = scratch_store("empty");
        // JSON stays machine-readable even when there is nothing to say —
        // an agent parsing it must never meet prose.
        let json = cap(|o| plugin_list(&cfg, &[], false, true, o));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, serde_json::json!([]));
        // The human shape says what to do next instead.
        let table = cap(|o| plugin_list(&cfg, &[], false, false, o));
        assert!(table.contains("no plugins installed"), "{table}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn install_from_a_path_then_list_as_table_and_json() {
        let (dir, cfg) = scratch_store("list");
        let wasm = dir.join("fixture.wasm");
        std::fs::write(&wasm, FIXTURE_WASM).unwrap();

        let out = cap(|o| plugin_install(&cfg, &[], Some(wasm.to_str().unwrap()), o));
        assert!(out.contains("installed fixture@0"), "{out}");
        assert!(out.contains("handles: .fix"), "{out}");

        let table = cap(|o| plugin_list(&cfg, &[], false, false, o));
        assert!(
            table.contains("NAME") && table.contains("EXTENSIONS"),
            "{table}"
        );
        assert!(
            table.contains("fixture") && table.contains(".fix"),
            "{table}"
        );

        // `--json` is the agent surface: the same records `plugin.list`
        // serves over RPC, parseable without scraping the table.
        let json = cap(|o| plugin_list(&cfg, &[], false, true, o));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["name"], "fixture");
        assert_eq!(parsed[0]["extensions"][0], "fix");
        assert_eq!(parsed[0]["sha256"].as_str().unwrap().len(), 64);
        // The fixture ships a manifest logo; the store records it and the
        // machine surface carries it to UIs.
        assert!(parsed[0]["logo"].as_str().unwrap().starts_with("<svg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `vectorize` embeds once, skips what is current, and re-embeds only
    /// what changed — the whole point of the `_embedded_from` hash.
    #[cfg(feature = "digest")]
    #[test]
    fn vectorize_is_incremental() {
        use dr_strange_core::{PropDesc, PropValue};
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("v", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        // A parser fact (projection) and a document entity (full text).
        let mut fact = Properties::new();
        fact.insert(
            "_generated_by".into(),
            PropDesc::described("parser", PropValue::Str("rust@2".into())),
        );
        fact.insert(
            "signature".into(),
            PropDesc::described("sig", PropValue::Str("fn go()".into())),
        );
        fact.insert(
            "line".into(),
            PropDesc::described("line", PropValue::Int(9)),
        );
        let fact_id = txn
            .create_node_with_key("k::go", &["Function"], fact)
            .unwrap();
        let mut doc = Properties::new();
        doc.insert(
            "year".into(),
            PropDesc::described("year", PropValue::Int(2020)),
        );
        txn.create_node_with_key("paper", &["Paper"], doc).unwrap();
        txn.commit().unwrap();

        let mock = dr_strange_llm::MockProvider::new(Vec::new(), 4);
        let run = |out: &mut Vec<u8>| vectorize(&db, "v", &mock, Metric::Cosine, out).unwrap();

        let mut out = Vec::new();
        run(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("embedded 2 node(s)"), "{text}");
        // The plane is searchable when vectorize returns: both labels indexed.
        assert!(text.contains("Function, Paper"), "{text}");
        let node = p.node(fact_id).unwrap().unwrap();
        assert!(matches!(
            node.properties.get("embedding").map(|d| &d.value),
            Some(PropValue::Vector(v)) if v.len() == 4
        ));

        // Nothing changed: nothing re-embeds.
        let mut out = Vec::new();
        run(&mut out);
        assert!(String::from_utf8(out).unwrap().contains("nothing to embed"));

        // A positional change on the fact: the projection is unchanged, so
        // still nothing to do — the stability the projection exists for.
        let mut txn = p.write().unwrap();
        txn.set_prop(
            fact_id,
            "line",
            PropDesc::described("line", PropValue::Int(99)),
        )
        .unwrap();
        txn.commit().unwrap();
        let mut out = Vec::new();
        run(&mut out);
        assert!(String::from_utf8(out).unwrap().contains("nothing to embed"));

        // A semantic change re-embeds exactly that node.
        let mut txn = p.write().unwrap();
        txn.set_prop(
            fact_id,
            "signature",
            PropDesc::described("sig", PropValue::Str("fn go(x: u8)".into())),
        )
        .unwrap();
        txn.commit().unwrap();
        let mut out = Vec::new();
        run(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("embedded 1 node(s)"), "{text}");
    }

    /// `index ensure <property>` sweeps exactly the labels that carry it.
    #[cfg(feature = "digest")]
    #[test]
    fn index_ensure_all_targets_only_labels_with_the_property() {
        use dr_strange_core::{PropDesc, PropValue};
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("v", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut with_vec = Properties::new();
        with_vec.insert(
            "embedding".into(),
            PropDesc::described("v", PropValue::Vector(vec![1.0, 0.0])),
        );
        txn.create_node(&["Function"], with_vec.clone()).unwrap();
        txn.create_node(&["Paper"], with_vec).unwrap();
        txn.create_node(&["Bare"], Properties::new()).unwrap();
        txn.commit().unwrap();

        let out = cap(|o| index_ensure_all(&db, "v", "embedding", Metric::Cosine, o));
        assert!(out.contains("Function.embedding"), "{out}");
        assert!(out.contains("Paper.embedding"), "{out}");
        assert!(
            !out.contains("Bare"),
            "a label without the property was indexed: {out}"
        );
        assert!(out.contains("2 label(s) indexed"), "{out}");

        // No label carries a made-up property: say so, index nothing.
        let none = cap(|o| index_ensure_all(&db, "v", "nope", Metric::Cosine, o));
        assert!(none.contains("no label"), "{none}");
    }

    /// The sync point round-trips through plane properties, and the watch
    /// startup can tell in-sync from behind from unknowable.
    #[cfg(feature = "digest")]
    #[test]
    fn sync_point_records_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("drsg-syncpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?}"
            );
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c1",
        ]);

        let db = Database::in_memory().unwrap();
        db.create_plane("p", Properties::new()).unwrap();

        // Nothing recorded yet — the graph cannot be compared.
        assert_eq!(recorded_sync_point(&db, "p"), (None, None));

        record_sync_point(&db, "p", &dir).unwrap();
        let (commit, root) = recorded_sync_point(&db, "p");
        let head = git_head(&dir).unwrap();
        assert_eq!(commit.as_deref(), Some(head.as_str()));
        assert_eq!(
            root.as_deref(),
            Some(dir.canonicalize().unwrap().to_str().unwrap())
        );
        assert!(commit_known(&dir, &head));
        assert!(!commit_known(
            &dir,
            "0000000000000000000000000000000000000000"
        ));

        // A new commit: the recorded point is behind but known — the catch-up
        // case — and re-recording moves it forward.
        run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "c2",
        ]);
        let new_head = git_head(&dir).unwrap();
        assert_ne!(commit.as_deref(), Some(new_head.as_str()));
        assert!(commit_known(&dir, commit.as_deref().unwrap()));
        record_sync_point(&db, "p", &dir).unwrap();
        assert_eq!(
            recorded_sync_point(&db, "p").0.as_deref(),
            Some(new_head.as_str())
        );

        // Outside a repository: recording is a quiet no-op.
        let plain =
            std::env::temp_dir().join(format!("drsg-syncpoint-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&plain);
        std::fs::create_dir_all(&plain).unwrap();
        db.create_plane("q", Properties::new()).unwrap();
        record_sync_point(&db, "q", &plain).unwrap();
        assert_eq!(recorded_sync_point(&db, "q"), (None, None));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
    }

    /// A repository whose first commit is unborn has no HEAD, exactly like a
    /// plain directory — but only one of them is worth waiting on, and
    /// `is_git_repo` is what tells `bootstrap_unborn` which it is looking at.
    #[cfg(feature = "digest")]
    #[test]
    fn an_unborn_repository_is_told_apart_from_a_plain_directory() {
        let base = std::env::temp_dir().join(format!("drsg-unborn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let unborn = base.join("repo");
        let plain = base.join("plain");
        std::fs::create_dir_all(&unborn).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&unborn)
                .args(["init", "-q"])
                .output()
                .unwrap()
                .status
                .success()
        );

        // Neither can name a commit …
        assert!(git_head(&unborn).is_err());
        assert!(git_head(&plain).is_err());
        // … but the repository will have one, and the directory will not.
        assert!(is_git_repo(&unborn));
        assert!(!is_git_repo(&plain));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `git diff --name-status -z` per status letter: one path each, except
    /// renames/copies which carry source then destination.
    #[cfg(feature = "digest")]
    #[test]
    fn name_status_parses_every_shape_the_diff_emits() {
        let raw =
            b"M src/lib.rs A src/new.rs D old.rs R100 from.rs to.rs C75 base.rs copy.rs T link.rs ";
        let (changed, deleted) = parse_name_status(raw);
        assert_eq!(
            changed,
            vec!["src/lib.rs", "src/new.rs", "to.rs", "copy.rs", "link.rs"]
        );
        // A rename's source is gone; a copy's still exists.
        assert_eq!(deleted, vec!["old.rs", "from.rs"]);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn name_status_survives_truncated_input() {
        // A status with no path (defensive; git won't produce it).
        assert_eq!(parse_name_status(b"M "), (vec![], vec![]));
        assert_eq!(parse_name_status(b""), (vec![], vec![]));
    }

    #[cfg(feature = "digest")]
    #[test]
    fn the_chooser_tags_installed_and_upgradable_against_the_release_hash() {
        let installed: std::collections::BTreeMap<String, String> = [
            ("rust".to_string(), "aaaa".to_string()),
            ("go".to_string(), "bbbb".to_string()),
        ]
        .into();
        // Hash matches the release artifact → nothing to do.
        assert_eq!(official_status(&installed, "rust", "aaaa"), "[installed]");
        // A hash the catalog writes in a different case is the same hash.
        assert_eq!(official_status(&installed, "rust", "AAAA"), "[installed]");
        // Same name, different bytes — an older release or a local build.
        assert_eq!(official_status(&installed, "go", "cccc"), "[upgradable]");
        // Absent stays unmarked.
        assert_eq!(official_status(&installed, "ts", "dddd"), "");
    }

    /// The catalog is now data fetched from the extensions repository, so
    /// "are the pinned hashes well-formed" is that repository's CI to answer
    /// (`just check-catalog` there). What is still this binary's to get right
    /// is how it *renders* what it was handed — including an entry it cannot
    /// run, which must appear with its reason rather than quietly vanish.
    #[cfg(feature = "digest")]
    #[test]
    fn the_catalog_table_tags_each_entry_against_the_store() {
        let catalog: dr_strange_llm::Catalog = serde_json::from_str(
            r#"{"schema":1,"plugins":[
                 {"name":"rust","version":"1.4.1","claims":".rs",
                  "url":"https://example.invalid/rust.wasm","sha256":"aaaa"},
                 {"name":"go","version":"1.4.0","claims":".go",
                  "url":"https://example.invalid/go.wasm","sha256":"bbbb"},
                 {"name":"ts","version":"1.3.0","claims":".ts",
                  "url":"https://example.invalid/ts.wasm","sha256":"cccc"},
                 {"name":"zig","version":"0.1.0","claims":".zig",
                  "url":"https://example.invalid/zig.wasm","sha256":"dddd",
                  "min_drsg":"99.0.0"}]}"#,
        )
        .unwrap();
        let installed: std::collections::BTreeMap<String, String> = [
            // Installed and current.
            ("rust".to_string(), "aaaa".to_string()),
            // Installed, but not these bytes.
            ("go".to_string(), "0000".to_string()),
        ]
        .into();

        let picks = catalog.current();
        let mut buf: Vec<u8> = Vec::new();
        print_catalog(&picks, &installed, true, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();

        let line = |name: &str| {
            text.lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("no row for {name} in:\n{text}"))
                .to_string()
        };
        assert!(line("rust").contains("1) "), "numbered: {text}");
        assert!(line("rust").contains("[installed]"), "{text}");
        assert!(line("go").contains("[upgradable]"), "{text}");
        // Absent: neither tag.
        assert!(!line("ts").contains('['), "{text}");
        // Unrunnable here, listed anyway, with the reason and the floor.
        let zig = line("zig");
        assert!(zig.contains("unsupported"), "{zig}");
        assert!(zig.contains("99.0.0"), "{zig}");

        // Unnumbered is the `plugin list --available` rendering.
        let mut plain: Vec<u8> = Vec::new();
        print_catalog(&picks, &installed, false, &mut plain).unwrap();
        assert!(!String::from_utf8(plain).unwrap().contains("1) "));
    }

    #[cfg(feature = "digest")]
    #[test]
    fn a_second_claimant_conflicts_but_a_reinstall_does_not() {
        let (dir, cfg) = scratch_store("conflict");
        let store = plugin_store(&cfg).unwrap();
        store.install(FIXTURE_WASM, "test").unwrap();

        // A different plugin claiming `.fix` collides with the incumbent…
        let hits =
            extension_conflicts(&store, "fixture2", std::slice::from_ref(&"fix".to_string()))
                .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "fixture");

        // …but the same name re-claiming its own extension is the upgrade
        // path, and a disjoint claim collides with nothing.
        assert!(
            extension_conflicts(&store, "fixture", std::slice::from_ref(&"fix".to_string()))
                .unwrap()
                .is_empty()
        );
        assert!(
            extension_conflicts(&store, "other", std::slice::from_ref(&"zig".to_string()))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the `init`/plugin-store tests need one, and both need the plugin
    /// host.
    #[cfg(feature = "digest")]
    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("drsg-cli-init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(feature = "digest")]
    #[test]
    fn pick_free_port_returns_a_bindable_loopback_address() {
        let addr = pick_free_port().unwrap();
        assert_eq!(addr.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        assert_ne!(addr.port(), 0);
        // The picked port is actually free to bind again immediately after.
        std::net::TcpListener::bind(addr).unwrap();
    }

    #[cfg(feature = "digest")]
    #[test]
    fn ensure_gitignore_patterns_is_idempotent_and_preserves_unrelated_lines() {
        let dir = scratch_dir("gitignore");
        std::fs::write(dir.join(".gitignore"), "node_modules/\n*.drsg\n").unwrap();

        ensure_gitignore_patterns(&dir).unwrap();
        let first = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            first.contains("node_modules/"),
            "kept unrelated line: {first}"
        );
        for pat in GITIGNORE_PATTERNS {
            assert!(first.contains(pat), "missing {pat}: {first}");
        }

        ensure_gitignore_patterns(&dir).unwrap();
        let second = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(first, second, "a second run must not duplicate lines");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn mcp_json_upsert_adds_overwrites_and_preserves_other_entries() {
        let dir = scratch_dir("mcpjson");
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        // Fresh file: creates `mcpServers` and the entry.
        write_mcp_json_entry(&dir, &addr, "tok1").unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["drsg-watch"]["url"],
            "http://127.0.0.1:12345/mcp"
        );
        assert_eq!(
            v["mcpServers"]["drsg-watch"]["headers"]["Authorization"],
            "Bearer tok1"
        );

        // An existing, unrelated server entry survives an overwrite.
        let mut v = v;
        v["mcpServers"]["other"] = json!({"type": "stdio", "command": "foo"});
        std::fs::write(
            dir.join(".mcp.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();

        write_mcp_json_entry(&dir, &addr, "tok2").unwrap();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            v["mcpServers"]["drsg-watch"]["headers"]["Authorization"], "Bearer tok2",
            "must overwrite in place, not duplicate"
        );
        assert_eq!(
            v["mcpServers"]["other"]["command"], "foo",
            "unrelated entry preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `.mcp.json` is the only place the generated token is ever written, so
    /// a restart that keeps agents' configs valid depends on reading back
    /// exactly what was written — and on declining to guess at anything else.
    #[cfg(feature = "digest")]
    #[test]
    fn the_recorded_endpoint_round_trips_and_rejects_what_init_did_not_write() {
        let dir = scratch_dir("recorded-endpoint");
        let addr: std::net::SocketAddr = "127.0.0.1:41111".parse().unwrap();

        // Nothing written yet: a first run, not a restart.
        assert_eq!(recorded_endpoint(&dir), None);

        write_mcp_json_entry(&dir, &addr, "tok-1").unwrap();
        assert_eq!(
            recorded_endpoint(&dir),
            Some((addr, "tok-1".to_string())),
            "the address and token must survive the round trip verbatim"
        );

        // A file that exists but says nothing `init` would recognise — a
        // hand-written stdio entry, a different server — is not an endpoint
        // to restart, and must not be read as one.
        let write = |v: Value| {
            std::fs::write(dir.join(".mcp.json"), serde_json::to_string(&v).unwrap()).unwrap()
        };
        write(json!({ "mcpServers": { "other": { "type": "stdio", "command": "foo" } } }));
        assert_eq!(recorded_endpoint(&dir), None, "another server's entry");
        write(json!({ "mcpServers": { "drsg-watch": { "type": "stdio", "command": "drsg" } } }));
        assert_eq!(recorded_endpoint(&dir), None, "no url to connect to");
        write(json!({ "mcpServers": { "drsg-watch": {
            "url": "http://127.0.0.1:41111/mcp", "headers": { "Authorization": "Basic nope" },
        } } }));
        assert_eq!(recorded_endpoint(&dir), None, "not a bearer token");
        std::fs::write(dir.join(".mcp.json"), "{ not json").unwrap();
        assert_eq!(recorded_endpoint(&dir), None, "unparseable file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe must answer "no drsg here" for silence *and* for a stranger
    /// on the port — after a reboot the OS may well have handed that
    /// arbitrary port to something else.
    #[cfg(feature = "digest")]
    #[test]
    fn health_says_no_for_a_dead_port_and_for_a_process_that_is_not_drsg() {
        use std::io::{Read, Write as _};
        let quick = std::time::Duration::from_millis(500);

        // Bound and drop: nothing is listening on that port any more.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        assert!(!health_ok(dead_addr, quick));

        // A listener that accepts and answers, but is not drsg.
        let stranger = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stranger_addr = stranger.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = stranger.accept() {
                let mut buf = [0u8; 512];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
            }
        });
        assert!(!health_ok(stranger_addr, quick), "200 OK is not enough");
        t.join().unwrap();
    }

    #[cfg(feature = "digest")]
    #[test]
    fn agent_probes_skip_when_their_marker_is_absent() {
        let dir = scratch_dir("no-markers");
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        assert!(!probe_and_write_cursor(&dir, &addr, "tok").unwrap());
        assert!(!probe_and_write_opencode(&dir, &addr, "tok").unwrap());
        assert!(!probe_and_write_gemini(&dir, &addr, "tok").unwrap());
        assert!(!probe_and_write_codex(&dir, &addr).unwrap());
        assert!(!dir.join(".cursor").exists());
        assert!(!dir.join(".opencode.json").exists());
        assert!(!dir.join(".gemini").exists());
        assert!(!dir.join(".codex").exists());
        assert!(!probe_and_write_claude_hooks(&dir, &dir.join("hooks"), false).unwrap());
        assert!(!dir.join(".claude").exists());
        assert!(!dir.join("hooks").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn claude_hooks_are_installed_once_and_repointed_not_repeated() {
        let dir = scratch_dir("claude-hooks");
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let hooks = dir.join("data").join("hooks");

        assert!(probe_and_write_claude_hooks(&dir, &hooks, false).unwrap());
        for (name, _) in CLAUDE_HOOKS {
            let script = hooks.join(name);
            assert!(script.is_file(), "{name} written");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_ne!(
                    std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
                    0,
                    "{name} executable"
                );
            }
        }
        let settings = dir.join(".claude/settings.local.json");
        let read = || -> Value {
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap()
        };
        let v = read();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            hooks.join("drsg-shell-guard").display().to_string()
        );
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert!(v["hooks"]["SessionStart"][0].get("matcher").is_none());

        // Again, from a moved data directory: repointed, and still one each.
        let moved = dir.join("elsewhere").join("hooks");
        assert!(probe_and_write_claude_hooks(&dir, &moved, true).unwrap());
        let v = read();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            moved.join("drsg-shell-guard").display().to_string()
        );
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);

        // Someone else's hooks and settings survive untouched.
        std::fs::write(
            &settings,
            r#"{"permissions": {"allow": ["Bash(git:*)"]}, "hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "/x/lint"}]}]}}"#,
        )
        .unwrap();
        assert!(probe_and_write_claude_hooks(&dir, &hooks, true).unwrap());
        let v = read();
        assert_eq!(v["permissions"]["allow"][0], "Bash(git:*)");
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/x/lint"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard blocks a shell search or read on code and lets everything
    /// else through, by the host's exit-code protocol. Runs the real script,
    /// so it needs bash and a JSON parser (jq or python3) — skipped without.
    #[cfg(feature = "digest")]
    #[test]
    fn the_shell_guard_redirects_searches_and_reads_and_nothing_else() {
        let has = |tool: &str| {
            std::process::Command::new("sh")
                .args(["-c", &format!("command -v {tool}")])
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !has("bash") || !(has("jq") || has("python3")) {
            eprintln!("bash and jq/python3 are needed to run the guard — skipping");
            return;
        }
        let dir = scratch_dir("guard");
        let script = dir.join("drsg-shell-guard");
        write_executable(&script, CLAUDE_HOOKS[0].1).unwrap();
        let run = |command: &str| -> (i32, String) {
            use std::io::Write as _;
            let mut child = std::process::Command::new("bash")
                .arg(&script)
                .stdin(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let input = json!({ "tool_name": "Bash", "tool_input": { "command": command } });
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.to_string().as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };

        for blocked in [
            "rg 'fn main' crates/",
            "grep -rn needle src",
            "cat src/lib.rs",
            "sed -n '10,40p' src/lib.rs",
            "head -50 src/lib.rs",
            "RUST_LOG=debug rg needle",
            "rtk grep needle src",
            "/usr/bin/grep needle x",
            "rg needle | head",
        ] {
            let (code, err) = run(blocked);
            assert_eq!(code, 2, "`{blocked}` should be redirected");
            assert!(err.contains("snippet(name | path:start-end)"), "{err}");
            assert!(err.contains("DRSG_RAW=1"), "{err}");
        }
        for allowed in [
            "git status",
            "cargo test -p x",
            "DRSG_RAW=1 rg needle src",
            "cat > out.txt <<'EOF'\nhello\nEOF",
            "sed -i 's/a/b/' src/lib.rs",
            "echo hi | grep h",
            "ls -la",
        ] {
            let (code, _) = run(allowed);
            assert_eq!(code, 0, "`{allowed}` should pass");
        }
        // Unparseable input is let through, never blocked.
        let mut child = std::process::Command::new("bash")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(b"not json").unwrap();
        }
        assert_eq!(child.wait().unwrap().code(), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn cursor_probe_writes_the_mcp_shape_when_dot_cursor_exists() {
        let dir = scratch_dir("cursor");
        std::fs::create_dir_all(dir.join(".cursor")).unwrap();
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        assert!(probe_and_write_cursor(&dir, &addr, "tok").unwrap());
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".cursor/mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["mcpServers"]["drsg-watch"]["url"],
            "http://127.0.0.1:12345/mcp"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn opencode_probe_only_fires_on_an_existing_opencode_json() {
        let dir = scratch_dir("opencode");
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        // No pre-existing `.opencode.json`: nothing is created.
        assert!(!probe_and_write_opencode(&dir, &addr, "tok").unwrap());
        assert!(!dir.join(".opencode.json").exists());

        // Once it exists, the entry is upserted under `mcp`, not `mcpServers`.
        std::fs::write(dir.join(".opencode.json"), "{}").unwrap();
        assert!(probe_and_write_opencode(&dir, &addr, "tok").unwrap());
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".opencode.json")).unwrap())
                .unwrap();
        assert_eq!(v["mcp"]["drsg-watch"]["type"], "remote");
        assert_eq!(v["mcp"]["drsg-watch"]["url"], "http://127.0.0.1:12345/mcp");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn gemini_probe_uses_http_url_field_not_url() {
        let dir = scratch_dir("gemini");
        std::fs::create_dir_all(dir.join(".gemini")).unwrap();
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        assert!(probe_and_write_gemini(&dir, &addr, "tok").unwrap());
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["mcpServers"]["drsg-watch"]["httpUrl"],
            "http://127.0.0.1:12345/mcp"
        );
        assert!(
            v["mcpServers"]["drsg-watch"]["url"].is_null(),
            "must use httpUrl, not url"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "digest")]
    #[test]
    fn codex_probe_writes_toml_with_env_var_name_never_the_raw_token() {
        let dir = scratch_dir("codex");
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        let addr: std::net::SocketAddr = "127.0.0.1:12345".parse().unwrap();

        assert!(probe_and_write_codex(&dir, &addr).unwrap());
        let rendered = std::fs::read_to_string(dir.join(".codex/config.toml")).unwrap();
        let v: toml::Value = rendered.parse().unwrap();
        assert_eq!(
            v["mcp_servers"]["drsg-watch"]["url"].as_str().unwrap(),
            "http://127.0.0.1:12345/mcp"
        );
        assert_eq!(
            v["mcp_servers"]["drsg-watch"]["bearer_token_env_var"]
                .as_str()
                .unwrap(),
            CODEX_TOKEN_ENV_VAR
        );
        assert!(
            !rendered.contains("Bearer"),
            "the raw token must never land in this file: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
