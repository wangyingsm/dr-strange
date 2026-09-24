//! Optional `config.toml` — one file for the server, logging, and LLM settings
//! so an operator needn't juggle a dozen environment variables (arch/08).
//!
//! Secrets (the API token, LLM keys) and the Origin allow-list are *applied to
//! the process environment* by [`apply_env`] before anything reads them. That
//! keeps the existing env-based plumbing — `SharedToken`, `AllowedOrigins`,
//! `dr_strange_log`, and the LLM provider layer, which all read the
//! environment — as the single source of truth, and preserves the rule that
//! provider keys are server-side and never travel from a client. An
//! already-set environment variable always wins, so any value can be
//! overridden at launch (`DRSG_TOKEN=… drsg serve`) without editing the file.
//!
//! The file is entirely optional: with no `--config`, no `$DRSG_CONFIG`, and no
//! `./drsg.toml`, [`load`] returns defaults and behaviour is unchanged.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dr_strange_web::{ServeOptions, TlsOptions};
use serde::Deserialize;

/// The parsed `config.toml`. Every section is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerCfg,
    #[serde(default)]
    pub logging: LoggingCfg,
    /// LLM API keys as environment-variable name → secret, e.g.
    /// `OPENAI_API_KEY = "sk-…"`. Applied to the environment for the provider
    /// layer (which looks keys up by name) to read.
    #[serde(default)]
    pub llm: BTreeMap<String, String>,
    /// Server-side defaults for `digest.run` (the web AIgest ingest); a request
    /// param overrides these, which override the built-ins (8 / 4000).
    #[serde(default)]
    pub digest: DigestCfg,
    /// URL-fetch policy for the web AIgest (ROADMAP §9).
    #[serde(default)]
    pub fetch: FetchCfg,
    /// Proxy policy for the requests this binary makes (issue #37).
    #[serde(default)]
    pub network: NetworkCfg,
    /// Preprocessor plugins (ROADMAP §11): sandbox budgets, and each plugin's
    /// own settings.
    ///
    /// Parsed even in a build with no plugin host, and unread there: one
    /// `drsg.toml` is shared by every binary an operator runs, and a
    /// `--no-default-features` build that *rejected* a `[plugins]` section
    /// would make that file un-shareable over a section it merely has no use
    /// for.
    #[serde(default)]
    #[cfg_attr(not(feature = "digest"), allow(dead_code))]
    pub plugins: PluginsCfg,
}

/// The `[digest]` section — server-side ingestion tuning.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestCfg {
    /// Per-chunk extraction chat calls to run concurrently (default 8).
    pub concurrency: Option<usize>,
    /// Target chunk size in characters (default 4000).
    pub chunk_chars: Option<usize>,
    /// Embedding provider (preset name or base URL). Setting it turns on
    /// embed-on-write for `/mcp`'s `write_nodes`: nodes an agent writes get a
    /// vector from the same recipe `digest` uses, so both land in one index.
    /// Unset leaves writes exactly as given.
    pub embed_provider: Option<String>,
    /// Embedding model, when the preset's default is not wanted.
    pub embed_model: Option<String>,
    /// Environment variable holding the embedding key. The key is read from
    /// the process environment at call time — never from config, never from a
    /// request.
    pub embed_key_env: Option<String>,
}

/// The `[network]` section — where this binary's own requests go.
///
/// A daemon started from systemd has no shell environment to read `https_proxy`
/// out of, which is why the file can say it at all. The environment still wins
/// when it is set, so one command can be run through a different proxy — or,
/// with an empty value, through none — without editing the file.
///
/// It governs the requests the operator asks for: `plugin install`, `update`,
/// and the LLM provider. A URL a *caller* named is fetched under the address
/// guard instead, which a proxy would make unenforceable.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCfg {
    /// e.g. `http://127.0.0.1:7897` or `socks5://127.0.0.1:7897`. Overridden
    /// by `ALL_PROXY`, `HTTPS_PROXY` or `HTTP_PROXY`.
    pub proxy: Option<String>,
    /// Hosts that bypass the proxy, `NO_PROXY`-style: `localhost, ::1,
    /// .internal`. Overridden by `NO_PROXY`. A local LLM endpoint belongs
    /// here.
    pub no_proxy: Option<String>,
}

/// The `[fetch]` section — URL ingestion policy.
///
/// Fetching is enabled by default. What is *not* a default is reaching the
/// private network: the address guard refuses loopback, RFC-1918, link-local
/// (where cloud metadata lives) and the rest of the non-routable space, and
/// `allow_private` is the one deliberate exception an operator can make —
/// e.g. `allow_private = ["10.0.0.0/8"]` to read an intranet wiki. It is not a
/// switch that turns the guard off.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchCfg {
    /// Set false to refuse URL fetching outright.
    pub enabled: Option<bool>,
    /// Ceiling on pages kept in one crawl (default 10). A request may ask for
    /// fewer, never more.
    pub max_pages: Option<usize>,
    /// Link-following depth (default 1).
    pub max_depth: Option<usize>,
    /// Requests in flight at once (default 4).
    pub concurrency: Option<usize>,
    /// CIDR blocks to re-permit despite not being publicly routable.
    pub allow_private: Option<Vec<String>>,
}

/// The `[plugins]` section — sandbox budgets, plus one sub-table per plugin.
///
/// ```toml
/// [plugins]
/// fuel = 200000000000    # instructions per sandbox call; 0 = unbounded
/// memory_mb = 3072       # linear memory per call
///
/// [plugins.rust]
/// include_source = true  # a plugin's own settings pass through untouched
/// ```
///
/// No `deny_unknown_fields` here, deliberately: the unknown fields *are* the
/// per-plugin sub-tables, and what a plugin can be configured to do is the
/// plugin's business, not this file's.
#[derive(Debug, Default, Deserialize)]
// Read by `plugin_config` below, which needs the plugin host; without it these
// are parsed and ignored, on purpose — see `Config::plugins`.
#[cfg_attr(not(feature = "digest"), allow(dead_code))]
pub struct PluginsCfg {
    /// Instructions one sandbox call may execute. `0` disables the check for a
    /// trusted plugin on an input big enough to make the ceiling a nuisance.
    pub fuel: Option<u64>,
    /// Linear memory per sandbox call, in MiB. No value lifts the 4 GiB
    /// ceiling wasm32 itself imposes.
    pub memory_mb: Option<u64>,
    /// Wall-clock deadline per sandbox call, in seconds; `0` switches it off
    /// (→ `DRSG_PLUGINS_DEADLINE_SECS`, which overrides it).
    pub deadline_secs: Option<u64>,
    /// Linear memory every sandbox call in the process may hold together, in
    /// MiB; `0` keeps the default (→ `DRSG_PLUGINS_TOTAL_MEMORY_MB`, which
    /// overrides it).
    pub total_memory_mb: Option<u64>,
    /// An explicit plugin-store directory; defaults to the per-user store.
    pub store_dir: Option<PathBuf>,
    /// `[plugins.<name>]` sub-tables, passed to each plugin uninterpreted.
    #[serde(flatten)]
    pub each: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

/// The `[plugins]` section as the routing layer wants it.
#[cfg(feature = "digest")]
pub fn plugin_config(cfg: &Config) -> Result<dr_strange_llm::PluginConfig> {
    let mut options: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (plugin, table) in &cfg.plugins.each {
        let mut kv = Vec::new();
        for (key, value) in table {
            // A plugin reads strings; scalars render as the text an author
            // would have quoted. Structured values have no such rendering, and
            // guessing one would hand the plugin something it never wrote.
            let rendered = match value {
                toml::Value::String(v) => v.clone(),
                toml::Value::Integer(v) => v.to_string(),
                toml::Value::Float(v) => v.to_string(),
                toml::Value::Boolean(v) => v.to_string(),
                other => anyhow::bail!(
                    "[plugins.{plugin}] {key}: a plugin setting must be a                      string, number or bool, not {}",
                    other.type_str()
                ),
            };
            kv.push((key.clone(), rendered));
        }
        options.insert(plugin.clone(), kv);
    }
    Ok(dr_strange_llm::PluginConfig {
        options,
        store_dir: cfg.plugins.store_dir.clone(),
        fuel: cfg.plugins.fuel,
        memory_bytes: cfg.plugins.memory_mb.map(|mb| (mb as usize) << 20),
        deadline_secs: cfg.plugins.deadline_secs,
        total_memory_mb: cfg.plugins.total_memory_mb,
    })
}

/// The `[server]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    /// Listen address; a CLI `--addr` overrides it.
    pub addr: Option<SocketAddr>,
    /// The shared API token (→ `DRSG_TOKEN`).
    pub token: Option<String>,
    /// Ceiling on requests in flight at once.
    pub max_concurrent: Option<usize>,
    /// The source tree behind the graph, for the MCP `grep` tool. `serve
    /// watch` attaches its `--dir` automatically; this covers a plain
    /// `serve` over a digested database.
    pub source_root: Option<std::path::PathBuf>,
    /// How long a write waits for the single writer slot before returning a
    /// retryable timeout. `0` waits forever (the embedded default); omitted
    /// means 30s.
    pub write_timeout_secs: Option<u64>,
    /// How long one request's queries may run before stopping with a retryable
    /// timeout. `0` runs to completion; omitted means 60s.
    pub query_timeout_secs: Option<u64>,
    /// How many commits of history stay reachable by time-travel; versions
    /// older than that are reclaimed at compaction. `0` keeps every version
    /// ever written (disk and compaction work then grow with the server's
    /// life); omitted means 20.
    pub retain_commits: Option<u64>,
    /// Extra allowed browser origins (→ `DRSG_ALLOWED_ORIGINS`).
    pub allowed_origins: Option<Vec<String>>,
    /// Whether the page served to a local browser may carry the token
    /// (→ `DRSG_PAGE_TOKEN`; omitted means yes). `false` for a loopback bind
    /// behind a reverse proxy the server cannot tell from a local browser.
    pub page_token: Option<bool>,
    /// Hostnames (or `host:port`) `/mcp` answers at besides loopback and the
    /// bind address — what a proxy or LAN client sends as `Host`. Honoured
    /// only with a token; merged with `DRSG_ALLOWED_HOSTS`.
    pub allowed_hosts: Option<Vec<String>>,
    /// Longest one MCP tool call may take over `/mcp`, queue included, in
    /// seconds; `0` runs without limit; omitted means the default (or
    /// `DRSG_MCP_TOOL_DEADLINE_SECS`).
    pub mcp_tool_deadline_secs: Option<u64>,
    /// TLS certificate/key; when present, `serve` speaks HTTPS.
    pub tls: Option<TlsCfg>,
}

/// The `[server.tls]` section — a PEM certificate chain and its private key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsCfg {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// The `[logging]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingCfg {
    /// Directory for the rolling log file (→ `DRSG_LOG_DIR`).
    pub dir: Option<PathBuf>,
}

/// Resolve which config file to read: an explicit `--config`, else
/// `$DRSG_CONFIG`, else `./drsg.toml` *if it exists*. An explicit path (flag or
/// env var) that can't be read is an error; the implicit `./drsg.toml` is
/// silently skipped when absent. With no file at all, returns defaults.
pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => match std::env::var_os("DRSG_CONFIG") {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                let default = Path::new("drsg.toml");
                default.exists().then(|| default.to_path_buf())
            }
        },
    };
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

/// Apply the environment-backed settings (token, origins, log dir, LLM keys) to
/// the process environment, *without clobbering* any variable already set — an
/// explicit env var always wins over the file.
///
/// MUST be called from `main` before the logging subsystem starts, before any
/// provider reads the environment, and before the tokio runtime spawns threads:
/// `set_var` is only sound while the process is single-threaded.
pub fn apply_env(cfg: &Config) {
    let set = |key: &str, val: &str| {
        if std::env::var_os(key).is_none() {
            // SAFETY: called from `main` before any thread is spawned, so no
            // other thread can be reading the environment concurrently.
            unsafe { std::env::set_var(key, val) };
        }
    };
    if let Some(token) = &cfg.server.token {
        set("DRSG_TOKEN", token);
    }
    if let Some(origins) = &cfg.server.allowed_origins {
        set("DRSG_ALLOWED_ORIGINS", &origins.join(","));
    }
    if let Some(page_token) = cfg.server.page_token {
        set("DRSG_PAGE_TOKEN", if page_token { "1" } else { "0" });
    }
    if let Some(dir) = &cfg.logging.dir {
        set("DRSG_LOG_DIR", &dir.to_string_lossy());
    }
    for (key, val) in &cfg.llm {
        set(key, val);
    }
}

/// The history retention every command opens the database with: `[server]
/// retain_commits`, or the server's default when the file does not say;
/// `0` is unbounded (`None`). One reading shared by `serve` and the rest of
/// the CLI, so a store sees one policy however it is reached.
pub fn retain_commits(cfg: &Config) -> Option<u64> {
    match cfg.server.retain_commits {
        // 0 means "keep everything", the engine's own encoding of unbounded.
        Some(commits) => (commits > 0).then_some(commits),
        None => Some(dr_strange_web::DEFAULT_RETAIN_COMMITS),
    }
}

/// The outbound proxy policy: `[network]` from the file, with the environment
/// winning wherever it is set.
///
/// One reading shared by every command that reaches the network, so `plugin
/// install`, `update` and the LLM provider cannot disagree about where the
/// traffic goes.
pub fn network(cfg: &Config) -> Result<dr_strange_llm::net::Network> {
    dr_strange_llm::net::Network::resolve(&network_config(cfg))
}

/// Just the file's half of it, with no environment read.
///
/// Separate so it can be tested: `network` resolves against the real
/// environment, and a machine that has `https_proxy` set — the very machine
/// this issue was reported from — would see it win over any fixture.
fn network_config(cfg: &Config) -> dr_strange_llm::net::NetworkConfig {
    dr_strange_llm::net::NetworkConfig {
        proxy: cfg.network.proxy.clone(),
        no_proxy: cfg.network.no_proxy.clone(),
    }
}

/// The web crate's bind rule, applied to the resolved `[server] addr` /
/// `--addr` before the database is opened: a non-loopback bind without a
/// token is refused with the same message `serve` itself would give. Here so
/// a `drsg.toml` that says `addr = "0.0.0.0:7700"` and no token fails at
/// config time, before a replica wipes its directory or a watch starts
/// folding a tree, rather than a few seconds later inside the web crate.
/// `token_configured` is the caller's reading of `[server] token` and
/// `DRSG_TOKEN` (after [`apply_env`] the two agree).
pub fn check_serve_bind(
    cfg: &Config,
    cli_addr: Option<SocketAddr>,
    token_configured: bool,
) -> Result<()> {
    let addr = cli_addr
        .or(cfg.server.addr)
        .unwrap_or_else(|| ServeOptions::default().addr);
    // The origins the server will see: after `apply_env` the file's list is
    // in the environment unless the variable was already set, so the
    // variable is the one reading that matches the server's own.
    let network_origins = std::env::var("DRSG_ALLOWED_ORIGINS")
        .ok()
        .into_iter()
        .chain(cfg.server.allowed_origins.iter().map(|o| o.join(",")))
        .any(|list| dr_strange_web::origins_off_loopback(&list));
    dr_strange_web::check_bind_policy(addr, token_configured, network_origins)
}

/// The per-call MCP deadline the file asks for, in the web crate's encoding:
/// `None` leaves the decision to the server (its default, or the environment
/// variable it reads), `Some(None)` is no deadline, `Some(Some(d))` a bound.
///
/// The file is honoured only when `DRSG_MCP_TOOL_DEADLINE_SECS` is not set:
/// the file's header promises that an environment variable already set
/// always wins over the file, and this knob is no exception. Passing the file
/// value through when the variable is set would have the server apply the
/// file over the environment, the reverse of every other key.
fn mcp_tool_deadline(file_secs: Option<u64>, env_set: bool) -> Option<Option<std::time::Duration>> {
    if env_set {
        return None;
    }
    // 0 means "no deadline", as the environment variable reads it.
    file_secs.map(|secs| (secs > 0).then(|| std::time::Duration::from_secs(secs)))
}

/// Build the web crate's [`ServeOptions`] from the `[server]` section, with an
/// explicit CLI `--addr` overriding the file's `addr`.
pub fn serve_options(cfg: &Config, cli_addr: Option<SocketAddr>) -> ServeOptions {
    let mut opts = ServeOptions::default();
    if let Some(addr) = cli_addr.or(cfg.server.addr) {
        opts.addr = addr;
    }
    if let Some(root) = &cfg.server.source_root {
        opts.source_root = Some(root.clone());
    }
    if let Some(max_concurrent) = cfg.server.max_concurrent {
        opts.max_concurrent = max_concurrent;
    }
    if let Some(secs) = cfg.server.write_timeout_secs {
        // 0 means "wait forever", matching the core's own encoding — an
        // operator who wants the embedded behaviour back can ask for it.
        opts.write_timeout = (secs > 0).then(|| std::time::Duration::from_secs(secs));
    }
    if let Some(secs) = cfg.server.query_timeout_secs {
        opts.query_timeout = (secs > 0).then(|| std::time::Duration::from_secs(secs));
    }
    opts.retain_commits = retain_commits(cfg);
    if let Some(hosts) = &cfg.server.allowed_hosts {
        opts.allowed_hosts = hosts.clone();
    }
    opts.mcp_tool_deadline = mcp_tool_deadline(
        cfg.server.mcp_tool_deadline_secs,
        std::env::var_os(dr_strange_mcp::ENV_TOOL_DEADLINE_SECS).is_some(),
    );
    if let Some(tls) = &cfg.server.tls {
        opts.tls = Some(TlsOptions {
            cert: tls.cert.clone(),
            key: tls.key.clone(),
        });
    }
    if let Some(c) = cfg.digest.concurrency {
        opts.digest.concurrency = c;
    }
    if let Some(c) = cfg.digest.chunk_chars {
        opts.digest.chunk_chars = c;
    }
    if let Some(provider) = &cfg.digest.embed_provider {
        opts.embed_provider = Some((
            provider.clone(),
            cfg.digest.embed_model.clone(),
            cfg.digest.embed_key_env.clone(),
        ));
    }
    if let Some(e) = cfg.fetch.enabled {
        opts.fetch.enabled = e;
    }
    if let Some(p) = cfg.fetch.max_pages {
        opts.fetch.max_pages = p;
    }
    if let Some(d) = cfg.fetch.max_depth {
        opts.fetch.max_depth = d;
    }
    if let Some(c) = cfg.fetch.concurrency {
        opts.fetch.concurrency = c;
    }
    if let Some(a) = &cfg.fetch.allow_private {
        opts.fetch.allow_private = a.clone();
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn full_config_parses() {
        let cfg = parse(
            r#"
            [server]
            addr = "0.0.0.0:9000"
            token = "secret"
            max_concurrent = 32
            retain_commits = 5
            allowed_origins = ["https://a.example", "https://b.example"]
            page_token = false

            [logging]
            dir = "/var/log/drsg"

            [llm]
            OPENAI_API_KEY = "sk-abc"
            DASHSCOPE_API_KEY = "ds-xyz"

            [digest]
            concurrency = 16
            chunk_chars = 6000

            [fetch]
            max_pages = 4
            allow_private = ["10.0.0.0/8"]
            "#,
        );
        assert_eq!(cfg.server.addr.unwrap().to_string(), "0.0.0.0:9000");
        assert_eq!(cfg.server.token.as_deref(), Some("secret"));
        assert_eq!(cfg.server.max_concurrent, Some(32));
        assert_eq!(cfg.server.retain_commits, Some(5));
        assert_eq!(cfg.server.allowed_origins.as_ref().unwrap().len(), 2);
        assert_eq!(cfg.server.page_token, Some(false));
        assert_eq!(cfg.logging.dir.as_deref(), Some(Path::new("/var/log/drsg")));
        assert_eq!(
            cfg.llm.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-abc")
        );
        assert_eq!(cfg.digest.concurrency, Some(16));
        assert_eq!(cfg.digest.chunk_chars, Some(6000));
        assert_eq!(cfg.fetch.max_pages, Some(4));
        assert_eq!(
            cfg.fetch.allow_private.as_deref(),
            Some(&["10.0.0.0/8".to_string()][..])
        );
        // Absent means "the built-in default", not "off".
        assert_eq!(cfg.fetch.enabled, None);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = parse("");
        assert!(cfg.server.addr.is_none());
        assert!(cfg.server.max_concurrent.is_none());
        assert!(cfg.llm.is_empty());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = toml::from_str::<Config>("[server]\nport = 8080\n").unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    #[test]
    fn cli_addr_overrides_file() {
        let cfg = parse("[server]\naddr = \"0.0.0.0:9000\"\nmax_concurrent = 5\n");
        let cli: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let opts = serve_options(&cfg, Some(cli));
        assert_eq!(opts.addr, cli);
        // A field the CLI doesn't touch still comes from the file.
        assert_eq!(opts.max_concurrent, 5);
    }

    #[test]
    fn file_addr_used_when_no_cli_addr() {
        let cfg = parse("[server]\naddr = \"0.0.0.0:9000\"\n");
        let opts = serve_options(&cfg, None);
        assert_eq!(opts.addr.to_string(), "0.0.0.0:9000");
        // Unset → the library default.
        assert_eq!(opts.max_concurrent, dr_strange_web::DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn tls_section_flows_into_serve_options() {
        let cfg =
            parse("[server.tls]\ncert = \"/etc/drsg/cert.pem\"\nkey = \"/etc/drsg/key.pem\"\n");
        let tls = serve_options(&cfg, None).tls.expect("tls set");
        assert_eq!(tls.cert, Path::new("/etc/drsg/cert.pem"));
        assert_eq!(tls.key, Path::new("/etc/drsg/key.pem"));
    }

    #[test]
    fn no_tls_section_means_plain_http() {
        assert!(serve_options(&parse("[server]\n"), None).tls.is_none());
    }

    /// The environment is what `network` layers on top; here only the file's
    /// half is asserted, so the test says the same thing on a machine that has
    /// `https_proxy` set as on one that does not.
    #[test]
    fn the_network_section_is_read_into_the_policy() {
        let cfg =
            parse("[network]\nproxy = \"http://127.0.0.1:7897\"\nno_proxy = \"localhost, ::1\"\n");
        let nc = network_config(&cfg);
        assert_eq!(nc.proxy.as_deref(), Some("http://127.0.0.1:7897"));
        assert_eq!(nc.no_proxy.as_deref(), Some("localhost, ::1"));
    }

    #[test]
    fn no_network_section_says_nothing() {
        let nc = network_config(&parse("[server]\n"));
        assert_eq!(nc.proxy, None);
        assert_eq!(nc.no_proxy, None);
    }

    #[test]
    fn a_bad_proxy_in_the_file_is_refused() {
        let nc = network_config(&parse("[network]\nproxy = \"ftp://nope\"\n"));
        let err = dr_strange_llm::net::Network::resolve(&nc).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("[network]"), "{err}");
    }

    #[test]
    fn an_unknown_network_key_is_refused() {
        assert!(
            toml::from_str::<Config>("[network]\nproxxy = \"x\"\n").is_err(),
            "deny_unknown_fields catches a typo in the section"
        );
    }

    /// A non-loopback `[server] addr` (or `--addr`) without a token is refused
    /// at config time with the web crate's own message; a token, or a
    /// loopback bind, passes.
    #[test]
    fn a_non_loopback_addr_without_a_token_is_refused_at_config_time() {
        let lan = parse("[server]\naddr = \"0.0.0.0:7700\"\n");
        let err = check_serve_bind(&lan, None, false).unwrap_err().to_string();
        assert!(err.contains("refusing to listen on 0.0.0.0:7700"), "{err}");
        assert!(err.contains("DRSG_TOKEN"), "{err}");
        check_serve_bind(&lan, None, true).expect("a token makes the bind acceptable");
        check_serve_bind(&parse(""), None, false).expect("the default bind is loopback");
        check_serve_bind(&parse(""), Some("127.0.0.1:7701".parse().unwrap()), false)
            .expect("an explicit loopback --addr passes");
        let err = check_serve_bind(&parse(""), Some("[::]:7700".parse().unwrap()), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to listen"), "{err}");
        // A loopback bind whose `allowed_origins` names a public origin is
        // a dashboard served through a proxy: refused without a token too,
        // while loopback-only origins pass.
        let proxied = parse("[server]\nallowed_origins = [\"https://graph.example.com\"]\n");
        let err = check_serve_bind(&proxied, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_origins"), "{err}");
        check_serve_bind(&proxied, None, true).expect("a token makes the proxied shape acceptable");
        let local = parse("[server]\nallowed_origins = [\"http://localhost:5173\"]\n");
        check_serve_bind(&local, None, false).expect("a loopback origin is not a proxy");
    }

    /// The `[server]` knobs that reach `/mcp`: `allowed_hosts` lands on the
    /// options verbatim, and `mcp_tool_deadline_secs` distinguishes "not
    /// said" (the server's default) from `0` (no deadline) from a number.
    #[test]
    fn allowed_hosts_and_the_mcp_tool_deadline_reach_the_serve_options() {
        let absent = serve_options(&parse(""), None);
        assert!(absent.allowed_hosts.is_empty());
        assert_eq!(absent.mcp_tool_deadline, None);

        let set = serve_options(
            &parse(
                "[server]\nallowed_hosts = [\"db.internal:7700\", \"graph.example\"]\n\
                 mcp_tool_deadline_secs = 45\n",
            ),
            None,
        );
        assert_eq!(set.allowed_hosts, vec!["db.internal:7700", "graph.example"]);
        assert_eq!(
            set.mcp_tool_deadline,
            Some(Some(std::time::Duration::from_secs(45)))
        );

        let unlimited = serve_options(&parse("[server]\nmcp_tool_deadline_secs = 0\n"), None);
        assert_eq!(unlimited.mcp_tool_deadline, Some(None));
    }

    /// An environment variable already set wins over the file, as the file's
    /// header promises: with `DRSG_MCP_TOOL_DEADLINE_SECS` in the environment
    /// the file's value is not passed on, whatever it says, and the server
    /// reads the variable itself.
    #[test]
    fn the_environment_deadline_wins_over_the_files() {
        assert_eq!(mcp_tool_deadline(Some(45), true), None);
        assert_eq!(mcp_tool_deadline(Some(0), true), None);
        assert_eq!(mcp_tool_deadline(None, true), None);
        assert_eq!(
            mcp_tool_deadline(Some(45), false),
            Some(Some(std::time::Duration::from_secs(45)))
        );
        assert_eq!(mcp_tool_deadline(Some(0), false), Some(None));
        assert_eq!(mcp_tool_deadline(None, false), None);
    }

    /// The `[plugins]` sandbox knobs map onto the llm crate's config as the
    /// numbers the file says; the llm crate applies the `0` readings.
    #[cfg(feature = "digest")]
    #[test]
    fn plugin_deadline_and_total_memory_reach_the_plugin_config() {
        let cfg = plugin_config(&parse(
            "[plugins]\ndeadline_secs = 7\ntotal_memory_mb = 512\nmemory_mb = 64\n",
        ))
        .unwrap();
        assert_eq!(cfg.deadline_secs, Some(7));
        assert_eq!(cfg.total_memory_mb, Some(512));
        assert_eq!(cfg.memory_bytes, Some(64 << 20));
        let absent = plugin_config(&parse("")).unwrap();
        assert_eq!(absent.deadline_secs, None);
        assert_eq!(absent.total_memory_mb, None);
    }

    #[test]
    fn retain_commits_absent_is_the_default_and_zero_is_unbounded() {
        let absent = serve_options(&parse("[server]\n"), None);
        assert_eq!(
            absent.retain_commits,
            Some(dr_strange_web::DEFAULT_RETAIN_COMMITS)
        );
        let set = serve_options(&parse("[server]\nretain_commits = 7\n"), None);
        assert_eq!(set.retain_commits, Some(7));
        let unbounded = serve_options(&parse("[server]\nretain_commits = 0\n"), None);
        assert_eq!(unbounded.retain_commits, None);
        // The rest of the CLI reads the same value the server does.
        assert_eq!(
            retain_commits(&parse("")),
            Some(dr_strange_web::DEFAULT_RETAIN_COMMITS)
        );
        assert_eq!(
            retain_commits(&parse("[server]\nretain_commits = 7\n")),
            Some(7)
        );
        assert_eq!(
            retain_commits(&parse("[server]\nretain_commits = 0\n")),
            None
        );
    }
}
