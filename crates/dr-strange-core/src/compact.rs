//! Compact text renderings for token-priced consumers — the agent surface.
//!
//! A model reading a tool result pays per token and parses nothing for free.
//! JSON with `$desc`/`$value` wrappers is built for programs; these renderers
//! answer the same questions as one fact per line, directly readable, with
//! vectors and provenance elided. They back the task-shaped MCP tools and
//! the `drsg find`/`callers`/`callees`/`describe` CLI verbs, so every agent
//! surface speaks one format.
//!
//! Symbol arguments resolve leniently: an exact external key first, else a
//! unique `…::name` / `…​.name` suffix, else a unique substring — and an
//! ambiguous name returns the candidates instead of an answer, which is
//! itself the useful reply ("which one did you mean") for one more call.

use crate::api::{Database, PlaneHandle};
use crate::attach::{Attachment, Attachments, Remote};
use crate::compute::catalog::key_prefix;
use crate::compute::expr::LogicOp;
use crate::compute::plan::Step as PlanStep;
use crate::compute::{Expr, LogicalPlan, Source};
use crate::error::Result;
use crate::types::{NodeId, NodeRecord, PropValue, Properties};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// How a lenient symbol lookup ended.
pub enum Resolved {
    One(NodeRecord),
    /// Several keys matched; the caller renders them as candidates.
    Many(Vec<NodeRecord>),
    None,
}

/// Longest value printed for any single property before truncation.
const PROP_CAP: usize = 500;
/// Candidate cap for fuzzy listings.
const FIND_CAP: usize = 20;

pub(crate) fn prop_str<'a>(props: &'a Properties, key: &str) -> Option<&'a str> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::Str(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

pub(crate) fn prop_int(props: &Properties, key: &str) -> Option<i64> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::Int(i)) => Some(*i),
        _ => None,
    }
}

/// `file:line` when the node carries them (`path` is the file-level node's
/// spelling of `file`).
fn site(props: &Properties) -> String {
    let file = prop_str(props, "file").or_else(|| prop_str(props, "path"));
    match (file, prop_int(props, "line")) {
        // The extent where a parser recorded one: `src/api.rs:10-25` is where
        // a reader learns a function is fifteen lines, or two hundred,
        // without opening it. An end at or before the start says nothing new.
        (Some(f), Some(l)) => match prop_int(props, "end_line") {
            Some(end) if end > l => format!("{f}:{l}-{end}"),
            _ => format!("{f}:{l}"),
        },
        (Some(f), None) => f.to_string(),
        _ => String::new(),
    }
}

fn one_line(n: &NodeRecord) -> String {
    let key = n.external_key.as_deref().unwrap_or("<keyless>");
    let label = n.labels.first().map(String::as_str).unwrap_or("");
    let site = site(&n.properties);
    let mut s = format!("{key}  {label}");
    if !site.is_empty() {
        s.push_str("  ");
        s.push_str(&site);
    }
    s
}

/// Resolve `name` against the plane's external keys, leniently.
pub fn resolve(plane: &PlaneHandle<'_>, name: &str) -> Result<Resolved> {
    if let Some(n) = plane.node_by_key(name)? {
        return Ok(Resolved::One(n));
    }
    let mut suffix: Vec<NodeRecord> = Vec::new();
    let mut contains: Vec<NodeRecord> = Vec::new();
    let lowered = name.to_lowercase();
    for n in plane.query().scan_all().nodes()? {
        let Some(key) = n.external_key.as_deref() else {
            continue;
        };
        if key.ends_with(&format!("::{name}")) || key.ends_with(&format!(".{name}")) {
            suffix.push(n);
        } else if key.to_lowercase().contains(&lowered) {
            contains.push(n);
        }
    }
    let hits = if suffix.is_empty() { contains } else { suffix };
    // An unresolved reference is one end of a call the parser could not bind,
    // recorded once per file that makes it — never a definition, so never an
    // answer to "what is this". Only the definitions decide whether a name is
    // ambiguous: one of them among a dozen stubs is an answer, not a choice.
    // The stubs travel on so the reply can say how many there were.
    let defined = hits.iter().filter(|n| !is_unresolved(n)).count();
    Ok(match (defined, hits.len()) {
        (_, 0) => Resolved::None,
        (1, _) => hits
            .into_iter()
            .find(|n| !is_unresolved(n))
            .map_or(Resolved::None, Resolved::One),
        (0, 1) => hits
            .into_iter()
            .next()
            .map_or(Resolved::None, Resolved::One),
        _ => Resolved::Many(hits),
    })
}

/// The label a plugin gives a reference it recorded but could not bind to a
/// definition — a placeholder for one end of a call, not a symbol.
pub const UNRESOLVED_LABEL: &str = "UnresolvedRef";

/// Whether this node is an unresolved reference rather than a definition.
pub fn is_unresolved(n: &NodeRecord) -> bool {
    n.labels.iter().any(|l| l == UNRESOLVED_LABEL)
}

/// The one-call reply to an ambiguous name: the candidates, one per line, so
/// the exact-key retry costs one call. Public because every verb that
/// resolves a name — in this crate and in the MCP layer — answers ambiguity
/// the same way.
///
/// Unresolved references are counted and named as a group rather than listed:
/// the same name unbound in eleven files is one fact about the parse, and
/// eleven lines of it crowd out the definitions that can actually be picked.
pub fn candidates(name: &str, hits: &[NodeRecord]) -> String {
    let (real, unresolved): (Vec<_>, Vec<_>) = hits.iter().partition(|n| !is_unresolved(n));
    if real.is_empty() {
        return unresolved_only(name, &unresolved);
    }
    let mut out = format!(
        "`{name}` is ambiguous — {} matches; call again with an exact key:\n",
        real.len()
    );
    for n in real.iter().take(FIND_CAP) {
        out.push_str(&one_line(n));
        out.push('\n');
    }
    if real.len() > FIND_CAP {
        out.push_str(&format!("… and {} more\n", real.len() - FIND_CAP));
    }
    if !unresolved.is_empty() {
        out.push_str(&format!(
            "note: {} further match(es) are unresolved references to this name — \
             calls this parse could not bind, one per file, with no definition to \
             read. Nothing to pick among them.\n",
            unresolved.len()
        ));
    }
    out
}

/// The reply when every match is an unresolved reference: the name is called
/// and never defined in this plane, which is an answer and not an ambiguity.
fn unresolved_only(name: &str, hits: &[&NodeRecord]) -> String {
    let mut out = format!(
        "`{name}` matches {} unresolved reference(s) in this plane and no \
         definition — it is called here, but what it names is not parsed into \
         this graph (a dependency, a macro's output, or a receiver this parse \
         could not type). The call sites:\n",
        hits.len()
    );
    for n in hits.iter().take(FIND_CAP) {
        out.push_str(&one_line(n));
        out.push('\n');
    }
    if hits.len() > FIND_CAP {
        out.push_str(&format!("… and {} more\n", hits.len() - FIND_CAP));
    }
    out.push_str("next: `grep` finds the text; `context` on the caller shows the edge\n");
    out
}

/// The property a full rebuild stamps on the plane while it refills it, and
/// clears when it finishes. Written by the digest side; read here.
pub const REBUILDING_PROP: &str = "rebuilding_since";

/// The property the digest writes with its account of what it read. Written by
/// the digest side (`dr-strange-llm`, which depends on this crate and not the
/// other way round); read here, which is why the property is the interface
/// rather than a call.
pub const LEDGER_PROP: &str = "ledger";

/// The plane property saying the installed plugins no longer match what built
/// this plane. Written by `dr-strange-llm`, which can see both sides; the
/// property is the interface between them, as [`LEDGER_PROP`] already is.
pub const DRIFT_PROP: &str = "plugin_drift";

/// The commit HEAD was at when a plane's facts were parsed, and the directory
/// they were parsed from.
///
/// Named here because this crate is what *reads* them, and because more than
/// one crate writes them: the CLI on a digest or fold, and anything that
/// rebuilds a plane — which drops it, and so has to put them back.
pub const SYNC_COMMIT_PROP: &str = "synced_commit";
pub const SYNC_ROOT_PROP: &str = "synced_root";

/// What the digest could not read, stated wherever an answer is read — and
/// only when there was something.
///
/// A graph says "nothing" the same way whether nothing is there or nothing was
/// ever parsed, and only the second is a reason to go and look at the source.
/// When the last ingest skipped files or met an extension no plugin claims,
/// every miss in this plane is ambiguous in that way, and a reader deserves to
/// know before concluding. When it read everything — the ordinary case — this
/// is silent, on the same terms as [`rebuilding_note`]: a line that says
/// "nothing unusual" on every answer is noise.
/// The plugin set changed after this plane was built (issue #39).
///
/// Written by the layer that can see both sides — this crate cannot ask what is
/// installed — and rendered here so it reaches every answer, not only a status
/// command. An incremental fold parses the files one commit touched, so after a
/// plugin lands the graph is parsed by the new set where commits have arrived
/// and by the old set everywhere else. "Half the calls resolve" is the symptom;
/// this is the cause, and a rebuild is the cure.
fn plugin_drift_note(props: &Properties) -> Option<String> {
    let what = props.get(DRIFT_PROP)?.value.as_text()?;
    Some(format!(
        "plugins changed since this plane was built ({what}) — files the \
         difference claims are parsed only where a later commit touched them; \
         rebuild to make the graph whole\n"
    ))
}

fn ledger_note(props: &Properties) -> Option<String> {
    let PropValue::Map(led) = &props.get(LEDGER_PROP)?.value else {
        return None;
    };
    let skipped = match led.get("skipped").map(|d| &d.value) {
        Some(PropValue::Int(n)) => *n,
        _ => 0,
    };
    let unclaimed: Vec<String> = match led.get("unclaimed").map(|d| &d.value) {
        Some(PropValue::List(items)) => items
            .iter()
            .filter_map(|v| v.as_text().map(|t| t.into_owned()))
            .collect(),
        _ => Vec::new(),
    };
    if skipped == 0 && unclaimed.is_empty() {
        return None;
    }
    let mut what = Vec::new();
    if skipped > 0 {
        what.push(format!("{skipped} file(s) it could not read"));
    }
    if !unclaimed.is_empty() {
        what.push(format!("{} that no plugin claims", unclaimed.join(", ")));
    }
    Some(format!(
        "note: the ingest behind this plane left {} — a miss here may mean \
         \"never parsed\" rather than \"not present\".\n",
        what.join(", and ")
    ))
}

/// A rebuild in flight, stated wherever an answer is read.
///
/// A full resync drops the plane and refills it, so in between a query meets a
/// plane that is not wrong but *incomplete* — and an incomplete plane answers
/// "nothing matches" in exactly the words it uses for a symbol that genuinely
/// is not there. Saying so is the whole point: silence here is the one failure
/// mode a caller cannot detect. A marker left behind by a rebuild that died
/// halfway deserves the same warning, which is why this is persisted rather
/// than held in the rebuilding process.
fn rebuilding_note(props: &Properties) -> Option<String> {
    let since = match props.get(REBUILDING_PROP).map(|d| &d.value) {
        Some(PropValue::Int(t)) => *t,
        _ => return None,
    };
    let ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 - since)
        .unwrap_or(0)
        .max(0);
    Some(format!(
        "note: this plane is being rebuilt (started {ago}s ago) — it holds only \
         what has been folded so far, so a miss here may mean \"not yet\" \
         rather than \"not present\"; ask again when it finishes.\n"
    ))
}

/// Freshness, stated where the answer is read: a watched plane records the
/// commit it last folded, and an agent holding a newer checkout should know
/// the graph may lag it. Silence means the plane records no sync point (a
/// plain digest), where staleness is simply the digest's age — unless a
/// rebuild is in flight, which outranks it and is said instead.
/// What to add after the commit a plane names, when the tree it was parsed from
/// did not match that commit (issue #38).
///
/// The facts come from the working tree, so a plane can name a commit and hold a
/// symbol from a file that commit never contained. Saying the number is the
/// difference between a reader trusting the label and a reader knowing what it
/// is worth.
///
/// Silence means agreement — or a plane written before these were recorded,
/// which is the same silence and deliberately so: inventing "clean" for a plane
/// that never measured would be the overclaim this exists to end.
fn drift_note(props: &crate::Properties) -> String {
    let count = |k: &str| match props.get(k).map(|d| &d.value) {
        Some(crate::PropValue::Int(n)) => Some(*n),
        _ => None,
    };
    let (modified, untracked) = (count("synced_modified"), count("synced_untracked"));
    let mut parts = Vec::new();
    if let Some(n) = modified.filter(|n| *n > 0) {
        parts.push(format!("{n} modified"));
    }
    if let Some(n) = untracked.filter(|n| *n > 0) {
        parts.push(format!("{n} untracked"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        " — parsed from a working tree with {}, so the graph holds what that \
         commit does not",
        parts.join(" and ")
    )
}

fn synced_note(plane: &PlaneHandle<'_>) -> Result<Option<String>> {
    let props = plane.properties()?;
    if let Some(note) = rebuilding_note(&props) {
        return Ok(Some(note));
    }
    // A plane can be both in sync and incomplete: the commit says *when* it
    // was parsed, the ledger says what the parse could not read, and a reader
    // weighing a miss needs the second more than the first.
    let synced = props.get(SYNC_COMMIT_PROP).and_then(|d| match &d.value {
        crate::PropValue::Str(commit) => Some(format!(
            "synced: commit {}{}\n",
            &commit[..12.min(commit.len())],
            drift_note(&props)
        )),
        _ => None,
    });
    let notes: Vec<String> = [synced, plugin_drift_note(&props), ledger_note(&props)]
        .into_iter()
        .flatten()
        .collect();
    Ok((!notes.is_empty()).then(|| notes.concat()))
}

/// What to say when a lenient lookup found nothing.
///
/// Its whole job is to keep "not present" and "not yet folded" apart; every
/// verb routes its empty case through here so no surface can quietly report
/// absence during a rebuild. It also names the key prefixes the plane does
/// hold, because a lookup that guessed the prefix wrong — a TS key is prefixed
/// with a `package.json` name, not the directory — is otherwise
/// indistinguishable from one that named nothing at all (issue #40).
pub fn no_match(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let mut out = format!("no symbol matches `{name}` in this plane\n");
    let props = plane.properties()?;
    if let Some(note) = rebuilding_note(&props) {
        out.push_str(&note);
    }
    // For the same reason a rebuild is mentioned here: a plugin that landed
    // after the build leaves the files it claims unparsed except where a commit
    // has since touched them, so "no symbol matches" may mean "never read"
    // rather than "not there" (issue #39). This is the answer that most needs
    // the distinction.
    if let Some(note) = plugin_drift_note(&props) {
        out.push_str(&note);
    }
    out.push_str(&miss_hint(plane, name)?);
    Ok(out)
}

/// A key's own last path segment — what a re-export offers under a shorter
/// path, and the one part of two spellings of a single symbol that agrees.
pub fn leaf(key: &str) -> &str {
    let tail = key.rsplit("::").next().unwrap_or(key);
    tail.rsplit('.').next().unwrap_or(tail)
}

/// Whether this node stands in for something the tree does not declare — the
/// `External` label a plugin carries alongside whatever kind it proved.
pub fn is_stand_in(n: &NodeRecord) -> bool {
    n.labels.iter().any(|l| l == EXTERNAL_LABEL)
}

/// Definitions in this plane whose own last segment is `wanted`, excluding
/// `besides` — what a shorter path was probably reaching for.
fn declarations_named(
    plane: &PlaneHandle<'_>,
    wanted: &str,
    besides: &str,
) -> Result<Vec<NodeRecord>> {
    let mut found: Vec<NodeRecord> = plane
        .query()
        .scan_all()
        .nodes()?
        .into_iter()
        .filter(|n| {
            !is_stand_in(n)
                && !is_unresolved(n)
                && n.external_key
                    .as_deref()
                    .is_some_and(|k| k != besides && leaf(k) == wanted)
        })
        .collect();
    found.sort_by(|a, b| a.external_key.cmp(&b.external_key));
    Ok(found)
}

/// What a miss is worth saying: where the name is declared under a longer key,
/// or failing that the prefixes this plane's keys do carry.
///
/// The first case is a re-export. `pub use capability::Grant` makes
/// `ip_core::Grant` a path the source writes and the compiler accepts, while
/// this graph keys a symbol by where it is defined — so the prefix was right,
/// the middle was elided, and listing the prefixes answers a question nobody
/// asked. In a workspace whose every `lib.rs` re-exports its types, that is
/// the common miss, not the rare one.
///
/// Reads keys only: the catalog would answer the prefix half too, but it scans
/// every node *and* every edge to build label and connectivity statistics,
/// which is not a price a mistyped name should pay.
fn miss_hint(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    // A path that ends in something declared here under a longer key: say
    // where, and stop — the prefix listing below is noise once the answer is
    // in hand. A name with a `/` in it is a file path, not a symbol path, and
    // its last segment is an extension: `src/zzz.rs` must not go looking for
    // everything whose key ends in `rs`.
    if !name.contains('/') && (name.contains("::") || name.contains('.')) {
        let declared = declarations_named(plane, leaf(name), name)?;
        if !declared.is_empty() {
            let mut out = format!(
                "`{}` is declared here under a longer key — a re-export makes both \
                 paths valid in source, and this graph keys a symbol by where it is \
                 declared:\n",
                leaf(name)
            );
            for n in declared.iter().take(FIND_CAP) {
                out.push_str("  ");
                out.push_str(&one_line(n));
                out.push('\n');
            }
            if declared.len() > FIND_CAP {
                out.push_str(&format!("  … and {} more\n", declared.len() - FIND_CAP));
            }
            return Ok(out);
        }
    }
    let nodes = plane.query().scan_all().nodes()?;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for node in &nodes {
        if let Some(key) = node.external_key.as_deref()
            && let Some(prefix) = key_prefix(key)
        {
            seen.insert(prefix);
        }
    }
    if seen.is_empty() {
        return Ok(String::new());
    }
    let shown: Vec<&str> = seen.iter().take(FIND_CAP).copied().collect();
    let mut out = format!("keys here are prefixed: {}\n", shown.join(", "));
    if seen.len() > FIND_CAP {
        out.push_str(&format!(
            "… and {} more prefix(es)\n",
            seen.len() - FIND_CAP
        ));
    }
    match key_prefix(name) {
        Some(guessed) if !seen.contains(guessed) => {
            let rest = name
                .strip_prefix(guessed)
                .map(|r| r.trim_start_matches('/').trim_start_matches("::"))
                .unwrap_or(name);
            out.push_str(&format!(
                "`{guessed}` is not one of them; a fragment matches anywhere in a key — try `{rest}`\n"
            ));
        }
        _ => out.push_str("a fragment matches anywhere in a key\n"),
    }
    Ok(out)
}

/// One symbol's description and the caveats that belong to it before any edge
/// is listed: how fresh the plane is, how the name was routed to this node,
/// and whether the node is a stand-in for a declaration elsewhere.
fn head(plane: &PlaneHandle<'_>, node: &NodeRecord, name: &str) -> Result<String> {
    let mut out = describe_record(plane, node)?;
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    if let Some(note) = route_note(node, name) {
        out.push_str(&note);
    }
    if let Some(note) = stand_in_note(plane, node)? {
        out.push_str(&note);
    }
    Ok(out)
}

/// Where a stand-in's declaration is, when this plane holds one.
///
/// A `pub use` makes two paths name one symbol, and a plugin that saw only the
/// short one minted a stand-in for it: labelled, edged, and empty. Answering
/// about that node without saying so describes a type with no source, no doc
/// and almost no edges — the shape of a dead symbol rather than of a
/// re-export, and the reason an `impact` on it reads zero.
fn stand_in_note(plane: &PlaneHandle<'_>, node: &NodeRecord) -> Result<Option<String>> {
    if !is_stand_in(node) {
        return Ok(None);
    }
    let key = node.external_key.as_deref().unwrap_or_default();
    let declared = declarations_named(plane, leaf(key), key)?;
    if declared.is_empty() {
        return Ok(None);
    }
    let mut out = String::from(
        "note: this key is a stand-in — nothing in this tree declares it, so it \
         carries no source and only the edges that named it. Counts taken from \
         here, `impact` included, are about the stand-in and not the declaration. \
         The declaration is in this plane; ask about it instead:\n",
    );
    for n in declared.iter().take(FIND_CAP) {
        out.push_str("  ");
        out.push_str(&one_line(n));
        out.push('\n');
    }
    if declared.len() > FIND_CAP {
        out.push_str(&format!("  … and {} more\n", declared.len() - FIND_CAP));
    }
    Ok(Some(out))
}

/// The honesty footer every call listing carries: what a recorded edge set
/// can and cannot claim.
const CALLS_NOTE: &str = "note: recorded call edges only — calls the parser could not resolve \
     (dynamic dispatch, untyped receivers) are absent, so this is a lower \
     bound.\n";

/// The same footer, plus how the edges above were bound.
///
/// A plugin records `_confidence` on every call edge it resolves, and until
/// now nothing said it: an edge bound by matching a bare name against what is
/// in scope read exactly like one the source spelled out as a path. That
/// difference is what a reader weighing a listing needs, and it is one
/// sentence rather than a marker on a quarter of the lines — the ordinary
/// case deserves no decoration.
fn calls_note(shown: usize, by_name: usize) -> String {
    if by_name == 0 {
        return CALLS_NOTE.to_string();
    }
    format!(
        "{}  Of the {shown} call(s) shown, {by_name} {} bound by matching a name \
         against what is in scope rather than a path the source wrote.\n",
        CALLS_NOTE.trim_end(),
        if by_name == 1 { "was" } else { "were" },
    )
}

/// Most entries printed per edge group before eliding with a count.
const GROUP_CAP: usize = 20;

/// Ceiling on a whole `context` reply, in characters. A hub node (a module
/// containing hundreds of symbols, a base type everything calls) must not
/// flood the caller's context window: when the full rendering exceeds this,
/// the per-group cap shrinks until it fits, and every elision names the
/// count it hides.
const CONTEXT_BUDGET: usize = 24_000;

// ---- the walk under every verb -------------------------------------------

/// What a walk does with one hop it has been offered.
enum Step {
    /// Follow it: an unseen node joins the region and the next frontier.
    Take,
    /// Leave it out — a filtered edge type, or a node the region already holds.
    Skip,
    /// The walk has what it came for; end it here. The hop's own level is left
    /// unfinished, so a caller that stops reads its answer out of what it kept
    /// along the way, not out of the region.
    Stop,
}

/// One hop, as [`walk`] offers it: the frontier node it was walked from, the
/// direction it was walked in, and what sits on the other end.
///
/// The edge arrives as an id rather than a record because not every caller
/// reads it, and one lookup per hop is most of what a walk costs.
struct Hop {
    from: crate::NodeId,
    dir: crate::Dir,
    node: crate::NodeId,
    edge: crate::EdgeId,
    /// False when the region already holds `node`. Such a hop is still offered,
    /// because a repeated node can arrive along an edge nothing has counted yet.
    fresh: bool,
}

/// Where a walk stops.
struct Bounds {
    /// Directions swept at every node; each hop is reported under the one it
    /// was walked in.
    dirs: &'static [crate::Dir],
    /// Edge type to follow, filtered by the store; `None` sweeps every type.
    ty: Option<&'static str>,
    /// Hops from the seed.
    depth: usize,
    /// Nodes held before the walk stops taking more — and says which bound
    /// stopped it. [`usize::MAX`] for a walk bounded by depth alone.
    budget: usize,
}

/// What a walk took.
struct Region {
    /// Nodes taken per hop: `levels[0]` is one hop from the seed. The seed is
    /// not in it — it is where the walk started, not something it found.
    levels: Vec<Vec<crate::NodeId>>,
    /// Every node held, the seed included.
    seen: std::collections::BTreeSet<crate::NodeId>,
    /// The budget refused at least one node: the region is the nearest part of
    /// a larger one, and a caller that states its bounds must say so.
    budget_hit: bool,
}

/// The one bounded breadth-first walk under [`context`], [`trace`], [`impact`]
/// and [`fathom`].
///
/// Those four differ in what they make of a hop, not in how they reach it, so
/// what they share lives here: the seen set, the frontier, breadth-first order
/// (which is what makes a truncated region the *nearest* one), and the budget
/// that stops a hub from dragging in the plane. Each passes its [`Bounds`] and
/// a visitor.
///
/// The visitor is offered every hop, repeats included, and answers with a
/// [`Step`]. Taking a node is a request the budget can still refuse — which is
/// why a caller counting the region reads it back from [`Region`] rather than
/// tallying as it visits.
fn walk(
    plane: &PlaneHandle<'_>,
    seed: crate::NodeId,
    bounds: Bounds,
    mut visit: impl FnMut(&Hop) -> Result<Step>,
) -> Result<Region> {
    let mut levels: Vec<Vec<crate::NodeId>> = Vec::new();
    let mut seen = std::collections::BTreeSet::from([seed]);
    let mut budget_hit = false;
    let root = [seed];
    'walk: for hop_no in 0..bounds.depth {
        let frontier: &[crate::NodeId] = if hop_no == 0 {
            &root
        } else {
            &levels[hop_no - 1]
        };
        let mut next = Vec::new();
        for &from in frontier {
            for &dir in bounds.dirs {
                for nb in plane.neighbors(from, dir, bounds.ty)? {
                    let hop = Hop {
                        from,
                        dir,
                        node: nb.node,
                        edge: nb.edge,
                        fresh: !seen.contains(&nb.node),
                    };
                    match visit(&hop)? {
                        Step::Take => {}
                        Step::Skip => continue,
                        Step::Stop => break 'walk,
                    }
                    if !hop.fresh {
                        continue;
                    }
                    if seen.len() >= bounds.budget {
                        budget_hit = true;
                        continue;
                    }
                    seen.insert(hop.node);
                    next.push(hop.node);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        levels.push(next);
    }
    Ok(Region {
        levels,
        seen,
        budget_hit,
    })
}

/// Both directions, for the verbs that read a neighborhood rather than follow
/// a flow.
const BOTH_WAYS: &[crate::Dir] = &[crate::Dir::Out, crate::Dir::In];
/// Outward only — the direction a call flows.
const OUTWARD: &[crate::Dir] = &[crate::Dir::Out];
/// Inward only — the direction a change propagates.
const INWARD: &[crate::Dir] = &[crate::Dir::In];

/// `context` — the primary agent verb: one symbol's whole neighborhood in a
/// single round trip. The head and properties are [`describe`]'s; then the
/// graph around it — who contains it, who calls it (with call sites), what
/// it calls, and every other edge type grouped — so callers/callees/what-is
/// questions are all answered by this one call, and the agent never has to
/// choose among narrower verbs. Last, and only where it has anything to say,
/// what [`spanned`] finds: the nodes whose lines hold this one and the nodes
/// its lines hold, among those no edge relates — position, labelled as
/// position. Ambiguity returns the candidate list, which is itself the
/// useful one-call reply.
/// What the `by span` sections are, said once wherever one appears.
const SPAN_NOTE: &str = "note: `by span` is position — same file, lines inside — \
     and not an edge any parser wrote. It lists only what no edge relates, \
     which is where two parsers described the same lines and neither could \
     name the other's key.\n";

/// The lines a node covers in one file.
struct Span<'a> {
    file: &'a str,
    from: i64,
    to: i64,
}

impl<'a> Span<'a> {
    /// A node's span, when it records one.
    fn of(props: &'a Properties) -> Option<Self> {
        let file = prop_str(props, "file")?;
        let from = prop_int(props, "line")?;
        Some(Self {
            file,
            from,
            to: prop_int(props, "end_line").unwrap_or(from).max(from),
        })
    }

    /// Whether this span holds `other` and is wider than it. Two parsers
    /// describing the same lines is not containment either way.
    fn holds(&self, other: &Span<'_>) -> bool {
        self.file == other.file
            && self.from <= other.from
            && self.to >= other.to
            && (self.from < other.from || self.to > other.to)
    }
}

/// Edge groups as titled sections, in fixed order: the named four first, then
/// whatever edge vocabulary remains (IMPLEMENTS, EXTENDS, HAS_METHOD,
/// IMPORTS, …) under its own name, direction marked.
fn titled(mut groups: BTreeMap<(&'static str, String), Vec<String>>) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for (title, (dir, ty)) in [
        ("contained by", ("in", "CONTAINS")),
        ("callers", ("in", "CALLS")),
        ("callees", ("out", "CALLS")),
        ("contains", ("out", "CONTAINS")),
    ] {
        if let Some(lines) = groups.remove(&(dir, ty.to_string())) {
            sections.push((title.to_string(), lines));
        }
    }
    for ((dir, ty), lines) in groups {
        let arrow = if dir == "in" { "←" } else { "→" };
        sections.push((format!("{ty} {arrow}"), lines));
    }
    sections
}

/// What this node's lines hold, and what holds them, among the nodes no edge
/// relates it to.
///
/// Position is not an edge, and this is the only answer `context` reads from
/// it. The reason it has to: a plugin can mint keys only for what it parses,
/// and an edge onto a key no node declares fails the entire write — so a
/// plugin that reads an SQL statement sitting inside a Rust function cannot
/// say so, however plainly the two overlap. Both sides record `file`, `line`
/// and `end_line`, which is enough to tell the reader. A pair some edge
/// already relates is left to that edge, so this stays silent for code one
/// parser read on its own and speaks where two parsers met.
fn spanned(
    plane: &PlaneHandle<'_>,
    node: &NodeRecord,
    related: &BTreeSet<NodeId>,
) -> Result<(Vec<String>, Vec<String>)> {
    let Some(span) = Span::of(&node.properties) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let (mut holds, mut inside) = (Vec::new(), Vec::new());
    for other in plane
        .query()
        .scan_all()
        .filter(Expr::Property("file".into()).eq(span.file))
        .nodes()?
    {
        if other.id == node.id || related.contains(&other.id) {
            continue;
        }
        let Some(theirs) = Span::of(&other.properties) else {
            continue;
        };
        if span.holds(&theirs) {
            holds.push((theirs.from, theirs.to, one_line(&other)));
        } else if theirs.holds(&span) {
            inside.push((theirs.from, theirs.to, one_line(&other)));
        }
    }
    Ok((ordered(holds), ordered(inside)))
}

/// Spans in reading order: where they start, then the wider one first, so a
/// nesting reads outermost down.
fn ordered(mut spans: Vec<(i64, i64, String)>) -> Vec<String> {
    spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    spans.into_iter().map(|(_, _, line)| line).collect()
}

pub fn context(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let node = match resolve(plane, name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(name, &hits)),
        Resolved::None => return no_match(plane, name),
    };
    let out = head(plane, &node, name)?;

    // Group every edge by (direction, type). CONTAINS-in is rendered as the
    // parent ("contained by"); CALLS carry their call-site lines.
    //
    // One hop, and nothing taken: the neighborhood *is* the answer here, so
    // the walk is swept for its hops alone and no region is built.
    let mut groups: BTreeMap<(&'static str, String), Vec<String>> = BTreeMap::new();
    let mut had_calls = false;
    // How the shown call edges were bound: everything, and the ones that rest
    // on a name matched in scope. Counted over what is rendered, so the note
    // agrees with the listing above it.
    let (mut calls_shown, mut calls_by_name) = (0usize, 0usize);
    // Everything an edge already reaches, so the span sections can say only
    // what no edge does.
    let mut related: BTreeSet<NodeId> = BTreeSet::new();
    walk(
        plane,
        node.id,
        Bounds {
            dirs: BOTH_WAYS,
            ty: None,
            depth: 1,
            budget: usize::MAX,
        },
        |hop| {
            let (Some(other), Some(edge)) = (plane.node(hop.node)?, plane.edge(hop.edge)?) else {
                return Ok(Step::Skip);
            };
            related.insert(hop.node);
            let tag = if hop.dir == crate::Dir::In {
                "in"
            } else {
                "out"
            };
            let mut line = one_line(&other);
            if edge.ty == "CALLS" {
                had_calls = true;
                calls_shown += 1;
                // `none` is the unresolved ledger, which already announces
                // itself with a reason below; `medium` is the one that looks
                // exactly like certainty and is not.
                if prop_str(&edge.properties, "_confidence") == Some("medium") {
                    calls_by_name += 1;
                }
                if let Some(l) = prop_int(&edge.properties, "line") {
                    line.push_str(&format!("  call@{l}"));
                }
                if hop.node == node.id {
                    line.push_str("  (self)");
                }
                // Where control went. Every code plugin that can tell records
                // it on the edge — `go` for a goroutine, `scheduled` /
                // `unawaited` for a promise or coroutine left running,
                // `spawned` / `blocking` for a Rust task — and a reader who
                // cannot tell that call from a synchronous one is reading a
                // different program, which is the whole reason it is written
                // down.
                if let Some(shape) = prop_str(&edge.properties, "concurrent") {
                    line.push_str(&format!("  [{shape}]"));
                }
                // The unresolved ledger (P1): a boundary is announced, not
                // hidden — the reason travels on the edge.
                if is_unresolved(&other)
                    && let Some(reason) = prop_str(&edge.properties, "_reason")
                {
                    line.push_str(&format!("  [{reason}]"));
                }
            }
            groups.entry((tag, edge.ty.clone())).or_default().push(line);
            Ok(Step::Skip)
        },
    )?;

    let mut sections = titled(groups);

    // Last, and marked: a reader who cannot tell an edge from an overlap is
    // reading a claim the graph never made.
    let (holds, inside) = spanned(plane, &node, &related)?;
    let had_span = !holds.is_empty() || !inside.is_empty();
    for (title, lines) in [("inside (by span)", inside), ("holds (by span)", holds)] {
        if !lines.is_empty() {
            sections.push((title.to_string(), lines));
        }
    }

    let render = |cap: usize| -> String {
        let mut buf = out.clone();
        for (title, lines) in &sections {
            buf.push_str(&format!("{title} ({}):\n", lines.len()));
            for l in lines.iter().take(cap) {
                buf.push_str("  ");
                buf.push_str(l);
                buf.push('\n');
            }
            if lines.len() > cap {
                buf.push_str(&format!("  … and {} more\n", lines.len() - cap));
            }
        }
        if had_calls {
            buf.push_str(&calls_note(calls_shown, calls_by_name));
        }
        if had_span {
            buf.push_str(SPAN_NOTE);
        }
        buf
    };
    // The budget beats the group cap: shrink until it fits (never below 3 —
    // a section reduced past that says nothing worth its lines).
    let mut cap = GROUP_CAP;
    let mut rendered = render(cap);
    while rendered.len() > CONTEXT_BUDGET && cap > 3 {
        cap = (cap / 2).max(3);
        rendered = render(cap);
    }
    Ok(rendered)
}

/// `search` — semantic lookup for questions that name no identifier: cosine
/// top-k over the plane's `embedding` vectors, one hit per line with its
/// score, the best hit expanded to its full [`describe`] block. The query
/// vector comes from the caller (the serving layer embeds the text — core
/// holds no provider).
pub fn search(plane: &PlaneHandle<'_>, query: &[f32], k: u64) -> Result<String> {
    let hits = plane
        .query()
        .vector_top_k(
            None,
            "embedding",
            query.to_vec(),
            crate::Metric::Cosine,
            k.max(1),
        )
        .scored_nodes()?;
    if hits.is_empty() {
        return Ok(
            "no embedded nodes in this plane — `drsg vectorize` builds the vectors\n".to_string(),
        );
    }
    let mut out = String::new();
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    for (n, score) in &hits {
        if let Some(score) = score {
            out.push_str(&format!("{score:.3}  "));
        }
        out.push_str(&one_line(n));
        out.push('\n');
    }
    if let Some((top, _)) = hits.first() {
        out.push_str("\nbest match:\n");
        out.push_str(&describe_record(plane, top)?);
    }
    Ok(out)
}

/// `describe` — one node's content as `prop: value` lines. Vectors and
/// `_`-provenance stay out (only `_generated_by` is kept, as one line —
/// which parser asserted this is worth a reader's glance).
/// `trace` — how `from` reaches `to`: breadth-first over outgoing CALLS
/// edges, the shortest recorded path rendered one hop per line. When the
/// forward direction holds nothing, the reverse is tried and said. Recorded
/// edges only — the honesty note rides every answer.
pub fn trace(plane: &PlaneHandle<'_>, from: &str, to: &str) -> Result<String> {
    let a = match resolve(plane, from)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(from, &hits)),
        Resolved::None => return no_match(plane, from),
    };
    let b = match resolve(plane, to)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(to, &hits)),
        Resolved::None => return no_match(plane, to),
    };
    let mut out = String::new();
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    match calls_path(plane, a.id, b.id)? {
        Some(path) => render_path(plane, &path, &mut out)?,
        None => match calls_path(plane, b.id, a.id)? {
            Some(path) => {
                out.push_str("no forward path; the call flow runs the other way:\n");
                render_path(plane, &path, &mut out)?;
            }
            None => out.push_str(
                "no recorded CALLS path in either direction — the graph holds \
                 resolved edges only, so an unresolved hop (dynamic dispatch, \
                 untyped receiver) breaks the chain; `impact` shows what IS \
                 recorded around each end\n",
            ),
        },
    }
    out.push_str(CALLS_NOTE);
    Ok(out)
}

/// Shortest recorded CALLS path, breadth-first, bounded.
///
/// The walk stops the moment the target is reached, so the predecessor chain
/// it leaves behind is the shortest one: breadth-first order reaches every
/// node by its fewest hops.
fn calls_path(
    plane: &PlaneHandle<'_>,
    from: crate::NodeId,
    to: crate::NodeId,
) -> Result<Option<Vec<crate::NodeId>>> {
    use std::collections::BTreeMap;
    /// Nodes one path search holds before giving up.
    const MAX_VISITED: usize = 20_000;
    if from == to {
        return Ok(Some(vec![from]));
    }
    let mut prev: BTreeMap<crate::NodeId, crate::NodeId> = BTreeMap::new();
    let mut found = false;
    walk(
        plane,
        from,
        Bounds {
            dirs: OUTWARD,
            ty: Some("CALLS"),
            depth: usize::MAX,
            budget: MAX_VISITED,
        },
        |hop| {
            if !hop.fresh {
                return Ok(Step::Skip);
            }
            prev.insert(hop.node, hop.from);
            if hop.node == to {
                found = true;
                return Ok(Step::Stop);
            }
            Ok(Step::Take)
        },
    )?;
    if !found {
        return Ok(None);
    }
    let mut path = vec![to];
    let mut cur = to;
    while let Some(&p) = prev.get(&cur) {
        path.push(p);
        cur = p;
    }
    path.reverse();
    Ok(Some(path))
}

fn render_path(plane: &PlaneHandle<'_>, path: &[crate::NodeId], out: &mut String) -> Result<()> {
    for (i, id) in path.iter().enumerate() {
        let Some(n) = plane.node(*id)? else { continue };
        if i == 0 {
            out.push_str(&one_line(&n));
        } else {
            out.push_str("  -> ");
            out.push_str(&one_line(&n));
        }
        out.push('\n');
    }
    Ok(())
}

// `USES_TYPE` is what makes `impact` answerable *about a type*: without it
// the walk saw only who constructed one, and reported a fraction of the
// radius as if it were the whole of it. `READS` / `WRITES` / `USES_COLUMN`
// are the same statement about a table and a column, which is what "who would
// a schema change break" asks.
//
// Containment is deliberately absent — `CONTAINS` and `HAS_COLUMN` say a
// thing is *part of* another, not that it would break with it.
const IMPACT_EDGES: &[&str] = &[
    "CALLS",
    "REFERENCES",
    "INSTANTIATES",
    "IMPORTS",
    "EXTENDS",
    "IMPLEMENTS",
    "USES_TYPE",
    "READS",
    "WRITES",
    "USES_COLUMN",
];

/// The edge a parse writes when a name is the only evidence: the source names
/// this symbol at a site whose binding could not be proved — a call through a
/// construct the parser does not expand, or a receiver it cannot type.
///
/// Deliberately absent from [`IMPACT_EDGES`]. A radius is what *would* break
/// if a symbol changed, and a candidate is not that; counted in the total it
/// would turn a documented lower bound into a guess. It is reported beside the
/// total instead, which is the whole point of writing it down.
pub const MAY_CALL: &str = "MAY_CALL";

/// What one `impact` call is asked, and how far it may look.
pub struct Impact<'a> {
    /// The plane the question is asked in.
    pub plane: &'a str,
    /// The symbol being changed.
    pub name: &'a str,
    /// Hops of incoming structural edges to walk, clamped to 1..=6.
    pub depth: usize,
    /// Which other graphs to answer in as well. `None` — the default — is
    /// this plane alone, and the answer is then what it has always been.
    ///
    /// Unlike [`wire`], which crosses graphs unasked because a route's two
    /// halves are in different repositories by nature, a radius is complete
    /// within one: adding graphs changes what the total counts, and reading
    /// thirty planes is not something a verb should do uninvited.
    pub across: Option<Scope>,
    /// How to read a graph another drsg serves. `None` leaves those unread
    /// and named as such.
    pub remote: Option<&'a dyn Remote>,
    /// Every database this process serves (issue #44). With one,
    /// [`Scope::All`] spans them — which is what lets a machine handed thirty
    /// repositories answer across them with nothing attached. `None` is this
    /// database's planes alone, which is every embedded caller.
    pub instance: Option<&'a crate::Instance>,
}

/// `impact` — the blast radius: what reaches this symbol, breadth-first over
/// INCOMING structural edges (CALLS, REFERENCES, INSTANTIATES, IMPORTS,
/// EXTENDS, IMPLEMENTS, USES_TYPE, READS, WRITES, USES_COLUMN), grouped by
/// distance, counts always exact even when listings elide.
///
/// [`MAY_CALL`] is not one of them. What a parse could not bind is counted
/// beside the total rather than in it, so a zero radius says whether it is an
/// absence or a boundary — see [`unproven`].
///
/// With [`Impact::across`] set it then says which *other* graphs the radius
/// reaches — the question a shared library asks, where a change is local to
/// one repository and its consequences are not.
pub fn impact(db: &Database, req: Impact<'_>) -> Result<String> {
    const LEVEL_CAP: usize = 20;
    // Taken before the graphs are opened and held for the whole answer: a
    // mounted database is owned by the instance, and a handle borrows it.
    let held = Held::of(db, req.plane, req.across, req.instance)?;
    let mut graphs = Graphs::open(db, req.plane, &held, req.remote)?;
    let plane = &graphs.here;
    let node = match resolve(plane, req.name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(req.name, &hits)),
        Resolved::None => return no_match(plane, req.name),
    };
    let depth = req.depth.clamp(1, 6);
    let mut out = one_line(&node);
    out.push('\n');
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    // A node is named for the edge that first reached it — the shortest way
    // in, since the walk arrives breadth-first.
    let mut lines: std::collections::BTreeMap<crate::NodeId, String> =
        std::collections::BTreeMap::new();
    // Breadth-first order, so a radius too large to carry whole is truncated
    // to the keys nearest the change.
    let mut offered: Vec<String> = node.external_key.iter().cloned().collect();
    let region = walk(
        plane,
        node.id,
        Bounds {
            dirs: INWARD,
            ty: None,
            depth,
            budget: usize::MAX,
        },
        |hop| {
            if !hop.fresh {
                return Ok(Step::Skip);
            }
            let (Some(edge), Some(n)) = (plane.edge(hop.edge)?, plane.node(hop.node)?) else {
                return Ok(Step::Skip);
            };
            if !IMPACT_EDGES.contains(&edge.ty.as_str()) {
                return Ok(Step::Skip);
            }
            if let Some(key) = &n.external_key {
                offered.push(key.clone());
            }
            lines.insert(hop.node, format!("{}  [{}]", one_line(&n), edge.ty));
            Ok(Step::Take)
        },
    )?;

    let mut total = 0usize;
    for (i, level) in region.levels.iter().enumerate() {
        total += level.len();
        out.push_str(&format!("depth {} ({}):\n", i + 1, level.len()));
        for id in level.iter().take(LEVEL_CAP) {
            if let Some(line) = lines.get(id) {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if level.len() > LEVEL_CAP {
            out.push_str(&format!("  … and {} more\n", level.len() - LEVEL_CAP));
        }
    }
    out.push_str(&format!("total affected within depth {depth}: {total}\n"));
    out.push_str(&unproven(plane, &node, total)?);
    out.push_str(CALLS_NOTE);
    if req.across.is_some() {
        let offered = Offered::new(plane, offered)?;
        out.push_str(&across(&mut graphs, &offered)?);
    }
    Ok(out)
}

/// What names this symbol without proving it does: the `MAY_CALL` candidates
/// a parse recorded, and the unresolved references that merely share its name.
///
/// Printed beside a radius and never inside one. The case this exists for is a
/// zero: "nothing reaches this" and "the parse could not tell" are the same
/// number and not the same answer, and nothing in a bare `0` lets a reader
/// tell them apart — so a zero with either of these non-zero says which it is.
fn unproven(plane: &PlaneHandle<'_>, node: &NodeRecord, total: usize) -> Result<String> {
    let candidates = may_call(plane, node)?;

    // Scanned by label, not over the plane: the unresolved refs are a slice of
    // it, and this runs on every radius.
    let wanted = node.external_key.as_deref().map(leaf).unwrap_or_default();
    let by_name = if wanted.is_empty() {
        0
    } else {
        plane
            .query()
            .scan_label(UNRESOLVED_LABEL)
            .nodes()?
            .iter()
            .filter(|n| n.external_key.as_deref().is_some_and(|k| leaf(k) == wanted))
            .count()
    };

    let mut out = String::new();
    if candidates > 0 {
        out.push_str(&format!("may-call candidates: {candidates}\n"));
    }
    if by_name > 0 {
        out.push_str(&format!("unresolved by name: {by_name}\n"));
    }
    if total == 0 && (candidates > 0 || by_name > 0) {
        out.push_str(
            "note: this zero is empty, not negative — nothing *recorded* reaches this \
             symbol, while the lines above are sites that name it and whose binding this \
             parse could not prove. `grep` the name, or `context` a candidate.\n",
        );
    }
    Ok(out)
}

/// The property a plugin writes on a declaration it has evidence is test
/// code; the value names *what was written down* — `attribute` (`#[test]`,
/// `@Test`), `build-rule` (`_test.go`, `#[cfg(test)]`), `filename`,
/// `annotation`, `build-layout`, `framework-include`.
///
/// A verb reads the key to find tests and never branches on the value: the
/// value is provenance for a reader weighing the mark, not a vocabulary to
/// switch over, and a plugin added tomorrow may write a word nothing here
/// knows.
pub const TEST_FLAG_PROP: &str = "test_flag";

/// How much [`TEST_FLAG_PROP`]'s evidence is worth: `definitive`, `strong`
/// or `circumstantial`. The `_` keeps it out of the compact renderers, and
/// [`affected`] is the reader it was written for.
const TEST_CONFIDENCE_PROP: &str = "_test_flag_confidence";

/// The confidence that needs no saying. Anything else is reported, because a
/// test named on circumstantial evidence may not be a test.
const DEFINITIVE: &str = "definitive";

/// What one `affected` call is asked.
pub struct Affected<'a> {
    /// The plane the question is asked in.
    pub plane: &'a str,
    /// Keys of the declarations a change touched, as a caller that read a
    /// diff names them.
    pub seeds: &'a [String],
    /// Hops of incoming structural edges to walk, clamped to 1..=6.
    pub depth: usize,
    /// Path prefixes whose every test joins the set however the walk ran —
    /// the packages the change touched.
    ///
    /// Naming those needs the build manifests, which this layer does not
    /// read: a caller holding them passes them, one that does not passes
    /// nothing and gets the reached set alone.
    pub widen: &'a [String],
    /// Every database this process serves.
    pub instance: Option<&'a crate::Instance>,
}

/// Why a test is in the set — the three are not equally strong, so they are
/// said rather than merged.
enum Why {
    /// The change touched the test itself.
    Changed,
    /// The test reaches a changed symbol, along this edge type.
    Reaches(String),
    /// A package the change touched holds it, and nothing recorded says it
    /// reaches anything that changed.
    Package,
}

impl Why {
    fn as_str(&self) -> &str {
        match self {
            Why::Changed => "changed",
            Why::Reaches(ty) => ty,
            Why::Package => "in a changed package",
        }
    }
}

/// `affected` — the tests a change reaches: every declaration carrying
/// [`TEST_FLAG_PROP`] that reaches one of `seeds` through incoming structural
/// edges, grouped by the file that holds it.
///
/// The composition of [`impact`] and what the plugins already mark, which is
/// why it is a verb rather than a query: the walk is the same one, the seeds
/// come from a diff instead of a name, and the answer is filtered to test
/// code and grouped by what a runner takes.
///
/// **It is a lower bound, and says so.** A call the parse could not bind is
/// not an edge, so a test that reaches the change only through dynamic
/// dispatch is missing from the set — the failure that matters here, since a
/// test wrongly left out is a regression shipped. [`Affected::widen`] is the
/// answer to that: the caller names the packages the change touched and every
/// test under them joins, reached or not.
pub fn affected(db: &Database, req: Affected<'_>) -> Result<String> {
    let held = Held::of(db, req.plane, None, req.instance)?;
    let graphs = Graphs::open(db, req.plane, &held, None)?;
    let plane = &graphs.here;
    let depth = req.depth.clamp(1, 6);
    let mut set = reaching(plane, req.seeds, depth)?;
    let reached = set.found.len();
    if !req.widen.is_empty() {
        for n in plane.query().scan_all().nodes()? {
            if !is_test(&n) || set.found.contains_key(&n.id) {
                continue;
            }
            let Some(file) = file_of(&n) else { continue };
            if req.widen.iter().any(|p| file.starts_with(p.as_str())) {
                set.found.insert(n.id, (n, Why::Package));
            }
        }
    }
    render(plane, &req, &set, depth, reached)
}

/// What a walk from each seed found: the tests, and what it could not settle.
struct Reaching {
    found: std::collections::BTreeMap<crate::NodeId, (NodeRecord, Why)>,
    /// Seeds that named no one declaration, each saying why.
    unsettled: Vec<String>,
    /// Seeds that resolved and were walked.
    read: usize,
    /// Sites naming a seed without proving they reach it.
    candidates: usize,
}

/// Every test reaching one of `seeds` through incoming structural edges.
fn reaching(plane: &PlaneHandle<'_>, seeds: &[String], depth: usize) -> Result<Reaching> {
    let mut found: std::collections::BTreeMap<crate::NodeId, (NodeRecord, Why)> =
        std::collections::BTreeMap::new();
    let mut unsettled: Vec<String> = Vec::new();
    let mut read = 0usize;
    let mut candidates = 0usize;

    for name in seeds {
        let node = match resolve(plane, name)? {
            Resolved::One(n) => n,
            Resolved::Many(hits) => {
                unsettled.push(format!(
                    "`{name}` names {} declarations here — nothing was walked from it",
                    hits.len()
                ));
                continue;
            }
            Resolved::None => {
                unsettled.push(format!("`{name}` is no declaration this plane holds"));
                continue;
            }
        };
        read += 1;
        // A test the change edited is a test to run. Nothing needs to reach
        // it, and nothing would: a test is a leaf of this walk.
        if is_test(&node) {
            found.insert(node.id, (node.clone(), Why::Changed));
        }
        walk(
            plane,
            node.id,
            Bounds {
                dirs: INWARD,
                ty: None,
                depth,
                budget: usize::MAX,
            },
            |hop| {
                if !hop.fresh {
                    return Ok(Step::Skip);
                }
                let (Some(edge), Some(n)) = (plane.edge(hop.edge)?, plane.node(hop.node)?) else {
                    return Ok(Step::Skip);
                };
                if !IMPACT_EDGES.contains(&edge.ty.as_str()) {
                    return Ok(Step::Skip);
                }
                if is_test(&n) {
                    found
                        .entry(hop.node)
                        .or_insert_with(|| (n, Why::Reaches(edge.ty.clone())));
                }
                Ok(Step::Take)
            },
        )?;
        candidates += may_call(plane, &node)?;
    }
    Ok(Reaching {
        found,
        unsettled,
        read,
        candidates,
    })
}

/// The set as a reader takes it: grouped by file, and followed by everything
/// the walk could not see.
fn render(
    plane: &PlaneHandle<'_>,
    req: &Affected<'_>,
    set: &Reaching,
    depth: usize,
    reached: usize,
) -> Result<String> {
    const FILE_CAP: usize = 12;
    let (found, unsettled) = (&set.found, &set.unsettled);
    let (read, candidates) = (set.read, set.candidates);
    let mut by_file: std::collections::BTreeMap<String, Vec<(&NodeRecord, &Why)>> =
        std::collections::BTreeMap::new();
    let mut weak = 0usize;
    for (node, why) in found.values() {
        if prop_str(&node.properties, TEST_CONFIDENCE_PROP).is_some_and(|c| c != DEFINITIVE) {
            weak += 1;
        }
        by_file
            .entry(file_of(node).unwrap_or("<no file recorded>").to_string())
            .or_default()
            .push((node, why));
    }

    let mut out = format!(
        "affected tests: {} in {} file(s) — from {} of {} changed symbol(s), depth {depth}\n",
        found.len(),
        by_file.len(),
        read,
        req.seeds.len()
    );
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    for (file, tests) in &by_file {
        out.push_str(&format!("{file}  ({})\n", tests.len()));
        for (node, why) in tests.iter().take(FILE_CAP) {
            let key = node.external_key.as_deref().unwrap_or("<keyless>");
            out.push_str(&format!("  {key}  [{}]\n", why.as_str()));
        }
        if tests.len() > FILE_CAP {
            out.push_str(&format!("  … and {} more\n", tests.len() - FILE_CAP));
        }
    }
    if !req.widen.is_empty() {
        out.push_str(&format!(
            "widened: {} reached a change, {} more are in a package it touched\n",
            reached,
            found.len() - reached
        ));
    }
    if !unsettled.is_empty() {
        out.push_str(&format!("unsettled ({}):\n", unsettled.len()));
        for line in unsettled {
            out.push_str(&format!("  {line}\n"));
        }
    }
    if weak > 0 {
        out.push_str(&format!(
            "note: {weak} of these are marked test code on evidence the plugin ranked below \
             `{DEFINITIVE}` — a framework header a file includes, a filename convention. \
             `get_node` reads `{TEST_CONFIDENCE_PROP}` where that matters.\n"
        ));
    }
    if candidates > 0 {
        out.push_str(&format!(
            "note: {candidates} site(s) name a changed symbol without proving they call it \
             (`{MAY_CALL}`), and are not in the count above.\n"
        ));
    }
    out.push_str(
        "note: recorded edges only — a test reaching the change through dynamic dispatch or an \
         untyped receiver has no edge to be found by, so this is a lower bound. Widen to the \
         packages the change touched where a miss would cost more than the time.\n",
    );
    Ok(out)
}

/// Whether a plugin marked this declaration test code.
fn is_test(n: &NodeRecord) -> bool {
    prop_str(&n.properties, TEST_FLAG_PROP).is_some()
}

/// The file a node was declared in, under either spelling.
fn file_of(n: &NodeRecord) -> Option<&str> {
    prop_str(&n.properties, "file").or_else(|| prop_str(&n.properties, "path"))
}

/// How many sites name this symbol without proving they reach it.
///
/// One hop only. A candidate is already unproven; a chain of candidates would
/// be a guess about a guess, and counting it would say more than the parse
/// knows.
fn may_call(plane: &PlaneHandle<'_>, node: &NodeRecord) -> Result<usize> {
    Ok(walk(
        plane,
        node.id,
        Bounds {
            dirs: INWARD,
            ty: Some(MAY_CALL),
            depth: 1,
            budget: usize::MAX,
        },
        |_| Ok(Step::Take),
    )?
    .levels
    .first()
    .map_or(0, Vec::len))
}

/// The label a plugin puts on a node standing in for something outside its
/// own tree, beside what the reference proves it is (`Class`, `Package`, …).
///
/// Two plugins asserting the same foreign key agree rather than collide, so
/// the stand-in a java file's `import` mints and the one a `pom.xml`'s
/// `<dependency>` mints are the same node — and either one, held in another
/// repository under a key this plane declares, *is* that repository
/// referencing this code.
const EXTERNAL_LABEL: &str = "External";

/// The label a build manifest carries, and the property naming what it is for.
const MANIFEST_LABEL: &str = "Manifest";
const DECLARES_PROP: &str = "declares";

/// The edge a build manifest's declaration is written on, carrying `version`
/// and `scope`.
const DEPENDS_EDGE: &str = "DEPENDS_ON";

/// Most keys one question carries to another graph. A radius wider than this
/// is asked about its nearest keys and says so.
const OFFER_CAP: usize = 500;

/// Most referring symbols listed under one key; the count stays exact.
const USES_CAP: usize = 10;

/// What a radius offers another repository to recognise it by.
///
/// Two vocabularies, because a repository states a dependency twice and in
/// two languages: its build declares a **coordinate**, and its source
/// references a **symbol**. Both are already in the graph — neither is
/// derived here, and nothing is guessed.
struct Offered {
    /// Every external key in the radius, nearest first, the changed symbol
    /// leading. Another graph holding one of these as a stand-in references
    /// this code.
    keys: Vec<String>,
    /// How many the radius held before [`OFFER_CAP`].
    held: usize,
    /// What this plane's own build manifests declare it *is*: a Maven
    /// coordinate, an npm name, a Go module path.
    coordinates: Vec<String>,
}

impl Offered {
    /// The radius's keys, and the coordinates this plane publishes.
    ///
    /// The coordinate is **read from this plane**, not mapped onto it: a
    /// coordinate (`com.acme:common-base`) and a package (`com.acme.common…`)
    /// cannot be mapped to one another without a guess — which is why the
    /// deps plugin records a Maven dependency and declines to join it to any
    /// import. Standing in the library there is nothing to guess: its own
    /// manifest says which coordinate it publishes, and a dependent's
    /// manifest keys its declaration the same way.
    fn new(plane: &PlaneHandle<'_>, keys: Vec<String>) -> Result<Self> {
        let held = keys.len();
        let mut keys = keys;
        keys.truncate(OFFER_CAP);
        let mut coordinates: Vec<String> = plane
            .query_from_plan(LogicalPlan::new(Source::ScanLabel(MANIFEST_LABEL.into())))
            .nodes()?
            .iter()
            .filter_map(|n| prop_str(&n.properties, DECLARES_PROP).map(str::to_string))
            .collect();
        coordinates.sort();
        coordinates.dedup();
        Ok(Self {
            keys,
            held,
            coordinates,
        })
    }

    /// Everything one graph is asked for in a single question.
    fn plan(&self) -> LogicalPlan {
        let mut asked = self.coordinates.clone();
        asked.extend(self.keys.iter().cloned());
        LogicalPlan::new(Source::SeekKeys(asked))
    }

    fn is_coordinate(&self, key: &str) -> bool {
        self.coordinates.iter().any(|c| c == key)
    }
}

/// What one graph holds of a radius.
struct Dependent {
    /// The name this call knows the graph by: the attachment's, or the
    /// plane's own.
    at: String,
    /// The attachment it came from, where it is a plane another drsg serves
    /// — which carries both the address it answered at and the name it goes
    /// by there, the two things a reader needs to ask it directly.
    served: Option<Attachment>,
    /// Its build declarations naming a coordinate this plane publishes.
    declares: Vec<Declared>,
    /// Each key of the radius it references, and what references it.
    uses: Vec<Used>,
    /// Keys it holds as declarations of its own rather than as references —
    /// this code again, vendored or digested twice. Not a dependent.
    copies: Vec<NodeRecord>,
}

impl Dependent {
    fn empty(at: &str) -> Self {
        Self {
            at: at.to_string(),
            served: None,
            declares: Vec::new(),
            uses: Vec::new(),
            copies: Vec::new(),
        }
    }

    /// Does it hold anything at all of this radius?
    fn holds(&self) -> bool {
        !self.declares.is_empty() || !self.uses.is_empty() || !self.copies.is_empty()
    }
}

/// A build declaration naming a coordinate this plane publishes.
struct Declared {
    /// The coordinate, as both manifests spell it.
    coordinate: String,
    /// The manifest declaring it, and the `version` and `scope` the
    /// declaration states — all three `None` where the edge could not be
    /// read, which is a graph another drsg serves: `plane.query` answers with
    /// nodes, and a version is a fact about the edge.
    manifest: Option<NodeRecord>,
    version: Option<String>,
    scope: Option<String>,
}

/// One key of the radius another graph references, and what references it.
struct Used {
    key: String,
    /// What references it, with the edge each was reached by — capped at
    /// [`USES_CAP`], and empty where the referrers were not read.
    by: Vec<(NodeRecord, String)>,
    /// How many reference it in all, or `None` where that was not read.
    total: Option<usize>,
}

/// What a radius reaches beyond the plane it was walked in.
fn across(graphs: &mut Graphs<'_>, offered: &Offered) -> Result<String> {
    graphs.fetch(&offered.plan());
    let mut found: Vec<Dependent> = Vec::new();
    for (at, p) in graphs.attached.iter() {
        found.push(dependent_here(at, p, offered)?);
    }
    for (at, nodes) in &graphs.elsewhere {
        let mut d = dependent_far(at, nodes, offered);
        d.served = graphs.served_by(at).cloned();
        found.push(d);
    }
    Ok(render_across(graphs, offered, &found))
}

/// What a plane on this instance holds of a radius, read from its edges.
fn dependent_here(at: &str, plane: &PlaneHandle<'_>, offered: &Offered) -> Result<Dependent> {
    let mut d = Dependent::empty(at);
    for n in plane.query_from_plan(offered.plan()).nodes()? {
        let Some(key) = n.external_key.clone() else {
            continue;
        };
        if offered.is_coordinate(&key) {
            let mut written = false;
            for nb in plane.neighbors(n.id, crate::Dir::In, Some(DEPENDS_EDGE))? {
                let (Some(edge), Some(m)) = (plane.edge(nb.edge)?, plane.node(nb.node)?) else {
                    continue;
                };
                d.declares.push(Declared {
                    coordinate: key.clone(),
                    manifest: Some(m),
                    version: prop_str(&edge.properties, "version").map(str::to_string),
                    scope: prop_str(&edge.properties, "scope").map(str::to_string),
                });
                written = true;
            }
            if !written {
                d.declares.push(Declared {
                    coordinate: key,
                    manifest: None,
                    version: None,
                    scope: None,
                });
            }
            continue;
        }
        if !n.labels.iter().any(|l| l == EXTERNAL_LABEL) {
            d.copies.push(n);
            continue;
        }
        let mut by = Vec::new();
        let mut seen = BTreeSet::new();
        for nb in plane.neighbors(n.id, crate::Dir::In, None)? {
            let Some(edge) = plane.edge(nb.edge)? else {
                continue;
            };
            if !IMPACT_EDGES.contains(&edge.ty.as_str()) || !seen.insert(nb.node) {
                continue;
            }
            if let Some(r) = plane.node(nb.node)?.filter(|_| by.len() < USES_CAP) {
                by.push((r, edge.ty.clone()));
            }
        }
        d.uses.push(Used {
            key,
            by,
            total: Some(seen.len()),
        });
    }
    Ok(d)
}

/// What a plane another drsg serves holds of a radius.
///
/// It answered with nodes, so a stand-in it holds is the whole of what was
/// read: that it holds one *is* the statement that something in it references
/// the key, since a stand-in exists only where a reference minted it. Which
/// symbol that is, and at which version a manifest declared the coordinate,
/// are edges — read where they live, which the answer says.
fn dependent_far(at: &str, nodes: &[NodeRecord], offered: &Offered) -> Dependent {
    let mut d = Dependent::empty(at);
    for n in nodes {
        let Some(key) = n.external_key.clone() else {
            continue;
        };
        if offered.is_coordinate(&key) {
            d.declares.push(Declared {
                coordinate: key,
                manifest: None,
                version: None,
                scope: None,
            });
        } else if n.labels.iter().any(|l| l == EXTERNAL_LABEL) {
            d.uses.push(Used {
                key,
                by: Vec::new(),
                total: None,
            });
        } else {
            d.copies.push(n.clone());
        }
    }
    d
}

/// The cross-repository half of an `impact` answer.
fn render_across(graphs: &Graphs<'_>, offered: &Offered, found: &[Dependent]) -> String {
    let read = graphs.read();
    let mut out = String::from("\n");
    if read.is_empty() && graphs.missed.is_empty() {
        out.push_str(NO_GRAPHS);
        return out;
    }
    out.push_str(&format!("across  {}", graphs.name));
    for name in &read {
        out.push_str(&format!(", {name}"));
    }
    out.push('\n');
    for m in graphs.unread() {
        out.push_str(&format!("not read  {} — {}\n", m.name, m.why));
    }
    if offered.held > offered.keys.len() {
        out.push_str(&format!(
            "asked about the {} key(s) nearest the change, of {} in the radius\n",
            offered.keys.len(),
            offered.held
        ));
    }
    let mut declaring = 0usize;
    let mut referencing = 0usize;
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for d in found.iter().filter(|d| d.holds()) {
        out.push_str(&format!("in {}", d.at));
        if let Some(url) = d.served.as_ref().and_then(|a| a.url.as_deref()) {
            out.push_str(&format!("  at {url}"));
        }
        out.push('\n');
        for dec in &d.declares {
            out.push_str(&format!("  declares  {}", dec.coordinate));
            if let Some(v) = &dec.version {
                out.push_str(&format!("  {v}"));
            }
            if let Some(s) = &dec.scope {
                out.push_str(&format!("  [{s}]"));
            }
            // The manifest's key *is* its path, so `one_line` would print it
            // twice over.
            match &dec.manifest {
                Some(m) => out.push_str(&format!(
                    "  {}",
                    m.external_key.as_deref().unwrap_or("<keyless>")
                )),
                None => out.push_str("  (the manifest and its version are read there)"),
            }
            out.push('\n');
        }
        for u in &d.uses {
            used.insert(&u.key);
            match u.total {
                Some(n) => out.push_str(&format!("  {}  ← {n}\n", u.key)),
                None => out.push_str(&format!(
                    "  {}  ← referenced; what references it is read there\n",
                    u.key
                )),
            }
            for (r, ty) in &u.by {
                out.push_str(&format!("    {}  [{ty}]\n", one_line(r)));
            }
            if let Some(n) = u.total.filter(|n| *n > u.by.len()) {
                out.push_str(&format!("    … and {} more\n", n - u.by.len()));
            }
        }
        for c in &d.copies {
            out.push_str(&format!("  holds its own  {}\n", one_line(c)));
        }
        // What a far graph did not answer with is one call away, so the row
        // names that call rather than leaving a reader to assemble it.
        if let Some(a) = &d.served {
            out.push_str(&format!(
                "  ask that drsg for the rest: `impact <key> --plane {}`\n",
                a.plane
            ));
        }
        declaring += usize::from(!d.declares.is_empty());
        referencing += usize::from(!d.uses.is_empty());
    }
    if read.is_empty() {
        out.push_str(UNREAD_GRAPHS);
        return out;
    }
    let holding = found.iter().filter(|d| d.holds()).count();
    out.push_str(&format!(
        "{} graph(s) read, {holding} hold(s) something: {declaring} declare(s) a dependency, \
         {referencing} reach(es) it in code\n",
        read.len()
    ));
    out.push_str(&format!(
        "{} of the {} key(s) offered are referenced elsewhere\n",
        used.len(),
        offered.keys.len()
    ));
    out.push_str(ACROSS_NOTE);
    out
}

/// Nothing to cross to.
const NO_GRAPHS: &str = "no other graph in scope: this plane attaches none, and \
     `--across all` reads every plane this instance holds\n";

/// Something to cross to, and none of it reached — which is not the same as
/// a radius that reaches nothing, so no tally is offered for it.
const UNREAD_GRAPHS: &str = "no graph in scope was read: what this radius reaches beyond \
     this plane is unknown, not empty\n";

/// What the cross-repository half of an `impact` answer is, and is not.
const ACROSS_NOTE: &str = "a dependent is a graph holding a key of this radius as `External`, \
     or declaring a coordinate this plane's own manifests publish; matched per call, \
     never stored\n";

/// `fathom` — the makeup of everything within `depth` hops of a symbol, both
/// directions: counts by label and edge type, nodes added per hop, and the
/// hubs holding the region together.
///
/// The complement of [`impact`], which lists the nodes reaching a symbol. A
/// region of a few hundred nodes is a wall of names; the counts are the
/// answer.
///
/// Bounded by `depth` and by [`FATHOM_BUDGET`], and the reply says which bound
/// stopped it. Counts are exact over what was walked — a budget stop truncates
/// the region, not the tallies.
pub fn fathom(plane: &PlaneHandle<'_>, name: &str, depth: usize) -> Result<String> {
    let node = match resolve(plane, name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(name, &hits)),
        Resolved::None => return no_match(plane, name),
    };
    let depth = depth.clamp(1, 6);
    let mut out = one_line(&node);
    out.push('\n');
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }

    // Breadth-first, so a truncated region is the nearest one. Every hop is
    // tallied, repeats included — a node already held can still arrive along
    // an edge nothing has counted.
    let mut tally = Tally::default();
    let region = walk(
        plane,
        node.id,
        Bounds {
            dirs: BOTH_WAYS,
            ty: None,
            depth,
            budget: FATHOM_BUDGET,
        },
        |hop| {
            let Some(edge) = plane.edge(hop.edge)? else {
                return Ok(Step::Skip);
            };
            tally.add(hop, &edge.ty);
            Ok(Step::Take)
        },
    )?;

    close_region(plane, &region, node.id, &mut tally)?;
    let labels = region_labels(plane, &node, &region)?;

    let reached = region.levels.len();
    out.push_str(&counts(&region, &labels, &tally, reached));
    out.push_str(&hubs(plane, &tally, node.id)?);
    out.push_str(&bound_note(&region, reached, depth));
    Ok(out)
}

/// Close the induced subgraph the walk left open.
///
/// A node is swept when it is *in* the frontier, so the last hop's nodes were
/// only ever reached, never walked from, and an edge between two of them would
/// go uncounted — leaving them all tied at one edge apiece and the tallies
/// short. No node is added here: the region is what the walk found.
fn close_region(
    plane: &PlaneHandle<'_>,
    region: &Region,
    seed: crate::NodeId,
    tally: &mut Tally,
) -> Result<()> {
    let seed_only = [seed];
    let last: &[crate::NodeId] = region.levels.last().map_or(&seed_only[..], Vec::as_slice);
    for &id in last {
        for &dir in BOTH_WAYS {
            for nb in plane.neighbors(id, dir, None)? {
                if !region.seen.contains(&nb.node) || tally.counted.contains(&nb.edge) {
                    continue;
                }
                if let Some(edge) = plane.edge(nb.edge)? {
                    tally.add(
                        &Hop {
                            from: id,
                            dir,
                            node: nb.node,
                            edge: nb.edge,
                            fresh: false,
                        },
                        &edge.ty,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Label counts over the region the walk settled on, not over what it offered:
/// a node the budget refused is not part of the count.
fn region_labels(
    plane: &PlaneHandle<'_>,
    seed: &NodeRecord,
    region: &Region,
) -> Result<std::collections::BTreeMap<String, usize>> {
    let mut labels = std::collections::BTreeMap::new();
    tally_labels(seed, &mut labels);
    for id in region.levels.iter().flatten() {
        if let Some(n) = plane.node(*id)? {
            tally_labels(&n, &mut labels);
        }
    }
    Ok(labels)
}

/// The four count lines: the region's size, what each hop added, and the label
/// and edge-type makeup.
fn counts(
    region: &Region,
    labels: &std::collections::BTreeMap<String, usize>,
    tally: &Tally,
    reached: usize,
) -> String {
    let mut out = format!(
        "region: {} hop{} out and in — {} nodes, {} edges\n",
        reached,
        if reached == 1 { "" } else { "s" },
        region.seen.len(),
        tally.counted.len(),
    );
    out.push_str(&format!(
        "per hop: {}\n",
        region
            .levels
            .iter()
            .enumerate()
            .map(|(i, level)| format!("{}:{}", i + 1, level.len()))
            .collect::<Vec<_>>()
            .join(" · ")
    ));
    out.push_str(&format!(
        "labels: {}\n",
        ranked(
            labels
                .iter()
                .map(|(l, n)| (format!("{l} {n}"), *n))
                .collect(),
            FATHOM_GROUPS
        )
    ));
    out.push_str(&format!(
        "edges: {}\n",
        ranked(
            tally
                .edges
                .iter()
                .map(|(ty, (o, i))| (format!("{ty} {} ({o} out, {i} in)", o + i), o + i))
                .collect(),
            FATHOM_GROUPS
        )
    ));
    out
}

/// The nodes holding the region together, ranked by degree *within it* — a
/// global degree would rank the plane's hubs, not this region's.
///
/// Without the seed, which is the centre by construction (at depth 1 every edge
/// touches it, so ranking it first says nothing), and without the nodes holding
/// a single edge, which hold nothing together.
fn hubs(plane: &PlaneHandle<'_>, tally: &Tally, seed: crate::NodeId) -> Result<String> {
    let mut ranked: Vec<(crate::NodeId, usize)> = tally
        .degree
        .iter()
        .filter(|&(&id, &deg)| id != seed && deg > 1)
        .map(|(&id, &deg)| (id, deg))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
    if ranked.is_empty() {
        return Ok("hubs: none — every node here but the seed holds a single edge\n".into());
    }
    let mut out = String::from("hubs (by edges inside the region):\n");
    for (id, deg) in ranked.iter().take(FATHOM_HUBS) {
        if let Some(n) = plane.node(*id)? {
            out.push_str(&format!("  {deg:>4}  {}\n", one_line(&n)));
        }
    }
    // A listing cut mid-tie names an arbitrary few of the tied: say how many
    // share the last degree shown, so the cut is visible.
    if let Some(&(_, cutoff)) = ranked.get(FATHOM_HUBS.saturating_sub(1)) {
        let tied = ranked
            .iter()
            .skip(FATHOM_HUBS)
            .filter(|(_, d)| *d == cutoff)
            .count();
        if tied > 0 {
            out.push_str(&format!("  … and {tied} more with {cutoff}\n"));
        }
    }
    Ok(out)
}

/// Which bound stopped the walk — the honesty footer every verb owes.
fn bound_note(region: &Region, reached: usize, depth: usize) -> String {
    match (region.budget_hit, reached < depth) {
        (true, _) => format!(
            "note: stopped at the {FATHOM_BUDGET}-node budget, so the region is the nearest \
             part of a larger one; counts are exact over what was walked. Narrow it with a \
             smaller depth.\n"
        ),
        (false, true) => format!(
            "note: the region ends at {reached} hop{}, short of the {depth} asked for — \
             nothing further connects.\n",
            if reached == 1 { "" } else { "s" }
        ),
        (false, false) => format!("note: walked every edge type to depth {depth}.\n"),
    }
}

/// Nodes one `fathom` walks before stopping and saying so: a hub two hops
/// from everything would otherwise pull in the plane.
const FATHOM_BUDGET: usize = 5_000;
/// Label and edge-type groups named before the rest are summed as "others".
const FATHOM_GROUPS: usize = 8;
/// Hubs listed.
const FATHOM_HUBS: usize = 5;

/// What [`fathom`] counts as it goes: edges by type and direction, and how
/// many of them each node holds.
///
/// A struct rather than three locals because the closing pass counts the same
/// way the walk does, and the two must not drift.
#[derive(Default)]
struct Tally {
    /// Edge type → (walked out, walked in).
    edges: std::collections::BTreeMap<String, (usize, usize)>,
    /// Node → edges it holds *inside the region*.
    degree: std::collections::BTreeMap<crate::NodeId, usize>,
    /// Edges already counted, so a hop walked from both ends stays one edge.
    counted: std::collections::BTreeSet<crate::EdgeId>,
}

impl Tally {
    /// Count one hop's edge, once, from whichever end reached it first.
    fn add(&mut self, hop: &Hop, ty: &str) {
        if !self.counted.insert(hop.edge) {
            return;
        }
        let slot = self.edges.entry(ty.to_string()).or_default();
        match hop.dir {
            crate::Dir::Out => slot.0 += 1,
            _ => slot.1 += 1,
        }
        *self.degree.entry(hop.from).or_default() += 1;
        *self.degree.entry(hop.node).or_default() += 1;
    }
}

fn tally_labels(n: &NodeRecord, out: &mut std::collections::BTreeMap<String, usize>) {
    match n.labels.first() {
        Some(l) => *out.entry(l.clone()).or_default() += 1,
        None => *out.entry("<unlabelled>".to_string()).or_default() += 1,
    }
}

/// Counted groups, biggest first, with the elided tail summed rather than
/// dropped, so the parts add up to the total above them.
///
/// Each item is its rendered text paired with the count it sorts and sums by:
/// an edge type's text also carries its two directions.
fn ranked(mut items: Vec<(String, usize)>, cap: usize) -> String {
    if items.is_empty() {
        return "none".to_string();
    }
    items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let shown: Vec<&str> = items
        .iter()
        .take(cap)
        .map(|(text, _)| text.as_str())
        .collect();
    let rest: usize = items.iter().skip(cap).map(|(_, n)| n).sum();
    match rest {
        0 => shown.join(" · "),
        rest => {
            let others = items.len() - cap;
            format!(
                "{} · {rest} more in {others} other{}",
                shown.join(" · "),
                if others == 1 { "" } else { "s" }
            )
        }
    }
}

// ---- history (the git plane) ---------------------------------------------

/// The verb a side states when it states none — matching every verb, and
/// saying so.
const ANY_VERB: &str = "*";

/// What `wire` says when nothing in the plane recorded a route.
const NO_ROUTES: &str = "no routes recorded in this plane: no symbol carries `serves` or \
                         `requests`.\nthose are written by the language plugins — re-digest \
                         with current plugins (`drsg plugin install`), or this tree declares \
                         none\n";

/// What `wire` says about a symbol that states no route of its own.
const STATES_NO_ROUTE: &str = "this symbol states no route: no `serves`, no `requests`\n\
                               context <key> shows what it does state; wire /<path> asks \
                               about a path instead\n";

/// How `wire` ends, because the answer is a computation rather than a record.
const WIRE_NOTE: &str = "matched on verb + path, computed for this call and never stored; \
                         `*` matches every verb\n";

/// One route a symbol states: the verb, the comparable path, and the node.
///
/// Read from the `serves` and `requests` properties the language plugins
/// write. Nothing here knows which plugin produced one, so a language that
/// learns to state routes widens this verb without touching it.
struct Stated {
    verb: String,
    path: String,
    node: NodeRecord,
    /// The attachment this was read through, or `None` for this plane.
    from: Option<String>,
}

impl Stated {
    /// Does this route answer that one? Equal paths, and verbs that are equal
    /// or either one `*` — a side naming no verb matches every verb on the
    /// path, and the reply says that it did.
    fn answers(&self, other: &Stated) -> bool {
        self.path == other.path
            && (self.verb == other.verb || self.verb == ANY_VERB || other.verb == ANY_VERB)
    }
}

/// The route strings one described property holds.
fn prop_list<'a>(props: &'a Properties, key: &str) -> Vec<&'a str> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                PropValue::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect(),
        // One route need not have been written as a list of one.
        Some(PropValue::Str(s)) => vec![s.as_str()],
        _ => Vec::new(),
    }
}

/// The plan that reads every node stating a route, and nothing else.
///
/// One plan for both sides of the boundary: a graph on another drsg is asked
/// exactly what the local scan reads, so the two cannot drift — and a plane of
/// any size answers with its routes rather than with itself.
fn route_plan() -> LogicalPlan {
    let has = |key: &str| Expr::Not(Box::new(Expr::IsNull(Box::new(Expr::Property(key.into())))));
    let mut plan = LogicalPlan::new(Source::ScanAll);
    plan.push(PlanStep::Filter(Expr::Logic {
        op: LogicOp::Or,
        lhs: Box::new(has("serves")),
        rhs: Box::new(has("requests")),
    }));
    plan
}

/// Every route a set of nodes states, both sides — appended to what the graphs
/// before them stated, since a match is computed over all of them.
fn stated_from(
    nodes: &[NodeRecord],
    from: Option<&str>,
    serves: &mut Vec<Stated>,
    requests: &mut Vec<Stated>,
) {
    for n in nodes {
        for (key, side) in [("serves", &mut *serves), ("requests", &mut *requests)] {
            for route in prop_list(&n.properties, key) {
                // `VERB path` is the whole contract with the plugins that
                // write it; anything else was not written by one.
                let Some((verb, path)) = route.split_once(' ') else {
                    continue;
                };
                side.push(Stated {
                    verb: verb.to_string(),
                    path: path.to_string(),
                    node: n.clone(),
                    from: from.map(str::to_string),
                });
            }
        }
    }
}

/// Which graphs beyond its own a verb reads.
///
/// The scope is asked for rather than inherited: adding a graph changes what
/// an answer's counts mean, so a verb that can cross says which graphs it
/// read and a caller says how far to look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// The planes this one attaches. The only scope that reaches a plane
    /// another drsg serves, an attachment being what carries the address.
    Attached,
    /// Every plane this instance holds, attached or not, and the attachments
    /// another drsg serves. A machine handed every repository then needs no
    /// setup to be asked across them — which is the ordinary case for a
    /// shared library and its dependents. A history plane (`…_git`) is not
    /// one of them: it holds commits, not code.
    All,
}

/// The graphs one call reads: this plane, and each graph in scope it could
/// open — keyed by the name it was attached under, which is what a reader
/// names to reach it again.
struct Graphs<'a> {
    name: String,
    here: PlaneHandle<'a>,
    attached: Vec<(String, PlaneHandle<'a>)>,
    /// Planes another drsg serves, unasked — [`Graphs::fetch`] puts the
    /// question, because a verb does not always know what to ask before it
    /// has read its own plane. Each keeps its place in the list it came from.
    far: Vec<(usize, Attachment)>,
    /// How to reach those, where the surface holds a network client.
    reader: Option<&'a dyn Remote>,
    /// What a plane on another drsg answered: the nodes themselves, since
    /// nothing here can query it a second time.
    elsewhere: Vec<(String, Vec<NodeRecord>)>,
    /// The attachment each of those came from, so an answer can name the
    /// address it was read at and the plane it is called there.
    served: Vec<(String, Attachment)>,
    /// Graphs left unread, and why — a cross-graph answer that came back
    /// empty must be distinguishable from one that was never asked.
    missed: Vec<Missed>,
}

/// Does this plane hold a single node? One row answers it.
fn holds_anything() -> LogicalPlan {
    let mut plan = LogicalPlan::new(Source::ScanAll);
    plan.push(PlanStep::Limit(1));
    plan
}

/// A graph in scope that was not read, and why.
///
/// `at` is where it sat in the list it was named in, so the report follows
/// the order an operator attached them in however late a failure was learned
/// — reaching a plane over the network is answered after every local one.
struct Missed {
    at: usize,
    name: String,
    why: String,
}

/// What a name resolved to across the graphs read.
enum Across {
    /// One key, held by one graph or by several — each with the graph it was
    /// found in, since a node id identifies a symbol only within one drsg.
    Same(Vec<(Option<String>, NodeRecord)>),
    /// Several different keys matched leniently; the caller lists them.
    Several(Vec<NodeRecord>),
    None,
}

/// A graph in scope, held open for the whole answer.
///
/// The database comes as an `Arc` rather than a borrow because a mounted one
/// is owned by an [`Instance`](crate::Instance) whose table may change while
/// this process runs, and [`PlaneHandle`] borrows the database it reads. So
/// every database an answer will touch is taken once, up front, into the
/// caller's own frame — and the handles [`Graphs`] opens borrow from there.
struct Reading {
    /// The name it answers to in this answer's rows.
    name: String,
    /// The database holding it, or `None` for the one the call was handed —
    /// which is every plane an embedded caller can reach, there being no
    /// instance to resolve through.
    db: Option<Arc<Database>>,
    /// Its name inside that database, which differs only on a prefixed mount.
    within: String,
}

/// Every graph a cross-graph call will read, gathered before any handle is
/// opened.
struct Held {
    local: Vec<Reading>,
    far: Vec<(usize, Attachment)>,
    missed: Vec<Missed>,
}

impl Held {
    /// What `scope` names, from the plane `plane` was asked in.
    ///
    /// `instance` widens "every plane this instance holds" from one database
    /// to every database this process serves (issue #44) — which is what lets
    /// a machine handed thirty repositories answer across them. Without one,
    /// it is this database's planes, exactly as before.
    fn of(
        db: &Database,
        plane: &str,
        scope: Option<Scope>,
        instance: Option<&crate::Instance>,
    ) -> Result<Self> {
        let mut held = Self {
            local: Vec::new(),
            far: Vec::new(),
            missed: Vec::new(),
        };
        if scope == Some(Scope::All) {
            let served: Vec<(String, Option<Arc<Database>>, String)> = match instance {
                Some(inst) => inst
                    .planes()?
                    .into_iter()
                    .map(|s| {
                        let at = inst.serving(&s.plane)?;
                        Ok((s.plane, Some(at.db), at.plane))
                    })
                    .collect::<Result<_>>()?,
                None => db
                    .planes()?
                    .into_iter()
                    .map(|(_, name)| (name.clone(), None, name))
                    .collect(),
            };
            for (name, holder, within) in served {
                if name == plane || name.ends_with(HISTORY_SUFFIX) {
                    continue;
                }
                // An empty plane holds nothing any question could match, and
                // every database carries `startup` whether or not anything
                // was ever digested into it — naming it in the answer would
                // be noise on every call. One row is the whole probe.
                if holder
                    .as_deref()
                    .unwrap_or(db)
                    .plane(&within)?
                    .query_from_plan(holds_anything())
                    .ids()?
                    .is_empty()
                {
                    continue;
                }
                held.local.push(Reading {
                    name,
                    db: holder,
                    within,
                });
            }
        }
        if scope.is_some() {
            let here = db.plane(plane)?;
            for (at, a) in Attachments::of(&here)?.list().iter().enumerate() {
                match (&a.url, scope) {
                    (Some(_), _) => held.far.push((at, a.clone())),
                    // Every plane is already in scope, so an attachment to one
                    // of them names nothing new.
                    (None, Some(Scope::All)) => {}
                    // An attachment names a plane *this instance serves*,
                    // which since mounts may be one another database holds.
                    (None, _) => match serving(db, instance, &a.plane) {
                        Some(r) => held.local.push(Reading {
                            name: a.name.clone(),
                            db: r.0,
                            within: r.1,
                        }),
                        None => held.missed.push(Missed {
                            at,
                            name: a.name.clone(),
                            why: format!("no plane '{}' on this instance", a.plane),
                        }),
                    },
                }
            }
        }
        Ok(held)
    }
}

/// The database serving `plane` and its name there — through `instance` where
/// there is one, else this database alone (`None`).
fn serving(
    db: &Database,
    instance: Option<&crate::Instance>,
    plane: &str,
) -> Option<(Option<Arc<Database>>, String)> {
    match instance {
        Some(inst) => inst.serving(plane).ok().map(|at| (Some(at.db), at.plane)),
        None => db.plane(plane).ok().map(|_| (None, plane.to_string())),
    }
}

impl<'a> Graphs<'a> {
    fn open(
        db: &'a Database,
        plane: &str,
        held: &'a Held,
        reader: Option<&'a dyn Remote>,
    ) -> Result<Self> {
        let here = db.plane(plane)?;
        let mut attached = Vec::new();
        for r in &held.local {
            let holder = r.db.as_deref().unwrap_or(db);
            attached.push((r.name.clone(), holder.plane(&r.within)?));
        }
        let far = held.far.clone();
        let missed = held
            .missed
            .iter()
            .map(|m| Missed {
                at: m.at,
                name: m.name.clone(),
                why: m.why.clone(),
            })
            .collect();
        Ok(Self {
            name: plane.to_string(),
            here,
            attached,
            far,
            reader,
            elsewhere: Vec::new(),
            served: Vec::new(),
            missed,
        })
    }

    /// Put one question to every plane another drsg serves, once.
    ///
    /// Taken rather than borrowed: a graph read over the wire is read with
    /// the plan the verb settled on, and asking twice would report an
    /// unreachable attachment twice.
    fn fetch(&mut self, plan: &LogicalPlan) {
        for (at, a) in std::mem::take(&mut self.far) {
            let why = match self.reader {
                Some(reader) => match reader.nodes(&a, plan) {
                    Ok(nodes) => {
                        self.elsewhere.push((a.name.clone(), nodes));
                        self.served.push((a.name.clone(), a.clone()));
                        continue;
                    }
                    Err(why) => why,
                },
                None => format!("served at {}", a.url.as_deref().unwrap_or("no url")),
            };
            self.missed.push(Missed {
                at,
                name: a.name.clone(),
                why,
            });
        }
    }

    /// Every graph this instance holds and this call reads, with the name it
    /// was attached under — this plane first, which is `None`.
    fn all(&self) -> impl Iterator<Item = (Option<&str>, &PlaneHandle<'a>)> {
        std::iter::once((None, &self.here))
            .chain(self.attached.iter().map(|(n, p)| (Some(n.as_str()), p)))
    }

    /// Which graphs this call read and which it did not — empty when no
    /// attachment is in play, so a single-graph answer is unchanged.
    fn note(&self) -> String {
        if self.attached.is_empty() && self.elsewhere.is_empty() && self.missed.is_empty() {
            return String::new();
        }
        let mut out = format!("across  {}", self.name);
        for name in self.read() {
            out.push_str(&format!(", {name}"));
        }
        out.push('\n');
        for m in self.unread() {
            out.push_str(&format!("not read  {} — {}\n", m.name, m.why));
        }
        out
    }

    /// The attachment a graph read over the wire came from.
    fn served_by(&self, name: &str) -> Option<&Attachment> {
        self.served.iter().find(|(n, _)| n == name).map(|(_, a)| a)
    }

    /// The graphs this call did not read, in the order they were named.
    fn unread(&self) -> Vec<&Missed> {
        let mut missed: Vec<&Missed> = self.missed.iter().collect();
        missed.sort_by_key(|m| m.at);
        missed
    }

    /// The names of the attachments read, however they were reached.
    fn read(&self) -> Vec<&str> {
        self.attached
            .iter()
            .map(|(n, _)| n.as_str())
            .chain(self.elsewhere.iter().map(|(n, _)| n.as_str()))
            .collect()
    }

    /// Every route stated by this plane and the attachments read.
    fn stated(&self) -> Result<(Vec<Stated>, Vec<Stated>)> {
        let mut serves = Vec::new();
        let mut requests = Vec::new();
        for (from, p) in self.all() {
            let nodes = p.query_from_plan(route_plan()).nodes()?;
            stated_from(&nodes, from, &mut serves, &mut requests);
        }
        for (name, nodes) in &self.elsewhere {
            stated_from(nodes, Some(name), &mut serves, &mut requests);
        }
        Ok((serves, requests))
    }

    /// What `name` names across the graphs read.
    ///
    /// An exact key held by several graphs is **one** question asked of each,
    /// not an ambiguity: the same string cannot be narrowed any further, and a
    /// handler and the stand-in another repository holds for it are the same
    /// key by construction. Only differing keys are candidates.
    fn named(&self, name: &str) -> Result<Across> {
        let mut exact = Vec::new();
        for (from, p) in self.all() {
            if let Some(n) = p.node_by_key(name)? {
                exact.push((from.map(str::to_string), n));
            }
        }
        for (at, nodes) in &self.elsewhere {
            for n in nodes {
                if n.external_key.as_deref() == Some(name) {
                    exact.push((Some(at.clone()), n.clone()));
                }
            }
        }
        if !exact.is_empty() {
            return Ok(Across::Same(exact));
        }
        let mut hits: Vec<(Option<String>, NodeRecord)> = Vec::new();
        for (from, p) in self.all() {
            match resolve(p, name)? {
                Resolved::One(n) => hits.push((from.map(str::to_string), n)),
                Resolved::Many(found) => {
                    hits.extend(found.into_iter().map(|n| (from.map(str::to_string), n)))
                }
                Resolved::None => {}
            }
        }
        // A graph read over the wire is matched by key only: its routes came
        // back already, and there is nothing left here to scan leniently.
        let keys: BTreeSet<&str> = hits
            .iter()
            .filter_map(|(_, n)| n.external_key.as_deref())
            .collect();
        Ok(match (hits.len(), keys.len()) {
            (0, _) => Across::None,
            (_, 0 | 1) => Across::Same(hits),
            _ => Across::Several(hits.into_iter().map(|(_, n)| n).collect()),
        })
    }
}

/// Is this argument a path rather than a symbol name?
fn is_path(name: &str) -> bool {
    let n = name.trim();
    n.starts_with('/') || n.starts_with("http://") || n.starts_with("https://")
}

/// A path *a person typed*, reduced to the form the plugins already wrote:
/// origin and query dropped, duplicate separators collapsed, no trailing
/// slash, and every variable segment `{}`.
///
/// The plugins' own copy of this lives in the extensions SDK
/// (`dr_strange_ext::route`), which a wasm guest compiles and this crate
/// cannot depend on. What the two share is the format — `VERB /a/{}/b` — and
/// a test here pins this side against the strings that SDK's own tests
/// assert.
fn normalize_query(path: &str) -> String {
    let mut rest = path.trim();
    for scheme in ["http://", "https://"] {
        if let Some(after) = rest.strip_prefix(scheme) {
            rest = match after.find('/') {
                Some(at) => &after[at..],
                None => "/",
            };
            break;
        }
    }
    for cut in ['?', '#'] {
        if let Some(at) = rest.find(cut) {
            rest = &rest[..at];
        }
    }
    let mut out = String::with_capacity(rest.len() + 1);
    out.push('/');
    for segment in rest.split('/') {
        if segment.is_empty() {
            continue;
        }
        if out.len() > 1 {
            out.push('/');
        }
        let variable = segment.starts_with('{')
            || segment.starts_with(':')
            || segment.starts_with("${")
            || segment.starts_with('<')
            || segment == "*"
            || segment == "**"
            || segment.contains("${")
            || (segment.contains('{') && segment.contains('}'));
        out.push_str(if variable { "{}" } else { segment });
    }
    out
}

/// The next call for a symbol that states an HTTP route.
///
/// [`describe_record`] already shows the `serves` / `requests` properties;
/// who is on the *other* side of one is a match across the whole plane, so
/// [`context`] names the verb that computes it rather than guessing here.
fn route_note(node: &NodeRecord, name: &str) -> Option<String> {
    if prop_list(&node.properties, "serves").is_empty()
        && prop_list(&node.properties, "requests").is_empty()
    {
        return None;
    }
    let key = node.external_key.as_deref().unwrap_or(name);
    Some(format!(
        "note: states an HTTP route — `wire {key}` names what is on the other side of it\n"
    ))
}

/// What one `wire` call is asked, and what it may read it with.
pub struct Wire<'a> {
    /// The plane the question is asked in.
    pub plane: &'a str,
    /// A path, a handler, or the function making a call.
    pub name: &'a str,
    /// Narrow a path to one method — `GET`, `post`.
    pub verb: Option<&'a str>,
    /// How to read an attachment another drsg serves. `None` leaves those
    /// unread and named as such, which is every surface that holds no network
    /// client.
    pub remote: Option<&'a dyn Remote>,
    /// Every database this process serves (issue #44), so an attachment
    /// naming a plane on this instance finds it wherever it is mounted.
    pub instance: Option<&'a crate::Instance>,
}

/// `wire` — what serves a path, and what asks for it.
///
/// The two halves of an HTTP route are written by different plugins, in
/// different languages, usually in different repositories, and they are
/// matched **here, per call** rather than joined into an edge at digest
/// time: a stored edge would freeze a guess about which handler answers a
/// request, and this can instead say what it matched, what it could not, and
/// why.
///
/// It reads this plane and every plane this one attaches, so the two halves
/// may live in separate graphs — which is the ordinary case, a frontend and a
/// service being separate repositories. Every row names the graph it came
/// from, and an attachment left unread is reported rather than skipped.
///
/// [`Wire::name`] is a path (`/bridge/check`), or a symbol — a handler, to
/// learn who asks for it, or a caller, to learn what serves what it asks for.
pub fn wire(db: &Database, req: Wire<'_>) -> Result<String> {
    let held = Held::of(db, req.plane, Some(Scope::Attached), req.instance)?;
    let mut graphs = Graphs::open(db, req.plane, &held, req.remote)?;
    graphs.fetch(&route_plan());
    let (serves, requests) = graphs.stated()?;
    if serves.is_empty() && requests.is_empty() {
        return Ok(format!("{}{NO_ROUTES}", graphs.note()));
    }
    let want = req.verb.map(|v| v.trim().to_uppercase());
    let mut out = graphs.note();
    if let Some(note) = synced_note(&graphs.here)? {
        out.push_str(&note);
    }
    let paths = match wire_paths(&graphs, req.name, &serves, &requests, &mut out)? {
        Some(paths) => paths,
        None => return Ok(out),
    };

    let mut matched = 0usize;
    let mut unanswered = 0usize;
    let mut unasked = 0usize;
    for path in &paths {
        let on_path = |side: &[Stated]| -> Vec<Stated> {
            side.iter()
                .filter(|s| &s.path == path)
                .filter(|s| {
                    want.as_deref()
                        .is_none_or(|w| s.verb == w || s.verb == ANY_VERB)
                })
                .map(|s| Stated {
                    verb: s.verb.clone(),
                    path: s.path.clone(),
                    node: s.node.clone(),
                    from: s.from.clone(),
                })
                .collect()
        };
        let here_serves = on_path(&serves);
        let here_requests = on_path(&requests);
        if here_serves.is_empty() && here_requests.is_empty() {
            continue;
        }
        out.push_str(&format!("path  {path}\n"));
        wire_rows("serves  ", &here_serves, &mut out);
        wire_rows("requests", &here_requests, &mut out);
        // With a verb named, "nothing" means nothing *under that verb* — a
        // declaration this call filtered out is not a declaration missing.
        let under = match want.as_deref() {
            Some(w) => format!(" with {w}"),
            None => String::new(),
        };
        let read = match graphs.read().is_empty() {
            true => "this plane",
            false => "the graphs read",
        };
        if here_serves.is_empty() {
            out.push_str(&format!("  nothing in {read} serves it{under}\n"));
        }
        if here_requests.is_empty() {
            out.push_str(&format!("  nothing in {read} asks for it{under}\n"));
        }
        for r in &here_requests {
            if here_serves.iter().any(|s| s.answers(r)) {
                matched += 1;
            } else {
                unanswered += 1;
            }
        }
        for s in &here_serves {
            if !here_requests.iter().any(|r| r.answers(s)) {
                unasked += 1;
            }
        }
    }
    out.push_str(&format!(
        "{} path(s); {matched} request(s) matched a declaration, {unanswered} reached none; \
         {unasked} declaration(s) nothing asks for\n",
        paths.len()
    ));
    out.push_str(WIRE_NOTE);
    Ok(out)
}

/// The paths `wire` answers about: those on the path a caller named, or those
/// the symbol it named states.
///
/// `None` when the answer is already written into `out` — an ambiguous name,
/// a name nothing matches, or a symbol that states no route.
fn wire_paths(
    graphs: &Graphs<'_>,
    name: &str,
    serves: &[Stated],
    requests: &[Stated],
    out: &mut String,
) -> Result<Option<BTreeSet<String>>> {
    if is_path(name) {
        let q = normalize_query(name);
        let under = format!("{}/", q.trim_end_matches('/'));
        // A prefix names everything under it, which is what a reader asking
        // about `/api/orders` means when the declaration is `/api/orders/{}`.
        return Ok(Some(
            serves
                .iter()
                .chain(requests)
                .filter(|s| s.path == q || s.path.starts_with(&under))
                .map(|s| s.path.clone())
                .collect(),
        ));
    }
    let nodes = match graphs.named(name)? {
        Across::Same(nodes) => nodes,
        Across::Several(hits) => {
            out.push_str(&candidates(name, &hits));
            return Ok(None);
        }
        Across::None => {
            out.push_str(&no_match(&graphs.here, name)?);
            let read = graphs.read();
            if !read.is_empty() {
                out.push_str(&format!("nor in the attached: {}\n", read.join(", ")));
            }
            return Ok(None);
        }
    };
    for (from, n) in &nodes {
        out.push_str(&one_line(n));
        if let Some(at) = from {
            out.push_str(&format!("  in {at}"));
        }
        out.push('\n');
    }
    // The graph is part of the identity: a node id is unique within one drsg
    // and means nothing across two, so a row read over the wire is only ever
    // matched against the graph it came from.
    let sites: BTreeSet<(Option<&str>, crate::NodeId)> = nodes
        .iter()
        .map(|(from, n)| (from.as_deref(), n.id))
        .collect();
    let mine: BTreeSet<String> = serves
        .iter()
        .chain(requests)
        .filter(|s| sites.contains(&(s.from.as_deref(), s.node.id)))
        .map(|s| s.path.clone())
        .collect();
    if mine.is_empty() {
        out.push_str(STATES_NO_ROUTE);
        return Ok(None);
    }
    Ok(Some(mine))
}

/// One side's rows under a path, each naming its verb and where it is
/// written.
fn wire_rows(tag: &str, side: &[Stated], out: &mut String) {
    for s in side {
        out.push_str(&format!("  {tag}  {:<7} {}", s.verb, one_line(&s.node)));
        if let Some(from) = &s.from {
            out.push_str(&format!("  in {from}"));
        }
        if s.verb == ANY_VERB {
            out.push_str("  [states no verb: every verb on this path]");
        }
        out.push('\n');
    }
}

/// The suffix a repository's history plane takes, beside the code plane it
/// belongs to: `myrepo` and `myrepo_git`.
///
/// Here rather than beside the preprocessor that writes it, because every
/// surface that *reads* one needs the same convention — the MCP tool, the CLI
/// verb and the digest that creates it must agree on one string.
pub const HISTORY_SUFFIX: &str = "_git";

/// `<plane>_git` — where a repository's history lands.
pub fn history_plane_name(code_plane: &str) -> String {
    format!("{code_plane}{HISTORY_SUFFIX}")
}

/// The suffix a repository's ops plane takes, beside the code plane it belongs
/// to: `myrepo` and `myrepo_ops` (`arch/10-ops.md` §2).
///
/// Here beside the history suffix rather than in [`crate::ops`], so that the
/// two plane conventions are read and edited together and cannot drift apart
/// in style.
pub const OPS_SUFFIX: &str = "_ops";

/// `<plane>_ops` — where a repository's declared topology lands.
pub fn ops_plane_name(code_plane: &str) -> String {
    format!("{code_plane}{OPS_SUFFIX}")
}

/// Commits shown when the caller names no limit.
const HISTORY_COMMITS: usize = 15;
/// Branches, tags and rebases shown before the listing is cut short.
const HISTORY_REFS: usize = 12;

pub(crate) fn prop_bool(props: &Properties, key: &str) -> bool {
    matches!(
        props.get(key).map(|d| &d.value),
        Some(PropValue::Bool(true))
    )
}

/// The date part of an ISO-8601 timestamp — `2026-08-25` out of
/// `2026-08-25T16:29:01+08:00`. History is read a day at a time; the seconds
/// are noise a reader pays for.
pub(crate) fn day(props: &Properties, key: &str) -> String {
    prop_str(props, key)
        .map(|t| t.chars().take(10).collect())
        .unwrap_or_default()
}

/// Orient a reader in a repository's history: where HEAD is, what the
/// branches and tags point at, what was rebased, and the newest commits.
///
/// The counterpart of [`context`] for the history plane — one call that
/// answers "what is this repository" rather than a schema an agent has to
/// discover and then write a query against. Every listing states what it is a
/// listing *of* (`newest 15 of 429`), because a truncated one that looked
/// complete would be the one failure a reader cannot see.
pub fn history(plane: &PlaneHandle<'_>, limit: Option<usize>) -> Result<String> {
    // A label scan per kind, not a whole-plane scan: `Commit` is nearly the
    // entire plane, and the refs — the part a reader looks at first — are a
    // handful of nodes the label index finds directly.
    let commits = plane.query().scan_label("Commit").nodes()?;
    if commits.is_empty() {
        let mut out = String::from("this plane holds no commits\n");
        out.push_str(
            "note: a repository's history lives in its own plane, named after \
             the code plane with `_git` appended — `list_planes` shows \
             which planes exist\n",
        );
        return Ok(out);
    }
    let branches = plane.query().scan_label("Branch").nodes()?;
    let tags = plane.query().scan_label("Tag").nodes()?;
    let rebases = plane.query().scan_label("Rebase").nodes()?;

    let merges = commits
        .iter()
        .filter(|c| prop_bool(&c.properties, "is_merge"))
        .count();
    // `reachable` is absent when the run did not read the reflog; absent is
    // not the same as false, so only an explicit `false` counts here.
    let unreachable = commits
        .iter()
        .filter(|c| {
            matches!(
                c.properties.get("reachable").map(|d| &d.value),
                Some(PropValue::Bool(false))
            )
        })
        .count();

    let mut out = format!(
        "{} commit(s), {merges} of them merges; {} branch(es), {} tag(s), {} rebase(s)\n",
        commits.len(),
        branches.len(),
        tags.len(),
        rebases.len()
    );
    if unreachable > 0 {
        out.push_str(&format!(
            "{unreachable} commit(s) no branch or tag can still reach — what a \
             rewrite left behind, kept because nothing else remembers it\n"
        ));
    }

    out.push_str(&branch_block(plane, branches));
    out.push_str(&tag_block(plane, tags));
    out.push_str(&rebase_block(rebases));
    out.push_str(&commit_block(commits, limit));
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    Ok(out)
}

/// A ref listing as a block rather than line by line: the name column is as
/// wide as the widest name actually present, so `origin/feat/preprocessor-plugins`
/// does not push every sha beside it out of alignment.
fn ref_block(
    plane: &PlaneHandle<'_>,
    nodes: &[NodeRecord],
    name_prop: &str,
    target_prop: &str,
) -> String {
    let rows: Vec<(bool, &str, String, String)> = nodes
        .iter()
        .map(|node| {
            let p = &node.properties;
            let target = prop_str(p, target_prop).unwrap_or_default();
            (
                prop_bool(p, "is_head"),
                prop_str(p, name_prop).unwrap_or("<unnamed>"),
                target.chars().take(7).collect(),
                tip_subject(plane, target).unwrap_or_default(),
            )
        })
        .collect();
    let width = rows.iter().map(|r| r.1.chars().count()).max().unwrap_or(0);
    rows.iter()
        .map(|(head, name, short, subject)| {
            let pad = " ".repeat(width - name.chars().count());
            let head = if *head { "*" } else { " " };
            format!("  {head} {name}{pad}  {short}  {subject}\n")
        })
        .collect()
}

/// Local branches first, then remote-tracking: a reader is asking about this
/// checkout before they are asking about someone else's.
fn branch_block(plane: &PlaneHandle<'_>, mut branches: Vec<NodeRecord>) -> String {
    if branches.is_empty() {
        return String::new();
    }
    branches.sort_by_key(|b| {
        (
            prop_bool(&b.properties, "remote"),
            prop_str(&b.properties, "name").unwrap_or("").to_string(),
        )
    });
    let mut out = format!(
        "branches ({} shown of {}):\n",
        branches.len().min(HISTORY_REFS),
        branches.len()
    );
    branches.truncate(HISTORY_REFS);
    out.push_str(&ref_block(plane, &branches, "name", "tip"));
    out
}

/// Tags, newest first by tag time.
fn tag_block(plane: &PlaneHandle<'_>, mut tags: Vec<NodeRecord>) -> String {
    if tags.is_empty() {
        return String::new();
    }
    tags.sort_by(|a, b| {
        prop_int(&b.properties, "tagged_ts")
            .unwrap_or(0)
            .cmp(&prop_int(&a.properties, "tagged_ts").unwrap_or(0))
    });
    let mut out = format!(
        "tags (newest {} of {}):\n",
        tags.len().min(HISTORY_REFS),
        tags.len()
    );
    tags.truncate(HISTORY_REFS);
    out.push_str(&ref_block(plane, &tags, "name", "target"));
    out
}

/// Rebases, newest first, each with the caveat that they are reconstructed
/// from a reflog rather than recorded in the commit graph.
fn rebase_block(mut rebases: Vec<NodeRecord>) -> String {
    if rebases.is_empty() {
        return String::new();
    }
    let mut out = format!("rebases ({}):\n", rebases.len());
    rebases.sort_by(|a, b| {
        prop_str(&b.properties, "finished_at")
            .unwrap_or_default()
            .cmp(prop_str(&a.properties, "finished_at").unwrap_or_default())
    });
    for r in rebases.iter().take(HISTORY_REFS) {
        let p = &r.properties;
        let short =
            |key: &str| -> String { prop_str(p, key).unwrap_or("").chars().take(7).collect() };
        out.push_str(&format!(
            "    {:<24} {}  onto {}, {} commit(s), replaced {}{}\n",
            prop_str(p, "branch").unwrap_or("<detached>"),
            day(p, "finished_at"),
            short("onto"),
            prop_int(p, "steps").unwrap_or(0),
            short("replaced"),
            if prop_bool(p, "completed") {
                ""
            } else {
                " — never finished"
            },
        ));
    }
    out.push_str(
        "note: rebases come from this clone's reflog, which is local and \
         expires (gc.reflogExpire, 90 days by default) — the commit graph \
         records none, so an absent rebase means \"no record\", not \"did not \
         happen\"\n",
    );
    out
}

/// The newest commits, one line each.
fn commit_block(mut commits: Vec<NodeRecord>, limit: Option<usize>) -> String {
    let limit = limit.unwrap_or(HISTORY_COMMITS).max(1);
    commits.sort_by(|a, b| {
        prop_int(&b.properties, "committed_ts")
            .unwrap_or(0)
            .cmp(&prop_int(&a.properties, "committed_ts").unwrap_or(0))
    });
    let mut out = format!(
        "commits (newest {} of {}):\n",
        commits.len().min(limit),
        commits.len()
    );
    for c in commits.iter().take(limit) {
        let p = &c.properties;
        out.push_str(&format!(
            "  {}  {}  {:<18} {}{}\n",
            prop_str(p, "short").unwrap_or_default(),
            day(p, "committed_at"),
            prop_str(p, "author_name").unwrap_or_default(),
            prop_str(p, "summary").unwrap_or_default(),
            if prop_bool(p, "is_merge") {
                "  [merge]"
            } else {
                ""
            },
        ));
    }
    out
}

/// The first line of the commit a ref points at, when that commit is in this
/// plane. A tip beyond a commit ceiling has none, and an empty subject says
/// exactly that rather than inventing one.
fn tip_subject(plane: &PlaneHandle<'_>, target: &str) -> Option<String> {
    if target.is_empty() {
        return None;
    }
    let node = plane.node_by_key(&format!("commit:{target}")).ok()??;
    prop_str(&node.properties, "summary").map(str::to_string)
}

pub fn describe(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let node = match resolve(plane, name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(name, &hits)),
        Resolved::None => return no_match(plane, name),
    };
    describe_record(plane, &node)
}

fn describe_record(_plane: &PlaneHandle<'_>, node: &NodeRecord) -> Result<String> {
    let mut out = one_line(node);
    out.push('\n');
    if node.labels.len() > 1 {
        out.push_str(&format!("labels: {}\n", node.labels.join(", ")));
    }
    for (k, p) in &node.properties {
        if k == "file" || k == "path" || k == "line" || k == "end_line" {
            continue; // already on the head line
        }
        if k.starts_with('_') && k != "_generated_by" {
            continue;
        }
        let text = match &p.value {
            PropValue::Vector(v) => format!("$vector({} dims, omitted)", v.len()),
            PropValue::List(items) => items
                .iter()
                .filter_map(|v| v.as_text().map(|t| t.trim().to_string()))
                .collect::<Vec<_>>()
                .join(", "),
            other => other.as_text().map(|t| t.into_owned()).unwrap_or_default(),
        };
        if text.is_empty() {
            continue;
        }
        let mut text = text.replace('\n', "\n  ");
        if text.len() > PROP_CAP {
            let mut end = PROP_CAP;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        out.push_str(&format!("{k}: {text}\n"));
    }
    Ok(out)
}

/// A query's node records as an agent reads them: a count, the plane's
/// freshness, then one line per node — key, label, site, and the score when
/// the query ranked. What `RETURN n` answers with on a digested plane, so a
/// `cypher` result costs what a `context` listing costs rather than a JSON
/// record per node; `context` on any key here expands it.
pub fn records(plane: &PlaneHandle<'_>, rows: &[(NodeRecord, Option<f32>)]) -> Result<String> {
    let mut out = format!("{} nodes\n", rows.len());
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    for (node, score) in rows {
        out.push_str(&one_line(node));
        if let Some(score) = score {
            out.push_str(&format!("  score {score:.3}"));
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Database;
    use crate::types::PropDesc;

    /// `impact` in plane `code` alone — the shape most of these ask for.
    fn radius(db: &Database, name: &str, depth: usize) -> Result<String> {
        impact(
            db,
            Impact {
                plane: "code",
                name,
                depth,
                across: None,
                instance: None,
                remote: None,
            },
        )
    }

    fn seeded() -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let node = |file: &str, line: i64, sig: &str| {
            let mut props = Properties::new();
            props.insert(
                "file".into(),
                PropDesc::described("file", PropValue::Str(file.into())),
            );
            props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
            if !sig.is_empty() {
                props.insert(
                    "signature".into(),
                    PropDesc::new(PropValue::Str(sig.into())),
                );
            }
            props
        };
        let f = txn
            .create_node_with_key(
                "m::api::go",
                &["Function"],
                node("src/api.rs", 10, "fn go()"),
            )
            .unwrap();
        let g = txn
            .create_node_with_key("m::util::go", &["Function"], node("src/util.rs", 5, ""))
            .unwrap();
        let caller = txn
            .create_node_with_key("m::api::run", &["Function"], node("src/api.rs", 40, ""))
            .unwrap();
        let mut eprops = Properties::new();
        eprops.insert("line".into(), PropDesc::new(PropValue::Int(44)));
        txn.create_edge(caller, f, "CALLS", eprops).unwrap();
        let _ = g;
        txn.commit().unwrap();
        db
    }

    /// A plane with production code and the tests that reach it: one test
    /// beside the code it tests, one a hop further out in a file of its own,
    /// and one that reaches neither.
    fn with_tests() -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let at = |file: &str, line: i64| {
            let mut props = Properties::new();
            props.insert(
                "file".into(),
                PropDesc::described("file", PropValue::Str(file.into())),
            );
            props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
            props
        };
        let test_at = |file: &str, line: i64, flag: &str, confidence: &str| {
            let mut props = at(file, line);
            props.insert(
                TEST_FLAG_PROP.into(),
                PropDesc::described(
                    "how this was known to be test code",
                    PropValue::Str(flag.into()),
                ),
            );
            props.insert(
                TEST_CONFIDENCE_PROP.into(),
                PropDesc::new(PropValue::Str(confidence.into())),
            );
            props
        };
        let go = txn
            .create_node_with_key("m::api::go", &["Function"], at("src/api.rs", 10))
            .unwrap();
        let run = txn
            .create_node_with_key("m::api::run", &["Function"], at("src/api.rs", 40))
            .unwrap();
        let other = txn
            .create_node_with_key("m::util::trim", &["Function"], at("src/util.rs", 7))
            .unwrap();
        let near = txn
            .create_node_with_key(
                "m::api::tests::go_returns_a_value",
                &["Function"],
                test_at("src/api.rs", 200, "attribute", DEFINITIVE),
            )
            .unwrap();
        let far = txn
            .create_node_with_key(
                "cli::end_to_end",
                &["Function"],
                test_at("tests/cli.rs", 12, "framework-include", "circumstantial"),
            )
            .unwrap();
        let elsewhere = txn
            .create_node_with_key(
                "cli::trims_whitespace",
                &["Function"],
                test_at("tests/cli.rs", 60, "attribute", DEFINITIVE),
            )
            .unwrap();
        txn.create_edge(run, go, "CALLS", Properties::new())
            .unwrap();
        txn.create_edge(near, go, "CALLS", Properties::new())
            .unwrap();
        txn.create_edge(far, run, "CALLS", Properties::new())
            .unwrap();
        txn.create_edge(elsewhere, other, "CALLS", Properties::new())
            .unwrap();
        txn.commit().unwrap();
        db
    }

    fn reached(db: &Database, seeds: &[&str], widen: &[&str], depth: usize) -> String {
        let seeds: Vec<String> = seeds.iter().map(|s| (*s).to_string()).collect();
        let widen: Vec<String> = widen.iter().map(|s| (*s).to_string()).collect();
        affected(
            db,
            Affected {
                plane: "code",
                seeds: &seeds,
                depth,
                widen: &widen,
                instance: None,
            },
        )
        .unwrap()
    }

    /// The whole point: a changed symbol names the tests that reach it, and
    /// not the ones that do not.
    #[test]
    fn a_change_names_the_tests_that_reach_it_and_no_others() {
        let out = reached(&with_tests(), &["m::api::go"], &[], 3);
        assert!(out.contains("affected tests: 2 in 2 file(s)"), "{out}");
        assert!(
            out.contains("m::api::tests::go_returns_a_value  [CALLS]"),
            "{out}"
        );
        assert!(out.contains("cli::end_to_end  [CALLS]"), "{out}");
        assert!(!out.contains("trims_whitespace"), "{out}");
    }

    /// A test is reached through the production code it tests, which is more
    /// than one hop in any real tree. A depth that stops short says so by
    /// returning fewer, not by failing.
    #[test]
    fn depth_bounds_how_far_a_test_may_sit_from_the_change() {
        let db = with_tests();
        let near = reached(&db, &["m::api::go"], &[], 1);
        assert!(near.contains("affected tests: 1 in 1 file(s)"), "{near}");
        assert!(near.contains("go_returns_a_value"), "{near}");
        assert!(!near.contains("end_to_end"), "{near}");
    }

    /// A change that edits a test is a change to run that test, and nothing
    /// reaches a test to find it that way.
    #[test]
    fn a_test_the_change_edited_is_in_the_set_itself() {
        let out = reached(&with_tests(), &["cli::trims_whitespace"], &[], 3);
        assert!(out.contains("cli::trims_whitespace  [changed]"), "{out}");
    }

    /// `--safe`: every test under a package the change touched, reached or
    /// not, and the line says how many of each so the widening is visible.
    #[test]
    fn widening_adds_every_test_under_a_touched_package() {
        let out = reached(&with_tests(), &["m::api::go"], &["tests/"], 3);
        assert!(
            out.contains("cli::trims_whitespace  [in a changed package]"),
            "{out}"
        );
        assert!(
            out.contains("widened: 2 reached a change, 1 more are in a package it touched"),
            "{out}"
        );
    }

    /// Evidence the plugin itself ranked below definitive is said, because a
    /// file flagged by the header it includes may hold no test at all.
    #[test]
    fn evidence_weaker_than_definitive_is_reported() {
        let out = reached(&with_tests(), &["m::api::go"], &[], 3);
        assert!(
            out.contains("1 of these are marked test code on evidence"),
            "{out}"
        );
        assert!(out.contains("below `definitive`"), "{out}");
    }

    /// A seed the plane does not hold is named and the others still answered:
    /// a diff naming a deleted symbol must not cost the rest of the set.
    #[test]
    fn a_seed_this_plane_does_not_hold_is_named_not_dropped() {
        let out = reached(
            &with_tests(),
            &["m::api::go", "zzz::deleted_last_week"],
            &[],
            3,
        );
        assert!(out.contains("from 1 of 2 changed symbol(s)"), "{out}");
        assert!(out.contains("unsettled (1):"), "{out}");
        assert!(
            out.contains("`zzz::deleted_last_week` is no declaration this plane holds"),
            "{out}"
        );
        assert!(out.contains("affected tests: 2"), "{out}");
    }

    /// The set is a lower bound and every answer says so — the one failure a
    /// reader of this verb cannot see for themselves.
    #[test]
    fn every_answer_states_that_it_is_a_lower_bound() {
        let out = reached(&with_tests(), &["m::api::go"], &[], 3);
        assert!(out.contains("recorded edges only"), "{out}");
        assert!(out.contains("lower bound"), "{out}");
    }

    #[test]
    fn records_render_a_line_per_node_with_count_and_score() {
        let db = seeded();
        let plane = db.plane("code").unwrap();
        let go = plane.node_by_key("m::api::go").unwrap().unwrap();
        let run = plane.node_by_key("m::api::run").unwrap().unwrap();
        let out = records(&plane, &[(go, Some(0.5)), (run, None)]).unwrap();
        assert_eq!(
            out,
            "2 nodes\nm::api::go  Function  src/api.rs:10  score 0.500\nm::api::run  Function  src/api.rs:40\n"
        );
        assert_eq!(records(&plane, &[]).unwrap(), "0 nodes\n");
    }

    #[test]
    fn a_hub_context_stays_within_budget_and_names_elisions() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let hub = txn
            .create_node_with_key("m::hub", &["Module"], Properties::new())
            .unwrap();
        // Enough long-keyed children that a GROUP_CAP rendering blows the
        // budget: the cap must shrink and the elision must count the rest.
        for i in 0..600 {
            let long = "x".repeat(120);
            let child = txn
                .create_node_with_key(
                    &format!("m::hub::{long}::symbol_{i}"),
                    &["Function"],
                    Properties::new(),
                )
                .unwrap();
            txn.create_edge(hub, child, "CONTAINS", Properties::new())
                .unwrap();
        }
        txn.commit().unwrap();
        let out = context(&db.plane("code").unwrap(), "m::hub").unwrap();
        assert!(
            out.len() <= CONTEXT_BUDGET,
            "context must respect its budget, got {} chars",
            out.len()
        );
        assert!(out.contains("contains (600):"), "the true count is stated");
        assert!(out.contains("more"), "the elision names what it hides");
    }

    /// A history plane, small but shaped exactly like the one the `git`
    /// plugin writes: two commits and a merge, a branch, a tag, a rebase.
    fn history_plane() -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("repo_git", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let commit = |sha: &str, summary: &str, ts: i64, merge: bool, reachable: bool| {
            let mut props = Properties::new();
            props.insert("sha".into(), PropDesc::new(PropValue::Str(sha.into())));
            props.insert(
                "short".into(),
                PropDesc::new(PropValue::Str(sha[..7].into())),
            );
            props.insert(
                "summary".into(),
                PropDesc::new(PropValue::Str(summary.into())),
            );
            props.insert(
                "author_name".into(),
                PropDesc::new(PropValue::Str("Ada".into())),
            );
            props.insert(
                "committed_at".into(),
                PropDesc::new(PropValue::Str(format!("2026-08-2{ts}T10:00:00+00:00"))),
            );
            props.insert("committed_ts".into(), PropDesc::new(PropValue::Int(ts)));
            props.insert("is_merge".into(), PropDesc::new(PropValue::Bool(merge)));
            props.insert(
                "reachable".into(),
                PropDesc::new(PropValue::Bool(reachable)),
            );
            props
        };
        for (sha, summary, ts, merge, reachable) in [
            (
                "aaaaaaa1111111111111111111111111111111111",
                "root",
                1,
                false,
                true,
            ),
            (
                "bbbbbbb2222222222222222222222222222222222",
                "a merge",
                3,
                true,
                true,
            ),
            (
                "ccccccc3333333333333333333333333333333333",
                "rewritten away",
                2,
                false,
                false,
            ),
        ] {
            let labels: &[&str] = if merge {
                &["Commit", "Merge"]
            } else {
                &["Commit"]
            };
            txn.create_node_with_key(
                &format!("commit:{sha}"),
                labels,
                commit(sha, summary, ts, merge, reachable),
            )
            .unwrap();
        }
        let mut branch = Properties::new();
        branch.insert("name".into(), PropDesc::new(PropValue::Str("main".into())));
        branch.insert("is_head".into(), PropDesc::new(PropValue::Bool(true)));
        branch.insert(
            "tip".into(),
            PropDesc::new(PropValue::Str(
                "bbbbbbb2222222222222222222222222222222222".into(),
            )),
        );
        txn.create_node_with_key("branch:refs/heads/main", &["Branch"], branch)
            .unwrap();

        let mut rebase = Properties::new();
        rebase.insert(
            "branch".into(),
            PropDesc::new(PropValue::Str("feature".into())),
        );
        rebase.insert("steps".into(), PropDesc::new(PropValue::Int(2)));
        rebase.insert("completed".into(), PropDesc::new(PropValue::Bool(true)));
        rebase.insert(
            "finished_at".into(),
            PropDesc::new(PropValue::Str("2026-08-25T09:00:00+00:00".into())),
        );
        rebase.insert(
            "onto".into(),
            PropDesc::new(PropValue::Str(
                "aaaaaaa1111111111111111111111111111111111".into(),
            )),
        );
        txn.create_node_with_key("rebase:refs/heads/feature@2026-08-25", &["Rebase"], rebase)
            .unwrap();
        txn.commit().unwrap();
        db
    }

    #[test]
    fn history_orients_a_reader_newest_first() {
        let db = history_plane();
        let out = history(&db.plane("repo_git").unwrap(), None).unwrap();

        assert!(out.starts_with("3 commit(s), 1 of them merges"), "{out}");
        assert!(
            out.contains("1 commit(s) no branch or tag can still reach"),
            "what a rewrite left behind is said, not silently listed: {out}"
        );
        assert!(out.contains("* main"), "HEAD's branch is marked: {out}");
        assert!(
            out.contains("bbbbbbb  a merge"),
            "a branch shows the subject of the commit it points at: {out}"
        );
        let commits: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.starts_with("commits ("))
            .skip(1)
            .collect();
        assert!(commits[0].contains("a merge"), "newest first: {commits:?}");
        assert!(commits[0].contains("[merge]"));
        assert!(commits[2].contains("root"), "oldest last: {commits:?}");
    }

    /// Every listing says what it is a listing *of*. A truncated one that
    /// looked complete is the one failure a reader cannot see.
    #[test]
    fn history_states_what_it_left_out() {
        let db = history_plane();
        let out = history(&db.plane("repo_git").unwrap(), Some(1)).unwrap();
        assert!(out.contains("commits (newest 1 of 3):"), "{out}");
        assert_eq!(
            out.matches("2026-08-2").count(),
            2,
            "one commit line and the rebase's date, and no more: {out}"
        );
    }

    /// A rebase is the one thing here that is not in the commit graph, so the
    /// answer carries the limits of where it came from.
    #[test]
    fn history_qualifies_what_it_knows_about_rebases() {
        let db = history_plane();
        let out = history(&db.plane("repo_git").unwrap(), None).unwrap();
        assert!(out.contains("feature"), "{out}");
        assert!(out.contains("onto aaaaaaa, 2 commit(s)"), "{out}");
        assert!(
            out.contains("reflog") && out.contains("expires"),
            "the reflog's limits travel with the answer: {out}"
        );
    }

    /// Asking the code plane for history is a wrong turn worth a signpost,
    /// not an empty answer.
    #[test]
    fn a_plane_with_no_commits_says_where_history_lives() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Properties::new()).unwrap();
        let out = history(&db.plane("code").unwrap(), None).unwrap();
        assert!(out.contains("no commits"), "{out}");
        assert!(out.contains("_git"), "it names where to look: {out}");
    }

    /// A plane whose facts came from a working tree that did not match the
    /// commit it names says so (issue #38): before this, a graph holding a
    /// symbol from an untracked file reported only `synced: commit <sha>`.
    #[test]
    fn a_plane_parsed_from_a_dirty_tree_says_the_commit_is_not_the_whole_story() {
        let db = seeded();
        let set = |modified: i64, untracked: i64| {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            props.insert(
                "synced_modified".into(),
                PropDesc::new(PropValue::Int(modified)),
            );
            props.insert(
                "synced_untracked".into(),
                PropDesc::new(PropValue::Int(untracked)),
            );
            p.set_properties(props).unwrap();
        };

        set(2, 1);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("synced: commit abcdef012345"), "{out}");
        assert!(out.contains("2 modified and 1 untracked"), "{out}");
        assert!(
            out.contains("the graph holds what that commit does not"),
            "it says what the drift means: {out}"
        );

        // Only one kind of drift reads naturally too.
        set(0, 3);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("with 3 untracked, so"), "{out}");
        assert!(!out.contains("modified"), "{out}");

        // A tree that matched says nothing extra: the common case is unchanged.
        set(0, 0);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("synced: commit abcdef012345"), "{out}");
        assert!(!out.contains("working tree"), "{out}");
    }

    /// A plane an older drsg wrote carries no drift props. That is unknown, not
    /// clean, and inventing "clean" would be the overclaim this fixes.
    #[test]
    fn a_plane_without_drift_props_claims_nothing_about_the_tree() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            p.set_properties(props).unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("synced: commit abcdef012345"), "{out}");
        assert!(!out.contains("working tree"), "{out}");
    }

    /// "No symbol matches" is the answer that most needs the plugin note: the
    /// symbol may be absent, or its file may never have been parsed (#39).
    #[test]
    fn a_miss_says_the_plugins_changed_so_absence_is_ambiguous() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                DRIFT_PROP.into(),
                PropDesc::new(PropValue::Str("added since: py@2+26f48733".into())),
            );
            p.set_properties(props).unwrap();
        }
        let out = no_match(&db.plane("code").unwrap(), "nothing::here").unwrap();
        assert!(out.contains("no symbol matches"), "{out}");
        assert!(
            out.contains("plugins changed since this plane was built"),
            "{out}"
        );
        assert!(out.contains("py@2+26f48733"), "{out}");
    }

    /// The plugin-set warning reaches the *answer*, not just a status command
    /// (issue #39): an agent reading `context` is exactly who needs to know that
    /// half this graph was parsed by a different set of plugins.
    #[test]
    fn a_plane_whose_plugins_changed_says_so_on_every_answer() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                DRIFT_PROP.into(),
                PropDesc::new(PropValue::Str("added since: ts@2+abc123".into())),
            );
            p.set_properties(props).unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(
            out.contains("plugins changed since this plane was built"),
            "{out}"
        );
        assert!(
            out.contains("added since: ts@2+abc123"),
            "it names which: {out}"
        );
        assert!(out.contains("rebuild"), "and what to do: {out}");
    }

    /// Both notes can be true at once, and both are said: the commit line
    /// answers "how fresh", the plugin line answers "how complete".
    #[test]
    fn the_commit_and_the_plugin_notes_both_speak() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            props.insert(
                DRIFT_PROP.into(),
                PropDesc::new(PropValue::Str("added since: py@2+def456".into())),
            );
            p.set_properties(props).unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("synced: commit abcdef012345"), "{out}");
        assert!(out.contains("plugins changed since"), "{out}");
    }

    /// A plane whose plugins have not moved says nothing extra — the ordinary
    /// case stays quiet.
    #[test]
    fn a_plane_with_matching_plugins_is_silent_about_them() {
        let db = seeded();
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(!out.contains("plugins changed"), "{out}");
    }

    #[test]
    fn a_synced_plane_states_its_commit() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            p.set_properties(props).unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(
            out.contains("synced: commit abcdef012345"),
            "freshness is stated where the answer is read: {out}"
        );
    }

    fn write_ledger(db: &Database, skipped: i64, unclaimed: &[&str]) {
        let mut led: std::collections::BTreeMap<String, PropDesc> = Default::default();
        led.insert("skipped".into(), PropDesc::new(PropValue::Int(skipped)));
        if !unclaimed.is_empty() {
            led.insert(
                "unclaimed".into(),
                PropDesc::new(PropValue::List(
                    unclaimed
                        .iter()
                        .map(|u| PropValue::Str((*u).into()))
                        .collect(),
                )),
            );
        }
        let p = db.plane("code").unwrap();
        let mut props = p.properties().unwrap();
        props.insert(LEDGER_PROP.into(), PropDesc::new(PropValue::Map(led)));
        p.set_properties(props).unwrap();
    }

    /// The reported failure: a lookup that guesses the prefix wrong looks
    /// exactly like one that named nothing. The miss now lists the prefixes the
    /// plane does hold and points at the fragment that would have matched
    /// (issue #40).
    #[test]
    fn a_miss_names_the_prefixes_it_did_not_match() {
        let db = seeded();
        let plane = db.plane("code").unwrap();
        {
            let mut txn = plane.write().unwrap();
            txn.create_node_with_key("highway_bridge-3.0/src/App", &["Module"], Properties::new())
                .unwrap();
            txn.commit().unwrap();
        }

        let out = no_match(&plane, "highway_bridge/src/App").unwrap();
        assert!(
            out.contains("keys here are prefixed:") && out.contains("highway_bridge-3.0"),
            "the miss must show the prefix that does exist: {out}"
        );
        assert!(
            out.contains("`highway_bridge` is not one of them"),
            "and name the guess that failed: {out}"
        );
        assert!(
            out.contains("try `src/App`"),
            "and offer the fragment that would match: {out}"
        );
    }

    /// A name with no prefix of its own guessed nothing, so there is nothing to
    /// correct — it still learns that a fragment matches anywhere.
    #[test]
    fn a_miss_on_a_bare_name_offers_the_fragment_rule_without_a_correction() {
        let db = seeded();
        let out = no_match(&db.plane("code").unwrap(), "Nope").unwrap();
        assert!(
            out.contains("a fragment matches anywhere in a key"),
            "{out}"
        );
        assert!(
            !out.contains("is not one of them"),
            "nothing was guessed: {out}"
        );
    }

    /// An empty plane has no prefixes to teach, and must not invent one.
    #[test]
    fn a_miss_on_a_plane_with_no_keys_says_nothing_about_prefixes() {
        let db = Database::in_memory().unwrap();
        let plane = db.create_plane("bare", Properties::new()).unwrap();
        let out = no_match(&plane, "anything").unwrap();
        assert_eq!(out, "no symbol matches `anything` in this plane\n");
    }

    /// A graph says "nothing" the same way whether nothing is there or nothing
    /// was parsed. When the ingest left something unread, every miss in the
    /// plane is ambiguous that way and the answer says so.
    #[test]
    fn a_plane_whose_ingest_left_files_unread_says_so() {
        let db = seeded();
        write_ledger(&db, 12, &[".mod (1)"]);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("12 file(s) it could not read"), "{out}");
        assert!(out.contains(".mod (1)"), "{out}");
        assert!(out.contains("never parsed"), "{out}");
    }

    /// The ordinary case is silence: a line saying "nothing unusual" on every
    /// answer is noise, and the codebase's other conditional notes agree.
    #[test]
    fn a_clean_ingest_says_nothing() {
        let db = seeded();
        write_ledger(&db, 0, &[]);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(!out.contains("never parsed"), "{out}");
        assert!(!out.contains("could not read"), "{out}");
    }

    /// A plane can be both in sync and incomplete; the two facts are different
    /// and both are said.
    #[test]
    fn the_commit_and_the_ledger_both_speak() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            p.set_properties(props).unwrap();
        }
        write_ledger(&db, 3, &[]);
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("synced: commit abcdef012345"), "{out}");
        assert!(out.contains("3 file(s) it could not read"), "{out}");
    }

    /// Every plugin that can tell writes where control went onto the CALLS
    /// edge. It has to be visible in the verb an agent actually reads, or the
    /// fact may as well not be recorded.
    #[test]
    fn a_call_that_departs_from_waiting_says_so_in_the_answer() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut txn = p.write().unwrap();
            let caller = p.node_by_key("m::api::run").unwrap().unwrap().id;
            let callee = p.node_by_key("m::util::go").unwrap().unwrap().id;
            let mut props = Properties::new();
            props.insert("line".into(), PropDesc::new(PropValue::Int(45)));
            props.insert(
                "concurrent".into(),
                PropDesc::described("how control departs", PropValue::Str("spawned".into())),
            );
            txn.create_edge(caller, callee, "CALLS", props).unwrap();
            txn.commit().unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::run").unwrap();
        assert!(out.contains("call@45  [spawned]"), "{out}");
        // The synchronous call beside it says nothing — waiting is the
        // expectation, and a marker on every edge would be noise.
        assert!(
            out.contains("call@44\n") || out.contains("call@44 "),
            "{out}"
        );
        assert!(!out.contains("call@44  ["), "{out}");
    }

    /// A parser records how sure it was of every call it bound. Saying it
    /// once beats a marker on a quarter of the lines, and beats silence:
    /// an edge matched by name read exactly like one the source spelled out.
    #[test]
    fn the_note_says_how_the_calls_were_bound() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut txn = p.write().unwrap();
            let caller = p.node_by_key("m::api::run").unwrap().unwrap().id;
            let callee = p.node_by_key("m::util::go").unwrap().unwrap().id;
            let mut props = Properties::new();
            props.insert("line".into(), PropDesc::new(PropValue::Int(46)));
            props.insert(
                "_confidence".into(),
                PropDesc::new(PropValue::Str("medium".into())),
            );
            txn.create_edge(caller, callee, "CALLS", props).unwrap();
            txn.commit().unwrap();
        }
        let out = context(&db.plane("code").unwrap(), "m::api::run").unwrap();
        assert!(
            out.contains("Of the 2 call(s) shown, 1 was bound by"),
            "{out}"
        );

        // A listing whose every edge was certain says nothing extra.
        let clean = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(clean.contains("lower bound."), "{clean}");
        assert!(!clean.contains("Of the"), "{clean}");
    }

    /// The extent belongs on the head line, where a reader learns a function
    /// is fifteen lines or two hundred without opening it — and it is said
    /// there instead of again in the property list.
    #[test]
    fn a_recorded_extent_shows_as_a_range() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut txn = p.write().unwrap();
            let id = p.node_by_key("m::api::go").unwrap().unwrap().id;
            txn.set_prop(id, "end_line", PropDesc::new(PropValue::Int(25)))
                .unwrap();
            txn.commit().unwrap();
        }
        let out = describe(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("src/api.rs:10-25"), "{out}");
        assert!(!out.contains("end_line:"), "said once, not twice: {out}");
    }

    fn mark_rebuilding(db: &Database, since: i64) {
        let p = db.plane("code").unwrap();
        let mut props = p.properties().unwrap();
        props.insert(REBUILDING_PROP.into(), PropDesc::new(PropValue::Int(since)));
        p.set_properties(props).unwrap();
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// The window a full rebuild opens: the plane has been dropped and is
    /// refilling, so it answers "nothing matches" for symbols it simply has
    /// not folded yet — word for word what it says about a symbol that does
    /// not exist. Every verb must break that tie.
    #[test]
    fn a_miss_during_a_rebuild_is_not_reported_as_absence() {
        let db = seeded();

        // Baseline: a real miss says nothing about rebuilding.
        let clean = context(&db.plane("code").unwrap(), "m::nope").unwrap();
        assert!(clean.contains("no symbol matches"), "{clean}");
        assert!(!clean.contains("being rebuilt"), "{clean}");

        mark_rebuilding(&db, now() - 3);
        let plane = db.plane("code").unwrap();
        for out in [
            context(&plane, "m::nope").unwrap(),
            describe(&plane, "m::nope").unwrap(),
            radius(&db, "m::nope", 3).unwrap(),
            trace(&plane, "m::nope", "m::api::go").unwrap(),
            trace(&plane, "m::api::go", "m::nope").unwrap(),
        ] {
            assert!(out.contains("no symbol matches"), "{out}");
            assert!(
                out.contains("being rebuilt"),
                "a miss mid-rebuild must not read as absence: {out}"
            );
        }
    }

    /// A *found* symbol mid-rebuild is equally provisional — the edges around
    /// it may not all be folded yet — so the warning rides the answer, and
    /// outranks the ordinary freshness line.
    #[test]
    fn a_hit_during_a_rebuild_says_so_instead_of_claiming_freshness() {
        let db = seeded();
        {
            let p = db.plane("code").unwrap();
            let mut props = p.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            p.set_properties(props).unwrap();
        }
        mark_rebuilding(&db, now());
        let out = context(&db.plane("code").unwrap(), "m::api::go").unwrap();
        assert!(out.contains("being rebuilt"), "{out}");
        assert!(
            !out.contains("synced: commit"),
            "a rebuilding plane must not also claim to be synced: {out}"
        );
    }

    #[test]
    fn trace_renders_the_shortest_calls_path_and_says_reverse() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let a = txn
            .create_node_with_key("m::a", &["Function"], Properties::new())
            .unwrap();
        let b = txn
            .create_node_with_key("m::b", &["Function"], Properties::new())
            .unwrap();
        let c = txn
            .create_node_with_key("m::c", &["Function"], Properties::new())
            .unwrap();
        txn.create_edge(a, b, "CALLS", Properties::new()).unwrap();
        txn.create_edge(b, c, "CALLS", Properties::new()).unwrap();
        txn.commit().unwrap();
        let plane = db.plane("code").unwrap();
        let out = trace(&plane, "m::a", "m::c").unwrap();
        assert!(
            out.contains("m::a") && out.contains("-> m::b") && out.contains("-> m::c"),
            "{out}"
        );
        let back = trace(&plane, "m::c", "m::a").unwrap();
        assert!(back.contains("the other way"), "{back}");

        // A symbol traced to itself is already there: the shortest path is the
        // one node, not a search for a cycle back to it.
        let itself = trace(&plane, "m::b", "m::b").unwrap();
        assert!(itself.contains("m::b"), "{itself}");
        assert!(!itself.contains("->"), "no hop to make: {itself}");
    }

    #[test]
    fn impact_groups_by_depth_with_exact_totals() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let target = txn
            .create_node_with_key("m::t", &["Function"], Properties::new())
            .unwrap();
        let d1 = txn
            .create_node_with_key("m::caller", &["Function"], Properties::new())
            .unwrap();
        let d2 = txn
            .create_node_with_key("m::outer", &["Function"], Properties::new())
            .unwrap();
        let reff = txn
            .create_node_with_key("m::wire", &["Function"], Properties::new())
            .unwrap();
        txn.create_edge(d1, target, "CALLS", Properties::new())
            .unwrap();
        txn.create_edge(reff, target, "REFERENCES", Properties::new())
            .unwrap();
        txn.create_edge(d2, d1, "CALLS", Properties::new()).unwrap();
        txn.commit().unwrap();
        let out = radius(&db, "m::t", 3).unwrap();
        assert!(out.contains("depth 1 (2):"), "{out}");
        assert!(out.contains("depth 2 (1):"), "{out}");
        assert!(out.contains("total affected within depth 3: 3"), "{out}");
        assert!(
            out.contains("[CALLS]") && out.contains("[REFERENCES]"),
            "{out}"
        );
    }

    /// What a schema change would break: a column is reached from the
    /// statements that read it, and through them from the method each
    /// implements. Containment is not reach — the table a column belongs to
    /// does not break with it.
    #[test]
    fn impact_on_a_column_reaches_the_statements_that_use_it() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let column = txn
            .create_node_with_key("orders.total", &["Column"], Properties::new())
            .unwrap();
        let table = txn
            .create_node_with_key("orders", &["Table"], Properties::new())
            .unwrap();
        let select = txn
            .create_node_with_key("m.xml#byId", &["Statement"], Properties::new())
            .unwrap();
        let method = txn
            .create_node_with_key("com.acme.M.byId", &["Method"], Properties::new())
            .unwrap();
        txn.create_edge(table, column, "HAS_COLUMN", Properties::new())
            .unwrap();
        txn.create_edge(select, column, "USES_COLUMN", Properties::new())
            .unwrap();
        txn.create_edge(select, table, "READS", Properties::new())
            .unwrap();
        txn.create_edge(select, method, "IMPLEMENTS", Properties::new())
            .unwrap();
        txn.commit().unwrap();

        let out = radius(&db, "orders.total", 3).unwrap();
        assert!(out.contains("m.xml#byId"), "the statement reads it: {out}");
        assert!(out.contains("[USES_COLUMN]"), "{out}");
        assert!(
            !out.contains("[HAS_COLUMN]"),
            "containment is not blast radius: {out}"
        );

        // And the table, through the statement that reads it.
        let table_out = radius(&db, "orders", 3).unwrap();
        assert!(table_out.contains("[READS]"), "{table_out}");
        assert!(table_out.contains("m.xml#byId"), "{table_out}");
    }

    #[test]
    fn fathom_reports_a_regions_makeup_not_its_members() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let hub = txn
            .create_node_with_key("m::hub", &["Function"], Properties::new())
            .unwrap();
        let types: Vec<_> = (0..3)
            .map(|i| {
                txn.create_node_with_key(&format!("m::T{i}"), &["Struct"], Properties::new())
                    .unwrap()
            })
            .collect();
        let callers: Vec<_> = (0..2)
            .map(|i| {
                txn.create_node_with_key(&format!("m::c{i}"), &["Function"], Properties::new())
                    .unwrap()
            })
            .collect();
        for t in &types {
            txn.create_edge(hub, *t, "REFERENCES", Properties::new())
                .unwrap();
        }
        for c in &callers {
            txn.create_edge(*c, hub, "CALLS", Properties::new())
                .unwrap();
        }
        // One node two hops out, which depth 1 stops short of.
        let far = txn
            .create_node_with_key("m::far", &["Function"], Properties::new())
            .unwrap();
        txn.create_edge(far, callers[0], "CALLS", Properties::new())
            .unwrap();
        txn.commit().unwrap();
        let plane = db.plane("code").unwrap();

        let out = fathom(&plane, "m::hub", 1).unwrap();
        // Counts by label and by edge type, each with its direction.
        assert!(
            out.contains("region: 1 hop out and in — 6 nodes, 5 edges"),
            "{out}"
        );
        assert!(
            out.contains("Function 3") && out.contains("Struct 3"),
            "{out}"
        );
        assert!(
            out.contains("REFERENCES 3 (3 out, 0 in)") && out.contains("CALLS 2 (0 out, 2 in)"),
            "{out}"
        );
        // A star's centre is the seed, which the ranking leaves out: nothing
        // else here holds more than one edge, and the reply says so rather
        // than naming an arbitrary few of the tied.
        assert!(
            out.contains("hubs: none — every node here but the seed holds a single edge"),
            "{out}"
        );

        // Two hops reaches the far node, and says it walked the whole depth.
        let deeper = fathom(&plane, "m::hub", 2).unwrap();
        assert!(deeper.contains("per hop: 1:5 · 2:1"), "{deeper}");
        assert!(
            deeper.contains("walked every edge type to depth 2"),
            "{deeper}"
        );

        // Asked for more hops than there are, it says where the region ends.
        let past = fathom(&plane, "m::hub", 5).unwrap();
        assert!(
            past.contains("region ends at 2 hops, short of the 5"),
            "{past}"
        );
    }

    #[test]
    fn fathom_names_what_it_cannot_group_and_sums_the_tail_it_elides() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let hub = txn
            .create_node_with_key("m::hub", &["Function"], Properties::new())
            .unwrap();
        // A node with no label at all — soft-schema data has them.
        let bare = txn.create_node(&[], Properties::new()).unwrap();
        txn.create_edge(hub, bare, "REFERENCES", Properties::new())
            .unwrap();
        // Ten label kinds, so the listing elides and accounts for the rest.
        for i in 0..10 {
            let n = txn
                .create_node_with_key(&format!("m::n{i}"), &[&format!("L{i}")], Properties::new())
                .unwrap();
            txn.create_edge(hub, n, "CALLS", Properties::new()).unwrap();
        }
        txn.commit().unwrap();

        let out = fathom(&db.plane("code").unwrap(), "m::hub", 1).unwrap();
        assert!(out.contains("<unlabelled> 1"), "{out}");
        // 12 nodes, 8 groups shown one each, 4 left across 4 groups.
        assert!(out.contains("· 4 more in 4 others"), "{out}");
    }

    #[test]
    fn fathom_says_how_many_hubs_tie_with_the_last_one_listed() {
        // A ring of eight around the seed: every ring node holds three edges
        // (two neighbours and the seed), so the five listed are an arbitrary
        // five of eight equals — which the reply has to say.
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let hub = txn
            .create_node_with_key("m::hub", &["Function"], Properties::new())
            .unwrap();
        let ring: Vec<_> = (0..8)
            .map(|i| {
                txn.create_node_with_key(&format!("m::r{i}"), &["Function"], Properties::new())
                    .unwrap()
            })
            .collect();
        for (i, &n) in ring.iter().enumerate() {
            txn.create_edge(hub, n, "CALLS", Properties::new()).unwrap();
            txn.create_edge(n, ring[(i + 1) % ring.len()], "CALLS", Properties::new())
                .unwrap();
        }
        txn.commit().unwrap();

        let out = fathom(&db.plane("code").unwrap(), "m::hub", 1).unwrap();
        assert!(out.contains("… and 3 more with 3"), "{out}");
    }

    #[test]
    fn fathom_stops_at_its_budget_and_says_which_bound_it_hit() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let hub = txn
            .create_node_with_key("m::hub", &["Function"], Properties::new())
            .unwrap();
        // More nodes than the budget, so the budget is what stops the walk.
        for i in 0..=FATHOM_BUDGET {
            let n = txn
                .create_node_with_key(&format!("m::n{i}"), &["Function"], Properties::new())
                .unwrap();
            txn.create_edge(hub, n, "CALLS", Properties::new()).unwrap();
        }
        txn.commit().unwrap();

        let out = fathom(&db.plane("code").unwrap(), "m::hub", 3).unwrap();
        assert!(
            out.contains(&format!("stopped at the {FATHOM_BUDGET}-node budget")),
            "{out}"
        );
        // The tallies still cover what it walked.
        assert!(out.contains("CALLS"), "{out}");
    }

    #[test]
    fn fathom_resolves_and_refuses_like_every_other_verb() {
        let db = seeded();
        let p = db.plane("code").unwrap();
        assert!(fathom(&p, "go", 1).unwrap().contains("ambiguous"));
        assert!(fathom(&p, "zzz", 1).unwrap().contains("no symbol"));
    }

    /// An unresolved reference is one end of a call the parser could not bind,
    /// recorded once per file that makes it. Offered as a candidate it is a key
    /// that resolves to a stub, and a dozen of them bury the definitions that
    /// can be picked — so they stop being candidates, and are counted instead.
    #[test]
    fn unresolved_references_are_not_offered_as_candidates() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let props = |file: &str| {
            let mut props = Properties::new();
            props.insert(
                "file".into(),
                PropDesc::described("file", PropValue::Str(file.into())),
            );
            props
        };
        for (key, labels) in [
            ("m::api::wrap", &["Function"][..]),
            ("m::util::wrap", &["Function"][..]),
            ("?::src/a.rs::wrap", &[UNRESOLVED_LABEL][..]),
            ("?::src/b.rs::wrap", &[UNRESOLVED_LABEL][..]),
            ("?::src/c.rs::wrap", &[UNRESOLVED_LABEL][..]),
        ] {
            txn.create_node_with_key(key, labels, props("src/x.rs"))
                .unwrap();
        }
        // A name whose only matches are unresolved: called, never defined here.
        for key in ["?::src/a.rs::absent", "?::src/b.rs::absent"] {
            txn.create_node_with_key(key, &[UNRESOLVED_LABEL], props("src/x.rs"))
                .unwrap();
        }
        // One definition among stubs: not ambiguous once the stubs are out.
        txn.create_node_with_key("m::api::solo", &["Function"], props("src/x.rs"))
            .unwrap();
        for key in ["?::src/a.rs::solo", "?::src/b.rs::solo"] {
            txn.create_node_with_key(key, &[UNRESOLVED_LABEL], props("src/x.rs"))
                .unwrap();
        }
        txn.commit().unwrap();

        // Two definitions and three stubs: the stubs are summarised, not listed,
        // and the count is of what can be picked.
        let out = context(&p, "wrap").unwrap();
        assert!(out.contains("ambiguous — 2 matches"), "{out}");
        assert!(
            out.contains("m::api::wrap") && out.contains("m::util::wrap"),
            "{out}"
        );
        assert!(!out.contains("?::src/a.rs::wrap"), "{out}");
        assert!(out.contains("3 further match(es) are unresolved"), "{out}");

        // One definition among stubs is not ambiguous at all: it resolves.
        assert!(matches!(resolve(&p, "solo").unwrap(), Resolved::One(_)));

        // All stubs: an answer about the parse, with the next call named —
        // never "no symbol matches", which would deny the name is used here.
        let only = context(&p, "absent").unwrap();
        assert!(only.contains("2 unresolved reference(s)"), "{only}");
        assert!(only.contains("no definition"), "{only}");
        assert!(only.contains("next: `grep`"), "{only}");
    }

    /// A radius of zero is the answer that most needs qualifying: "nothing
    /// reaches this" and "the parse could not tell" print the same `0`. The
    /// candidates a parse did record, and the unresolved references that share
    /// the name, are counted beside the total and never inside it.
    #[test]
    fn a_radius_counts_what_names_a_symbol_without_proving_it() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let props = |file: &str, line: i64| {
            let mut props = Properties::new();
            props.insert(
                "file".into(),
                PropDesc::described("file", PropValue::Str(file.into())),
            );
            props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
            props
        };
        let target = txn
            .create_node_with_key("m::db::purge", &["Function"], props("src/db.rs", 10))
            .unwrap();
        let guesser = txn
            .create_node_with_key("m::api::handle", &["Function"], props("src/api.rs", 4))
            .unwrap();
        let prover = txn
            .create_node_with_key("m::api::direct", &["Function"], props("src/api.rs", 9))
            .unwrap();
        // Two unresolved references sharing the name, in two files.
        for key in ["?::src/a.rs::purge", "?::src/b.rs::purge"] {
            txn.create_node_with_key(key, &[UNRESOLVED_LABEL], props("src/a.rs", 1))
                .unwrap();
        }
        txn.create_edge(guesser, target, MAY_CALL, Properties::new())
            .unwrap();
        txn.commit().unwrap();

        // No recorded edge reaches it, but two things name it: the zero says so
        // rather than reading as "nothing calls this".
        let empty = radius(&db, "m::db::purge", 3).unwrap();
        assert!(
            empty.contains("total affected within depth 3: 0"),
            "{empty}"
        );
        assert!(empty.contains("may-call candidates: 1"), "{empty}");
        assert!(empty.contains("unresolved by name: 2"), "{empty}");
        assert!(
            empty.contains("this zero is empty, not negative"),
            "{empty}"
        );

        // A real caller counts in the total; the candidate still does not, and
        // with a non-zero total the note is not printed.
        let mut txn = p.write().unwrap();
        txn.create_edge(prover, target, "CALLS", Properties::new())
            .unwrap();
        txn.commit().unwrap();
        let some = radius(&db, "m::db::purge", 3).unwrap();
        assert!(some.contains("total affected within depth 3: 1"), "{some}");
        assert!(some.contains("may-call candidates: 1"), "{some}");
        assert!(!some.contains("this zero is empty"), "{some}");
        // The candidate is not listed among what would break.
        assert!(!some.contains("m::api::handle"), "{some}");

        // A symbol nothing names at all carries neither line.
        let quiet = radius(&db, "m::api::direct", 3).unwrap();
        assert!(!quiet.contains("may-call candidates"), "{quiet}");
        assert!(!quiet.contains("unresolved by name"), "{quiet}");
    }

    /// A `pub use` makes two paths name one symbol while this graph keys by
    /// where a thing is declared, so the path the source writes can miss — or,
    /// worse, land on the stand-in a plugin minted for it. Both answers name
    /// the declaration instead of describing an empty node or reciting the
    /// prefixes the plane happens to hold.
    #[test]
    fn a_re_export_path_is_pointed_at_the_declaration() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut props = Properties::new();
        props.insert(
            "file".into(),
            PropDesc::described("file", PropValue::Str("src/capability.rs".into())),
        );
        props.insert("line".into(), PropDesc::new(PropValue::Int(14)));
        txn.create_node_with_key("ip_core::capability::Grant", &["Struct"], props)
            .unwrap();
        // What a plugin mints when it saw only the re-exported path: a kind, the
        // `External` label, and nothing else.
        txn.create_node_with_key("ip_core::Grant", &["Type", "External"], Properties::new())
            .unwrap();
        txn.commit().unwrap();

        // The miss: the prefix was right and the middle elided, so the prefix
        // listing is not the answer — the declaration's key is.
        let miss = context(&p, "ip_core::other::Grant").unwrap();
        assert!(miss.contains("no symbol matches"), "{miss}");
        assert!(miss.contains("declared here under a longer key"), "{miss}");
        assert!(
            miss.contains("ip_core::capability::Grant  Struct"),
            "{miss}"
        );
        assert!(!miss.contains("keys here are prefixed"), "{miss}");

        // The hit on the stand-in: described, but said to be a stand-in, with
        // the declaration named and the effect on counts stated.
        let stub = context(&p, "ip_core::Grant").unwrap();
        assert!(stub.contains("this key is a stand-in"), "{stub}");
        assert!(stub.contains("`impact` included"), "{stub}");
        assert!(
            stub.contains("ip_core::capability::Grant  Struct"),
            "{stub}"
        );

        // The declaration itself carries no such note.
        let real = context(&p, "ip_core::capability::Grant").unwrap();
        assert!(!real.contains("stand-in"), "{real}");

        // A name with no path cannot be a re-export miss, and a stand-in with
        // nothing of its name declared here says nothing extra.
        let bare = context(&p, "Absent").unwrap();
        assert!(!bare.contains("declared here under a longer key"), "{bare}");

        // A file path is not a symbol path: its last segment is an extension,
        // and chasing it would offer every key that ends the same way.
        let mut file_props = Properties::new();
        file_props.insert(
            "file".into(),
            PropDesc::described("file", PropValue::Str("src/capability.rs".into())),
        );
        let mut txn = p.write().unwrap();
        txn.create_node_with_key("src/capability.rs", &["File"], file_props)
            .unwrap();
        txn.commit().unwrap();
        let path = context(&p, "src/zzz.rs").unwrap();
        assert!(!path.contains("declared here under a longer key"), "{path}");
    }

    /// A plugin can key only what it parses, so the SQL statement inside a
    /// Rust function and the function around it are written by two parsers
    /// that cannot name each other. Both record their lines, which is enough
    /// to join them — and the join is reported as position, never as an edge.
    #[test]
    fn a_span_joins_what_no_edge_relates() {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let at = |file: &str, from: i64, to: i64| {
            let mut props = Properties::new();
            props.insert(
                "file".into(),
                PropDesc::described("file", PropValue::Str(file.into())),
            );
            props.insert("line".into(), PropDesc::new(PropValue::Int(from)));
            props.insert("end_line".into(), PropDesc::new(PropValue::Int(to)));
            props
        };
        let load = txn
            .create_node_with_key("m::dao::load", &["Function"], at("src/dao.rs", 10, 20))
            .unwrap();
        txn.create_node_with_key("src/dao.rs:12", &["Statement"], at("src/dao.rs", 12, 13))
            .unwrap();
        // Inside the same lines, but an edge already says so.
        let inner = txn
            .create_node_with_key(
                "m::dao::load::inner",
                &["Function"],
                at("src/dao.rs", 14, 16),
            )
            .unwrap();
        txn.create_edge(load, inner, "CONTAINS", Properties::new())
            .unwrap();
        // Same file, no overlap.
        txn.create_node_with_key("m::dao::other", &["Function"], at("src/dao.rs", 30, 40))
            .unwrap();
        // The same lines in another file.
        txn.create_node_with_key("m::far::load", &["Function"], at("src/far.rs", 12, 13))
            .unwrap();
        // Exactly the same extent: two parsers describing one thing, which is
        // not one holding the other.
        txn.create_node_with_key("src/dao.rs#block", &["Block"], at("src/dao.rs", 10, 20))
            .unwrap();
        txn.commit().unwrap();

        // Up: the statement names the function it was written in.
        let stmt = context(&p, "src/dao.rs:12").unwrap();
        assert!(stmt.contains("inside (by span) (2)"), "{stmt}");
        assert!(stmt.contains("m::dao::load  Function"), "{stmt}");
        assert!(stmt.contains("src/dao.rs#block  Block"), "{stmt}");
        assert!(!stmt.contains("m::dao::other"), "{stmt}");
        assert!(!stmt.contains("m::far::load"), "{stmt}");
        assert!(stmt.contains("`by span` is position"), "{stmt}");

        // Down: the function lists the statement, and leaves to `contains`
        // the child an edge already reaches.
        let fun = context(&p, "m::dao::load").unwrap();
        assert!(fun.contains("holds (by span) (1)"), "{fun}");
        assert!(fun.contains("src/dao.rs:12  Statement"), "{fun}");
        assert!(fun.contains("contains (1)"), "{fun}");
        assert!(
            !fun.split("holds (by span)")
                .nth(1)
                .unwrap()
                .contains("inner"),
            "{fun}"
        );
        // `#block` covers exactly the same lines as `load`: two parsers
        // describing one thing, which is neither holding the other.
        assert!(!fun.contains("src/dao.rs#block"), "{fun}");

        // Code one parser read on its own says nothing about spans.
        let other = context(&p, "m::dao::other").unwrap();
        assert!(!other.contains("by span"), "{other}");
    }

    #[test]
    fn fuzzy_resolution_walks_exact_suffix_substring() {
        let db = seeded();
        let p = db.plane("code").unwrap();
        // Exact key wins outright.
        assert!(matches!(
            resolve(&p, "m::api::go").unwrap(),
            Resolved::One(_)
        ));
        // A suffix shared by two symbols is ambiguous — and says so.
        let out = context(&p, "go").unwrap();
        assert!(out.contains("ambiguous"), "{out}");
        assert!(out.contains("m::api::go") && out.contains("m::util::go"));
        // A unique substring resolves.
        assert!(matches!(resolve(&p, "util").unwrap(), Resolved::One(_)));
        // Nothing matches: said plainly.
        assert!(context(&p, "zzz").unwrap().contains("no symbol"));
    }

    #[test]
    fn context_answers_callers_callees_and_what_is_in_one_call() {
        let db = seeded();
        let p = db.plane("code").unwrap();
        let out = context(&p, "m::api::go").unwrap();
        // The describe half: head line + signature.
        assert!(
            out.starts_with("m::api::go  Function  src/api.rs:10"),
            "{out}"
        );
        assert!(out.contains("signature: fn go()"), "{out}");
        // The callers half, call site included.
        assert!(out.contains("callers (1):"), "{out}");
        assert!(
            out.contains("m::api::run  Function  src/api.rs:40  call@44"),
            "{out}"
        );
        assert!(out.contains("lower"), "honesty footer missing: {out}");
        // The caller's own context shows the same edge from the other side.
        let caller = context(&p, "m::api::run").unwrap();
        assert!(caller.contains("callees (1):"), "{caller}");
        assert!(caller.contains("m::api::go"), "{caller}");
    }

    #[test]
    fn describe_prints_props_without_positional_repeats() {
        let db = seeded();
        let p = db.plane("code").unwrap();
        let out = describe(&p, "m::api::go").unwrap();
        assert!(
            out.starts_with("m::api::go  Function  src/api.rs:10"),
            "{out}"
        );
        assert!(out.contains("signature: fn go()"), "{out}");
        // file/line live on the head line only.
        assert_eq!(out.matches("src/api.rs").count(), 1, "{out}");
    }

    /// A plane holding both halves of one route, written by two different
    /// language plugins the way they write them — the join `wire` computes.
    fn routed() -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("code", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut node =
            |key: &str, label: &str, file: &str, line: i64, prop: &str, routes: &[&str]| {
                let mut props = Properties::new();
                props.insert("file".into(), PropDesc::new(PropValue::Str(file.into())));
                props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
                props.insert(
                    prop.into(),
                    PropDesc::described(
                        "the routes this symbol states",
                        PropValue::List(
                            routes
                                .iter()
                                .map(|r| PropValue::Str((*r).to_string()))
                                .collect(),
                        ),
                    ),
                );
                txn.create_node_with_key(key, &[label], props).unwrap()
            };
        node(
            "com.acme.BridgeController.check",
            "Method",
            "src/main/java/com/acme/BridgeController.java",
            42,
            "serves",
            &["GET /bridge/check/{}"],
        );
        node(
            "web/src/api.check",
            "Function",
            "web/src/api.ts",
            12,
            "requests",
            &["GET /bridge/check/{}"],
        );
        // A project-local wrapper names no verb, so it matches every verb on
        // the path and must be told that it did.
        node(
            "web/src/page.load",
            "Function",
            "web/src/page.ts",
            30,
            "requests",
            &["* /bridge/check/{}"],
        );
        // A request nothing serves, and a declaration nothing asks for.
        node(
            "web/src/api.orphan",
            "Function",
            "web/src/api.ts",
            50,
            "requests",
            &["POST /api/missing"],
        );
        node(
            "com.acme.DeadController.gone",
            "Method",
            "src/main/java/com/acme/DeadController.java",
            7,
            "serves",
            &["DELETE /api/dead"],
        );
        // A symbol beside them that states nothing — what a routeless
        // question must be answered about.
        let mut plain = Properties::new();
        plain.insert("line".into(), PropDesc::new(PropValue::Int(60)));
        txn.create_node_with_key("com.acme.BridgeController.helper", &["Method"], plain)
            .unwrap();
        txn.commit().unwrap();
        db
    }

    /// The two halves meet on `verb + path`, whichever language wrote them,
    /// and the one that names no verb is matched and told so.
    #[test]
    fn wire_matches_a_request_to_the_handler_that_serves_it() {
        let db = routed();
        let out = wire(
            &db,
            Wire {
                plane: "code",
                name: "/bridge/check",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(out.contains("path  /bridge/check/{}"), "{out}");
        assert!(
            out.contains("serves    GET     com.acme.BridgeController.check  Method"),
            "{out}"
        );
        assert!(
            out.contains("requests  GET     web/src/api.check  Function"),
            "{out}"
        );
        assert!(
            out.contains("requests  *       web/src/page.load  Function  web/src/page.ts:30  [states no verb: every verb on this path]"),
            "{out}"
        );
        assert!(
            out.contains("1 path(s); 2 request(s) matched a declaration, 0 reached none"),
            "{out}"
        );
        assert!(out.contains("never stored"), "{out}");
    }

    /// A handler is asked who calls it; a caller is asked what serves it.
    /// Both are the same question put from the two ends.
    #[test]
    fn wire_answers_from_either_end_of_a_route() {
        let db = routed();
        let from_handler = wire(
            &db,
            Wire {
                plane: "code",
                name: "BridgeController.check",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(
            from_handler.contains("requests  GET     web/src/api.check"),
            "{from_handler}"
        );
        let from_caller = wire(
            &db,
            Wire {
                plane: "code",
                name: "web/src/api.check",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(
            from_caller.contains("serves    GET     com.acme.BridgeController.check"),
            "{from_caller}"
        );
    }

    /// What it could not match is part of the answer: a request reaching no
    /// handler, and an endpoint nothing asks for.
    #[test]
    fn wire_counts_what_it_could_not_match() {
        let db = routed();
        let orphan = wire(
            &db,
            Wire {
                plane: "code",
                name: "/api/missing",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(
            orphan.contains("nothing in this plane serves it"),
            "{orphan}"
        );
        assert!(
            orphan.contains("0 request(s) matched a declaration, 1 reached none"),
            "{orphan}"
        );
        let dead = wire(
            &db,
            Wire {
                plane: "code",
                name: "/api/dead",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(dead.contains("nothing in this plane asks for it"), "{dead}");
        assert!(dead.contains("1 declaration(s) nothing asks for"), "{dead}");
    }

    /// A verb narrows a path, and a side that names no verb still answers.
    #[test]
    fn wire_narrows_a_path_to_one_verb() {
        let db = routed();
        let get = wire(
            &db,
            Wire {
                plane: "code",
                name: "/bridge/check",
                verb: Some("get"),
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(get.contains("web/src/api.check"), "{get}");
        assert!(
            get.contains("web/src/page.load"),
            "a `*` request answers every verb: {get}"
        );
        let post = wire(
            &db,
            Wire {
                plane: "code",
                name: "/bridge/check",
                verb: Some("POST"),
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(!post.contains("web/src/api.check"), "{post}");
        assert!(post.contains("web/src/page.load"), "{post}");
        assert!(
            post.contains("nothing in this plane serves it with POST"),
            "a declaration this call filtered out is not one missing: {post}"
        );
    }

    /// `context` shows the route properties it holds and names the verb that
    /// matches them, because the other side is a question about the plane
    /// rather than a fact about this symbol.
    #[test]
    fn context_names_wire_for_a_symbol_that_states_a_route() {
        let db = routed();
        let plane = db.plane("code").unwrap();
        let out = context(&plane, "com.acme.BridgeController.check").unwrap();
        assert!(out.contains("serves"), "{out}");
        assert!(
            out.contains("`wire com.acme.BridgeController.check` names what is on the other side"),
            "{out}"
        );
        let plain = context(&plane, "BridgeController.helper").unwrap();
        assert!(
            !plain.contains("states an HTTP route"),
            "a symbol stating none is told nothing: {plain}"
        );
    }

    /// The two halves in two planes, as they actually sit: a service
    /// repository and a frontend repository, digested separately.
    fn split() -> Database {
        let db = Database::in_memory().unwrap();
        let route = |plane: &str, key: &str, label: &str, file: &str, prop: &str, r: &str| {
            let p = match db.plane(plane) {
                Ok(p) => p,
                Err(_) => db.create_plane(plane, Properties::new()).unwrap(),
            };
            let mut txn = p.write().unwrap();
            let mut props = Properties::new();
            props.insert("file".into(), PropDesc::new(PropValue::Str(file.into())));
            props.insert("line".into(), PropDesc::new(PropValue::Int(7)));
            props.insert(
                prop.into(),
                PropDesc::new(PropValue::List(vec![PropValue::Str(r.into())])),
            );
            txn.create_node_with_key(key, &[label], props).unwrap();
            txn.commit().unwrap();
        };
        route(
            "api",
            "com.acme.Orders.one",
            "Method",
            "src/Orders.java",
            "serves",
            "GET /api/orders/{}",
        );
        route(
            "web",
            "web/src/client.loadOne",
            "Function",
            "src/client.ts",
            "requests",
            "GET /api/orders/{}",
        );
        db
    }

    /// The route the issue is about, with its halves in two graphs: attached,
    /// `wire` matches them and every row names the graph it came from.
    #[test]
    fn wire_matches_across_an_attached_plane() {
        let db = split();
        let alone = wire(
            &db,
            Wire {
                plane: "api",
                name: "/api/orders",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(
            alone.contains("nothing in this plane asks for it"),
            "{alone}"
        );
        assert!(!alone.contains("across"), "unattached, nothing is crossed");

        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("web", None, None),
        )
        .unwrap();
        let out = wire(
            &db,
            Wire {
                plane: "api",
                name: "/api/orders",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(out.starts_with("across  api, web\n"), "{out}");
        assert!(
            out.contains("serves    GET     com.acme.Orders.one  Method  src/Orders.java:7\n"),
            "a row from this plane names no graph: {out}"
        );
        assert!(
            out.contains(
                "requests  GET     web/src/client.loadOne  Function  src/client.ts:7  in web\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("1 request(s) matched a declaration, 0 reached none"),
            "{out}"
        );
    }

    /// Either end asks the same question: the caller lives in the attached
    /// graph, and naming it still finds what serves what it asks for.
    #[test]
    fn wire_resolves_a_symbol_the_attachment_holds() {
        let db = split();
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("web", None, None),
        )
        .unwrap();
        let out = wire(
            &db,
            Wire {
                plane: "api",
                name: "client.loadOne",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(out.contains("path  /api/orders/{}"), "{out}");
        assert!(
            out.contains("serves    GET     com.acme.Orders.one"),
            "{out}"
        );

        let missing = wire(
            &db,
            Wire {
                plane: "api",
                name: "nothing.at.all",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(missing.contains("no symbol matches"), "{missing}");
        assert!(missing.contains("nor in the attached: web"), "{missing}");
    }

    /// An attachment it could not read is named, because an empty answer from
    /// a graph that was never asked is not an answer.
    #[test]
    fn wire_names_the_attachment_it_could_not_read() {
        let db = split();
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("front", None, Some("http://127.0.0.1:1".into())),
        )
        .unwrap();
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("web", None, None),
        )
        .unwrap();
        db.drop_plane(db.plane("web").unwrap().id()).unwrap();
        let out = wire(
            &db,
            Wire {
                plane: "api",
                name: "/api/orders",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert_eq!(
            out.lines().take(3).collect::<Vec<_>>(),
            [
                "across  api",
                "not read  front — served at http://127.0.0.1:1",
                "not read  web — no plane 'web' on this instance",
            ],
            "{out}"
        );
        assert!(out.contains("nothing in this plane asks for it"), "{out}");
    }

    /// A reader standing in for another drsg: it runs the plan against a
    /// second database, which is exactly what the HTTP one does over the wire.
    struct Elsewhere(Database);

    impl crate::attach::Remote for Elsewhere {
        fn nodes(
            &self,
            at: &crate::attach::Attachment,
            plan: &LogicalPlan,
        ) -> std::result::Result<Vec<NodeRecord>, String> {
            let p = self.0.plane(&at.plane).map_err(|e| e.to_string())?;
            p.query_from_plan(plan.clone())
                .nodes()
                .map_err(|e| e.to_string())
        }
    }

    /// A reader that cannot reach what it was asked for.
    struct Unreachable;

    impl crate::attach::Remote for Unreachable {
        fn nodes(
            &self,
            _at: &crate::attach::Attachment,
            _plan: &LogicalPlan,
        ) -> std::result::Result<Vec<NodeRecord>, String> {
            Err("connection refused".into())
        }
    }

    /// One plane, holding only one side of a route and one unrelated request —
    /// what a second drsg answers with.
    fn served_elsewhere() -> Database {
        let db = Database::in_memory().unwrap();
        let p = db.create_plane("web", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let mut node = |key: &str, file: &str, route: &str| {
            let mut props = Properties::new();
            props.insert("file".into(), PropDesc::new(PropValue::Str(file.into())));
            props.insert("line".into(), PropDesc::new(PropValue::Int(3)));
            props.insert(
                "requests".into(),
                PropDesc::new(PropValue::List(vec![PropValue::Str(route.into())])),
            );
            txn.create_node_with_key(key, &["Function"], props).unwrap()
        };
        // First, so it takes the same node id the other database gave its
        // handler — the collision the site key exists to survive.
        node("web/src/other.ping", "src/other.ts", "GET /health");
        node(
            "web/src/client.loadOne",
            "src/client.ts",
            "GET /api/orders/{}",
        );
        txn.commit().unwrap();
        db
    }

    /// One drsg holding the handler, another the caller: given a reader, the
    /// two halves meet and the remote row names the graph it came from.
    #[test]
    fn wire_folds_in_a_plane_another_drsg_serves() {
        let db = split();
        let far = served_elsewhere();
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("front", Some("web".into()), Some("http://h:1".into())),
        )
        .unwrap();

        let ask = |remote: Option<&dyn crate::attach::Remote>| {
            wire(
                &db,
                Wire {
                    plane: "api",
                    name: "/api/orders",
                    verb: None,
                    remote,
                    instance: None,
                },
            )
            .unwrap()
        };

        // Without a reader the attachment is named unread, never skipped.
        let alone = ask(None);
        assert!(
            alone.starts_with("across  api\nnot read  front — served at http://h:1\n"),
            "{alone}"
        );
        assert!(
            alone.contains("nothing in this plane asks for it"),
            "{alone}"
        );

        let reader = Elsewhere(far);
        let out = ask(Some(&reader));
        assert!(out.starts_with("across  api, front\n"), "{out}");
        assert!(
            out.contains(
                "requests  GET     web/src/client.loadOne  Function  src/client.ts:3  in front\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("1 request(s) matched a declaration, 0 reached none"),
            "{out}"
        );
        // `/health` is stated only by the remote graph and nothing here asks
        // about it, so it is not an answer to this question.
        assert!(!out.contains("/health"), "{out}");
    }

    /// A node id means nothing across two drsg instances, so a symbol here is
    /// never matched against a row that merely shares its id there.
    #[test]
    fn wire_keeps_a_remote_node_id_apart_from_its_own() {
        let db = split();
        let far = served_elsewhere();
        assert_eq!(
            db.plane("api")
                .unwrap()
                .node_by_key("com.acme.Orders.one")
                .unwrap()
                .unwrap()
                .id,
            far.plane("web")
                .unwrap()
                .node_by_key("web/src/other.ping")
                .unwrap()
                .unwrap()
                .id,
            "the fixtures must collide for this to prove anything"
        );
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("front", Some("web".into()), Some("http://h:1".into())),
        )
        .unwrap();
        let reader = Elsewhere(far);
        let out = wire(
            &db,
            Wire {
                plane: "api",
                name: "com.acme.Orders.one",
                verb: None,
                instance: None,
                remote: Some(&reader),
            },
        )
        .unwrap();
        assert!(out.contains("path  /api/orders/{}"), "{out}");
        assert!(
            !out.contains("/health"),
            "an id collision is not a route: {out}"
        );
        assert!(out.contains("1 path(s)"), "{out}");
    }

    /// A reader that fails says why, in place of the attachment's rows.
    #[test]
    fn wire_reports_why_it_could_not_read_elsewhere() {
        let db = split();
        crate::attach::attach(
            &db,
            "api",
            crate::attach::Attachment::new("front", Some("web".into()), Some("http://h:1".into())),
        )
        .unwrap();
        let out = wire(
            &db,
            Wire {
                plane: "api",
                name: "/api/orders",
                verb: None,
                instance: None,
                remote: Some(&Unreachable),
            },
        )
        .unwrap();
        assert!(
            out.starts_with("across  api\nnot read  front — connection refused\n"),
            "{out}"
        );
    }

    /// A symbol that states no route, and a plane where nothing does, each
    /// say so and name the next call.
    #[test]
    fn wire_says_when_there_is_nothing_to_match() {
        let db = routed();
        let out = wire(
            &db,
            Wire {
                plane: "code",
                name: "BridgeController.helper",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(out.contains("states no route"), "{out}");
        let bare = seeded();
        let out = wire(
            &bare,
            Wire {
                plane: "code",
                name: "/anything",
                verb: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        assert!(out.contains("no routes recorded in this plane"), "{out}");
        assert!(out.contains("drsg plugin install"), "{out}");
    }

    /// The query form this crate computes is the form the plugins write.
    ///
    /// The two normalisers live in different repositories — this one for what
    /// a person types, the extensions SDK's for what a parser reads — so the
    /// cases its own tests assert are pinned here too. A drift between them
    /// is a path that silently matches nothing.
    #[test]
    fn the_query_form_matches_what_the_plugins_write() {
        for written in [
            "/orders/{id}",
            "/orders/{id:[0-9]+}",
            "/orders/:id",
            "/orders/${id}",
            "/orders/<int:id>",
            "/orders/*",
        ] {
            assert_eq!(normalize_query(written), "/orders/{}", "{written}");
        }
        assert_eq!(normalize_query("/files/report-${id}.pdf"), "/files/{}");
        assert_eq!(normalize_query("/api//orders/"), "/api/orders");
        assert_eq!(normalize_query("/api/orders?page=2"), "/api/orders");
        assert_eq!(normalize_query("/api/orders#top"), "/api/orders");
        assert_eq!(
            normalize_query("https://api.example.com/v1/orders/{id}"),
            "/v1/orders/{}"
        );
        assert_eq!(normalize_query("https://api.example.com"), "/");
        assert_eq!(normalize_query("/"), "/");
    }

    /// A shared library and the repositories that depend on it — the shape
    /// issue #43 names. Both joins are present: `common`'s own manifest
    /// declares the coordinate it publishes, and `orders` both declares that
    /// coordinate and stands in for the class by name.
    fn library_and_dependents() -> Database {
        let db = Database::in_memory().unwrap();
        library_plane(&db);
        dependents_of(&db);
        db
    }

    /// The shared library: the coordinate it publishes, a base class, and a
    /// subclass of it in the same tree — so the radius offers two keys.
    fn library_plane(db: &Database) {
        let p = db.create_plane("common", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        txn.create_node_with_key(
            "pom.xml",
            &[MANIFEST_LABEL],
            manifest("pom.xml", "com.acme:common-base"),
        )
        .unwrap();
        let base = txn
            .create_node_with_key(
                "com.acme.common.BaseEntity",
                &["Class"],
                at("src/BaseEntity.java", 7),
            )
            .unwrap();
        let dated = txn
            .create_node_with_key("com.acme.common.Dated", &["Class"], at("src/Dated.java", 4))
            .unwrap();
        txn.create_edge(dated, base, "EXTENDS", Properties::new())
            .unwrap();
        txn.commit().unwrap();
    }

    fn at(file: &str, line: i64) -> Properties {
        let mut props = Properties::new();
        props.insert("file".into(), PropDesc::new(PropValue::Str(file.into())));
        props.insert("line".into(), PropDesc::new(PropValue::Int(line)));
        props
    }

    fn manifest(file: &str, coordinate: &str) -> Properties {
        let mut props = at(file, 1);
        props.insert(
            DECLARES_PROP.into(),
            PropDesc::new(PropValue::Str(coordinate.into())),
        );
        props
    }

    fn declared(version: &str, scope: &str) -> Properties {
        let mut props = Properties::new();
        props.insert(
            "version".into(),
            PropDesc::new(PropValue::Str(version.into())),
        );
        props.insert("scope".into(), PropDesc::new(PropValue::Str(scope.into())));
        props
    }

    /// The three repositories around [`library_and_dependents`]'s library:
    /// one that declares the dependency and uses it, one that declares it and
    /// uses nothing, and one holding the same code again.
    fn dependents_of(db: &Database) {
        // A dependent that declares the coordinate *and* names both classes.
        let p = db.create_plane("orders", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let pom = txn
            .create_node_with_key(
                "backend/pom.xml",
                &[MANIFEST_LABEL],
                manifest("backend/pom.xml", "svc.orders:orders"),
            )
            .unwrap();
        let dep = txn
            .create_node_with_key(
                "com.acme:common-base",
                &["Package", EXTERNAL_LABEL],
                Properties::new(),
            )
            .unwrap();
        txn.create_edge(pom, dep, DEPENDS_EDGE, declared("2.4.1", "runtime"))
            .unwrap();
        let stand_in = txn
            .create_node_with_key(
                "com.acme.common.BaseEntity",
                &["Class", EXTERNAL_LABEL],
                Properties::new(),
            )
            .unwrap();
        let order = txn
            .create_node_with_key("svc.orders.Order", &["Class"], at("src/Order.java", 9))
            .unwrap();
        txn.create_edge(order, stand_in, "EXTENDS", Properties::new())
            .unwrap();
        let far_key = txn
            .create_node_with_key(
                "com.acme.common.Dated",
                &["Class", EXTERNAL_LABEL],
                Properties::new(),
            )
            .unwrap();
        txn.create_edge(order, far_key, "USES_TYPE", Properties::new())
            .unwrap();
        // Containment is not a reference: it would otherwise count the
        // package a stand-in sits under as a symbol that uses it.
        txn.create_edge(dep, stand_in, "CONTAINS", Properties::new())
            .unwrap();
        txn.commit().unwrap();

        // A dependent that declares the dependency and uses none of it.
        let p = db.create_plane("billing", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        let pom = txn
            .create_node_with_key(
                "pom.xml",
                &[MANIFEST_LABEL],
                manifest("pom.xml", "svc.billing:billing"),
            )
            .unwrap();
        let dep = txn
            .create_node_with_key(
                "com.acme:common-base",
                &["Package", EXTERNAL_LABEL],
                Properties::new(),
            )
            .unwrap();
        txn.create_edge(pom, dep, DEPENDS_EDGE, declared("2.3.0", "runtime"))
            .unwrap();
        txn.commit().unwrap();

        // The same class again, declared rather than referenced.
        let p = db.create_plane("vendored", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        txn.create_node_with_key(
            "com.acme.common.BaseEntity",
            &["Class"],
            at("vendor/BaseEntity.java", 7),
        )
        .unwrap();
        txn.commit().unwrap();

        // A history plane, which holds commits rather than code.
        let p = db.create_plane("common_git", Properties::new()).unwrap();
        let mut txn = p.write().unwrap();
        txn.create_node_with_key("abc123", &["Commit"], Properties::new())
            .unwrap();
        txn.commit().unwrap();
    }

    fn radius_across(
        db: &Database,
        scope: Scope,
        remote: Option<&dyn crate::attach::Remote>,
    ) -> String {
        impact(
            db,
            Impact {
                plane: "common",
                name: "com.acme.common.BaseEntity",
                depth: 3,
                across: Some(scope),
                remote,
                instance: None,
            },
        )
        .unwrap()
    }

    /// The question issue #43 asks: a machine holding every repository
    /// answers across them with nothing attached, by the two keys they
    /// already state.
    #[test]
    fn impact_across_every_plane_names_the_repositories_a_change_reaches() {
        let db = library_and_dependents();
        let out = radius_across(&db, Scope::All, None);

        // The local answer is still the first thing said.
        assert!(out.contains("depth 1 (1):"), "{out}");
        assert!(out.contains("total affected within depth 3: 1"), "{out}");

        assert!(
            out.contains("across  common, orders, billing, vendored\n"),
            "{out}"
        );
        // A history plane holds commits, not code; an empty one holds
        // nothing at all, and every database carries `startup`.
        assert!(!out.contains("common_git"), "{out}");
        assert!(!out.contains("startup"), "{out}");

        // The build join: the coordinate this plane's own manifest publishes,
        // with the version and scope the dependent's edge states.
        assert!(
            out.contains("  declares  com.acme:common-base  2.4.1  [runtime]  backend/pom.xml\n"),
            "{out}"
        );
        assert!(
            out.contains("  declares  com.acme:common-base  2.3.0  [runtime]  pom.xml\n"),
            "{out}"
        );

        // The code join, on the changed symbol and on a key its radius holds.
        assert!(
            out.contains("  com.acme.common.BaseEntity  \u{2190} 1\n"),
            "{out}"
        );
        assert!(
            out.contains("    svc.orders.Order  Class  src/Order.java:9  [EXTENDS]\n"),
            "{out}"
        );
        assert!(
            out.contains("  com.acme.common.Dated  \u{2190} 1\n"),
            "{out}"
        );

        // The same declaration held twice is not a dependent.
        assert!(
            out.contains(
                "  holds its own  com.acme.common.BaseEntity  Class  vendor/BaseEntity.java:7\n"
            ),
            "{out}"
        );

        assert!(
            out.contains(
                "3 graph(s) read, 3 hold(s) something: 2 declare(s) a dependency, \
                 1 reach(es) it in code\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("2 of the 2 key(s) offered are referenced elsewhere\n"),
            "{out}"
        );
    }

    /// The two issues meet here: a machine holding every repository in its
    /// own store (#44) answers "which repositories would this change break"
    /// (#43) across all of them, with nothing attached and nothing
    /// re-digested.
    #[test]
    fn across_all_spans_the_databases_this_process_mounts() {
        let tmp = tempfile::tempdir().unwrap();
        // The library in one store, its two dependents in another — the
        // shape of thirty-four repositories digested one at a time.
        let lib = tmp.path().join("commonlib");
        {
            let db = Database::open(&lib).unwrap();
            library_plane(&db);
            db.save_sidecars();
        }
        let deps = tmp.path().join("services");
        {
            let db = Database::open(&deps).unwrap();
            dependents_of(&db);
            db.save_sidecars();
        }

        let inst = crate::Instance::new(Arc::new(Database::open(&lib).unwrap()));
        inst.mount(crate::Mounting {
            path: &deps,
            as_prefix: None,
            grant: None,
            remember: false,
        })
        .unwrap();

        let out = impact(
            inst.host(),
            Impact {
                plane: "common",
                name: "com.acme.common.BaseEntity",
                depth: 3,
                across: Some(Scope::All),
                remote: None,
                instance: Some(&inst),
            },
        )
        .unwrap();

        // Both dependents are in another database entirely, and both answer.
        assert!(
            out.contains("across  common, orders, billing, vendored\n"),
            "{out}"
        );
        assert!(
            out.contains("  declares  com.acme:common-base  2.4.1  [runtime]  backend/pom.xml\n"),
            "{out}"
        );
        assert!(
            out.contains("    svc.orders.Order  Class  src/Order.java:9  [EXTENDS]\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "3 graph(s) read, 3 hold(s) something: 2 declare(s) a dependency, \
                          1 reach(es) it in code\n"
            ),
            "{out}"
        );

        // Without the instance it is the library's own store alone — which is
        // the answer that was available before one process served both.
        let alone = impact(
            inst.host(),
            Impact {
                plane: "common",
                name: "com.acme.common.BaseEntity",
                depth: 3,
                across: Some(Scope::All),
                remote: None,
                instance: None,
            },
        )
        .unwrap();
        assert!(alone.ends_with(NO_GRAPHS), "{alone}");
    }

    /// Containment is not a reference: the package a stand-in sits under did
    /// not ask for it.
    #[test]
    fn across_counts_references_not_the_container_of_a_stand_in() {
        let db = library_and_dependents();
        let out = radius_across(&db, Scope::All, None);
        assert!(!out.contains("[CONTAINS]"), "{out}");
    }

    /// The default scope is what was attached, which is one graph here and
    /// not every plane on the instance.
    #[test]
    fn across_the_attached_reads_those_and_no_others() {
        let db = library_and_dependents();
        crate::attach::attach(
            &db,
            "common",
            crate::attach::Attachment::new("orders", None, None),
        )
        .unwrap();
        let out = radius_across(&db, Scope::Attached, None);
        assert!(out.contains("across  common, orders\n"), "{out}");
        assert!(!out.contains("billing"), "{out}");
        assert!(!out.contains("vendored"), "{out}");
        assert!(
            out.contains("1 graph(s) read, 1 hold(s) something"),
            "{out}"
        );
    }

    /// Asked for nothing, the answer is what it has always been — no extra
    /// section, and no plane read but its own.
    #[test]
    fn without_a_scope_the_answer_is_unchanged() {
        let db = library_and_dependents();
        let alone = impact(
            &db,
            Impact {
                plane: "common",
                name: "com.acme.common.BaseEntity",
                depth: 3,
                across: None,
                instance: None,
                remote: None,
            },
        )
        .unwrap();
        let across = radius_across(&db, Scope::All, None);
        assert!(!alone.contains("across"), "{alone}");
        assert!(across.starts_with(&alone), "{across}");
    }

    /// Nothing in scope says so, and names the two ways to widen it.
    #[test]
    fn across_says_when_there_is_nothing_to_cross_to() {
        let db = library_and_dependents();
        let out = radius_across(&db, Scope::Attached, None);
        assert!(out.ends_with(NO_GRAPHS), "{out}");
    }

    /// A plane another drsg serves answers with nodes, so it reports the keys
    /// it holds and says where the rest is read.
    #[test]
    fn a_plane_another_drsg_serves_reports_the_keys_it_holds() {
        let db = library_and_dependents();
        let far = library_and_dependents();
        crate::attach::attach(
            &db,
            "common",
            crate::attach::Attachment::new("svc", Some("orders".into()), Some("http://h:1".into())),
        )
        .unwrap();
        let reader = Elsewhere(far);
        let out = radius_across(&db, Scope::Attached, Some(&reader));
        assert!(out.contains("across  common, svc\n"), "{out}");
        assert!(out.contains("in svc  at http://h:1\n"), "{out}");
        assert!(
            out.contains(
                "  declares  com.acme:common-base  (the manifest and its version are read there)\n"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "  com.acme.common.BaseEntity  \u{2190} referenced; \
                 what references it is read there\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("  ask that drsg for the rest: `impact <key> --plane orders`\n"),
            "{out}"
        );
    }

    /// An attachment that cannot be reached is reported, never skipped — a
    /// radius that crossed nothing must not read as a radius that reached
    /// nothing.
    #[test]
    fn across_reports_a_graph_it_could_not_read() {
        let db = library_and_dependents();
        crate::attach::attach(
            &db,
            "common",
            crate::attach::Attachment::new("svc", Some("orders".into()), Some("http://h:1".into())),
        )
        .unwrap();
        let out = radius_across(&db, Scope::Attached, Some(&Unreachable));
        assert!(
            out.contains("not read  svc — connection refused\n"),
            "{out}"
        );
        // Unknown, not empty: no tally is offered for a scope nothing was
        // read from.
        assert!(out.ends_with(UNREAD_GRAPHS), "{out}");
        // Without a reader at all it is named unread for the other reason.
        let unasked = radius_across(&db, Scope::Attached, None);
        assert!(
            unasked.contains("not read  svc — served at http://h:1\n"),
            "{unasked}"
        );
    }
}
