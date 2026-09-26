//! `redirect_target` asks a URL where it points and does not follow it.
//!
//! A characterization test, written before the ureq 3 migration so the
//! migration has something to preserve. What `drsg update` needs from this is
//! narrow and easy to lose: the `Location` of a 3xx that was *not* followed. A
//! client configured to allow zero redirects can just as easily report "too many
//! redirects" and discard the header, which would leave every version check
//! saying the release page answered with nothing.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use dr_strange_web::fetch::{Prefix, Route, redirect_target};

/// The stub listens on loopback, which the address guard refuses by default —
/// so these tests make the one grant an operator would make for an intranet
/// host. The guard itself is tested in `guard`; this is about the redirect.
fn to_loopback() -> Vec<Prefix> {
    vec![Prefix::parse("127.0.0.0/8").unwrap()]
}

/// Answers one request with `status` and the given headers, then closes.
fn stub(status: &'static str, headers: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            // Read the request line so the client is not writing into a closed
            // socket when we answer.
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let mut stream = stream;
            let _ = stream.write_all(format!("HTTP/1.1 {status}\r\n{headers}\r\n").as_bytes());
        }
    });
    format!("http://{addr}/releases/latest")
}

/// The shape `drsg update` depends on: a 302 whose `Location` names the tag.
#[test]
fn a_redirect_is_reported_and_not_followed() {
    let url = stub(
        "302 Found",
        "Location: https://github.com/o/r/releases/tag/v9.9.9\r\nContent-Length: 0\r\n",
    );
    let allow = to_loopback();
    let got = redirect_target(&url, Route::guarded(&allow)).expect("the Location is the answer");
    assert_eq!(got, "https://github.com/o/r/releases/tag/v9.9.9");
}

/// GitHub answers `releases/latest` with a 301 in some configurations, so both
/// permanent and temporary forms have to work.
#[test]
fn a_permanent_redirect_is_reported_too() {
    let url = stub(
        "301 Moved Permanently",
        "Location: https://example.invalid/tag/v1.2.3\r\nContent-Length: 0\r\n",
    );
    let allow = to_loopback();
    let got = redirect_target(&url, Route::guarded(&allow)).unwrap();
    assert_eq!(got, "https://example.invalid/tag/v1.2.3");
}

/// A 2xx with no `Location` is not a redirect, and says so rather than
/// inventing one.
#[test]
fn a_response_without_a_location_is_an_error_naming_the_status() {
    let url = stub("200 OK", "Content-Length: 0\r\n");
    let allow = to_loopback();
    let err = format!(
        "{:#}",
        redirect_target(&url, Route::guarded(&allow)).unwrap_err()
    );
    assert!(err.contains("no Location"), "{err}");
    assert!(err.contains("200"), "it names the status: {err}");
}
