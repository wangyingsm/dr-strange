//! `robots.txt` for the URL fetcher (ROADMAP §9).
//!
//! The bandwidth a crawl spends belongs to someone else, so §9 requires the
//! file be respected rather than treated as advisory. This is a deliberately
//! small implementation of the parts that carry meaning — group selection by
//! `User-agent`, `Allow`/`Disallow` with `*` and `$`, longest-match-wins, and
//! `Crawl-delay` — and it fails **open** on a malformed or missing file, since
//! a site that publishes nothing has not objected to anything.

use std::time::Duration;

#[derive(Debug, Clone)]
struct Rule {
    allow: bool,
    pattern: String,
}

/// The rules that apply to one user agent on one host.
#[derive(Debug, Clone, Default)]
pub struct Robots {
    rules: Vec<Rule>,
    crawl_delay: Option<Duration>,
}

impl Robots {
    /// No file, no objection.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Parse and keep exactly one group: the one whose `User-agent` token is the
    /// **longest prefix** of our product token, falling back to `*`.
    ///
    /// Prefix of the *product token* — the part of the UA before the version —
    /// not a substring of the whole header. That distinction is not pedantry:
    /// Wikipedia publishes a `User-agent: Fetch` group (an offline-download
    /// tool) with `Disallow: /`, and a substring test makes `drsg-fetch` claim
    /// it and refuse to read Wikipedia at all. Found by reading a real page.
    ///
    /// A matching group *replaces* the wildcard rather than adding to it, which
    /// is what the convention means by the most specific group winning.
    pub fn parse(text: &str, ua: &str) -> Self {
        /// One `User-agent:` block and the directives beneath it.
        #[derive(Default)]
        struct Group {
            agents: Vec<String>,
            rules: Vec<Rule>,
            delay: Option<Duration>,
        }

        let product = ua
            .split(['/', ' '])
            .next()
            .unwrap_or(ua)
            .to_ascii_lowercase();
        let mut groups: Vec<Group> = Vec::new();
        // Several consecutive `User-agent` lines share one block of rules; the
        // first directive after them ends the run.
        let mut reading_agents = false;

        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    if !reading_agents {
                        groups.push(Group::default());
                        reading_agents = true;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.agents.push(value.to_ascii_lowercase());
                    }
                }
                "disallow" | "allow" => {
                    reading_agents = false;
                    let allow = key == "allow";
                    // An empty `Disallow:` is the idiom for "everything is
                    // permitted" and must not become a rule matching all paths.
                    if value.is_empty() && !allow {
                        continue;
                    }
                    if let Some(g) = groups.last_mut() {
                        g.rules.push(Rule {
                            allow,
                            pattern: value.to_string(),
                        });
                    }
                }
                "crawl-delay" => {
                    reading_agents = false;
                    if let Ok(secs) = value.parse::<f32>()
                        && secs.is_finite()
                        && secs > 0.0
                        && let Some(g) = groups.last_mut()
                    {
                        g.delay = Some(Duration::from_secs_f32(secs.min(60.0)));
                    }
                }
                _ => reading_agents = false,
            }
        }

        let (mut best, mut wildcard) = (None::<(usize, usize)>, None::<usize>);
        for (i, g) in groups.iter().enumerate() {
            for agent in &g.agents {
                // A trailing `*` (`Mediapartners-Google*`) is decoration on what
                // is already a prefix match.
                let agent = agent.trim_end_matches('*');
                if agent.is_empty() {
                    wildcard.get_or_insert(i);
                } else if product.starts_with(agent)
                    && best.is_none_or(|(len, _)| agent.len() > len)
                {
                    best = Some((agent.len(), i));
                }
            }
        }
        match best.map(|(_, i)| i).or(wildcard) {
            Some(i) => {
                let g = &groups[i];
                Self {
                    rules: g.rules.clone(),
                    crawl_delay: g.delay,
                }
            }
            None => Self::allow_all(),
        }
    }

    /// The delay the site asked for between requests, if any.
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay
    }

    /// Whether `path` may be fetched. The longest matching pattern decides; a
    /// tie goes to `Allow`, which is how a site carves an exception out of a
    /// broader `Disallow`.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for rule in &self.rules {
            if !matches(&rule.pattern, path) {
                continue;
            }
            let len = rule.pattern.len();
            best = match best {
                Some((best_len, best_allow)) if best_len > len => Some((best_len, best_allow)),
                // Equal specificity: `Allow` wins, so a site can carve an
                // exception without having to out-length its own rule.
                Some((best_len, best_allow)) if best_len == len => {
                    Some((len, best_allow || rule.allow))
                }
                _ => Some((len, rule.allow)),
            };
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Prefix match with the two wildcards the convention defines: `*` for any run
/// of characters and a trailing `$` anchoring the end of the path.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let mut pos = 0usize;
    for (i, part) in pattern.split('*').enumerate() {
        if part.is_empty() {
            continue;
        }
        let hay = path.get(pos..).unwrap_or("");
        let found = if i == 0 {
            hay.starts_with(part).then_some(0)
        } else {
            hay.find(part)
        };
        match found {
            Some(idx) => pos += idx + part.len(),
            None => return false,
        }
    }
    // With a `$`, whatever matched last has to have reached the end. A pattern
    // ending in `*$` consumed everything by construction.
    !anchored || pos == path.len() || pattern.ends_with('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "drsg-fetch/1.0";

    #[test]
    fn a_missing_file_permits_everything() {
        assert!(Robots::allow_all().allows("/anything"));
    }

    #[test]
    fn the_wildcard_group_applies_when_we_are_not_named() {
        let r = Robots::parse("User-agent: *\nDisallow: /private\n", UA);
        assert!(!r.allows("/private/x"));
        assert!(r.allows("/public"));
    }

    #[test]
    fn a_group_naming_us_replaces_the_wildcard_group() {
        let txt = "User-agent: *\nDisallow: /\n\nUser-agent: drsg-fetch\nDisallow: /admin\n";
        let r = Robots::parse(txt, UA);
        assert!(
            r.allows("/docs"),
            "our own group permits everything but /admin"
        );
        assert!(!r.allows("/admin/panel"));
    }

    #[test]
    fn an_empty_disallow_permits_everything() {
        let r = Robots::parse("User-agent: *\nDisallow:\n", UA);
        assert!(r.allows("/anything"));
    }

    #[test]
    fn the_longest_match_decides_and_a_tie_goes_to_allow() {
        let r = Robots::parse("User-agent: *\nDisallow: /docs\nAllow: /docs/public\n", UA);
        assert!(!r.allows("/docs/secret"));
        assert!(
            r.allows("/docs/public/page"),
            "the more specific Allow wins"
        );
    }

    #[test]
    fn wildcards_and_the_end_anchor_are_honoured() {
        let r = Robots::parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /a/*/b\n", UA);
        assert!(!r.allows("/papers/x.pdf"));
        assert!(r.allows("/papers/x.pdf.html"), "$ anchors the end");
        assert!(!r.allows("/a/anything/b"));
        assert!(r.allows("/a/anything/c"));
    }

    #[test]
    fn a_group_for_another_agent_whose_name_we_merely_contain_is_not_ours() {
        // Wikipedia really does publish this, for an offline-download tool. A
        // substring test made `drsg-fetch` obey it and refuse the whole site.
        let txt = "User-agent: Fetch\nDisallow: /\n\nUser-agent: *\nDisallow: /w/\n";
        let r = Robots::parse(txt, UA);
        assert!(r.allows("/wiki/Anything"), "we are not `Fetch`");
        assert!(!r.allows("/w/index.php"), "but the wildcard group is ours");
    }

    #[test]
    fn the_longest_matching_agent_token_wins() {
        let txt = "User-agent: drsg\nDisallow: /a\n\nUser-agent: drsg-fetch\nDisallow: /b\n";
        let r = Robots::parse(txt, UA);
        assert!(
            r.allows("/a"),
            "the more specific group replaces the shorter"
        );
        assert!(!r.allows("/b"));
    }

    #[test]
    fn several_agent_lines_share_one_block_of_rules() {
        let txt = "User-agent: googlebot\nUser-agent: drsg-fetch\nDisallow: /no\n";
        let r = Robots::parse(txt, UA);
        assert!(!r.allows("/no/thing"));
    }

    #[test]
    fn a_crawl_delay_is_read_and_bounded() {
        let r = Robots::parse("User-agent: *\nCrawl-delay: 2.5\n", UA);
        assert_eq!(r.crawl_delay(), Some(Duration::from_secs_f32(2.5)));
        let silly = Robots::parse("User-agent: *\nCrawl-delay: 86400\n", UA);
        assert_eq!(silly.crawl_delay(), Some(Duration::from_secs(60)), "capped");
    }

    #[test]
    fn comments_and_junk_lines_do_not_derail_the_parse() {
        let txt = "# hello\nUser-agent: *   # everyone\nDisallow: /x  # nope\nSitemap: /s.xml\n";
        let r = Robots::parse(txt, UA);
        assert!(!r.allows("/x/y"));
        assert!(r.allows("/y"));
    }
}
