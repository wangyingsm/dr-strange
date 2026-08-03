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
}

/// The `[digest]` section — server-side ingestion tuning.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestCfg {
    /// Per-chunk extraction chat calls to run concurrently (default 8).
    pub concurrency: Option<usize>,
    /// Target chunk size in characters (default 4000).
    pub chunk_chars: Option<usize>,
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
    /// Extra allowed browser origins (→ `DRSG_ALLOWED_ORIGINS`).
    pub allowed_origins: Option<Vec<String>>,
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
    if let Some(dir) = &cfg.logging.dir {
        set("DRSG_LOG_DIR", &dir.to_string_lossy());
    }
    for (key, val) in &cfg.llm {
        set(key, val);
    }
}

/// Build the web crate's [`ServeOptions`] from the `[server]` section, with an
/// explicit CLI `--addr` overriding the file's `addr`.
pub fn serve_options(cfg: &Config, cli_addr: Option<SocketAddr>) -> ServeOptions {
    let mut opts = ServeOptions::default();
    if let Some(addr) = cli_addr.or(cfg.server.addr) {
        opts.addr = addr;
    }
    if let Some(max_concurrent) = cfg.server.max_concurrent {
        opts.max_concurrent = max_concurrent;
    }
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
            allowed_origins = ["https://a.example", "https://b.example"]

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
        assert_eq!(cfg.server.allowed_origins.as_ref().unwrap().len(), 2);
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
}
