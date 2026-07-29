//! Write authorization for the JSON-RPC surface (arch/08 security model).
//!
//! v1 model (locked 2026-07-29): a single shared bearer token. The whole thing
//! is a swappable seam — [`Authorizer`] — so issuable, scoped API keys can
//! replace [`SharedToken`] later without touching dispatch or any client.
//!
//! Two independent layers protect a write, and they defend against *different*
//! attackers:
//!   * an **Origin guard** in [`crate::server`] rejects a browser request whose
//!     `Origin` isn't loopback — this defeats cross-site (CSRF / DNS-rebinding)
//!     writes, which binding to localhost alone does NOT. `Origin` is a
//!     browser-set header a page cannot forge, which is exactly what makes it a
//!     usable CSRF signal. Native clients (curl, the language SDKs) send no
//!     `Origin` and sail past this layer — the *token*, not the Origin, is what
//!     authenticates them.
//!   * a **bearer token** here gates every non-read method, for every client.

/// How much authority a method needs. Every dispatch arm names one explicitly,
/// so a new method cannot ship ungated by omission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Pure reads. Still authenticated — the *whole* surface requires a
    /// credential (or the same-origin-UI fallback); the level matters only to a
    /// future scoped-key backend that could mint read-only keys.
    Read,
    /// Mutations, or operations that spend the server's provider credentials
    /// (e.g. `digest.run`'s LLM call).
    Write,
    /// Administrative operations (plane create / rename / delete / set-props;
    /// future key management). Same gate as `Write` under the single-token
    /// model; kept distinct so scoped keys can separate the two later.
    Admin,
}

/// Per-request credentials the server extracts from the transport headers.
#[derive(Default, Clone)]
pub struct Credentials {
    /// Token from `Authorization: Bearer …` (HTTP) or `?token=` (WebSocket —
    /// the browser WS API can't set request headers).
    pub bearer: Option<String>,
    /// True when the request carried an *allowed* `Origin` — i.e. it is our own
    /// same-origin browser UI. The server rejects disallowed origins before we
    /// reach here, so this is only ever "trusted local UI" vs "native client".
    pub local_ui: bool,
}

/// The authorization backend. Swap [`SharedToken`] for an issuable-key store
/// later without changing dispatch or any method.
pub trait Authorizer: Send + Sync {
    fn allows(&self, access: Access, creds: &Credentials) -> bool;
}

/// A single shared secret (the v1 model), read from `DRSG_TOKEN`.
pub struct SharedToken {
    token: Option<String>,
}

impl SharedToken {
    /// Build from an explicit secret (`None`/empty = no token configured).
    /// Tests use this; production uses [`SharedToken::from_env`].
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: token.filter(|t| !t.is_empty()),
        }
    }

    /// Read the shared secret from `DRSG_TOKEN` (unset or empty = none).
    pub fn from_env() -> Self {
        Self::new(std::env::var("DRSG_TOKEN").ok())
    }

    /// Whether a token is configured — drives the startup banner and the
    /// zero-config local-UI write fallback.
    pub fn is_configured(&self) -> bool {
        self.token.is_some()
    }

    fn token_ok(&self, bearer: &Option<String>) -> bool {
        match (&self.token, bearer) {
            (Some(want), Some(got)) => ct_eq(want.as_bytes(), got.as_bytes()),
            _ => false,
        }
    }
}

impl Authorizer for SharedToken {
    fn allows(&self, _access: Access, creds: &Credentials) -> bool {
        // Single shared token: authentication is uniform across read / write /
        // admin — the entire surface is protected. (A future scoped-key backend
        // would branch on `access`; that's why the trait still carries it.)
        //
        // A valid token authorizes any client — an SDK, curl, or the browser
        // once the token is injected into the page.
        if self.token_ok(&creds.bearer) {
            return true;
        }
        // Zero-config desktop: with NO token set, our own same-origin browser
        // UI is trusted (the Origin guard is its CSRF shield). Any client
        // without the token — even for a read — is denied, so programmatic
        // (SDK / curl) access requires an explicit DRSG_TOKEN.
        self.token.is_none() && creds.local_ui
    }
}

/// Constant-time byte comparison so a near-miss token can't be recovered by
/// timing. (Length is compared up front — a token's *length* is not a secret.)
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The resolved authorization for one request: a backend plus the caller's
/// extracted credentials. Threaded through [`crate::rpc::handle`] so each
/// method's [`Access`] is enforced at dispatch.
pub struct Auth<'a> {
    authorizer: &'a dyn Authorizer,
    creds: Credentials,
}

impl<'a> Auth<'a> {
    pub fn new(authorizer: &'a dyn Authorizer, creds: Credentials) -> Self {
        Self { authorizer, creds }
    }

    /// Does this caller satisfy `access`?
    pub fn allows(&self, access: Access) -> bool {
        self.authorizer.allows(access, &self.creds)
    }

    /// Permissive auth for unit tests: no token, treated as the local UI, so
    /// reads and writes are both allowed.
    #[cfg(test)]
    pub fn allow_all() -> Auth<'static> {
        static LOCAL: SharedToken = SharedToken { token: None };
        Auth::new(
            &LOCAL,
            Credentials {
                bearer: None,
                local_ui: true,
            },
        )
    }
}

// ---- Origin allow-list (the browser CSRF guard) ---------------------------

/// Allow-list for browser `Origin`s. v1: loopback hosts on any port, plus any
/// exact origins from `DRSG_ALLOWED_ORIGINS` (comma-separated) for an operator
/// serving the UI behind a known host. A network deployment configures the
/// latter; the loopback default is safe for the local tool.
pub struct AllowedOrigins {
    extra: Vec<String>,
}

impl AllowedOrigins {
    pub fn from_env() -> Self {
        let extra = std::env::var("DRSG_ALLOWED_ORIGINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self { extra }
    }

    /// Is this `Origin` header value trusted? A loopback host (any port) or an
    /// exact configured origin. `null` and non-loopback hosts are not.
    pub fn allows(&self, origin: &str) -> bool {
        if self.extra.iter().any(|o| o == origin) {
            return true;
        }
        matches!(host_of(origin), Some(h) if is_loopback_host(h))
    }
}

/// Extract the host from an `Origin` (`scheme://host[:port]`), stripping IPv6
/// brackets. Returns `None` for `null` or a malformed origin.
fn host_of(origin: &str) -> Option<&str> {
    let after_scheme = origin.split_once("://")?.1;
    let host = if let Some(rest) = after_scheme.strip_prefix('[') {
        // IPv6 literal: [::1]:port
        &rest[..rest.find(']')?]
    } else {
        after_scheme.split(':').next()?
    };
    (!host.is_empty()).then_some(host)
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(bearer: Option<&str>) -> Credentials {
        Credentials {
            bearer: bearer.map(String::from),
            local_ui: false,
        }
    }
    fn browser(bearer: Option<&str>) -> Credentials {
        Credentials {
            bearer: bearer.map(String::from),
            local_ui: true,
        }
    }

    #[test]
    fn every_access_level_needs_a_credential_when_a_token_is_set() {
        // The whole surface is protected: with a token configured, no request —
        // read, write, or admin — gets through without it.
        let guarded = SharedToken::new(Some("s3cret".into()));
        for access in [Access::Read, Access::Write, Access::Admin] {
            assert!(!guarded.allows(access, &native(None)));
            assert!(!guarded.allows(access, &native(Some("nope"))));
            assert!(guarded.allows(access, &native(Some("s3cret"))));
        }
    }

    #[test]
    fn no_token_allows_local_ui_but_not_native_clients() {
        let open = SharedToken::new(None);
        // Same-origin browser UI works with no token (CSRF-shielded) — including
        // reads, so the local dashboard needs zero configuration.
        assert!(open.allows(Access::Read, &browser(None)));
        assert!(open.allows(Access::Write, &browser(None)));
        // A programmatic client with no token is denied even a read — it must
        // set DRSG_TOKEN to reach the API at all.
        assert!(!open.allows(Access::Read, &native(None)));
        assert!(!open.allows(Access::Write, &native(None)));
    }

    #[test]
    fn configured_token_is_required_even_from_the_browser() {
        // Once a token is set, the local UI must present it too (the server
        // injects it into the page) — the origin bypass only applies when no
        // token is configured.
        let guarded = SharedToken::new(Some("s3cret".into()));
        assert!(!guarded.allows(Access::Read, &browser(None)));
        assert!(!guarded.allows(Access::Write, &browser(None)));
        assert!(guarded.allows(Access::Read, &browser(Some("s3cret"))));
        assert!(guarded.allows(Access::Write, &browser(Some("s3cret"))));
    }

    #[test]
    fn ct_eq_matches_std_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn loopback_origins_are_allowed() {
        let o = AllowedOrigins { extra: vec![] };
        assert!(o.allows("http://127.0.0.1:7700"));
        assert!(o.allows("http://localhost:5173"));
        assert!(o.allows("http://[::1]:7700"));
        assert!(o.allows("https://127.0.0.1"));
    }

    #[test]
    fn foreign_and_null_origins_are_rejected() {
        let o = AllowedOrigins { extra: vec![] };
        assert!(!o.allows("https://evil.example.com"));
        assert!(!o.allows("http://169.254.1.1")); // link-local, not loopback
        assert!(!o.allows("null"));
        assert!(!o.allows("garbage"));
    }

    #[test]
    fn extra_origins_are_honored() {
        let o = AllowedOrigins {
            extra: vec!["https://graph.internal".into()],
        };
        assert!(o.allows("https://graph.internal"));
        assert!(!o.allows("https://graph.internal.evil.com"));
    }
}
