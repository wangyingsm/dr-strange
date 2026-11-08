//! What a crawl reads, what it throws away, and whether it says why.
//!
//! `fetch/mod.rs` is the orchestration around the parts that already have
//! their own tests — the address guard, the HTML reader, `robots.txt`, the
//! relevance scorer. What is left is the sequencing: two gates, three budgets,
//! and a dropped-page list that has to be explainable rather than merely
//! short. All of it is reachable only with a server answering, so these run
//! against a loopback stub and the real `ureq` agent rather than a fake.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use dr_strange_web::fetch::{FetchOptions, Fetched, Prefix, Progress, fetch_with_progress};

/// One stubbed response: the path it answers, its status line, its
/// `Content-Type`, and its body.
struct Stub {
    path: &'static str,
    status: &'static str,
    content_type: &'static str,
    body: String,
}

fn page(path: &'static str, body: impl Into<String>) -> Stub {
    Stub {
        path,
        status: "200 OK",
        content_type: "text/html",
        body: body.into(),
    }
}

/// Serves each stub once per request, 404s anything else, and keeps answering
/// until the test ends. Returns the root URL.
///
/// The thread is left running: a crawl stops asking when its budget says so,
/// not when the server runs out of pages, so there is no count to join on.
fn serve(stubs: Vec<Stub>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            // `GET /path HTTP/1.1`
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let found = stubs.iter().find(|s| s.path == path);
            let mut stream = stream;
            let response = match found {
                Some(s) => format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    s.status,
                    s.content_type,
                    s.body.len(),
                    s.body
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}/")
}

/// The stub listens on loopback, which the address guard refuses by default —
/// so every test here makes the one grant an operator would make for an
/// intranet host, exactly as `redirect.rs` does. `host_delay` goes to zero
/// because the gap between requests is a politeness policy, not behaviour
/// under test, and the default would add half a second per page.
fn opts() -> FetchOptions {
    FetchOptions {
        allow_private: vec![Prefix::parse("127.0.0.0/8").unwrap()],
        host_delay: Duration::ZERO,
        request_timeout: Duration::from_secs(5),
        ..FetchOptions::default()
    }
}

fn crawl(root: &str, opts: &FetchOptions) -> Fetched {
    fetch_with_progress(root, opts, &mut |_| {}).expect("the root loads")
}

fn dropped_reason(out: &Fetched, needle: &str) -> String {
    out.dropped
        .iter()
        .find(|d| d.url.contains(needle))
        .map(|d| d.reason.clone())
        .unwrap_or_else(|| {
            panic!(
                "nothing mentioning `{needle}` was dropped; dropped: {:?}",
                out.dropped
            )
        })
}

/// The shape a digest consumes: the root first, its links after, and every
/// page carrying the marker that tells the chunker where it came from.
#[test]
fn the_root_leads_and_every_page_carries_its_provenance() {
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants with fronds.</p>\
             <a href=\"/fronds\">fern fronds in detail</a>\
             </main></body></html>",
        ),
        page(
            "/fronds",
            "<html><head><title>Fronds</title></head><body><main>\
             <p>A frond is the leaf of a fern, vascular and divided.</p>\
             </main></body></html>",
        ),
    ]);

    let out = crawl(&root, &opts());

    assert_eq!(out.pages.len(), 2, "root and its one link: {:?}", out.pages);
    assert!(out.pages[0].url.ends_with('/'), "the root leads");
    assert_eq!(out.pages[0].depth, 0);
    assert_eq!(out.pages[0].score, 1.0, "the root is what was asked for");
    assert_eq!(out.pages[1].depth, 1);
    assert_eq!(out.pages[1].title, "Fronds");

    // Each block names its own source, and the document is the kept blocks.
    let doc = out.document();
    for p in &out.pages {
        assert!(p.block.contains(&p.url), "{} has no marker", p.url);
        assert!(doc.contains(&p.block));
        assert_eq!(p.chars, p.block.chars().count());
    }
}

/// A page the reader cannot use is dropped with the reason, and the crawl
/// carries on — a thin result has to be explainable.
#[test]
fn what_cannot_be_read_is_dropped_with_the_reason_and_the_crawl_goes_on() {
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants with fronds.</p>\
             <a href=\"/fronds\">fern fronds in detail</a>\
             <a href=\"/notes.txt\">fern notes as plain text</a>\
             <a href=\"/sporophyte.bin\">fern sporophyte data</a>\
             <a href=\"/missing\">fern pages that moved</a>\
             </main></body></html>",
        ),
        page(
            "/fronds",
            "<html><body><main><p>A frond is the leaf of a fern.</p></main></body></html>",
        ),
        Stub {
            path: "/notes.txt",
            status: "200 OK",
            content_type: "text/plain; charset=utf-8",
            body: "Ferns and their fronds, in plain text.".into(),
        },
        Stub {
            path: "/sporophyte.bin",
            status: "200 OK",
            content_type: "application/octet-stream",
            body: "\u{0}\u{1}not a document".into(),
        },
    ]);

    let mut o = opts();
    o.max_pages = 20;
    let out = crawl(&root, &o);

    // The unreadable content type says what it was, not merely that it failed.
    let why = dropped_reason(&out, "sporophyte.bin");
    assert!(
        why.contains("application/octet-stream") && why.contains("not a document"),
        "{why}"
    );
    // A missing page reports the status, terse: no wall of transport detail.
    assert_eq!(dropped_reason(&out, "/missing"), "HTTP 404");

    // And the readable ones are still there, the text one included — it
    // contributes prose and no links.
    let urls: Vec<&str> = out.pages.iter().map(|p| p.url.as_str()).collect();
    assert!(
        urls.iter().any(|u| u.ends_with("/fronds")),
        "kept: {urls:?}"
    );
    let text = out
        .pages
        .iter()
        .find(|p| p.url.ends_with("/notes.txt"))
        .expect("the plain-text page is a document like any other");
    assert!(text.block.contains("plain text"));
    assert!(text.title.is_empty(), "plain text has no <title> to read");
}

/// `max_pages` is a budget on what is kept, and spending it is reported once
/// rather than once per link — a page offering a thousand links would
/// otherwise bury the pages that were read.
#[test]
fn the_page_budget_is_reported_once_for_all_the_links_it_cuts() {
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants.</p>\
             <a href=\"/a\">fern a</a><a href=\"/b\">fern b</a>\
             <a href=\"/c\">fern c</a><a href=\"/d\">fern d</a>\
             </main></body></html>",
        ),
        page("/a", "<html><body><p>fern a</p></body></html>"),
        page("/b", "<html><body><p>fern b</p></body></html>"),
        page("/c", "<html><body><p>fern c</p></body></html>"),
        page("/d", "<html><body><p>fern d</p></body></html>"),
    ]);

    let mut o = opts();
    o.max_pages = 3;
    let out = crawl(&root, &o);

    assert!(
        out.pages.len() <= 3,
        "kept past the budget: {:?}",
        out.pages
    );
    let note = dropped_reason(&out, "lower-ranked link");
    assert!(note.contains("3-page budget"), "{note}");
    assert_eq!(
        out.dropped
            .iter()
            .filter(|d| d.url.contains("lower-ranked link"))
            .count(),
        1,
        "one line for the whole cut, not one per link"
    );
}

/// The byte budget is the crawl's, not the page's. A body that will not fit in
/// what is left is dropped naming the remainder, so the reason distinguishes
/// *this page is too big* from *this crawl is finished*.
#[test]
fn a_page_too_big_for_the_remaining_budget_says_what_was_left() {
    let big = "<html><body><main><p>".to_string() + &"fern fronds ".repeat(400) + "</p></main>";
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants with fronds.</p>\
             <a href=\"/one\">fern one</a><a href=\"/two\">fern two</a>\
             </main></body></html>",
        ),
        page("/one", big.clone()),
        page("/two", big),
    ]);

    let mut o = opts();
    // Enough for the root, not for either of the pages it links.
    o.max_total_bytes = 5_000;
    o.max_pages = 10;
    let out = crawl(&root, &o);

    let why = dropped_reason(&out, "/one");
    assert!(
        why.contains("larger than") && why.contains("KiB"),
        "the reason should name what was left: {why}"
    );
    assert_eq!(out.pages.len(), 1, "only the root fit: {:?}", out.pages);
}

/// And when the budget is spent rather than merely short, the next page is
/// refused before a request — a different sentence, because it is a different
/// situation for the reader to act on.
#[test]
fn a_spent_budget_refuses_the_next_page_outright() {
    let root_body = "<html><head><title>Ferns</title></head><body><main>\
                     <p>Ferns are vascular plants with fronds.</p>\
                     <a href=\"/one\">fern one</a></main></body></html>";
    let root = serve(vec![
        page("/", root_body),
        page("/one", "<html><body><p>fern one</p></body></html>"),
    ]);

    let mut o = opts();
    // Exactly the root: it loads, and leaves nothing for the link.
    o.max_total_bytes = root_body.len();
    let out = crawl(&root, &o);

    assert_eq!(out.pages.len(), 1, "the root itself still loads");
    let why = dropped_reason(&out, "/one");
    assert!(why.contains("budget is spent"), "{why}");
}

/// `robots.txt` is obeyed per path, and a host that publishes nothing has
/// objected to nothing.
#[test]
fn robots_txt_is_obeyed_for_the_paths_it_names() {
    let root = serve(vec![
        Stub {
            path: "/robots.txt",
            status: "200 OK",
            content_type: "text/plain",
            body: "User-agent: *\nDisallow: /private\n".into(),
        },
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants with fronds.</p>\
             <a href=\"/private\">fern private notes</a>\
             <a href=\"/public\">fern public notes</a>\
             </main></body></html>",
        ),
        page("/private", "<html><body><p>fern private</p></body></html>"),
        page("/public", "<html><body><p>fern public</p></body></html>"),
    ]);

    let out = crawl(&root, &opts());

    let why = dropped_reason(&out, "/private");
    assert!(
        why.contains("robots.txt") && why.contains("disallows"),
        "{why}"
    );
    assert!(
        out.pages.iter().any(|p| p.url.ends_with("/public")),
        "the allowed path is still read: {:?}",
        out.pages
    );
}

/// Depth 0 is the root alone: the frontier is never walked, however many
/// links the page offers.
#[test]
fn depth_zero_reads_only_what_was_asked_for() {
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns.</p><a href=\"/fronds\">fern fronds</a></main></body></html>",
        ),
        page("/fronds", "<html><body><p>fronds</p></body></html>"),
    ]);

    let mut o = opts();
    o.max_depth = 0;
    let out = crawl(&root, &o);

    assert_eq!(out.pages.len(), 1);
    assert!(
        out.dropped.is_empty(),
        "nothing was tried: {:?}",
        out.dropped
    );
}

/// Progress is reported per request, counting the root, so a caller's bar can
/// move before anything has been scored.
#[test]
fn progress_names_each_url_as_it_is_requested() {
    let root = serve(vec![
        page(
            "/",
            "<html><head><title>Ferns</title></head><body><main>\
             <p>Ferns are vascular plants with fronds.</p>\
             <a href=\"/fronds\">fern fronds in detail</a></main></body></html>",
        ),
        page(
            "/fronds",
            "<html><body><main><p>A frond is a fern leaf.</p></main></body></html>",
        ),
    ]);

    let mut seen: Vec<Progress> = Vec::new();
    let out = fetch_with_progress(&root, &opts(), &mut |p| seen.push(p)).unwrap();

    assert!(
        seen.first().is_some_and(|p| p.done == 0 && p.total == 1),
        "the root is announced before it is fetched: {seen:?}"
    );
    assert!(
        seen.iter().any(|p| p.url.ends_with("/fronds")),
        "each request names its URL: {seen:?}"
    );
    assert_eq!(out.pages.len(), 2);
}

/// A URL the guard refuses never reaches the network, and the refusal is the
/// error rather than a timeout.
#[test]
fn an_address_the_guard_refuses_fails_before_any_request() {
    // Default options: no private block re-permitted, so loopback is refused.
    let err = fetch_with_progress(
        "http://127.0.0.1:9/x",
        &FetchOptions::default(),
        &mut |_| {},
    )
    .expect_err("loopback is not a public address");
    let said = format!("{err:#}");
    assert!(
        said.contains("127.0.0.1") || said.to_lowercase().contains("private"),
        "{said}"
    );
}
