//! The official plugin catalog (ROADMAP §11) — **data, fetched, not code**.
//!
//! The catalog used to be a `const` in this crate: nine entries, each a
//! release URL and the SHA-256 of the artifact behind it. That made every
//! plugin release a change to the database's source tree — tag `rust-v1.4.2`
//! in the extensions repository, then edit a Rust file here, bump, ship — for
//! a fact the host merely *repeats*. The two projects release at their own
//! pace on purpose; a pin that forces them to release together defeats the
//! reason they are apart.
//!
//! So the list lives where the plugins do:
//!
//! ```text
//! https://raw.githubusercontent.com/wangyingsm/dr-strange-extension/master/catalog.json
//! ```
//!
//! and this module knows how to read it, cache it, and say which of its
//! entries this build can actually run.
//!
//! ## What the host still decides
//!
//! Moving the list out does not move the *judgement* out. A catalog entry is
//! a claim by the extensions repository; three things here weigh it:
//!
//! * **`contract`** — the WIT world the artifact was built against, checked
//!   against [`CONTRACT_VERSION`], the one this host speaks. Same major, and
//!   no newer than ours, because the world grows additively.
//! * **`min_drsg`** — the oldest host the entry claims to work with, checked
//!   against this build's version.
//! * **`sha256`** — pinned at install and re-checked at every load, exactly as
//!   before ([`super::registry`]). What changed is where the expected hash is
//!   read from, not that it is enforced.
//!
//! An entry this build cannot vouch for is **shown and warned about, never
//! hidden**: the wasm loader is the real gate — it refuses a component with a
//! forbidden import or no manifest — and a catalog that silently omitted a
//! plugin would leave an operator debugging why `drsg plugin install` never
//! offers `zig`.
//!
//! Several entries may share a name. That is how a plugin keeps serving older
//! hosts: publish `rust@2.0.0` needing drsg 3, leave `rust@1.4.1` in the file,
//! and each host picks the newest it can run ([`Catalog::current`]).
//!
//! ## Offline
//!
//! Every successful fetch writes `catalog.json` beside the installed plugins,
//! and a fetch that fails falls back to it, saying how old it is. With no
//! cache and no network there is nothing honest to show, so [`load_catalog`]
//! fails naming the URL and the escape hatch — `drsg plugin install <file |
//! url>` needs no catalog at all. Deliberately no copy vendored into this
//! binary: a snapshot in this tree is the very thing being removed, and one
//! that silently went stale would be worse than an error that says so.

use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use super::registry::PluginStore;

/// Where the official list lives — the extensions repository's default
/// branch, so a plugin release is a commit there and nothing here.
pub const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/wangyingsm/dr-strange-extension/master/catalog.json";

/// The plugin contract this host speaks: the version of the `drsg:preprocess`
/// WIT package it is built against.
///
/// Written out rather than parsed from the WIT at build time, and held to the
/// vendored copy by a test in this module — a constant
/// that can disagree with the file it describes is a constant with a bug
/// waiting, and the test is cheaper than a build script.
pub const CONTRACT_VERSION: &str = "1.0.0";

/// This build, for `min_drsg`. The workspace version, so the number an
/// operator reads in `drsg --version` is the number the catalog is judged
/// against.
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The schema this code understands. A file declaring a higher one is still
/// read — the fields below are the floor, and everything since has been
/// additive — but entries it cannot make sense of are its own business.
const KNOWN_SCHEMA: u32 = 1;

/// A catalog is small; anything near this is not one.
pub const CATALOG_DOWNLOAD_CAP: usize = 4 << 20;

/// The cache, beside the plugins it describes.
const CACHE_FILE: &str = "catalog.json";

/// One official plugin, as `catalog.json` records it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OfficialPlugin {
    pub name: String,
    /// The plugin's own version — the `<name>-v<version>` tag it was cut from.
    pub version: String,
    /// What it claims, as display text (`.rs`, `.ts .tsx …`). Prose for the
    /// one entry whose input is not a file: `git` reads a repository.
    ///
    /// Display only. The component's manifest is the authority on which
    /// extensions it actually routes, and install reads it from there.
    pub claims: String,
    pub url: String,
    /// Hex SHA-256 of the artifact at `url`, verified on download.
    pub sha256: String,
    /// The WIT world the artifact was built against. Absent means the entry
    /// makes no claim, which is read as "assume it fits" — the loader still
    /// checks the imports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// The oldest drsg this entry claims to work with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_drsg: Option<String>,
}

impl OfficialPlugin {
    /// Check downloaded bytes against the hash the catalog pins for them.
    ///
    /// A release tag never moves and a tagged asset never changes, so a
    /// mismatch is not staleness: either the file at `url` is not the one this
    /// entry describes, or something rewrote it in flight. Both are refusals —
    /// the alternative is installing an artifact whose identity nothing
    /// vouches for, and identity is the whole of what the hash is for.
    ///
    /// This is a *download-time* check. It does not replace the pin the store
    /// records at install and re-checks at every load; it is what makes that
    /// pin the hash the catalog meant rather than the hash of whatever
    /// arrived.
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        let found = super::registry::hex_sha256(bytes);
        if !found.eq_ignore_ascii_case(self.sha256.trim()) {
            bail!(
                "{}@{} does not match the hash the official catalog pins\n  \
                 expected sha256:{}\n  found    sha256:{found}\n\
                 {} was not installed — the artifact at that URL is not the \
                 one the catalog describes",
                self.name,
                self.version,
                self.sha256,
                self.url,
            );
        }
        Ok(())
    }
}

/// The file as a whole.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub schema: u32,
    /// The contract the file as a whole was written for, when its entries do
    /// not each say. Informational.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    pub plugins: Vec<OfficialPlugin>,
}

/// Whether this build can run an entry, and why not when it cannot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "compat", rename_all = "snake_case")]
pub enum Compat {
    /// Known-good with this build's contract and version.
    Ok,
    /// The entry asks for a newer drsg than this one.
    NeedsHost { min_drsg: String },
    /// Built against a plugin contract this host does not speak.
    OtherContract { contract: String },
}

impl Compat {
    pub fn is_ok(&self) -> bool {
        matches!(self, Compat::Ok)
    }

    /// One line for a terminal, or nothing when the entry is fine.
    pub fn note(&self) -> Option<String> {
        match self {
            Compat::Ok => None,
            Compat::NeedsHost { min_drsg } => {
                Some(format!("needs drsg >= {min_drsg}, this is {HOST_VERSION}"))
            }
            Compat::OtherContract { contract } => Some(format!(
                "built against plugin contract {contract}, this host speaks {CONTRACT_VERSION}"
            )),
        }
    }
}

/// One entry, weighed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Pick<'a> {
    #[serde(flatten)]
    pub plugin: &'a OfficialPlugin,
    #[serde(flatten)]
    pub compat: Compat,
}

impl Catalog {
    /// Parse a fetched or cached file.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let catalog: Catalog =
            serde_json::from_slice(bytes).context("parsing the official plugin catalog")?;
        if catalog.plugins.is_empty() {
            bail!("the official plugin catalog lists no plugins");
        }
        Ok(catalog)
    }

    /// True when the file declares a schema this build has not seen — the
    /// surfaces say so rather than pretending they read all of it.
    pub fn from_the_future(&self) -> bool {
        self.schema > KNOWN_SCHEMA
    }

    /// What this host should offer: one entry per name, in the order the file
    /// first mentions each name.
    ///
    /// The newest **runnable** version wins. When no version of a plugin is
    /// runnable the newest one is still returned, carrying the reason — an
    /// operator who upgrades a host and finds a plugin missing learns nothing;
    /// one who is told "needs drsg >= 3.0.0" knows exactly what to do.
    pub fn current(&self) -> Vec<Pick<'_>> {
        let mut order: Vec<&str> = Vec::new();
        for p in &self.plugins {
            if !order.contains(&p.name.as_str()) {
                order.push(&p.name);
            }
        }
        order
            .into_iter()
            .filter_map(|name| self.best(name))
            .collect()
    }

    /// The one entry this host would install for `name`, or `None` when the
    /// catalog does not mention it.
    pub fn best(&self, name: &str) -> Option<Pick<'_>> {
        let mut candidates: Vec<Pick<'_>> = self
            .plugins
            .iter()
            .filter(|p| p.name == name)
            .map(|plugin| Pick {
                plugin,
                compat: compat(plugin),
            })
            .collect();
        // Runnable first, then newest. `sort_by` is stable, so entries that
        // tie on both keep the file's order and the pick is deterministic.
        candidates.sort_by(|a, b| {
            b.compat
                .is_ok()
                .cmp(&a.compat.is_ok())
                .then_with(|| cmp_version(&b.plugin.version, &a.plugin.version))
        });
        candidates.into_iter().next()
    }
}

/// Weigh one entry against this build.
fn compat(plugin: &OfficialPlugin) -> Compat {
    if let Some(min) = &plugin.min_drsg
        && cmp_version(HOST_VERSION, min) == Ordering::Less
    {
        return Compat::NeedsHost {
            min_drsg: min.clone(),
        };
    }
    // The world grows additively, so a plugin built against an older minor of
    // the same major still fits; one built against a newer minor may import
    // something this host does not export.
    if let Some(contract) = &plugin.contract {
        let same_major = major(contract) == major(CONTRACT_VERSION);
        if !same_major || cmp_version(contract, CONTRACT_VERSION) == Ordering::Greater {
            return Compat::OtherContract {
                contract: contract.clone(),
            };
        }
    }
    Compat::Ok
}

fn major(v: &str) -> u64 {
    version_key(v).0.first().copied().unwrap_or(0)
}

/// `X.Y.Z` split into its numeric parts, and whether a pre-release suffix
/// followed. Non-numeric parts read as 0 rather than failing: a version this
/// cannot parse should sort low, not take down the catalog.
fn version_key(v: &str) -> (Vec<u64>, bool) {
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((core, _)) => (core, true),
        None => (v, false),
    };
    let parts = core
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0))
        .collect();
    (parts, pre)
}

/// Compare two dotted versions, missing components reading as 0 and a
/// pre-release sorting below the release it led to (`2.0.0-alpha` < `2.0.0`).
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    let (ka, pre_a) = version_key(a);
    let (kb, pre_b) = version_key(b);
    for i in 0..ka.len().max(kb.len()) {
        let x = ka.get(i).copied().unwrap_or(0);
        let y = kb.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    pre_b.cmp(&pre_a)
}

// ---- fetching, and the cache behind it -----------------------------------

/// Where a catalog came from — carried alongside it, because "these are the
/// official plugins" and "these were the official plugins three days ago" are
/// different statements and only one of them needs saying out loud.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Source {
    /// Fetched just now.
    Network { url: String },
    /// Served from the store's copy.
    Cache {
        /// How old the cached file is.
        age: Duration,
        /// What went wrong reaching the network, verbatim — or `None` when
        /// nothing did, because the copy was young enough to use and the
        /// network was never asked. Only the first of those is staleness.
        #[serde(skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
}

impl Source {
    /// Whether this answer is older than it was meant to be — a fetch that
    /// failed, not a cache that was still fresh.
    pub fn is_stale(&self) -> bool {
        matches!(self, Source::Cache { why: Some(_), .. })
    }

    /// One line for a terminal, or nothing when the answer is current.
    pub fn note(&self) -> Option<String> {
        match self {
            Source::Network { .. } | Source::Cache { why: None, .. } => None,
            Source::Cache {
                age,
                why: Some(why),
            } => Some(format!(
                "showing the catalog cached {} ago — {why}",
                humanize(*age)
            )),
        }
    }
}

/// A catalog and where it came from.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub catalog: Catalog,
    pub source: Source,
}

/// Read the official catalog: the network first, the store's cache when that
/// fails.
///
/// The fetch itself is the caller's — `dr-strange-llm` has no HTTP client and
/// wants none, and the surfaces that call this already carry the network
/// policy (address guards, redirect limits, size caps) that any download of
/// theirs must go through. They pass it in; this owns the decision of what to
/// do with a failure.
pub fn load_catalog(
    store: &PluginStore,
    fetch: impl FnOnce(&str) -> Result<Vec<u8>>,
) -> Result<Fetched> {
    load_catalog_within(store, Duration::ZERO, fetch)
}

/// [`load_catalog`], but content with a cached copy younger than `max_age`.
///
/// For surfaces that are *polled* rather than asked — a dashboard refreshing
/// its Extensions panel is not a reason to fetch a file from GitHub, and a
/// list that changes when a plugin is released is not one that needs to be
/// current to the second. `Duration::ZERO` is "always fetch", which is what
/// someone who just typed a command means.
pub fn load_catalog_within(
    store: &PluginStore,
    max_age: Duration,
    fetch: impl FnOnce(&str) -> Result<Vec<u8>>,
) -> Result<Fetched> {
    if !max_age.is_zero()
        && let Some((catalog, age)) = read_cache(store)
        && age <= max_age
    {
        return Ok(Fetched {
            catalog,
            source: Source::Cache { age, why: None },
        });
    }

    let why = match fetch(CATALOG_URL).and_then(|bytes| Ok((Catalog::parse(&bytes)?, bytes))) {
        Ok((catalog, bytes)) => {
            // Best effort: a store that cannot be written is a worse-off next
            // run, not a failed this one.
            let _ = write_cache(store, &bytes);
            return Ok(Fetched {
                catalog,
                source: Source::Network {
                    url: CATALOG_URL.to_string(),
                },
            });
        }
        Err(e) => format!("{e:#}"),
    };

    match read_cache(store) {
        Some((catalog, age)) => Ok(Fetched {
            catalog,
            source: Source::Cache {
                age,
                why: Some(why),
            },
        }),
        None => bail!(
            "cannot reach the official plugin catalog at {CATALOG_URL}\n  {why}\n\
             nothing is cached in {}, so there is no list to show.\n\
             A plugin can still be installed without one:\n  \
             drsg plugin install <file.wasm | url>",
            store.dir().display()
        ),
    }
}

/// Fetch the catalog and write it to the store's cache, for a caller that
/// wants the *next* read to be current rather than this one.
///
/// Separate from [`load_catalog`] because a server serving a panel must not
/// wait on GitHub: it answers from the copy it has and calls this behind the
/// response. Failure is deliberately quiet — nobody is waiting on it, and the
/// copy it failed to replace is still there.
pub fn refresh_cache(store: &PluginStore, fetch: impl FnOnce(&str) -> Result<Vec<u8>>) {
    if let Ok(bytes) = fetch(CATALOG_URL)
        && Catalog::parse(&bytes).is_ok()
    {
        let _ = write_cache(store, &bytes);
    }
}

fn cache_path(store: &PluginStore) -> PathBuf {
    store.dir().join(CACHE_FILE)
}

/// The cached catalog and its age, or `None` when there is none, it is
/// unreadable, or it no longer parses — every one of which means the same
/// thing to the caller.
pub fn read_cache(store: &PluginStore) -> Option<(Catalog, Duration)> {
    let path = cache_path(store);
    let bytes = std::fs::read(&path).ok()?;
    let catalog = Catalog::parse(&bytes).ok()?;
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or_default();
    Some((catalog, age))
}

/// Write-then-rename, as the registry does: a crash mid-write must not leave
/// half a catalog that the next run reads as the whole one.
fn write_cache(store: &PluginStore, bytes: &[u8]) -> Result<()> {
    let path = cache_path(store);
    let tmp = store.dir().join(format!(".{CACHE_FILE}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("moving {} into place", path.display()))?;
    Ok(())
}

/// An age a person reads at a glance. Coarse on purpose — the question a
/// staleness note answers is "roughly how out of date is this", never "by how
/// many seconds".
fn humanize(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=90 => "moments".to_string(),
        91..=5399 => format!("{}m", secs / 60),
        5400..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored contract and the constant that names it cannot drift:
    /// this is the same guarantee the extensions repository's `check-wit`
    /// gives its own copies, one repository over.
    #[test]
    fn contract_version_matches_the_wit() {
        let wit = include_str!("../../wit/preprocess.wit");
        let declared = wit
            .lines()
            .find_map(|l| l.trim().strip_prefix("package drsg:preprocess@"))
            .map(|rest| rest.trim_end_matches(';').trim().to_string())
            .expect("the vendored WIT declares a versioned package");
        assert_eq!(
            declared, CONTRACT_VERSION,
            "CONTRACT_VERSION disagrees with the vendored wit/preprocess.wit"
        );
    }

    fn entry(
        name: &str,
        version: &str,
        min: Option<&str>,
        contract: Option<&str>,
    ) -> OfficialPlugin {
        OfficialPlugin {
            name: name.into(),
            version: version.into(),
            claims: ".x".into(),
            url: format!("https://example.invalid/{name}-{version}.wasm"),
            sha256: "00".repeat(32),
            contract: contract.map(str::to_string),
            min_drsg: min.map(str::to_string),
        }
    }

    #[test]
    fn versions_compare_by_component_not_by_string() {
        assert_eq!(cmp_version("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(cmp_version("2.0", "2.0.0"), Ordering::Equal);
        assert_eq!(cmp_version("2.0.0-alpha", "2.0.0"), Ordering::Less);
        assert_eq!(cmp_version("2.2.1", "2.0.0"), Ordering::Greater);
    }

    #[test]
    fn the_newest_runnable_version_wins() {
        let catalog = Catalog {
            schema: 1,
            contract: None,
            plugins: vec![
                entry("rust", "1.4.1", Some("2.0.0"), Some("1.0.0")),
                // Newer, but asks for a host from the future.
                entry("rust", "9.0.0", Some("99.0.0"), Some("1.0.0")),
            ],
        };
        let pick = catalog.best("rust").expect("rust is in the catalog");
        assert_eq!(pick.plugin.version, "1.4.1");
        assert!(pick.compat.is_ok());
    }

    #[test]
    fn an_unrunnable_plugin_is_shown_with_its_reason_not_hidden() {
        let catalog = Catalog {
            schema: 1,
            contract: None,
            plugins: vec![entry("zig", "1.0.0", Some("99.0.0"), None)],
        };
        let picks = catalog.current();
        assert_eq!(picks.len(), 1);
        assert_eq!(
            picks[0].compat,
            Compat::NeedsHost {
                min_drsg: "99.0.0".into()
            }
        );
        assert!(picks[0].compat.note().is_some());
    }

    #[test]
    fn a_newer_contract_is_refused_and_an_older_one_of_the_same_major_is_not() {
        assert_eq!(
            compat(&entry("a", "1.0.0", None, Some("2.0.0"))),
            Compat::OtherContract {
                contract: "2.0.0".into()
            }
        );
        assert_eq!(
            compat(&entry("a", "1.0.0", None, Some("1.99.0"))),
            Compat::OtherContract {
                contract: "1.99.0".into()
            }
        );
        assert_eq!(
            compat(&entry("a", "1.0.0", None, Some("1.0.0"))),
            Compat::Ok
        );
        // No claim is not a failed claim.
        assert_eq!(compat(&entry("a", "1.0.0", None, None)), Compat::Ok);
    }

    #[test]
    fn catalog_order_is_the_files_order_of_first_mention() {
        let catalog = Catalog {
            schema: 1,
            contract: None,
            plugins: vec![
                entry("rust", "1.0.0", None, None),
                entry("go", "1.0.0", None, None),
                entry("rust", "1.1.0", None, None),
            ],
        };
        let names: Vec<&str> = catalog
            .current()
            .iter()
            .map(|p| p.plugin.name.as_str())
            .collect();
        assert_eq!(names, ["rust", "go"]);
    }

    #[test]
    fn a_fetch_is_cached_and_the_cache_answers_when_the_network_does_not() {
        let dir = std::env::temp_dir().join(format!("drsg-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PluginStore::open(dir.clone()).unwrap();
        let body = br#"{"schema":1,"plugins":[{"name":"rust","version":"1.4.1","claims":".rs","url":"https://example.invalid/rust.wasm","sha256":"ab"}]}"#;

        let fresh = load_catalog(&store, |_| Ok(body.to_vec())).unwrap();
        assert!(!fresh.source.is_stale());
        assert_eq!(fresh.catalog.plugins.len(), 1);

        let offline = load_catalog(&store, |_| bail!("dns went away")).unwrap();
        assert!(offline.source.is_stale());
        assert_eq!(offline.catalog.plugins[0].name, "rust");
        assert!(offline.source.note().unwrap().contains("dns went away"));

        // A cold cache and no network is an error that names the way out.
        std::fs::remove_file(dir.join(CACHE_FILE)).unwrap();
        let err = load_catalog(&store, |_| bail!("dns went away")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(CATALOG_URL), "{msg}");
        assert!(
            msg.contains("drsg plugin install <file.wasm | url>"),
            "{msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_downloaded_artifact_is_held_to_the_catalogs_hash() {
        let mut e = entry("rust", "1.4.1", None, None);
        e.sha256 = super::super::registry::hex_sha256(b"the real artifact");
        assert!(e.verify(b"the real artifact").is_ok());
        let err = format!("{:#}", e.verify(b"something else").unwrap_err());
        assert!(err.contains("does not match the hash"), "{err}");
        assert!(err.contains("was not installed"), "{err}");
    }

    #[test]
    fn a_fresh_cache_is_served_without_asking_the_network() {
        let dir = std::env::temp_dir().join(format!("drsg-catalog-ttl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PluginStore::open(dir.clone()).unwrap();
        let body = br#"{"schema":1,"plugins":[{"name":"rust","version":"1.4.1","claims":".rs","url":"https://example.invalid/rust.wasm","sha256":"ab"}]}"#;
        load_catalog(&store, |_| Ok(body.to_vec())).unwrap();

        // The fetch would panic if it were called.
        let got = load_catalog_within(&store, Duration::from_secs(3600), |_| {
            panic!("a fresh cache must not hit the network")
        })
        .unwrap();
        assert_eq!(got.catalog.plugins[0].name, "rust");
        // Cached, but not *stale*: nothing failed and nothing is out of date.
        assert!(!got.source.is_stale());
        assert!(got.source.note().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A body that is not a catalog must not overwrite a good cache — the
    /// classic captive-portal failure, where the fetch "succeeds" with a
    /// login page.
    #[test]
    fn a_junk_response_falls_back_rather_than_poisoning_the_cache() {
        let dir = std::env::temp_dir().join(format!("drsg-catalog-junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = PluginStore::open(dir.clone()).unwrap();
        let body = br#"{"schema":1,"plugins":[{"name":"rust","version":"1.4.1","claims":".rs","url":"https://example.invalid/rust.wasm","sha256":"ab"}]}"#;
        load_catalog(&store, |_| Ok(body.to_vec())).unwrap();

        let got = load_catalog(&store, |_| Ok(b"<html>sign in</html>".to_vec())).unwrap();
        assert!(got.source.is_stale());
        assert_eq!(got.catalog.plugins[0].version, "1.4.1");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
