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

use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use ahash::AHashMap;

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
    /// Whether the listener this token guards is bound to a loopback address.
    /// The zero-config fallback (no token, same-origin UI) is only ever
    /// granted when it is: on any other bind, "our own UI" is a page anyone
    /// on the network can load, and an allowed `Origin` no longer implies a
    /// local human (arch/08 §4.2 invariant 2).
    loopback_bind: bool,
}

impl SharedToken {
    /// Build from an explicit secret (`None`/empty = no token configured).
    /// Tests use this; production uses [`SharedToken::from_env`].
    ///
    /// Assumes a loopback listener — the desktop shape every in-process test
    /// exercises. [`SharedToken::bound_to_loopback`] narrows it for the real
    /// server, which knows its bind address.
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: token.filter(|t| !t.is_empty()),
            loopback_bind: true,
        }
    }

    /// Record whether the listener is loopback-bound. With `false`, the
    /// zero-config local-UI fallback is never granted: only a token
    /// authenticates. `server::run` refuses to start a tokenless non-loopback
    /// listener at all, so this is the second line behind that check — a
    /// future code path that builds the state some other way still cannot
    /// hand a LAN browser unauthenticated write access.
    pub fn bound_to_loopback(mut self, loopback: bool) -> Self {
        self.loopback_bind = loopback;
        self
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
        // (SDK / curl) access requires an explicit DRSG_TOKEN. Only on a
        // loopback bind: elsewhere the "own UI" is reachable by anyone who can
        // reach the port, and an Origin proves nothing about who is behind it.
        self.loopback_bind && self.token.is_none() && creds.local_ui
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

/// Wraps another [`Authorizer`], never granting `Write`/`Admin` regardless of
/// what the inner backend would allow (`serve --follow`, arch/01 §9) — a
/// third, orthogonal layer alongside the Origin guard and the bearer token:
/// even a valid token cannot mutate a replica. Defense-in-depth alongside
/// `NativeEngine`'s `read_only` flag, which also blocks any write that
/// somehow bypassed HTTP entirely.
pub struct ReadOnlyAuthorizer<A>(pub A);

impl<A: Authorizer> Authorizer for ReadOnlyAuthorizer<A> {
    fn allows(&self, access: Access, creds: &Credentials) -> bool {
        access == Access::Read && self.0.allows(Access::Read, creds)
    }
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
        static LOCAL: SharedToken = SharedToken {
            token: None,
            loopback_bind: true,
        };
        Auth::new(
            &LOCAL,
            Credentials {
                bearer: None,
                local_ui: true,
            },
        )
    }
}

// ---- Failed-auth throttle --------------------------------------------------

/// How many wrong bearers a peer may present before it is made to wait.
/// Five covers a human retyping a token and a client with one stale
/// credential and one fresh one; an enumeration of tokens is not five.
pub const FREE_FAILURES: u32 = 5;
/// The longest a peer is made to wait between attempts. Long enough that a
/// guess costs more than a keystroke, short enough that a locked-out
/// operator who fixed their token is not locked out of their own server.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(300);
/// The most peers the throttle remembers at once. Bounds memory against a
/// client rotating source addresses: past this the idle-longest entry is
/// forgotten, which weakens the throttle for that flood and nothing else.
pub const TRACKED_PEERS: usize = 4096;
/// A peer that has been quiet this long is forgotten — its failures no
/// longer count, and its entry no longer costs memory.
const FORGET_AFTER: Duration = Duration::from_secs(15 * 60);

/// Per-peer brute-force protection on the bearer check, in memory and
/// bounded. Each wrong bearer past [`FREE_FAILURES`] doubles the wait a
/// peer serves before its next request is even read, up to
/// [`MAX_LOCKOUT`]; a correct bearer clears the slate. Consulted by the
/// server's throttle middleware before any handler runs, so a locked-out
/// peer costs a map lookup and nothing else. Failures are what the request
/// *presented*: a bearer that authorizes nothing. A request with no bearer
/// is not a guess and is not counted.
///
/// The clock is a parameter rather than `Instant::now()` inside, so a test
/// can move time without sleeping.
pub struct FailedAuthLimiter {
    peers: Mutex<AHashMap<IpAddr, Strikes>>,
}

struct Strikes {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

impl Default for FailedAuthLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl FailedAuthLimiter {
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(AHashMap::new()),
        }
    }

    /// How much longer `peer` must wait, if it is locked out at `now`.
    pub fn locked_for(&self, peer: IpAddr, now: Instant) -> Option<Duration> {
        let peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        let until = peers.get(&peer)?.locked_until?;
        (until > now).then(|| until - now)
    }

    /// Record a wrong bearer from `peer`. Returns the lockout now imposed,
    /// if this failure crossed the free allowance.
    pub fn failed(&self, peer: IpAddr, now: Instant) -> Option<Duration> {
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        if !peers.contains_key(&peer) && peers.len() >= TRACKED_PEERS {
            Self::make_room(&mut peers, now);
        }
        let strikes = peers.entry(peer).or_insert(Strikes {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });
        strikes.failures = strikes.failures.saturating_add(1);
        strikes.last_seen = now;
        let over = strikes.failures.checked_sub(FREE_FAILURES + 1)?;
        // 1 s, 2 s, 4 s, … — `min(20)` keeps the shift in range long before
        // the cap does.
        let wait = Duration::from_secs(1u64 << over.min(20)).min(MAX_LOCKOUT);
        strikes.locked_until = Some(now + wait);
        Some(wait)
    }

    /// A correct bearer from `peer`: forget its failures.
    pub fn succeeded(&self, peer: IpAddr) {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&peer);
    }

    /// Drop every entry idle past [`FORGET_AFTER`]; if the map is still full,
    /// drop the one idle longest. Called only when a new peer must be
    /// admitted to a full map, so the sweep's cost is paid by the flood that
    /// caused it.
    fn make_room(peers: &mut AHashMap<IpAddr, Strikes>, now: Instant) {
        peers.retain(|_, s| now.duration_since(s.last_seen) < FORGET_AFTER);
        if peers.len() >= TRACKED_PEERS
            && let Some(oldest) = peers
                .iter()
                .min_by_key(|(_, s)| s.last_seen)
                .map(|(ip, _)| *ip)
        {
            peers.remove(&oldest);
        }
    }

    /// How many peers are currently remembered (tests).
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
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
        Self::is_loopback(origin)
    }

    /// Is this `Origin` a loopback host — a page this machine served to a
    /// browser on this machine? Only such an origin can be "the local human's
    /// own UI"; a configured public origin is allowed through the CSRF guard
    /// but is by definition reached over the network.
    pub fn is_loopback(origin: &str) -> bool {
        matches!(host_of(origin), Some(h) if is_loopback_host(h))
    }

    /// Whether any configured origin is off loopback. An operator lists one
    /// so a dashboard served at a public name (through a reverse proxy in
    /// front of this listener) can call the API — which is the operator
    /// saying this deployment faces the network, whatever address the
    /// listener itself is bound to.
    pub fn has_network_origins(&self) -> bool {
        self.extra.iter().any(|o| !Self::is_loopback(o))
    }
}

/// Is a `Host` header value (`host[:port]`, IPv6 in brackets) this machine's
/// loopback? A browser on this machine addressing this listener sends one;
/// a page reached at any other name was addressed to something in front.
pub(crate) fn host_header_is_loopback(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &rest[..end],
            None => return false,
        }
    } else {
        host.split(':').next().unwrap_or("")
    };
    is_loopback_host(name)
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
    fn the_zero_config_fallback_is_only_for_a_loopback_listener() {
        // arch/08 §4.2 invariant 2: on a non-loopback bind, an allowed Origin
        // no longer means "the local human's own UI" — anyone on the network
        // can load the page — so the tokenless fallback is never granted.
        let open = SharedToken::new(None).bound_to_loopback(false);
        assert!(!open.allows(Access::Read, &browser(None)));
        assert!(!open.allows(Access::Write, &browser(None)));
        // A token still works there, from the browser and from a native client.
        let guarded = SharedToken::new(Some("s3cret".into())).bound_to_loopback(false);
        assert!(guarded.allows(Access::Write, &browser(Some("s3cret"))));
        assert!(guarded.allows(Access::Write, &native(Some("s3cret"))));
        assert!(!guarded.allows(Access::Read, &browser(None)));
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
    fn read_only_authorizer_never_grants_write_or_admin() {
        let inner = SharedToken::new(Some("s3cret".into()));
        let ro = ReadOnlyAuthorizer(inner);
        let valid = native(Some("s3cret"));
        assert!(ro.allows(Access::Read, &valid));
        assert!(!ro.allows(Access::Write, &valid));
        assert!(!ro.allows(Access::Admin, &valid));
        // A bad token still fails, same as the inner authorizer would.
        assert!(!ro.allows(Access::Read, &native(Some("wrong"))));
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

    // ---- failed-auth throttle ----

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn free_failures_cost_nothing_then_the_wait_doubles_to_the_cap() {
        let lim = FailedAuthLimiter::new();
        let t0 = Instant::now();
        for _ in 0..FREE_FAILURES {
            assert_eq!(lim.failed(ip(1), t0), None);
        }
        assert_eq!(lim.locked_for(ip(1), t0), None);
        assert_eq!(lim.failed(ip(1), t0), Some(Duration::from_secs(1)));
        assert_eq!(lim.locked_for(ip(1), t0), Some(Duration::from_secs(1)));
        assert_eq!(lim.locked_for(ip(1), t0 + Duration::from_secs(1)), None);
        assert_eq!(lim.failed(ip(1), t0), Some(Duration::from_secs(2)));
        assert_eq!(lim.failed(ip(1), t0), Some(Duration::from_secs(4)));
        let mut last = Duration::ZERO;
        for _ in 0..40 {
            last = lim.failed(ip(1), t0).unwrap();
        }
        assert_eq!(last, MAX_LOCKOUT);
        // Another peer is unaffected.
        assert_eq!(lim.locked_for(ip(2), t0), None);
    }

    #[test]
    fn a_correct_bearer_clears_the_slate() {
        let lim = FailedAuthLimiter::new();
        let t0 = Instant::now();
        for _ in 0..=FREE_FAILURES {
            lim.failed(ip(1), t0);
        }
        assert!(lim.locked_for(ip(1), t0).is_some());
        lim.succeeded(ip(1));
        assert_eq!(lim.locked_for(ip(1), t0), None);
        assert_eq!(lim.failed(ip(1), t0), None);
    }

    #[test]
    fn the_table_is_bounded_and_forgets_the_idle() {
        let lim = FailedAuthLimiter::new();
        let t0 = Instant::now();
        for i in 0..TRACKED_PEERS as u32 {
            lim.failed(IpAddr::from(i.to_be_bytes()), t0);
        }
        assert_eq!(lim.tracked(), TRACKED_PEERS);
        // A new peer while full and nothing idle: the oldest is evicted, the
        // table does not grow.
        lim.failed(ip(200), t0 + Duration::from_secs(1));
        assert_eq!(lim.tracked(), TRACKED_PEERS);
        // Once everyone else has been idle past the forget window, one new
        // peer sweeps them all.
        let later = t0 + FORGET_AFTER + Duration::from_secs(2);
        lim.failed(IpAddr::from([192, 168, 0, 1]), later);
        assert_eq!(lim.tracked(), 1);
    }

    #[test]
    fn a_configured_public_origin_is_allowed_but_never_local() {
        let origins = AllowedOrigins {
            extra: vec!["https://graph.example.com".into()],
        };
        assert!(origins.allows("https://graph.example.com"));
        assert!(!AllowedOrigins::is_loopback("https://graph.example.com"));
        assert!(AllowedOrigins::is_loopback("http://localhost:5173"));
        assert!(AllowedOrigins::is_loopback("http://[::1]:7700"));
        assert!(origins.has_network_origins());
        let local_only = AllowedOrigins {
            extra: vec!["http://127.0.0.1:5173".into()],
        };
        assert!(!local_only.has_network_origins());
        assert!(!AllowedOrigins { extra: vec![] }.has_network_origins());
    }

    #[test]
    fn a_host_header_is_loopback_only_for_this_machine() {
        assert!(host_header_is_loopback("127.0.0.1:7700"));
        assert!(host_header_is_loopback("localhost"));
        assert!(host_header_is_loopback("[::1]:7700"));
        assert!(!host_header_is_loopback("graph.example.com"));
        assert!(!host_header_is_loopback("192.168.1.20:7700"));
        assert!(!host_header_is_loopback("[::1"));
        assert!(!host_header_is_loopback(""));
    }
}
