//! URL ingestion for the digest page (ROADMAP §9).
//!
//! Fetch a page, convert it to Markdown, follow its links as far as a budget
//! allows, keep what is relevant to the target, and assemble one document the
//! existing digest pipeline consumes unchanged.
//!
//! Relevance is decided twice and hops are only a tiebreak — see
//! [`relevance`] for why, and [`guard`] for why the server is careful about
//! which addresses it will connect to at all.
//!
//! The crawl is deliberately small: a handful of pages, one hop by default. The
//! budget is the real control, and everything a budget drops is reported rather
//! than silently truncated.

pub mod guard;
mod html;
mod relevance;
mod robots;

use ahash::{AHashMap, AHashSet};
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dr_strange_core::{Analyzer, Language};
use dr_strange_llm::SOURCE_MARKER;
use url::Url;

pub use guard::Prefix;
use relevance::Target;
use robots::Robots;

/// Identifies the crawler to the sites it reads. A server that will not say who
/// it is has no business asking for anyone's bandwidth.
pub const USER_AGENT: &str = concat!(
    "drsg-fetch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/wangyingsm/dr-strange)"
);

/// How to crawl. Every field is a budget or a policy; none of them changes what
/// relevance *means*.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Sharpens the target beyond what the root page says about itself.
    pub topic: Option<String>,
    /// Ceiling on pages kept, the root included.
    pub max_pages: usize,
    /// 0 fetches only the root; 1 (the default) also its links.
    pub max_depth: usize,
    /// Ceiling on one response body.
    pub max_page_bytes: usize,
    /// Ceiling on everything downloaded in one crawl.
    pub max_total_bytes: usize,
    /// A page must reach this fraction of the best page's score to be kept.
    /// A *relative* floor because BM25 scores are not comparable across
    /// corpora — an absolute threshold would mean something different on every
    /// document.
    pub min_ratio: f32,
    /// Requests in flight at once, across all hosts.
    pub concurrency: usize,
    /// Ceiling on one request, connect through read.
    pub request_timeout: Duration,
    /// Minimum gap between two requests to the same host, raised by that
    /// host's `Crawl-delay` when it publishes one.
    pub host_delay: Duration,
    /// Analyzer language for the relevance scoring.
    pub language: Language,
    /// Address blocks an operator has deliberately re-permitted.
    pub allow_private: Vec<Prefix>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            topic: None,
            max_pages: 10,
            // One hop. Two is 50 links x 50 links, and the interesting material
            // a page points at is almost always one step away.
            max_depth: 1,
            max_page_bytes: 4 << 20,
            max_total_bytes: 24 << 20,
            min_ratio: 0.25,
            concurrency: 4,
            request_timeout: Duration::from_secs(20),
            host_delay: Duration::from_millis(500),
            language: Language::English,
            allow_private: Vec::new(),
        }
    }
}

/// A page that was fetched and read.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    /// The page's Markdown, already carrying its provenance marker — the unit
    /// the caller selects and joins. One rendering, in one place.
    pub block: String,
    /// Relevance to the target, 0..1. The root is always 1.
    pub score: f32,
    pub depth: usize,
    pub chars: usize,
    /// Whether it cleared the relevance floor. The dashboard pre-ticks these;
    /// the reader has the last word.
    pub kept: bool,
}

/// Something that was not fetched, or was fetched and thrown away, and why.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Dropped {
    pub url: String,
    pub reason: String,
}

/// The result of one crawl.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Fetched {
    pub pages: Vec<Page>,
    pub dropped: Vec<Dropped>,
}

impl Fetched {
    /// Assemble the kept pages into the document the digest reads.
    pub fn document(&self) -> String {
        self.pages
            .iter()
            .filter(|p| p.kept)
            .map(|p| p.block.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Where a crawl currently is, for a progress bar.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub url: String,
}

/// Fetch `root` and, under `opts`, the pages it points at.
///
/// `progress` is called as each page completes; it may be invoked from several
/// threads, so the caller's closure is serialized behind a lock.
pub fn fetch_with_progress(
    root: &str,
    opts: &FetchOptions,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Fetched> {
    let root_url = parse_url(root)?;
    guard::check_url(&root_url)?;

    let agent = ureq::AgentBuilder::new()
        .user_agent(USER_AGENT)
        // Every hop resolves through the guard, so a redirect cannot walk the
        // request inward after the first address was approved.
        .resolver(guard::PublicOnly {
            allow: opts.allow_private.clone(),
        })
        .redirects(5)
        .timeout(opts.request_timeout)
        .build();

    let analyzer = Analyzer::new(opts.language);
    let ctx = Crawl {
        agent,
        opts,
        robots: Mutex::new(AHashMap::new()),
        last_hit: Mutex::new(AHashMap::new()),
        spent: AtomicUsize::new(0),
    };

    let mut out = Fetched::default();
    let mut seen: AHashSet<String> = AHashSet::new();
    seen.insert(normalize(&root_url));

    // The root is not a candidate — it is what the reader asked for, so it is
    // fetched and kept regardless of what it turns out to say.
    progress(Progress {
        done: 0,
        total: 1,
        url: root_url.to_string(),
    });
    let root_doc = ctx
        .load(&root_url)
        .with_context(|| format!("fetching {root_url}"))?;
    let target = Target::new(&analyzer, &root_doc.text, opts.topic.as_deref());
    out.pages.push(Page {
        url: root_url.to_string(),
        title: root_doc.title.clone(),
        chars: 0,
        block: String::new(),
        score: 1.0,
        depth: 0,
        kept: true,
    });
    render(&mut out.pages[0], &root_doc.text);

    let mut frontier = candidates(&root_url, &root_doc.links, 1, &mut seen);
    let mut depth = 1;

    while depth <= opts.max_depth && !frontier.is_empty() && out.pages.len() < opts.max_pages {
        // Gate one: what is worth a request, judged on anchor text, title and
        // the words in the URL. No network, no model.
        for c in &mut frontier {
            c.pre = pre_score(&analyzer, &target, c, &root_url) * decay(depth);
        }
        frontier.sort_by(|a, b| {
            b.pre
                .partial_cmp(&a.pre)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.url.as_str().cmp(b.url.as_str()))
        });
        let room = opts.max_pages - out.pages.len();
        let (take, skip) = frontier.split_at(room.min(frontier.len()));
        // One line, not one per link. A budget that cuts a thousand candidates
        // still has to say so — a Wikipedia article offers over a thousand —
        // but listing each of them buries the pages that *were* read under a
        // page of noise, which is its own kind of silence.
        if !skip.is_empty() {
            out.dropped.push(Dropped {
                url: format!("{} lower-ranked link(s)", skip.len()),
                reason: format!("beyond the {}-page budget", opts.max_pages),
            });
        }

        let base = out.pages.len();
        let loaded = ctx.load_all(take, base, progress);

        // Gate two: the page itself, re-judged now that it can be read.
        let avg_len = average_len(&analyzer, &loaded);
        let mut fresh = Vec::new();
        for (cand, result) in take.iter().zip(loaded) {
            match result {
                Err(why) => out.dropped.push(Dropped {
                    url: cand.url.to_string(),
                    reason: why,
                }),
                Ok(doc) => {
                    let score = if target.is_empty() {
                        1.0
                    } else {
                        target.score(&analyzer, &doc.text, avg_len) * decay(cand.depth)
                    };
                    let mut page = Page {
                        url: cand.url.to_string(),
                        title: doc.title.clone(),
                        block: String::new(),
                        chars: 0,
                        score,
                        depth: cand.depth,
                        kept: true,
                    };
                    render(&mut page, &doc.text);
                    fresh.push((page, doc));
                }
            }
        }

        // The floor is relative to the best page found at this depth, so it
        // adapts to a corpus rather than asserting an absolute meaning for a
        // BM25 score.
        let best = fresh.iter().map(|(p, _)| p.score).fold(0.0f32, f32::max);
        let floor = best * opts.min_ratio;
        let mut next = Vec::new();
        for (mut page, doc) in fresh {
            if page.score < floor {
                page.kept = false;
                out.dropped.push(Dropped {
                    url: page.url.clone(),
                    reason: format!(
                        "relevance {:.2} is below the floor {floor:.2} for this crawl",
                        page.score
                    ),
                });
                out.pages.push(page);
                continue;
            }
            if depth < opts.max_depth {
                let url = Url::parse(&page.url).expect("a fetched URL parses");
                next.extend(candidates(&url, &doc.links, depth + 1, &mut seen));
            }
            out.pages.push(page);
        }
        frontier = next;
        depth += 1;
    }

    // Best page first, but the root always leads: it is the document the reader
    // actually named.
    out.pages[1..].sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.url.cmp(&b.url))
    });
    Ok(out)
}

/// Hop decay, the same idiom `plane.hybrid`'s graph channel uses. It breaks
/// ties toward the root; it does not decide relevance, because depth measures
/// how far the crawl walked rather than whether a page is about anything.
fn decay(depth: usize) -> f32 {
    0.5f32.powi(depth.saturating_sub(1) as i32)
}

/// Render a page's Markdown with its provenance marker. The marker is its own
/// paragraph, which is what makes the chunker break there — so no chunk ever
/// straddles two pages.
fn render(page: &mut Page, text: &str) {
    let mut block = format!("{SOURCE_MARKER} {} -->\n\n", page.url);
    // The `<title>` and the page's own `<h1>` are usually the same sentence.
    // Writing both bills the model twice for it.
    let repeats_title = text
        .lines()
        .next()
        .is_some_and(|l| l.trim_start_matches('#').trim() == page.title);
    if !page.title.is_empty() && !repeats_title {
        block.push_str(&format!("# {}\n\n", page.title));
    }
    block.push_str(text);
    page.chars = block.chars().count();
    page.block = block;
}

fn average_len(analyzer: &Analyzer, loaded: &[Result<Loaded, String>]) -> f32 {
    let lens: Vec<f32> = loaded
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|d| analyzer.analyze(&d.text).len() as f32)
        .collect();
    if lens.is_empty() {
        1.0
    } else {
        (lens.iter().sum::<f32>() / lens.len() as f32).max(1.0)
    }
}

/// A link considered for fetching.
#[derive(Debug, Clone)]
struct Candidate {
    url: Url,
    anchor: String,
    in_main: bool,
    same_origin: bool,
    depth: usize,
    pre: f32,
}

/// Resolve, filter and dedupe the links a page points at.
fn candidates(
    base: &Url,
    links: &[html::Link],
    depth: usize,
    seen: &mut AHashSet<String>,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for link in links {
        let Ok(url) = base.join(&link.href) else {
            continue;
        };
        if guard::check_url(&url).is_err() {
            continue;
        }
        let key = normalize(&url);
        if !seen.insert(key) {
            // Already queued or fetched. Merge the anchor text, since a second
            // link to the same page describes it too.
            if let Some(existing) = out
                .iter_mut()
                .find(|c| normalize(&c.url) == normalize(&url))
                && !existing.anchor.contains(&link.anchor)
            {
                existing.anchor.push(' ');
                existing.anchor.push_str(&link.anchor);
            }
            continue;
        }
        let same_origin = url.host_str() == base.host_str();
        out.push(Candidate {
            url,
            anchor: link.anchor.clone(),
            in_main: link.in_main,
            same_origin,
            depth,
            pre: 0.0,
        });
    }
    out
}

/// Gate one. Anchor text and the URL's own words are all that exist before a
/// request; two small bonuses encode what a link's *placement* says about it.
fn pre_score(analyzer: &Analyzer, target: &Target, c: &Candidate, _root: &Url) -> f32 {
    if target.is_empty() {
        return 1.0;
    }
    let text = format!("{} {}", c.anchor, relevance::url_words(&c.url));
    let mut s = target.coverage(analyzer, &text);
    if c.in_main {
        // A link in the prose is a citation; one in a sidebar might be
        // furniture. A nudge, not a rule — a documentation table of contents
        // lives in a `<nav>` and is exactly what a reader wants.
        s *= 1.25;
    }
    if c.same_origin {
        s *= 1.15;
    }
    s
}

/// Collapse the spellings of one address: case-insensitive host, no fragment,
/// no trailing slash on the path.
fn normalize(u: &Url) -> String {
    let mut u = u.clone();
    u.set_fragment(None);
    let path = u.path().trim_end_matches('/').to_string();
    u.set_path(&path);
    format!(
        "{}://{}{}{}",
        u.scheme(),
        u.host_str().unwrap_or("").to_ascii_lowercase(),
        u.path(),
        u.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
}

/// Accept `example.com/x` as well as a full URL — a reader pasting an address
/// should not have to remember the scheme.
fn parse_url(s: &str) -> Result<Url> {
    let s = s.trim();
    match Url::parse(s) {
        Ok(u) => Ok(u),
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            Url::parse(&format!("https://{s}")).with_context(|| format!("`{s}` is not a URL"))
        }
        Err(e) => bail!("`{s}` is not a URL: {e}"),
    }
}

/// One page, read.
struct Loaded {
    title: String,
    text: String,
    links: Vec<html::Link>,
}

struct Crawl<'a> {
    agent: ureq::Agent,
    opts: &'a FetchOptions,
    robots: Mutex<AHashMap<String, Robots>>,
    last_hit: Mutex<AHashMap<String, Instant>>,
    spent: AtomicUsize,
}

impl Crawl<'_> {
    /// Fetch a batch concurrently, preserving input order so a re-run applies
    /// the same answers in the same sequence however the requests interleave.
    fn load_all(
        &self,
        cands: &[Candidate],
        base_done: usize,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Vec<Result<Loaded, String>> {
        let slots: Vec<Mutex<Option<Result<Loaded, String>>>> =
            cands.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(base_done);
        let total = base_done + cands.len();
        let progress = Mutex::new(progress);

        let workers = self.opts.concurrency.max(1).min(cands.len().max(1));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(cand) = cands.get(i) else { break };
                        let out = self.load(&cand.url).map_err(|e| e.to_string());
                        *slots[i].lock().expect("slot") = Some(out);
                        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                        (progress.lock().expect("progress"))(Progress {
                            done: n,
                            total,
                            url: cand.url.to_string(),
                        });
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|m| m.into_inner().expect("slot").expect("worker filled it"))
            .collect()
    }

    fn load(&self, url: &Url) -> Result<Loaded> {
        // Says *why* an address is refused; PublicOnly is what enforces it.
        guard::precheck(url, &self.opts.allow_private)?;
        let host = url.host_str().unwrap_or_default().to_string();
        let delay = self.check_robots(url, &host)?;
        self.wait_turn(&host, delay);

        let resp = self
            .agent
            .get(url.as_str())
            .call()
            .map_err(|e| anyhow::anyhow!("{}", terse(&e)))?;
        let content_type = resp
            .header("content-type")
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        let budget = self
            .opts
            .max_total_bytes
            .saturating_sub(self.spent.load(Ordering::Relaxed))
            .min(self.opts.max_page_bytes);
        if budget == 0 {
            bail!("the crawl's total download budget is spent");
        }
        let mut body = Vec::new();
        resp.into_reader()
            .take(budget as u64 + 1)
            .read_to_end(&mut body)?;
        if body.len() > budget {
            bail!("larger than the {} KiB this crawl had left", budget / 1024);
        }
        self.spent.fetch_add(body.len(), Ordering::Relaxed);

        match content_type.as_str() {
            "text/html" | "application/xhtml+xml" | "" => {
                let got = html::extract(&String::from_utf8_lossy(&body));
                Ok(Loaded {
                    title: got.title,
                    text: got.markdown,
                    links: got.links,
                })
            }
            "text/plain" | "text/markdown" | "text/x-markdown" => Ok(Loaded {
                title: String::new(),
                text: String::from_utf8_lossy(&body).into_owned(),
                links: Vec::new(),
            }),
            // A linked PDF is a document like any other, and the extractor is
            // already in the tree. It contributes no links, which is fine —
            // its prose is what was wanted.
            "application/pdf" => {
                let text =
                    crate::extract::extract_text_with_progress("x.pdf", &body, &mut |_, _| {})
                        .map_err(|e| anyhow::anyhow!("reading the PDF: {e}"))?;
                Ok(Loaded {
                    title: String::new(),
                    text,
                    links: Vec::new(),
                })
            }
            other => bail!("`{other}` is not a document this reads"),
        }
    }

    /// Fetch and cache a host's `robots.txt`, returning the delay it asks for.
    /// A host that publishes nothing has objected to nothing.
    fn check_robots(&self, url: &Url, host: &str) -> Result<Option<Duration>> {
        let cached = self.robots.lock().expect("robots").get(host).cloned();
        let rules = match cached {
            Some(r) => r,
            None => {
                let mut u = url.clone();
                u.set_path("/robots.txt");
                u.set_query(None);
                u.set_fragment(None);
                let fetched = match self.agent.get(u.as_str()).call() {
                    Ok(r) if r.status() == 200 => r
                        .into_string()
                        .map(|t| Robots::parse(&t, USER_AGENT))
                        .unwrap_or_else(|_| Robots::allow_all()),
                    _ => Robots::allow_all(),
                };
                self.robots
                    .lock()
                    .expect("robots")
                    .insert(host.to_string(), fetched.clone());
                fetched
            }
        };
        if !rules.allows(url.path()) {
            bail!("robots.txt for {host} disallows it");
        }
        Ok(rules.crawl_delay())
    }

    /// Space requests to one host, honouring its own `Crawl-delay` when it
    /// asks for more than our default.
    fn wait_turn(&self, host: &str, asked: Option<Duration>) {
        let gap = asked
            .unwrap_or(self.opts.host_delay)
            .max(self.opts.host_delay);
        let sleep = {
            let mut last = self.last_hit.lock().expect("last_hit");
            let now = Instant::now();
            let wait = last
                .get(host)
                .map(|t| gap.saturating_sub(now.duration_since(*t)))
                .unwrap_or_default();
            last.insert(host.to_string(), now + wait);
            wait
        };
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
    }
}

/// ureq prints the whole URL and a wall of transport detail; a reader scanning
/// a list of dropped pages wants the reason.
fn terse(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t
            .message()
            .map(str::to_string)
            .unwrap_or_else(|| "the request failed".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_is_read_as_https() {
        assert_eq!(
            parse_url("example.com/docs").unwrap().as_str(),
            "https://example.com/docs"
        );
        assert_eq!(
            parse_url("http://example.com/").unwrap().as_str(),
            "http://example.com/"
        );
    }

    #[test]
    fn spellings_of_one_address_collapse() {
        let a = parse_url("https://Example.com/docs/").unwrap();
        let b = parse_url("https://example.com/docs#section").unwrap();
        assert_eq!(normalize(&a), normalize(&b));
        // A query is part of the address, not decoration.
        let c = parse_url("https://example.com/docs?page=2").unwrap();
        assert_ne!(normalize(&a), normalize(&c));
    }

    #[test]
    fn decay_only_breaks_ties() {
        assert_eq!(decay(1), 1.0, "the first hop is not penalized");
        assert_eq!(decay(2), 0.5);
    }

    #[test]
    fn candidates_resolve_dedupe_and_refuse_what_is_not_a_web_page() {
        let base = parse_url("https://x.test/a/b").unwrap();
        let links = vec![
            html::Link {
                href: "../c".into(),
                anchor: "C".into(),
                in_main: true,
            },
            html::Link {
                href: "https://x.test/c".into(),
                anchor: "again".into(),
                in_main: false,
            },
            html::Link {
                href: "mailto:a@b.test".into(),
                anchor: "mail".into(),
                in_main: true,
            },
            html::Link {
                href: "https://y.test/d".into(),
                anchor: "D".into(),
                in_main: true,
            },
        ];
        let mut seen = AHashSet::new();
        seen.insert(normalize(&base));
        let got = candidates(&base, &links, 1, &mut seen);
        let urls: Vec<&str> = got.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(urls, vec!["https://x.test/c", "https://y.test/d"]);
        assert!(
            got[0].anchor.contains("C") && got[0].anchor.contains("again"),
            "a second link to one page also describes it: {}",
            got[0].anchor
        );
        assert!(got[0].same_origin && !got[1].same_origin);
    }

    #[test]
    fn a_page_carries_its_provenance_and_the_chunker_can_see_it() {
        let mut p = Page {
            url: "https://x.test/a".into(),
            title: "Title".into(),
            block: String::new(),
            score: 1.0,
            depth: 0,
            chars: 0,
            kept: true,
        };
        render(&mut p, "Body text.");
        assert!(p.block.starts_with(SOURCE_MARKER), "{}", p.block);
        assert!(p.block.contains("https://x.test/a"));
        assert!(p.block.contains("# Title"));
        // The marker is its own paragraph — that is what forces a chunk break.
        assert!(p.block.contains("-->\n\n"), "{}", p.block);
        assert_eq!(p.chars, p.block.chars().count());
    }

    #[test]
    fn the_document_is_the_kept_pages_only() {
        let page = |url: &str, kept| Page {
            url: url.into(),
            title: String::new(),
            block: format!("{SOURCE_MARKER} {url} -->\n\nbody"),
            score: 1.0,
            depth: 0,
            chars: 0,
            kept,
        };
        let f = Fetched {
            pages: vec![page("https://a.test", true), page("https://b.test", false)],
            dropped: vec![],
        };
        let doc = f.document();
        assert!(doc.contains("a.test"));
        assert!(
            !doc.contains("b.test"),
            "an unselected page is not in the document"
        );
    }
}
