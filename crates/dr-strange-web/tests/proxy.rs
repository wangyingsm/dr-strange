//! Outbound requests go through the configured proxy, and the proxy is the
//! thing that resolves the destination (issue #37).
//!
//! The names fetched here are deliberately unresolvable on this machine. That
//! is the whole point of the bug: the reporter's resolver answered `::` for a
//! host their proxy reached perfectly well, and drsg refused the fetch before a
//! packet moved. If any of these tests resolved the name locally they would
//! fail, which is what makes them evidence.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use dr_strange_llm::net::{Network, NoProxy, ProxyUrl};
use dr_strange_web::fetch::{Route, fetch_bytes, redirect_target};

/// A stub HTTP proxy: records the first request line it is sent, answers 502
/// and hangs up. Enough to prove what the client asked it to do, with no TLS.
fn stub_proxy() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            if tx.send(line).is_err() {
                return;
            }
            let mut stream = stream;
            let _ = stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
        }
    });
    (format!("http://{addr}"), rx)
}

/// Built without consulting the environment: this machine may itself have
/// `ALL_PROXY` set, and `Network::resolve` would rightly let it win — sending
/// these requests somewhere other than the stub.
fn proxied_to(proxy: String) -> Network {
    Network::through(proxy.parse::<ProxyUrl>().unwrap(), NoProxy::default())
}

fn first_line(rx: &mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the proxy was never contacted")
}

/// An `https:` fetch tunnels, and the CONNECT names the *host* — so the proxy
/// resolves it, not us. This is the reporter's `drsg plugin install` path.
#[test]
fn an_https_fetch_connects_through_the_proxy_by_name() {
    let (proxy, rx) = stub_proxy();
    let net = proxied_to(proxy);
    let route = Route {
        net: &net,
        allow: &[],
    };

    let err = fetch_bytes(
        "https://raw.githubusercontent.invalid/o/r/main/catalog.json",
        1 << 20,
        route,
    )
    .expect_err("the stub answers 502, so the fetch cannot succeed");

    let line = first_line(&rx);
    assert!(
        line.starts_with("CONNECT raw.githubusercontent.invalid:443"),
        "the destination must reach the proxy as a name: {line}"
    );
    // And it failed at the proxy, not in a local address check.
    let err = format!("{err:#}");
    assert!(
        !err.contains("refusing to connect"),
        "the address guard must not have run: {err}"
    );
}

/// A plain `http:` fetch is sent in absolute form instead of tunnelled, and
/// again carries the name.
#[test]
fn an_http_fetch_is_sent_to_the_proxy_in_absolute_form() {
    let (proxy, rx) = stub_proxy();
    let net = proxied_to(proxy);
    let route = Route {
        net: &net,
        allow: &[],
    };

    let _ = fetch_bytes("http://example.invalid/catalog.json", 1 << 20, route);

    let line = first_line(&rx);
    assert!(
        line.starts_with("GET http://example.invalid/catalog.json"),
        "{line}"
    );
}

/// `drsg update`'s version check goes the same way. It failed first in the
/// report, which is why the update never got as far as the installer.
#[test]
fn the_update_version_check_goes_through_the_proxy() {
    let (proxy, rx) = stub_proxy();
    let net = proxied_to(proxy);
    let route = Route {
        net: &net,
        allow: &[],
    };

    let _ = redirect_target("https://github.invalid/o/r/releases/latest", route);

    let line = first_line(&rx);
    assert!(line.starts_with("CONNECT github.invalid:443"), "{line}");
}

/// A bypassed host never reaches the proxy: it is fetched directly, and the
/// address guard still judges it. A local LLM endpoint depends on this.
#[test]
fn a_bypassed_host_does_not_reach_the_proxy() {
    let (proxy, rx) = stub_proxy();
    let net = Network::through(
        proxy.parse::<ProxyUrl>().unwrap(),
        NoProxy::parse("127.0.0.1, localhost"),
    );
    let route = Route {
        net: &net,
        allow: &[],
    };

    let err = fetch_bytes("http://127.0.0.1:9/x", 1 << 20, route)
        .expect_err("loopback is refused by the guard when nothing proxies it");
    let err = format!("{err:#}");
    assert!(err.contains("refusing to connect"), "{err}");
    assert!(err.contains("loopback"), "{err}");

    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "the proxy must not have been contacted"
    );
}

/// A transport failure through a proxy names the proxy. Without that, "the
/// request failed" sends the reader to debug the destination when the proxy is
/// what is broken — the complaint issue #37 makes about the old messages.
#[test]
fn a_failure_through_a_proxy_says_so_without_leaking_the_password() {
    // Port 9 (discard) refuses, so this is a proxy that cannot be reached.
    let net = Network::through(
        "http://alice:hunter2@127.0.0.1:9"
            .parse::<ProxyUrl>()
            .unwrap(),
        NoProxy::default(),
    );
    let route = Route {
        net: &net,
        allow: &[],
    };

    let err = fetch_bytes("https://example.invalid/x", 1 << 20, route).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("via the proxy"), "{err}");
    assert!(err.contains("alice:***@127.0.0.1:9"), "{err}");
    assert!(
        !err.contains("hunter2"),
        "the password must not appear: {err}"
    );
}

/// A direct failure says nothing about a proxy, because there is not one.
#[test]
fn a_direct_failure_does_not_mention_a_proxy() {
    let err = fetch_bytes("https://example.invalid/x", 1 << 20, Route::guarded(&[])).unwrap_err();
    let err = format!("{err:#}");
    assert!(!err.contains("proxy"), "{err}");
}

/// With no proxy configured nothing changes: the address guard is in force and
/// refuses what it always refused.
#[test]
fn without_a_proxy_the_guard_is_unchanged() {
    let err = fetch_bytes(
        "http://169.254.169.254/latest/meta-data/",
        1 << 20,
        Route::guarded(&[]),
    )
    .expect_err("the metadata endpoint is refused");
    let err = format!("{err:#}");
    assert!(err.contains("refusing to connect"), "{err}");
    assert!(err.contains("link-local"), "{err}");
}
