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

/// How long a marker sits before the rebuild that set it is presumed dead.
///
/// Ten minutes is far past an honest fold — a forty-thousand-node plane
/// rebuilds in seconds — so the only thing this reclassifies is a rebuild that
/// is not coming back.
const REBUILD_PRESUMED_DEAD: i64 = 600;

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
    // Past a point, "ask again when it finishes" is advice for something that
    // is not going to happen: a rebuild that failed or was killed leaves the
    // marker behind — deliberately, see above — and only its age distinguishes
    // that from one still running. Saying which costs one comparison and is
    // the difference between a plane a caller waits on and one a caller fixes.
    if ago >= REBUILD_PRESUMED_DEAD {
        return Some(format!(
            "note: this plane was left mid-rebuild {ago}s ago and nothing has \
             finished it — it holds only what had been folded by then, so a miss \
             here may mean \"never folded\" rather than \"not present\". Re-run \
             the rebuild (`serve watch --force`) to replace it.\n"
        ));
    }
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

    /// A marker outlives the process that set it — on purpose, because a plane
    /// left half-folded is exactly what a caller needs warning about. But past
    /// a point "ask again when it finishes" is advice about something that is
    /// never going to finish, and the note has to say so instead.
    #[test]
    fn an_abandoned_rebuild_stops_telling_callers_to_wait() {
        let db = seeded();

        mark_rebuilding(&db, now() - REBUILD_PRESUMED_DEAD + 5);
        let waiting = context(&db.plane("code").unwrap(), "m::nope").unwrap();
        assert!(waiting.contains("being rebuilt"), "{waiting}");

        mark_rebuilding(&db, now() - REBUILD_PRESUMED_DEAD - 5);
        let stalled = context(&db.plane("code").unwrap(), "m::nope").unwrap();
        assert!(stalled.contains("no symbol matches"), "{stalled}");
        assert!(
            stalled.contains("left mid-rebuild") && stalled.contains("--force"),
            "an abandoned rebuild should name the fix, not ask for patience: {stalled}"
        );
        assert!(
            !stalled.contains("ask again when it finishes"),
            "nothing is going to finish it: {stalled}"
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
