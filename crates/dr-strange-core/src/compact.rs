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

/// The honesty footer every call listing carries: what a recorded edge set
/// can and cannot claim.
const CALLS_NOTE: &str = "note: recorded call edges only — calls the parser could not resolve \
     (dynamic dispatch, untyped receivers) are absent, so this is a lower \
     bound.\n";

/// Most entries printed per edge group before eliding with a count.
const GROUP_CAP: usize = 20;

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
        Resolved::None => return Ok(format!("no symbol matches `{name}` in this plane\n")),
    };
    let mut out = describe_record(plane, &node)?;

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
            }
            groups.entry((tag, edge.ty.clone())).or_default().push(line);
        }
    }

    let mut section = |title: &str, key: (&'static str, String), out: &mut String| {
        if let Some(lines) = groups.remove(&key) {
            out.push_str(&format!("{title} ({}):\n", lines.len()));
            for l in lines.iter().take(GROUP_CAP) {
                out.push_str("  ");
                out.push_str(l);
                out.push('\n');
            }
            if lines.len() > GROUP_CAP {
                out.push_str(&format!("  … and {} more\n", lines.len() - GROUP_CAP));
            }
        }
    };
    section("contained by", ("in", "CONTAINS".into()), &mut out);
    section("callers", ("in", "CALLS".into()), &mut out);
    section("callees", ("out", "CALLS".into()), &mut out);
    section("contains", ("out", "CONTAINS".into()), &mut out);
    // Whatever edge vocabulary remains (IMPLEMENTS, EXTENDS, HAS_METHOD,
    // IMPORTS, STYLED_BY, …) renders under its own name, direction marked.
    let rest: Vec<((&'static str, String), Vec<String>)> = groups.into_iter().collect();
    for ((dir, ty), lines) in rest {
        let arrow = if dir == "in" { "←" } else { "→" };
        let mut buf = format!("{ty} {arrow} ({}):\n", lines.len());
        for l in lines.iter().take(GROUP_CAP) {
            buf.push_str("  ");
            buf.push_str(l);
            buf.push('\n');
        }
        if lines.len() > GROUP_CAP {
            buf.push_str(&format!("  … and {} more\n", lines.len() - GROUP_CAP));
        }
        out.push_str(&buf);
    }
    if had_calls {
        out.push_str(CALLS_NOTE);
    }
    Ok(out)
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
pub fn describe(plane: &PlaneHandle<'_>, name: &str) -> Result<String> {
    let node = match resolve(plane, name)? {
        Resolved::One(n) => n,
        Resolved::Many(hits) => return Ok(candidates(name, &hits)),
        Resolved::None => return Ok(format!("no symbol matches `{name}` in this plane\n")),
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
