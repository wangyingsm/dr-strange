//! Outbound network policy: the proxy a request goes through, and the hosts
//! that bypass it (issue #37).
//!
//! It lives in this crate rather than in `dr-strange-web` because the LLM
//! client is the lowest crate that makes an outbound request, and
//! `dr-strange-web` depends on this one.
//!
//! ureq 2 reads no proxy variables of its own here (the `proxy-from-env`
//! feature is off) and has no `NO_PROXY` support at all, so both are resolved
//! here. That is what makes a bypass list possible: an agent is built per call
//! against a known destination, so the proxy is a per-request decision rather
//! than a property of a shared agent.

use anyhow::{Context, Result, bail};

/// Where a request is going, which is all the proxy decision is made from.
#[derive(Clone, Copy, Debug)]
pub struct Destination<'a> {
    /// `http` or `https`.
    pub scheme: &'a str,
    /// The host, without brackets for an IPv6 literal.
    pub host: &'a str,
    /// The port the request would connect to.
    pub port: u16,
}

/// A proxy URL as configured, kept verbatim so messages can quote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyUrl(String);

impl ProxyUrl {
    /// Parse and validate against ureq's own grammar, so a bad value is
    /// refused where it was written rather than at the first request.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("a proxy URL cannot be empty");
        }
        ureq::Proxy::new(s).map_err(|_| {
            anyhow::anyhow!(
                "`{}` is not a proxy URL — expected <protocol>://[user:password@]host[:port], \
                 where protocol is http, socks4, socks4a, socks5 or socks",
                Self(s.to_string()).shown()
            )
        })?;
        Ok(Self(s.to_string()))
    }

    /// The URL with any password replaced, for logs and errors.
    pub fn shown(&self) -> String {
        let (prefix, rest) = match self.0.split_once("://") {
            Some((p, r)) => (format!("{p}://"), r),
            None => (String::new(), self.0.as_str()),
        };
        match rest.rsplit_once('@') {
            Some((creds, host)) => {
                let user = creds.split_once(':').map_or(creds, |(u, _)| u);
                format!("{prefix}{user}:***@{host}")
            }
            None => format!("{prefix}{rest}"),
        }
    }

    /// The ureq value, rebuilt per agent because `ureq::Proxy` is not `Eq`.
    fn to_ureq(&self) -> Result<ureq::Proxy> {
        ureq::Proxy::new(&self.0)
            .map_err(|e| anyhow::anyhow!("proxy {}: {e}", self.shown()))
            .context("building the proxy for this request")
    }
}

/// One `NO_PROXY` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rule {
    /// `*` — everything bypasses.
    All,
    /// An exact host, matched without case.
    Exact(String),
    /// `.suffix` or `*.suffix`, matching the suffix itself and anything under
    /// it, as curl does.
    Suffix(String),
    /// A host (either form above) that only bypasses on one port.
    OnPort(Box<Rule>, u16),
}

impl Rule {
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        if entry == "*" {
            return Some(Self::All);
        }
        // A bracketed IPv6 literal may carry a port; a bare one is all colons
        // and must not be split on the last of them.
        if let Some(rest) = entry.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            let host = Self::host_rule(host);
            return match tail.strip_prefix(':') {
                Some(p) => Some(Self::OnPort(Box::new(host), p.parse().ok()?)),
                None => Some(host),
            };
        }
        if entry.matches(':').count() == 1
            && let Some((host, port)) = entry.rsplit_once(':')
            && let Ok(port) = port.parse::<u16>()
        {
            return Some(Self::OnPort(Box::new(Self::host_rule(host)), port));
        }
        Some(Self::host_rule(entry))
    }

    fn host_rule(host: &str) -> Self {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        match host.strip_prefix("*.").or_else(|| host.strip_prefix('.')) {
            Some(suffix) => Self::Suffix(suffix.to_string()),
            None => Self::Exact(host),
        }
    }

    fn matches(&self, to: Destination<'_>) -> bool {
        let host = to.host.trim_end_matches('.').to_ascii_lowercase();
        match self {
            Self::All => true,
            Self::Exact(h) => *h == host,
            Self::Suffix(s) => host == *s || host.ends_with(&format!(".{s}")),
            Self::OnPort(inner, port) => *port == to.port && inner.matches(to),
        }
    }
}

/// The hosts that bypass the proxy, parsed from a `NO_PROXY`-style list.
///
/// Entries are separated by commas or whitespace. CIDR blocks are not
/// supported; an IP address is matched literally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoProxy(Vec<Rule>);

impl NoProxy {
    /// Parse a list, ignoring empty entries and anything unparseable.
    pub fn parse(list: &str) -> Self {
        Self(
            list.split([',', ' ', '\t', '\n'])
                .filter_map(Rule::parse)
                .collect(),
        )
    }

    /// Whether a request to `to` goes direct.
    pub fn bypasses(&self, to: Destination<'_>) -> bool {
        self.0.iter().any(|r| r.matches(to))
    }

    /// Whether the list has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A proxy policy as written in the configuration file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Used for both schemes when no environment variable overrides it.
    pub proxy: Option<String>,
    /// A `NO_PROXY`-style bypass list.
    pub no_proxy: Option<String>,
}

/// Where outbound requests go: a proxy per scheme, and the bypass list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    https: Option<ProxyUrl>,
    http: Option<ProxyUrl>,
    no_proxy: NoProxy,
}

impl Network {
    /// No proxy for anything — the policy for a caller-named URL, where the
    /// address guard is the protection and a proxy would make it unenforceable.
    pub const DIRECT: &'static Self = &Self {
        https: None,
        http: None,
        no_proxy: NoProxy(Vec::new()),
    };

    /// An owned [`Network::DIRECT`], for a field that holds one.
    pub fn direct() -> Self {
        Self::default()
    }

    /// A policy the caller has already decided, with no environment read.
    ///
    /// `resolve` lets the environment win, which is right for a command an
    /// operator runs and wrong for a caller that means exactly this proxy —
    /// a test with a stub, above all, since the machine running it may well
    /// have `ALL_PROXY` set.
    pub fn through(proxy: ProxyUrl, no_proxy: NoProxy) -> Self {
        Self {
            https: Some(proxy.clone()),
            http: Some(proxy),
            no_proxy,
        }
    }

    /// Resolve the policy, with the environment winning over `cfg`.
    ///
    /// `ALL_PROXY` covers both schemes; `HTTPS_PROXY` and `HTTP_PROXY` cover
    /// one each. Each is read uppercase first, then lowercase. A variable that
    /// is set but empty means "go direct", which is how a proxy configured in
    /// the file is turned off for one command.
    pub fn resolve(cfg: &NetworkConfig) -> Result<Self> {
        Self::resolve_from(cfg, &env_var)
    }

    /// The precedence itself, over an arbitrary lookup.
    ///
    /// Separate from [`Network::resolve`] so it can be tested without setting
    /// process-wide variables: the test runner shares one environment across
    /// threads, and CI runs the suite in a single process.
    fn resolve_from(cfg: &NetworkConfig, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        // Uppercase first, then lowercase, which is the order ureq reads them.
        let get = |upper: &str| lookup(upper).or_else(|| lookup(&upper.to_ascii_lowercase()));
        let named = |upper: &str| -> Result<Option<Option<ProxyUrl>>> {
            let Some(v) = get(upper) else {
                return Ok(None);
            };
            if v.trim().is_empty() {
                return Ok(Some(None));
            }
            let p =
                ProxyUrl::parse(&v).with_context(|| format!("the {upper} environment variable"))?;
            Ok(Some(Some(p)))
        };

        let from_cfg = cfg
            .proxy
            .as_deref()
            .map(|s| ProxyUrl::parse(s).context("the `proxy` key in the [network] config section"))
            .transpose()?;

        let all = named("ALL_PROXY")?;
        let pick = |one: Option<Option<ProxyUrl>>| match all.clone().or(one) {
            // The variable was set: its value stands, empty meaning direct.
            Some(found) => found,
            None => from_cfg.clone(),
        };
        let https = pick(named("HTTPS_PROXY")?);
        let http = pick(named("HTTP_PROXY")?);

        let no_proxy = match get("NO_PROXY") {
            Some(v) => NoProxy::parse(&v),
            None => cfg
                .no_proxy
                .as_deref()
                .map(NoProxy::parse)
                .unwrap_or_default(),
        };
        Ok(Self {
            https,
            http,
            no_proxy,
        })
    }

    /// The proxy that would carry a request to `to`, or `None` for direct.
    pub fn proxy_for(&self, to: Destination<'_>) -> Option<&ProxyUrl> {
        if self.no_proxy.bypasses(to) {
            return None;
        }
        match to.scheme {
            "https" => self.https.as_ref(),
            _ => self.http.as_ref(),
        }
    }

    /// Attach the proxy for `to`, if there is one, to an agent builder.
    pub fn apply(&self, b: ureq::AgentBuilder, to: Destination<'_>) -> Result<ureq::AgentBuilder> {
        match self.proxy_for(to) {
            Some(p) => Ok(b.proxy(p.to_ureq()?)),
            None => Ok(b),
        }
    }

    /// Whether any request could be proxied, for the messages that have to say
    /// so before a destination is known.
    pub fn is_direct(&self) -> bool {
        self.https.is_none() && self.http.is_none()
    }
}

/// One variable, by exact name — the case fallback belongs to the precedence
/// in [`Network::resolve_from`], where it is tested.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(scheme: &'static str, host: &'static str, port: u16) -> Destination<'static> {
        Destination { scheme, host, port }
    }

    /// A stand-in environment, so no test touches the process's own.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        }
    }

    fn resolved(cfg: &NetworkConfig, pairs: &[(&str, &str)]) -> Network {
        Network::resolve_from(cfg, &env(pairs)).unwrap()
    }

    fn cfg(proxy: Option<&str>, no_proxy: Option<&str>) -> NetworkConfig {
        NetworkConfig {
            proxy: proxy.map(str::to_string),
            no_proxy: no_proxy.map(str::to_string),
        }
    }

    #[test]
    fn a_proxy_url_is_validated_where_it_is_written() {
        assert!(ProxyUrl::parse("http://127.0.0.1:7897").is_ok());
        assert!(ProxyUrl::parse("socks5://127.0.0.1:7897").is_ok());
        // No scheme means http, as ureq reads it.
        assert!(ProxyUrl::parse("127.0.0.1:7897").is_ok());
        let err = ProxyUrl::parse("ftp://nope").unwrap_err().to_string();
        assert!(err.contains("is not a proxy URL"), "{err}");
        assert!(ProxyUrl::parse("   ").is_err(), "empty is refused");
    }

    #[test]
    fn a_password_never_reaches_a_message() {
        let p = ProxyUrl::parse("http://alice:hunter2@proxy.example:8080").unwrap();
        assert_eq!(p.shown(), "http://alice:***@proxy.example:8080");
        assert!(!p.shown().contains("hunter2"));
        // Nothing to redact, nothing changed.
        let plain = ProxyUrl::parse("http://proxy.example:8080").unwrap();
        assert_eq!(plain.shown(), "http://proxy.example:8080");
    }

    #[test]
    fn no_proxy_matches_exact_hosts_without_case() {
        let n = NoProxy::parse("localhost, Example.COM");
        assert!(n.bypasses(to("http", "localhost", 80)));
        assert!(n.bypasses(to("https", "example.com", 443)));
        assert!(!n.bypasses(to("https", "other.com", 443)));
        // A sibling is not a match; only a suffix rule reaches subdomains.
        assert!(!n.bypasses(to("https", "a.example.com", 443)));
    }

    #[test]
    fn a_suffix_rule_covers_the_domain_and_everything_under_it() {
        for list in [".internal", "*.internal"] {
            let n = NoProxy::parse(list);
            assert!(n.bypasses(to("https", "internal", 443)), "{list}");
            assert!(n.bypasses(to("https", "wiki.internal", 443)), "{list}");
            assert!(n.bypasses(to("https", "a.b.internal", 443)), "{list}");
            assert!(!n.bypasses(to("https", "notinternal", 443)), "{list}");
        }
    }

    #[test]
    fn a_trailing_dot_is_the_same_host() {
        let n = NoProxy::parse("example.com");
        assert!(n.bypasses(to("https", "example.com.", 443)));
    }

    #[test]
    fn a_star_bypasses_everything() {
        let n = NoProxy::parse("*");
        assert!(n.bypasses(to("https", "anything.example", 443)));
    }

    #[test]
    fn an_entry_with_a_port_only_bypasses_that_port() {
        let n = NoProxy::parse("example.com:8080");
        assert!(n.bypasses(to("http", "example.com", 8080)));
        assert!(!n.bypasses(to("https", "example.com", 443)));
    }

    #[test]
    fn an_ipv6_literal_is_matched_whole_and_may_carry_a_port() {
        let bare = NoProxy::parse("::1");
        assert!(bare.bypasses(to("http", "::1", 80)));
        let ported = NoProxy::parse("[::1]:11434");
        assert!(ported.bypasses(to("http", "::1", 11434)));
        assert!(!ported.bypasses(to("http", "::1", 80)));
    }

    #[test]
    fn separators_and_empty_entries_are_tolerated() {
        let n = NoProxy::parse(" localhost,, 127.0.0.1 \n ::1 ");
        assert!(n.bypasses(to("http", "localhost", 80)));
        assert!(n.bypasses(to("http", "127.0.0.1", 80)));
        assert!(n.bypasses(to("http", "::1", 80)));
        assert!(NoProxy::parse("  ").is_empty());
    }

    #[test]
    fn the_scheme_picks_the_variable() {
        let net = Network {
            https: ProxyUrl::parse("http://secure:1").ok(),
            http: ProxyUrl::parse("http://plain:2").ok(),
            no_proxy: NoProxy::default(),
        };
        assert_eq!(
            net.proxy_for(to("https", "a.example", 443))
                .unwrap()
                .shown(),
            "http://secure:1"
        );
        assert_eq!(
            net.proxy_for(to("http", "a.example", 80)).unwrap().shown(),
            "http://plain:2"
        );
    }

    #[test]
    fn a_bypassed_host_has_no_proxy_whatever_the_scheme() {
        let net = Network {
            https: ProxyUrl::parse("http://secure:1").ok(),
            http: ProxyUrl::parse("http://plain:2").ok(),
            no_proxy: NoProxy::parse("localhost"),
        };
        assert!(net.proxy_for(to("http", "localhost", 11434)).is_none());
        assert!(net.proxy_for(to("https", "localhost", 443)).is_none());
        assert!(!net.is_direct(), "a proxy is still configured");
    }

    #[test]
    fn direct_is_direct() {
        let net = Network::direct();
        assert!(net.is_direct());
        assert!(net.proxy_for(to("https", "a.example", 443)).is_none());
    }

    #[test]
    fn the_environment_wins_over_the_config_file() {
        let net = resolved(
            &cfg(Some("http://from-config:1"), None),
            &[("https_proxy", "http://from-env:2")],
        );
        assert_eq!(
            net.proxy_for(to("https", "a.example", 443))
                .unwrap()
                .shown(),
            "http://from-env:2"
        );
        // Nothing named the http scheme, so the file still supplies it.
        assert_eq!(
            net.proxy_for(to("http", "a.example", 80)).unwrap().shown(),
            "http://from-config:1"
        );
    }

    #[test]
    fn the_config_file_is_used_when_nothing_is_set() {
        let net = resolved(&cfg(Some("http://from-config:1"), Some("localhost")), &[]);
        assert_eq!(
            net.proxy_for(to("https", "a.example", 443))
                .unwrap()
                .shown(),
            "http://from-config:1"
        );
        assert!(net.proxy_for(to("http", "localhost", 11434)).is_none());
    }

    #[test]
    fn an_empty_variable_turns_a_configured_proxy_off() {
        let net = resolved(
            &cfg(Some("http://from-config:1"), None),
            &[("https_proxy", "")],
        );
        assert!(
            net.proxy_for(to("https", "a.example", 443)).is_none(),
            "set-but-empty means direct, not unset"
        );
    }

    #[test]
    fn all_proxy_covers_both_schemes_and_outranks_the_named_ones() {
        let net = resolved(
            &cfg(None, None),
            &[
                ("ALL_PROXY", "http://everything:1"),
                ("HTTPS_PROXY", "http://ignored:2"),
            ],
        );
        for d in [to("https", "a.example", 443), to("http", "a.example", 80)] {
            assert_eq!(net.proxy_for(d).unwrap().shown(), "http://everything:1");
        }
    }

    #[test]
    fn uppercase_is_read_before_lowercase() {
        let net = resolved(
            &cfg(None, None),
            &[
                ("HTTPS_PROXY", "http://upper:1"),
                ("https_proxy", "http://lower:2"),
            ],
        );
        assert_eq!(
            net.proxy_for(to("https", "a.example", 443))
                .unwrap()
                .shown(),
            "http://upper:1"
        );
    }

    #[test]
    fn no_proxy_from_the_environment_replaces_the_configured_list() {
        let net = resolved(
            &cfg(Some("http://p:1"), Some("localhost")),
            &[("NO_PROXY", "internal.example")],
        );
        assert!(
            net.proxy_for(to("http", "localhost", 11434)).is_some(),
            "the configured list is replaced, not merged"
        );
        assert!(
            net.proxy_for(to("https", "internal.example", 443))
                .is_none()
        );
    }

    #[test]
    fn a_bad_proxy_value_names_where_it_came_from() {
        let err = Network::resolve_from(&cfg(None, None), &env(&[("HTTPS_PROXY", "ftp://nope")]))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("HTTPS_PROXY"), "{msg}");

        let err = Network::resolve_from(&cfg(Some("ftp://nope"), None), &env(&[])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("[network]"), "{msg}");
    }
}
