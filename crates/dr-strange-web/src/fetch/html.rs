//! HTML → Markdown for the URL fetcher (ROADMAP §9).
//!
//! Two jobs in one pass over the parsed tree: produce the page's prose as
//! Markdown, and collect its outbound links with the anchor text that describes
//! them. Markdown rather than plain text because the digest chunker already
//! splits on blank lines and because headings and list structure are exactly
//! the cues a model uses to tell a definition from an aside.
//!
//! This is not the tag-stripping in `extract.rs` (which exists for docx XML and
//! would happily render a navigation bar as prose). Chrome is dropped by
//! element: `<script>`/`<style>` and friends vanish entirely, while
//! `<nav>`/`<aside>`/`<footer>` are walked for their **links only** — a
//! documentation sidebar is noise as text and a table of contents as hyperlinks.
//!
//! Link URLs never enter the prose; only the anchor text does. The URLs are
//! returned separately for the crawler to score, so the model is not billed for
//! a page's worth of hrefs.

use scraper::{ElementRef, Html, Node, Selector};

/// One outbound link, exactly as written in the document.
#[derive(Debug, Clone)]
pub struct Link {
    /// The `href`, unresolved — the caller joins it against the page URL.
    pub href: String,
    /// Anchor text, plus the `title` attribute when it adds anything.
    pub anchor: String,
    /// Whether the link sits in the page's main content rather than its chrome.
    /// Not a filter: it feeds the score, so a docs sidebar can still win.
    pub in_main: bool,
}

/// A page reduced to what is worth reading and what is worth following.
#[derive(Debug, Default)]
pub struct Extracted {
    pub title: String,
    pub markdown: String,
    pub links: Vec<Link>,
}

/// Elements whose subtree is dropped whole: they carry no prose and no link
/// worth following.
const DROP: &[&str] = &[
    "script", "style", "noscript", "svg", "template", "iframe", "canvas", "video", "audio",
    "object", "embed", "form", "input", "select", "textarea", "button", "map", "picture",
];

/// Elements walked for links but not for text — page chrome.
const CHROME: &[&str] = &["nav", "aside", "footer", "header"];

/// Elements that force a paragraph break around their content.
const BLOCK: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "table",
    "tr",
    "dl",
    "dt",
    "dd",
    "figure",
    "figcaption",
    "address",
    "details",
    "summary",
    "hr",
    "br",
];

/// Parse a document into prose plus links.
pub fn extract(source: &str) -> Extracted {
    let doc = Html::parse_document(source);

    let title = title_of(&doc);
    // Prefer an explicit content container, but only when it actually holds the
    // page — a decorative `<main>` wrapping a teaser would otherwise throw the
    // article away.
    let main = Selector::parse("article, main, [role='main']")
        .ok()
        .and_then(|sel| {
            let body = Selector::parse("body").ok()?;
            let body_len = doc.select(&body).next().map_or(0, |b| text_len(b));
            doc.select(&sel)
                .max_by_key(|e| text_len(*e))
                .filter(|e| body_len == 0 || text_len(*e) * 4 >= body_len)
        });

    let mut w = Walker {
        blocks: Vec::new(),
        cur: String::new(),
        list: Vec::new(),
        links: Vec::new(),
        // With no main container every non-chrome element is content.
        text_on: main.is_none(),
        main_node: main.map(|e| e.id()),
        in_main: main.is_none(),
        pre: false,
        pending_space: false,
    };
    let root = doc
        .select(&Selector::parse("body").unwrap())
        .next()
        .unwrap_or(doc.root_element());
    w.node(*root);
    w.flush();

    Extracted {
        title,
        markdown: w.blocks.join("\n\n"),
        links: w.links,
    }
}

fn title_of(doc: &Html) -> String {
    let pick = |css: &str| {
        Selector::parse(css).ok().and_then(|s| {
            doc.select(&s)
                .next()
                .map(|e| {
                    e.text()
                        .collect::<String>()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|t| !t.is_empty())
        })
    };
    pick("title").or_else(|| pick("h1")).unwrap_or_default()
}

fn text_len(e: ElementRef<'_>) -> usize {
    e.text().map(str::len).sum()
}

struct Walker {
    blocks: Vec<String>,
    cur: String,
    list: Vec<Option<usize>>,
    links: Vec<Link>,
    /// Whether text is currently being kept (off inside chrome, and off outside
    /// the main container when there is one).
    text_on: bool,
    main_node: Option<ego_tree::NodeId>,
    in_main: bool,
    pre: bool,
    /// Whitespace the document had but we have not emitted yet.
    pending_space: bool,
}

impl Walker {
    fn flush(&mut self) {
        let block = self.cur.trim().to_string();
        self.cur.clear();
        self.pending_space = false;
        if !block.is_empty() {
            self.blocks.push(block);
        }
    }

    /// Text from the document. Whitespace is collapsed as we go — HTML's is not
    /// significant and the digest chunker counts characters — but a separation
    /// that *existed* is remembered rather than emitted, so a marker written
    /// next lands tight against the word it decorates: `**bold**`, not `** bold`.
    fn push_text(&mut self, s: &str) {
        if !self.text_on {
            return;
        }
        if self.pre {
            self.cur.push_str(s);
            return;
        }
        if s.starts_with(char::is_whitespace) {
            self.pending_space = true;
        }
        for (i, word) in s.split_whitespace().enumerate() {
            if i > 0 {
                self.pending_space = true;
            }
            self.space();
            self.cur.push_str(word);
        }
        if s.ends_with(char::is_whitespace) {
            self.pending_space = true;
        }
    }

    /// Markup we emit ourselves — emphasis markers, list bullets, headings.
    /// Settles any owed whitespace *before* the marker.
    fn push_inline(&mut self, s: &str) {
        if !self.text_on {
            return;
        }
        self.space();
        self.cur.push_str(s);
    }

    fn space(&mut self) {
        if self.pending_space && !self.cur.is_empty() && !self.cur.ends_with('\n') {
            self.cur.push(' ');
        }
        self.pending_space = false;
    }

    fn node(&mut self, node: ego_tree::NodeRef<'_, Node>) {
        match node.value() {
            Node::Text(t) => self.push_text(&t.text),
            Node::Element(_) => {
                let Some(el) = ElementRef::wrap(node) else {
                    return;
                };
                self.element(el);
            }
            _ => {
                for child in node.children() {
                    self.node(child);
                }
            }
        }
    }

    fn element(&mut self, el: ElementRef<'_>) {
        let name = el.value().name().to_ascii_lowercase();
        if DROP.contains(&name.as_str()) {
            return;
        }

        // Entering the main container turns text on for its whole subtree.
        let entered_main = self.main_node == Some(el.id());
        let (was_text, was_main) = (self.text_on, self.in_main);
        if entered_main {
            self.text_on = true;
            self.in_main = true;
        }
        // Chrome contributes links but no prose.
        let chrome = CHROME.contains(&name.as_str());
        if chrome {
            self.text_on = false;
            self.in_main = false;
        }

        if name == "a" {
            self.anchor(el);
            self.text_on = was_text;
            self.in_main = was_main;
            return;
        }

        let is_block = BLOCK.contains(&name.as_str());
        if is_block {
            self.flush();
        }

        match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.push_inline(&format!("{} ", "#".repeat(level)));
                self.children(el);
                self.flush();
            }
            "li" => {
                let depth = self.list.len().saturating_sub(1);
                let marker = match self.list.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{n}. ")
                    }
                    _ => "- ".to_string(),
                };
                self.push_inline(&format!("{}{marker}", "  ".repeat(depth)));
                self.children(el);
                self.flush();
            }
            "ul" | "ol" => {
                self.list.push(if name == "ol" { Some(0) } else { None });
                self.children(el);
                self.list.pop();
            }
            "pre" => {
                self.raw("```\n");
                let prev = std::mem::replace(&mut self.pre, true);
                self.children(el);
                self.pre = prev;
                if self.text_on && !self.cur.ends_with('\n') {
                    self.raw("\n");
                }
                self.raw("```");
                self.flush();
            }
            "code" if !self.pre => {
                self.push_inline("`");
                self.children(el);
                self.raw("`");
            }
            "blockquote" => {
                let start = self.blocks.len();
                self.children(el);
                self.flush();
                for b in &mut self.blocks[start..] {
                    *b = b
                        .lines()
                        .map(|l| format!("> {l}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                }
            }
            "strong" | "b" => self.wrap(el, "**"),
            "em" | "i" => self.wrap(el, "*"),
            "br" => self.raw("\n"),
            "hr" => {
                self.push_inline("---");
                self.flush();
            }
            "td" | "th" => {
                if !self.cur.is_empty() && !self.cur.ends_with("| ") {
                    self.raw(" | ");
                }
                self.children(el);
            }
            _ => self.children(el),
        }

        if is_block {
            self.flush();
        }
        self.text_on = was_text;
        self.in_main = was_main;
    }

    /// Markup we emit ourselves that must not settle owed whitespace — a
    /// closing marker, a line break. A no-op where text is not being kept, so
    /// a `<code>` inside a navigation bar cannot leave a stray backtick behind.
    fn raw(&mut self, s: &str) {
        if !self.text_on {
            return;
        }
        self.pending_space = false;
        self.cur.push_str(s);
    }

    /// Emphasis markers are only worth emitting around actual text — an empty
    /// `<strong>` would otherwise leave `****` in the prose.
    fn wrap(&mut self, el: ElementRef<'_>, mark: &str) {
        // Where text is off the marker was never written, so there is nothing
        // to balance or undo — but the links inside still count.
        if !self.text_on {
            self.children(el);
            return;
        }
        self.push_inline(mark);
        let before = self.cur.len();
        self.children(el);
        if self.cur.len() == before {
            self.cur.truncate(before - mark.len());
        } else {
            self.raw(mark);
        }
    }

    fn anchor(&mut self, el: ElementRef<'_>) {
        let text = el
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(href) = el.value().attr("href") {
            let href = href.trim();
            // In-page anchors and javascript: handlers are not documents.
            if !href.is_empty() && !href.starts_with('#') {
                let title = el.value().attr("title").unwrap_or("").trim();
                let anchor = if title.is_empty() || text.contains(title) {
                    text.clone()
                } else {
                    format!("{text} {title}")
                };
                self.links.push(Link {
                    href: href.to_string(),
                    anchor,
                    in_main: self.in_main,
                });
            }
        }
        // The anchor's words are part of the sentence around it.
        self.push_text(&text);
    }

    fn children(&mut self, el: ElementRef<'_>) {
        for child in el.children() {
            self.node(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_survives_as_markdown() {
        let md = extract(
            r#"<html><body><h2>Heading</h2><p>Some <strong>bold</strong> text.</p>
               <ul><li>one</li><li>two</li></ul>
               <pre><code>fn main() {}</code></pre></body></html>"#,
        )
        .markdown;
        assert!(md.contains("## Heading"), "{md}");
        assert!(md.contains("Some **bold** text."), "{md}");
        assert!(md.contains("- one"), "{md}");
        assert!(md.contains("```"), "{md}");
        assert!(md.contains("fn main() {}"), "{md}");
    }

    #[test]
    fn scripts_and_styles_leave_nothing_behind() {
        let md = extract(
            "<html><body><script>var x = 'hello script'</script>\
             <style>.a{color:red}</style><p>real</p></body></html>",
        )
        .markdown;
        assert_eq!(md, "real");
    }

    #[test]
    fn chrome_gives_up_its_text_but_keeps_its_links() {
        let got = extract(
            r#"<html><body>
                 <nav><a href="/guide">Guide</a> Navigation junk</nav>
                 <p>The actual article.</p>
               </body></html>"#,
        );
        assert_eq!(got.markdown, "The actual article.");
        assert_eq!(got.links.len(), 1);
        assert_eq!(got.links[0].href, "/guide");
        assert_eq!(got.links[0].anchor, "Guide");
        assert!(!got.links[0].in_main, "a nav link is not main content");
    }

    #[test]
    fn a_main_container_wins_over_the_page_around_it() {
        let got = extract(
            r#"<html><body>
                 <div>Cookie banner and other furniture that is fairly long.</div>
                 <main><p>The paper's own words, at some length, so main wins.</p>
                       <a href="/cited">Cited work</a></main>
               </body></html>"#,
        );
        assert!(
            got.markdown.contains("The paper's own words"),
            "{}",
            got.markdown
        );
        assert!(!got.markdown.contains("Cookie banner"), "{}", got.markdown);
        assert!(got.links.iter().any(|l| l.in_main && l.href == "/cited"));
    }

    #[test]
    fn a_decorative_main_does_not_throw_the_page_away() {
        // `<main>` holding a sliver of the text is a teaser, not the article.
        let long = "word ".repeat(200);
        let html = format!(
            "<html><body><main><p>Read more</p></main><article><p>{long}</p></article></body></html>"
        );
        let md = extract(&html).markdown;
        assert!(md.contains("word word"), "the long article must survive");
    }

    #[test]
    fn link_urls_never_reach_the_prose() {
        let got =
            extract(r#"<p>See <a href="https://example.com/deep/path?x=1">the docs</a>.</p>"#);
        assert!(got.markdown.contains("the docs"));
        assert!(
            !got.markdown.contains("example.com"),
            "hrefs are for the crawler, not the model: {}",
            got.markdown
        );
        assert_eq!(got.links[0].href, "https://example.com/deep/path?x=1");
    }

    #[test]
    fn markup_inside_chrome_leaves_no_residue() {
        // Emphasis and code in a nav emitted their markers into the buffer even
        // though the text they wrapped was being discarded — a stray backtick,
        // and an arithmetic overflow when an empty `<strong>` tried to undo a
        // marker it had never written. Found crawling a real page.
        let got = extract(
            "<nav><strong></strong><em><a href=\"/x\">X</a></em><code>y</code></nav>\
             <p>Body.</p>",
        );
        assert_eq!(got.markdown, "Body.");
        assert_eq!(got.links.len(), 1, "links inside chrome markup still count");
    }

    #[test]
    fn in_page_anchors_are_not_documents() {
        // `r##`: the `"#` in `href="#section"` would close an `r#` string.
        let got = extract(r##"<a href="#section">Jump</a><a href="/real">Real</a>"##);
        assert_eq!(got.links.len(), 1);
        assert_eq!(got.links[0].href, "/real");
    }

    #[test]
    fn the_title_comes_from_the_head_or_the_first_heading() {
        assert_eq!(
            extract("<title>From head</title><h1>H</h1>").title,
            "From head"
        );
        assert_eq!(
            extract("<body><h1>From body</h1></body>").title,
            "From body"
        );
        assert_eq!(extract("<p>none</p>").title, "");
    }

    #[test]
    fn entities_are_decoded_and_whitespace_collapsed() {
        let md = extract("<p>a &amp; b   \n   c&nbsp;d</p>").markdown;
        assert!(md.starts_with("a & b c"), "{md}");
    }
}
