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

use crate::api::PlaneHandle;
use crate::error::Result;
use crate::types::{NodeRecord, PropValue, Properties};

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

fn prop_str<'a>(props: &'a Properties, key: &str) -> Option<&'a str> {
    match props.get(key).map(|d| &d.value) {
        Some(PropValue::Str(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn prop_int(props: &Properties, key: &str) -> Option<i64> {
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
        (Some(f), Some(l)) => format!("{f}:{l}"),
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
    Ok(match hits.len() {
        0 => Resolved::None,
        1 => Resolved::One(hits.into_iter().next().unwrap()),
        _ => Resolved::Many(hits),
    })
}

fn candidates(name: &str, hits: &[NodeRecord]) -> String {
    let mut out = format!(
        "`{name}` is ambiguous — {} matches; call again with an exact key:\n",
        hits.len()
    );
    for n in hits.iter().take(FIND_CAP) {
        out.push_str(&one_line(n));
        out.push('\n');
    }
    if hits.len() > FIND_CAP {
        out.push_str(&format!("… and {} more\n", hits.len() - FIND_CAP));
    }
    out
}

/// The property a full rebuild stamps on the plane while it refills it, and
/// clears when it finishes. Written by the digest side; read here.
pub const REBUILDING_PROP: &str = "rebuilding_since";

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
fn synced_note(plane: &PlaneHandle<'_>) -> Result<Option<String>> {
    let props = plane.properties()?;
    if let Some(note) = rebuilding_note(&props) {
        return Ok(Some(note));
    }
    Ok(props.get("synced_commit").and_then(|d| match &d.value {
        crate::PropValue::Str(commit) => Some(format!(
            "synced: commit {}\n",
            &commit[..12.min(commit.len())]
        )),
        _ => None,
    }))
}

/// What to say when a lenient lookup found nothing.
///
/// Its whole job is to keep "not present" and "not yet folded" apart; every
/// verb routes its empty case through here so no surface can quietly report
/// absence during a rebuild.
pub fn no_match(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let mut out = format!("no symbol matches `{name}` in this plane\n");
    if let Some(note) = rebuilding_note(&plane.properties()?) {
        out.push_str(&note);
    }
    Ok(out)
}

/// The honesty footer every call listing carries: what a recorded edge set
/// can and cannot claim.
const CALLS_NOTE: &str = "note: recorded call edges only — calls the parser could not resolve \
     (dynamic dispatch, untyped receivers) are absent, so this is a lower \
     bound.\n";

/// Most entries printed per edge group before eliding with a count.
const GROUP_CAP: usize = 20;

/// Ceiling on a whole `context` reply, in characters. A hub node (a module
/// containing hundreds of symbols, a base type everything calls) must not
/// flood the caller's context window: when the full rendering exceeds this,
/// the per-group cap shrinks until it fits, and every elision names the
/// count it hides.
const CONTEXT_BUDGET: usize = 24_000;

/// `context` — the primary agent verb: one symbol's whole neighborhood in a
/// single round trip. The head and properties are [`describe`]'s; then the
/// graph around it — who contains it, who calls it (with call sites), what
/// it calls, and every other edge type grouped — so callers/callees/what-is
/// questions are all answered by this one call, and the agent never has to
/// choose among narrower verbs. Ambiguity returns the candidate list, which
/// is itself the useful one-call reply.
pub fn context(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let node = match resolve(plane, name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(name, &hits)),
        Resolved::None => return no_match(plane, name),
    };
    let mut out = describe_record(plane, &node)?;
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }

    // Group every edge by (direction, type). CONTAINS-in is rendered as the
    // parent ("contained by"); CALLS carry their call-site lines.
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(&'static str, String), Vec<String>> = BTreeMap::new();
    let mut had_calls = false;
    for (dir, tag) in [(crate::Dir::In, "in"), (crate::Dir::Out, "out")] {
        for hop in plane.neighbors(node.id, dir, None)? {
            let Some(other) = plane.node(hop.node)? else {
                continue;
            };
            let Some(edge) = plane.edge(hop.edge)? else {
                continue;
            };
            let mut line = one_line(&other);
            if edge.ty == "CALLS" {
                had_calls = true;
                if let Some(l) = prop_int(&edge.properties, "line") {
                    line.push_str(&format!("  call@{l}"));
                }
                if hop.node == node.id {
                    line.push_str("  (self)");
                }
                // The unresolved ledger (P1): a boundary is announced, not
                // hidden — the reason travels on the edge.
                if other.labels.first().map(String::as_str) == Some("UnresolvedRef")
                    && let Some(reason) = prop_str(&edge.properties, "_reason")
                {
                    line.push_str(&format!("  [{reason}]"));
                }
            }
            groups.entry((tag, edge.ty.clone())).or_default().push(line);
        }
    }

    // Sections in fixed order: the named four first, then whatever edge
    // vocabulary remains (IMPLEMENTS, EXTENDS, HAS_METHOD, IMPORTS, …) under
    // its own name, direction marked.
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for (title, key) in [
        ("contained by", ("in", "CONTAINS")),
        ("callers", ("in", "CALLS")),
        ("callees", ("out", "CALLS")),
        ("contains", ("out", "CONTAINS")),
    ] {
        if let Some(lines) = groups.remove(&(key.0, key.1.to_string())) {
            sections.push((title.to_string(), lines));
        }
    }
    for ((dir, ty), lines) in std::mem::take(&mut groups) {
        let arrow = if dir == "in" { "←" } else { "→" };
        sections.push((format!("{ty} {arrow}"), lines));
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
            buf.push_str(CALLS_NOTE);
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
fn calls_path(
    plane: &PlaneHandle<'_>,
    from: crate::NodeId,
    to: crate::NodeId,
) -> Result<Option<Vec<crate::NodeId>>> {
    use std::collections::{BTreeMap, VecDeque};
    const MAX_VISITED: usize = 20_000;
    let mut prev: BTreeMap<crate::NodeId, crate::NodeId> = BTreeMap::new();
    let mut queue = VecDeque::from([from]);
    let mut seen = std::collections::BTreeSet::from([from]);
    while let Some(node) = queue.pop_front() {
        if node == to {
            let mut path = vec![to];
            let mut cur = to;
            while let Some(&p) = prev.get(&cur) {
                path.push(p);
                cur = p;
            }
            path.reverse();
            return Ok(Some(path));
        }
        if seen.len() > MAX_VISITED {
            break;
        }
        for hop in plane.neighbors(node, crate::Dir::Out, Some("CALLS"))? {
            if seen.insert(hop.node) {
                prev.insert(hop.node, node);
                queue.push_back(hop.node);
            }
        }
    }
    Ok(None)
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

/// `impact` — the blast radius: what reaches this symbol, breadth-first over
/// INCOMING structural edges (CALLS, REFERENCES, INSTANTIATES, IMPORTS,
/// EXTENDS, IMPLEMENTS), grouped by distance, counts always exact even when
/// listings elide.
pub fn impact(plane: &PlaneHandle<'_>, name: &str, depth: usize) -> Result<String> {
    const LEVEL_CAP: usize = 20;
    const IMPACT_EDGES: &[&str] = &[
        "CALLS",
        "REFERENCES",
        "INSTANTIATES",
        "IMPORTS",
        "EXTENDS",
        "IMPLEMENTS",
    ];
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
    let mut seen = std::collections::BTreeSet::from([node.id]);
    let mut frontier = vec![node.id];
    let mut total = 0usize;
    for level in 1..=depth {
        let mut next: Vec<(crate::NodeId, String)> = Vec::new();
        for id in &frontier {
            for hop in plane.neighbors(*id, crate::Dir::In, None)? {
                let Some(edge) = plane.edge(hop.edge)? else {
                    continue;
                };
                if !IMPACT_EDGES.contains(&edge.ty.as_str()) {
                    continue;
                }
                if seen.insert(hop.node)
                    && let Some(n) = plane.node(hop.node)?
                {
                    next.push((hop.node, format!("{}  [{}]", one_line(&n), edge.ty)));
                }
            }
        }
        if next.is_empty() {
            break;
        }
        total += next.len();
        out.push_str(&format!("depth {level} ({}):\n", next.len()));
        for (_, line) in next.iter().take(LEVEL_CAP) {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        if next.len() > LEVEL_CAP {
            out.push_str(&format!("  … and {} more\n", next.len() - LEVEL_CAP));
        }
        frontier = next.into_iter().map(|(id, _)| id).collect();
    }
    out.push_str(&format!("total affected within depth {depth}: {total}\n"));
    out.push_str(CALLS_NOTE);
    Ok(out)
}

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
    use std::collections::{BTreeMap, BTreeSet};

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

    // Breadth-first, so a truncated region is the nearest one.
    let mut seen = BTreeSet::from([node.id]);
    let mut per_level: Vec<usize> = Vec::new();
    let mut labels: BTreeMap<String, usize> = BTreeMap::new();
    let mut edges: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut degree: BTreeMap<crate::NodeId, usize> = BTreeMap::new();
    let mut counted: BTreeSet<crate::EdgeId> = BTreeSet::new();
    let mut frontier = vec![node.id];
    let mut budget_hit = false;
    tally_labels(&node, &mut labels);

    for _ in 1..=depth {
        let mut next = Vec::new();
        for &id in &frontier {
            for dir in [crate::Dir::Out, crate::Dir::In] {
                for hop in plane.neighbors(id, dir, None)? {
                    let Some(edge) = plane.edge(hop.edge)? else {
                        continue;
                    };
                    // Counted once, from whichever end reaches it first: a
                    // hop walked from both ends is one edge.
                    if counted.insert(hop.edge) {
                        let slot = edges.entry(edge.ty.clone()).or_default();
                        match dir {
                            crate::Dir::Out => slot.0 += 1,
                            _ => slot.1 += 1,
                        }
                        *degree.entry(id).or_default() += 1;
                        *degree.entry(hop.node).or_default() += 1;
                    }
                    if seen.len() >= FATHOM_BUDGET {
                        budget_hit = true;
                        continue;
                    }
                    if seen.insert(hop.node)
                        && let Some(n) = plane.node(hop.node)?
                    {
                        tally_labels(&n, &mut labels);
                        next.push(hop.node);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        per_level.push(next.len());
        frontier = next;
    }

    let reached = per_level.len();
    let region_edges: usize = counted.len();
    out.push_str(&format!(
        "region: {} hop{} out and in — {} nodes, {} edges\n",
        reached,
        if reached == 1 { "" } else { "s" },
        seen.len(),
        region_edges,
    ));
    out.push_str(&format!(
        "per hop: {}\n",
        per_level
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{}:{n}", i + 1))
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
            edges
                .iter()
                .map(|(ty, (o, i))| (format!("{ty} {} ({o} out, {i} in)", o + i), o + i))
                .collect(),
            FATHOM_GROUPS
        )
    ));

    // Ranked by degree *within the region*: a global degree would rank the
    // plane's hubs, not this region's.
    let mut hubs: Vec<(crate::NodeId, usize)> = degree.into_iter().collect();
    hubs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
    if !hubs.is_empty() {
        out.push_str("hubs (by edges inside the region):\n");
        for (id, deg) in hubs.iter().take(FATHOM_HUBS) {
            if let Some(n) = plane.node(*id)? {
                out.push_str(&format!("  {deg:>4}  {}\n", one_line(&n)));
            }
        }
    }

    out.push_str(&match (budget_hit, reached < depth) {
        // Which bound stopped the walk.
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
    });
    Ok(out)
}

/// Nodes one `fathom` walks before stopping and saying so: a hub two hops
/// from everything would otherwise pull in the plane.
const FATHOM_BUDGET: usize = 5_000;
/// Label and edge-type groups named before the rest are summed as "others".
const FATHOM_GROUPS: usize = 8;
/// Hubs listed.
const FATHOM_HUBS: usize = 5;

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

/// Commits shown when the caller names no limit.
const HISTORY_COMMITS: usize = 15;
/// Branches, tags and rebases shown before the listing is cut short.
const HISTORY_REFS: usize = 12;

fn prop_bool(props: &Properties, key: &str) -> bool {
    matches!(
        props.get(key).map(|d| &d.value),
        Some(PropValue::Bool(true))
    )
}

/// The date part of an ISO-8601 timestamp — `2026-08-25` out of
/// `2026-08-25T16:29:01+08:00`. History is read a day at a time; the seconds
/// are noise a reader pays for.
fn day(props: &Properties, key: &str) -> String {
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

    // Rendered as a block rather than line by line: the name column is as wide
    // as the widest name actually present, so `origin/feat/preprocessor-plugins`
    // does not push every sha beside it out of alignment.
    let ref_block = |nodes: &[NodeRecord], name_prop: &str, target_prop: &str| -> String {
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
    };

    if !branches.is_empty() {
        let mut sorted = branches.clone();
        // Local branches first, then remote-tracking: a reader is asking about
        // this checkout before they are asking about someone else's.
        sorted.sort_by_key(|b| {
            (
                prop_bool(&b.properties, "remote"),
                prop_str(&b.properties, "name").unwrap_or("").to_string(),
            )
        });
        out.push_str(&format!(
            "branches ({} shown of {}):\n",
            sorted.len().min(HISTORY_REFS),
            sorted.len()
        ));
        sorted.truncate(HISTORY_REFS);
        out.push_str(&ref_block(&sorted, "name", "tip"));
    }

    if !tags.is_empty() {
        let mut sorted = tags.clone();
        sorted.sort_by(|a, b| {
            prop_int(&b.properties, "tagged_ts")
                .unwrap_or(0)
                .cmp(&prop_int(&a.properties, "tagged_ts").unwrap_or(0))
        });
        out.push_str(&format!(
            "tags (newest {} of {}):\n",
            sorted.len().min(HISTORY_REFS),
            sorted.len()
        ));
        sorted.truncate(HISTORY_REFS);
        out.push_str(&ref_block(&sorted, "name", "target"));
    }

    if !rebases.is_empty() {
        out.push_str(&format!("rebases ({}):\n", rebases.len()));
        let mut sorted = rebases.clone();
        sorted.sort_by(|a, b| {
            prop_str(&b.properties, "finished_at")
                .unwrap_or_default()
                .cmp(prop_str(&a.properties, "finished_at").unwrap_or_default())
        });
        for r in sorted.iter().take(HISTORY_REFS) {
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
    }

    let limit = limit.unwrap_or(HISTORY_COMMITS).max(1);
    let mut newest = commits;
    newest.sort_by(|a, b| {
        prop_int(&b.properties, "committed_ts")
            .unwrap_or(0)
            .cmp(&prop_int(&a.properties, "committed_ts").unwrap_or(0))
    });
    out.push_str(&format!(
        "commits (newest {} of {}):\n",
        newest.len().min(limit),
        newest.len()
    ));
    for c in newest.iter().take(limit) {
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
    if let Some(note) = synced_note(plane)? {
        out.push_str(&note);
    }
    Ok(out)
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
        if k == "file" || k == "path" || k == "line" {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Database;
    use crate::types::PropDesc;

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
            impact(&plane, "m::nope", 3).unwrap(),
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
        let plane = db.plane("code").unwrap();
        let out = impact(&plane, "m::t", 3).unwrap();
        assert!(out.contains("depth 1 (2):"), "{out}");
        assert!(out.contains("depth 2 (1):"), "{out}");
        assert!(out.contains("total affected within depth 3: 3"), "{out}");
        assert!(
            out.contains("[CALLS]") && out.contains("[REFERENCES]"),
            "{out}"
        );
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
        // The seed holds the region together, and says by how much.
        assert!(out.contains("hubs (by edges inside the region):"), "{out}");
        let hub_line = out
            .lines()
            .find(|l| l.starts_with("  ") && l.contains("m::hub"))
            .unwrap_or_else(|| panic!("{out}"));
        assert!(hub_line.trim().starts_with('5'), "{out}");

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
}
