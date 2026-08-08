//! Address policy for the URL fetcher (ROADMAP §9).
//!
//! §9 is the first feature where a client names an address and the **server**
//! connects to it, and the server's network position is the privileged one:
//! cloud metadata endpoints, localhost admin ports, RFC-1918 neighbours. So the
//! guard checks the *resolved address*, never the hostname — a name that
//! resolves to `127.0.0.1` is precisely the attack, and hostname allow-lists
//! cannot see it.
//!
//! It does that through ureq's resolver hook rather than as a check before the
//! request, because the addresses this returns are the addresses ureq connects
//! to. Approving a name and then letting the client resolve it again is the DNS
//! rebinding hole: the second lookup can answer differently from the first.
//! Every redirect hop runs through here too, since each hop is a fresh
//! connection through the same agent.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use anyhow::{Result, bail};
use url::Url;

/// A CIDR prefix an operator has deliberately re-permitted, e.g. to let the
/// server read an intranet wiki. Parsed from `10.0.0.0/8` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefix {
    addr: IpAddr,
    bits: u8,
}

impl Prefix {
    pub fn parse(s: &str) -> Result<Self> {
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("`{s}` is not an IP address or CIDR block"))?;
        let full = if addr.is_ipv4() { 32 } else { 128 };
        let bits = match bits {
            Some(b) => b
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|b| *b <= full)
                .ok_or_else(|| anyhow::anyhow!("`{s}` has an out-of-range prefix length"))?,
            None => full,
        };
        Ok(Self { addr, bits })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        fn matches(a: &[u8], b: &[u8], bits: u8) -> bool {
            let (whole, rest) = (bits as usize / 8, bits as usize % 8);
            if a[..whole] != b[..whole] {
                return false;
            }
            if rest == 0 {
                return true;
            }
            let mask = 0xffu8 << (8 - rest);
            a[whole] & mask == b[whole] & mask
        }
        match (self.addr, ip) {
            (IpAddr::V4(p), IpAddr::V4(x)) => matches(&p.octets(), &x.octets(), self.bits),
            (IpAddr::V6(p), IpAddr::V6(x)) => matches(&p.octets(), &x.octets(), self.bits),
            // A v4-mapped v6 address is compared against v4 prefixes on its
            // embedded address, so `::ffff:10.0.0.1` honours a `10.0.0.0/8`
            // grant rather than silently missing it.
            (IpAddr::V4(_), IpAddr::V6(x)) => match embedded_v4(x) {
                Some(v4) => self.contains(IpAddr::V4(v4)),
                None => false,
            },
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// The v4 address carried inside a v6 one, for the forms that carry one:
/// v4-mapped (`::ffff:a.b.c.d`), v4-compatible (`::a.b.c.d`), 6to4
/// (`2002:AABB:CCDD::/16`) and Teredo (`2001:0::/32`, where the client address
/// is stored inverted). Each is a route to an address the v6 rules alone would
/// wave through — `::ffff:127.0.0.1` is loopback however it is spelled.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let o = ip.octets();
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    // v4-compatible ::a.b.c.d (deprecated, still routable by some stacks).
    if s[0..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] != 0) {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    // 6to4: the v4 address is the 32 bits after the 2002::/16 prefix.
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    // Teredo: 2001:0000::/32, client v4 in the last 32 bits, bitwise inverted.
    if s[0] == 0x2001 && s[1] == 0 {
        return Some(Ipv4Addr::new(!o[12], !o[13], !o[14], !o[15]));
    }
    None
}

/// Why an address may not be connected to, or `None` when it is fine to fetch.
///
/// `Ipv4Addr::is_global` is still unstable, so the ranges are spelled out. The
/// list is deliberately broader than "private": link-local carries the cloud
/// metadata endpoint (169.254.169.254), and shared/benchmark/reserved space is
/// nothing a document should live in.
pub fn classify(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            // Check any embedded v4 first: the v6 rules below would pass
            // `::ffff:127.0.0.1`, which is loopback.
            if let Some(v4) = embedded_v4(v6)
                && let Some(why) = classify_v4(v4)
            {
                return Some(why);
            }
            classify_v6(v6)
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, ..] = ip.octets();
    Some(match () {
        _ if ip.is_unspecified() || a == 0 => "an unspecified address",
        _ if ip.is_loopback() => "loopback",
        _ if ip.is_private() => "a private address",
        // The one that matters most in a cloud: 169.254.169.254 is the
        // instance metadata service, and it answers credentials.
        _ if ip.is_link_local() => "link-local (cloud metadata lives here)",
        _ if ip.is_broadcast() => "broadcast",
        _ if ip.is_multicast() => "multicast",
        _ if ip.is_documentation() => "documentation space",
        _ if a == 100 && (64..128).contains(&b) => "shared address space (CGNAT)",
        _ if a == 198 && (18..20).contains(&b) => "benchmarking space",
        _ if a == 192 && b == 0 && ip.octets()[2] == 0 => "IETF protocol assignments",
        _ if a >= 240 => "reserved space",
        _ => return None,
    })
}

fn classify_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let s = ip.segments();
    Some(match () {
        _ if ip.is_unspecified() => "an unspecified address",
        _ if ip.is_loopback() => "loopback",
        _ if ip.is_multicast() => "multicast",
        // Unique-local fc00::/7 and link-local fe80::/10.
        _ if s[0] & 0xfe00 == 0xfc00 => "a unique-local address",
        _ if s[0] & 0xffc0 == 0xfe80 => "link-local",
        _ if s[0] == 0x2001 && s[1] == 0x0db8 => "documentation space",
        _ => return None,
    })
}

/// Refuse anything that is not a plain web URL before a single packet moves.
pub fn check_url(u: &Url) -> Result<()> {
    match u.scheme() {
        "http" | "https" => {}
        s => bail!("refusing to fetch a `{s}:` URL — only http and https are fetched"),
    }
    if u.host_str().is_none_or(str::is_empty) {
        bail!("`{u}` has no host");
    }
    Ok(())
}

/// Keep the addresses that may be connected to, and say why if none may be.
///
/// When a name resolves to a mix of public and refused addresses the public
/// ones are kept and the rest discarded, so a record that lists `127.0.0.1`
/// beside a real address cannot steer the connection inward.
fn filter(addrs: Vec<SocketAddr>, allow: &[Prefix]) -> Result<Vec<SocketAddr>, String> {
    let mut ok = Vec::new();
    let mut refused: Option<String> = None;
    for a in addrs {
        match classify(a.ip()) {
            None => ok.push(a),
            Some(_) if allow.iter().any(|p| p.contains(a.ip())) => ok.push(a),
            Some(why) => {
                let _ = refused.get_or_insert_with(|| format!("{} is {why}", a.ip()));
            }
        }
    }
    if ok.is_empty() {
        return Err(refused.unwrap_or_else(|| "the name resolved to nothing".into()));
    }
    Ok(ok)
}

/// Check an address before the request, purely so the *reason* survives.
///
/// The enforcement is [`PublicOnly`] — this adds nothing to it, and the fetch
/// would be refused with or without this call. But ureq reports a resolver
/// failure as `resolve dns name '…'` and discards the message, so an operator
/// wondering why their intranet wiki will not load would be told the name did
/// not resolve when in truth it resolved fine and was refused. Belt and
/// braces, where the braces are the ones that can talk.
pub fn precheck(url: &Url, allow: &[Prefix]) -> Result<()> {
    check_url(url)?;
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("cannot resolve {host}: {e}"))?
        .collect();
    match filter(addrs, allow) {
        Ok(_) => Ok(()),
        Err(why) => bail!("refusing to connect to {host}: {why}"),
    }
}

/// A [`ureq::Resolver`] that resolves normally and then drops every address
/// that is not publicly routable. This is where the policy is *enforced*, on
/// the original request and on every redirect hop alike.
pub struct PublicOnly {
    /// Prefixes an operator has explicitly re-permitted.
    pub allow: Vec<Prefix>,
}

impl ureq::Resolver for PublicOnly {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        filter(addrs, &self.allow)
            .map_err(|why| io::Error::other(format!("refusing to connect to {netloc}: {why}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_addresses_worth_protecting_are_all_refused() {
        for (addr, hint) in [
            ("127.0.0.1", "loopback"),
            ("10.1.2.3", "private"),
            ("172.16.0.1", "private"),
            ("192.168.1.1", "private"),
            ("169.254.169.254", "link-local"),
            ("100.100.0.1", "CGNAT"),
            ("0.0.0.0", "unspecified"),
            ("255.255.255.255", "broadcast"),
            ("::1", "loopback"),
            ("fe80::1", "link-local"),
            ("fd00::1", "unique-local"),
        ] {
            let why = classify(ip(addr));
            assert!(why.is_some(), "{addr} ({hint}) should be refused");
        }
    }

    #[test]
    fn a_v6_spelling_of_a_v4_address_is_still_that_address() {
        // The whole point of `embedded_v4`: these all reach 127.0.0.1 or
        // 169.254.169.254 while looking like ordinary v6 to the v6 rules.
        for addr in [
            "::ffff:127.0.0.1", // v4-mapped
            "::ffff:169.254.169.254",
            "::127.0.0.1",              // v4-compatible
            "2002:7f00:1::",            // 6to4 wrapping 127.0.0.1
            "2001:0:0:0:0:0:a9fe:a9fe", // Teredo — inverted, so not metadata
        ] {
            let ip = ip(addr);
            if addr.starts_with("2001:0:") {
                // Teredo inverts the client address: !169.254.169.254 is
                // 86.1.86.1, which is public — and refusing it would be wrong.
                assert_eq!(classify(ip), None, "{addr} decodes to a public address");
            } else {
                assert!(classify(ip).is_some(), "{addr} must be refused");
            }
        }
    }

    #[test]
    fn ordinary_public_addresses_pass() {
        for addr in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert_eq!(classify(ip(addr)), None, "{addr} should be fetchable");
        }
    }

    #[test]
    fn an_operator_grant_re_permits_exactly_its_block() {
        let allow = Prefix::parse("10.0.0.0/8").unwrap();
        assert!(allow.contains(ip("10.255.0.1")));
        assert!(!allow.contains(ip("11.0.0.1")));
        assert!(!allow.contains(ip("192.168.0.1")));
        // A grant is not a blanket: everything else stays refused.
        assert!(classify(ip("192.168.0.1")).is_some());
        // And it follows the address through its v6 spelling.
        assert!(allow.contains(ip("::ffff:10.0.0.1")));
    }

    #[test]
    fn a_bare_address_grant_is_a_single_host() {
        let one = Prefix::parse("192.168.1.7").unwrap();
        assert!(one.contains(ip("192.168.1.7")));
        assert!(!one.contains(ip("192.168.1.8")));
    }

    #[test]
    fn only_web_schemes_are_fetched() {
        for good in ["http://example.com", "https://example.com/a?b=c"] {
            assert!(check_url(&Url::parse(good).unwrap()).is_ok());
        }
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com",
            "data:text/html,hi",
        ] {
            let u = Url::parse(bad).unwrap();
            assert!(check_url(&u).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn a_mixed_record_keeps_only_the_public_addresses() {
        // Through `filter` itself. An earlier version of this test re-created
        // the filtering inline and asserted on the copy, which proved the test
        // right and said nothing about the code — coverage caught it.
        let kept = filter(
            vec![
                SocketAddr::new(ip("127.0.0.1"), 80),
                SocketAddr::new(ip("1.1.1.1"), 80),
            ],
            &[],
        )
        .expect("a public address survives");
        assert_eq!(kept.len(), 1, "the loopback answer must be dropped");
        assert_eq!(kept[0].ip(), ip("1.1.1.1"));
    }

    #[test]
    fn a_record_with_nothing_public_is_refused_and_says_why() {
        let err = filter(
            vec![
                SocketAddr::new(ip("10.0.0.1"), 80),
                SocketAddr::new(ip("169.254.169.254"), 80),
            ],
            &[],
        )
        .expect_err("nothing here may be connected to");
        // The first refusal is the one reported, and it names the address.
        assert!(err.contains("10.0.0.1"), "{err}");
        assert!(err.contains("private"), "{err}");

        let empty = filter(vec![], &[]).expect_err("a name that resolved to nothing");
        assert!(empty.contains("resolved to nothing"), "{empty}");
    }

    #[test]
    fn an_operator_grant_survives_the_filter() {
        let allow = vec![Prefix::parse("10.0.0.0/8").unwrap()];
        let kept = filter(vec![SocketAddr::new(ip("10.1.2.3"), 80)], &allow)
            .expect("the granted block is reachable");
        assert_eq!(kept.len(), 1);
        // …and the grant is not a blanket over the rest of private space.
        assert!(filter(vec![SocketAddr::new(ip("192.168.1.1"), 80)], &allow).is_err());
    }

    #[test]
    fn precheck_refuses_before_a_packet_moves() {
        // Literal addresses, so this resolves locally and needs no network.
        let url = |s: &str| Url::parse(s).unwrap();

        let err = guard_err(&url("http://127.0.0.1:8080/x"), &[]);
        assert!(err.contains("refusing to connect"), "{err}");
        assert!(err.contains("loopback"), "{err}");

        let meta = guard_err(&url("http://169.254.169.254/latest/meta-data/"), &[]);
        assert!(meta.contains("link-local"), "the metadata endpoint: {meta}");

        // A public address passes, and a granted private one passes too.
        assert!(precheck(&url("http://1.1.1.1/"), &[]).is_ok());
        let allow = vec![Prefix::parse("10.0.0.0/8").unwrap()];
        assert!(precheck(&url("http://10.0.0.5/wiki"), &allow).is_ok());
        // The scheme check runs first, so a `file:` URL never reaches resolution.
        assert!(precheck(&url("file:///etc/passwd"), &[]).is_err());
    }

    fn guard_err(u: &Url, allow: &[Prefix]) -> String {
        precheck(u, allow).expect_err("must be refused").to_string()
    }

    #[test]
    fn a_prefix_matches_v6_and_non_byte_aligned_blocks() {
        // /12 is not byte-aligned, so it exercises the mask path.
        let twelve = Prefix::parse("172.16.0.0/12").unwrap();
        assert!(twelve.contains(ip("172.31.255.254")));
        assert!(!twelve.contains(ip("172.32.0.1")));

        // A v6 prefix against v6 addresses.
        let v6 = Prefix::parse("fd00::/8").unwrap();
        assert!(v6.contains(ip("fd00::1")));
        assert!(!v6.contains(ip("2606:4700::1111")));
        // A v6 prefix never matches a bare v4 address.
        assert!(!v6.contains(ip("10.0.0.1")));
    }
}
