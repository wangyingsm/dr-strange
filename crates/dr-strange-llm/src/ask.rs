//! Natural-language querying (ROADMAP §3): turn an English/Chinese question
//! into a [`LogicalPlan`] the engine runs.
//!
//! When an [`Embedder`] is supplied this is an **agentic tool loop**: rather
//! than guess edge types and entity keys from the static schema, the model can
//! call two retrieval tools that ground it in the real graph —
//! - `find_edge(query)`: embed a relationship phrase and rank the plane's edge
//!   types by cosine similarity (so 「任职」 → `EMPLOYED_AT` cross-lingually);
//! - `find_entity(query, label?)`: embed a name/description and vector-search
//!   the plane's node embeddings for the matching node's real key + label.
//!
//! Each turn the model returns ONE JSON object: a tool call, or the final
//! `{"plan": …}`. We run tools and feed results back; a plan is deserialized
//! (read-only by construction), executed, and repaired on error — all within a
//! bounded step budget. Without an embedder it degrades to a single-shot,
//! schema-grounded prompt.

use anyhow::{Result, bail};
use dr_strange_core::{
    CatalogSnapshot, LogicalPlan, Metric, NodeRecord, PlaneHandle, PropValue, Step,
};
use serde::Serialize;

use crate::provider::{Chat, Embedder};

/// Knobs for [`ask`].
#[derive(Debug, Clone, Copy)]
pub struct AskOptions {
    /// Total model turns, including tool calls and repairs (default 6).
    pub max_attempts: u32,
    /// Validate + return the plan without executing it.
    pub dry_run: bool,
    /// A safety cap appended as a final `Limit` when the plan has none
    /// (default 100; 0 disables).
    pub limit: u64,
}

impl Default for AskOptions {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            dry_run: false,
            limit: 100,
        }
    }
}

/// The outcome of an [`ask`]: the plan that ran (or would run), how many model
/// turns it took, and the result rows (empty when `dry_run`).
#[derive(Debug)]
pub struct AskResult {
    pub plan: LogicalPlan,
    pub attempts: u32,
    pub nodes: Vec<NodeRecord>,
    pub ran: bool,
}

/// Property the digest pipeline stores node embeddings under; `find_entity`
/// vector-searches it.
const EMBED_PROP: &str = "embedding";
/// Candidates each tool returns.
const TOOL_K: usize = 5;

/// Translate `question` into a [`LogicalPlan`] over `plane` and (unless
/// `dry_run`) run it. With `embedder`, the model can call `find_edge` /
/// `find_entity` to ground the plan; without it, a single schema-grounded shot.
pub fn ask(
    chat: &dyn Chat,
    embedder: Option<&dyn Embedder>,
    plane: &PlaneHandle<'_>,
    question: &str,
    opts: &AskOptions,
) -> Result<AskResult> {
    let catalog = plane
        .catalog()
        .map_err(|e| anyhow::anyhow!("reading the plane catalog: {e}"))?;
    let tools = embedder.is_some();
    let system = system_prompt(&catalog, tools);
    let question = question.trim();
    let mut transcript = format!("Question: {question}");
    let steps = opts.max_attempts.max(1);
    let mut turns = 0u32;
    let mut last_err = String::new();

    for _ in 0..steps {
        turns += 1;
        let user = if tools {
            format!(
                "{transcript}\n\nReply with ONE JSON object — a tool call \
                 ({{\"tool\":…}}) or the final plan ({{\"plan\":…}})."
            )
        } else {
            format!("{transcript}\n\nReturn the plan JSON.")
        };
        let reply = chat.complete(&system, &user)?;
        let json = extract_json(&reply.text).to_string();

        // A tool call short-circuits: run it, feed the result back, continue.
        if tools
            && let Some(call) = parse_tool_call(&json)
        {
            let result = run_tool(embedder.expect("tools ⇒ embedder"), plane, &catalog, &call);
            let result = result.unwrap_or_else(|e| format!("tool error: {e}"));
            transcript.push_str(&format!(
                "\n\nYou called {}(\"{}\"):\n{result}",
                call.tool, call.query
            ));
            continue;
        }

        // Otherwise it should be a plan (bare or wrapped in {"plan": …}).
        match serde_json::from_str::<LogicalPlan>(&unwrap_plan(&json)) {
            Ok(mut plan) => {
                ensure_limit(&mut plan, opts.limit);
                if opts.dry_run {
                    return Ok(AskResult {
                        plan,
                        attempts: turns,
                        nodes: Vec::new(),
                        ran: false,
                    });
                }
                match plane.query_from_plan(plan.clone()).nodes() {
                    Ok(nodes) => {
                        return Ok(AskResult {
                            plan,
                            attempts: turns,
                            nodes,
                            ran: true,
                        });
                    }
                    Err(e) => last_err = format!("running that plan failed: {e}"),
                }
            }
            Err(e) => last_err = format!("that was not a valid tool call or plan JSON: {e}"),
        }
        transcript.push_str(&format!(
            "\n\nYour previous answer:\n{json}\nIt failed — {last_err}\nTry again.",
        ));
    }
    bail!("couldn't produce a runnable plan after {turns} steps: {last_err}")
}

// ---- tools ----------------------------------------------------------------

struct ToolCall {
    tool: String,
    query: String,
    label: Option<String>,
}

/// Recognize a tool call `{"tool": "...", "query": "...", "label": ...}`.
/// Returns `None` for a plan (which has no `tool` field).
fn parse_tool_call(json: &str) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tool = v.get("tool")?.as_str()?.to_string();
    Some(ToolCall {
        tool,
        query: v.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string(),
        label: v.get("label").and_then(|l| l.as_str()).map(str::to_string),
    })
}

#[derive(Serialize)]
struct EdgeHit {
    #[serde(rename = "type")]
    edge_type: String,
    connects: Vec<String>,
}

#[derive(Serialize)]
struct EntityHit {
    key: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

fn run_tool(
    embedder: &dyn Embedder,
    plane: &PlaneHandle<'_>,
    catalog: &CatalogSnapshot,
    call: &ToolCall,
) -> Result<String> {
    match call.tool.as_str() {
        "find_edge" => {
            let hits = find_edge(embedder, catalog, &call.query, TOOL_K)?;
            Ok(serde_json::to_string(&hits)?)
        }
        "find_entity" => {
            let hits = find_entity(embedder, plane, &call.query, call.label.as_deref(), TOOL_K)?;
            Ok(serde_json::to_string(&hits)?)
        }
        other => Ok(format!(
            "unknown tool '{other}' (use find_edge or find_entity)"
        )),
    }
}

/// Rank the plane's edge types by embedding similarity to `query`. Both the
/// query and each edge-type descriptor are embedded by the same model in one
/// batch, so the match is self-consistent (and cross-lingual).
fn find_edge(
    embedder: &dyn Embedder,
    catalog: &CatalogSnapshot,
    query: &str,
    k: usize,
) -> Result<Vec<EdgeHit>> {
    let types: Vec<(String, Vec<String>)> = catalog
        .edge_types
        .iter()
        .map(|(t, st)| {
            let conns = st
                .connections
                .iter()
                .map(|c| format!("{}→{}", c.src_label, c.dst_label))
                .collect();
            (t.clone(), conns)
        })
        .collect();
    if types.is_empty() {
        return Ok(Vec::new());
    }

    let mut texts = Vec::with_capacity(types.len() + 1);
    texts.push(query.to_string());
    for (t, conns) in &types {
        texts.push(format!("{t}: {}", conns.join(", ")));
    }
    let reply = embedder.embed(&texts)?;
    let q = reply
        .vectors
        .first()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))?;

    let mut scored: Vec<(usize, f32)> = (0..types.len())
        .map(|i| (i, cosine(q, &reply.vectors[i + 1])))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored
        .into_iter()
        .take(k)
        .map(|(i, _)| EdgeHit {
            edge_type: types[i].0.clone(),
            connects: types[i].1.clone(),
        })
        .collect())
}

/// Embedding-search the plane's nodes for the ones matching `query`, returning
/// their real keys + labels (the grounding `SeekKeys` needs). Requires the
/// nodes to carry an `embedding` property; empty otherwise.
fn find_entity(
    embedder: &dyn Embedder,
    plane: &PlaneHandle<'_>,
    query: &str,
    label: Option<&str>,
    k: usize,
) -> Result<Vec<EntityHit>> {
    let reply = embedder.embed(std::slice::from_ref(&query.to_string()))?;
    let q = reply
        .vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
    let nodes = plane
        .query()
        .vector_top_k(label, EMBED_PROP, q, Metric::Cosine, k as u64)
        .nodes()
        .map_err(|e| anyhow::anyhow!("entity search failed: {e}"))?;
    Ok(nodes
        .into_iter()
        .map(|n| {
            let description = match n.properties.get("description").map(|p| &p.value) {
                Some(PropValue::Str(s)) => Some(s.chars().take(140).collect()),
                _ => None,
            };
            EntityHit {
                key: n.external_key.unwrap_or_else(|| format!("#{}", n.id.0)),
                label: n.labels.first().cloned().unwrap_or_default(),
                description,
            }
        })
        .collect())
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ---- plan helpers ---------------------------------------------------------

/// Append a final `Limit` cap when the plan declares none.
fn ensure_limit(plan: &mut LogicalPlan, limit: u64) {
    if limit == 0 {
        return;
    }
    if !plan.steps.iter().any(|s| matches!(s, Step::Limit(_))) {
        plan.push(Step::Limit(limit));
    }
}

/// Unwrap a `{"plan": <plan>}` envelope to the inner plan; otherwise return the
/// JSON unchanged (a bare plan object).
fn unwrap_plan(json: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(plan) = v.get("plan")
    {
        return plan.to_string();
    }
    json.to_string()
}

/// Pull the JSON object out of a model reply — tolerate ```json fences and
/// leading/trailing prose.
fn extract_json(raw: &str) -> &str {
    let t = raw.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.trim().trim_end_matches("```").trim();
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &t[a..=b],
        _ => t,
    }
}

/// The system prompt: the plan-JSON grammar, the plane's schema, and (when
/// `tools`) the retrieval-tool protocol.
fn system_prompt(catalog: &CatalogSnapshot, tools: bool) -> String {
    let tool_section = if tools {
        "\nTOOLS — ground your plan in the real graph before planning; do NOT guess edge types or \
         entity keys. Each turn, reply with ONE JSON object:\n\
         - {\"tool\":\"find_edge\",\"query\":\"<relationship phrase, e.g. 任职 / works at>\"} → the \
           closest real edge types with their src→dst.\n\
         - {\"tool\":\"find_entity\",\"query\":\"<name or description>\",\"label\":\"<Label>\"|null} → \
           matching nodes as {key,label,description}. Use the returned `key` in SeekKeys.\n\
         - {\"plan\": <the LogicalPlan>} → your final answer.\n\
         Flow: find_edge for each relationship in the question, find_entity for each named entity, \
         THEN emit {\"plan\":…} using the EXACT edge types and keys returned. Keep tool queries short.\n"
    } else {
        ""
    };
    format!(
        "You translate a natural-language question about a graph into a QUERY PLAN as strict JSON. \
         Reply with ONLY the JSON object — no prose, no markdown fences.\n\
         {tool_section}\
         \n\
         A plan is {{\"source\": <Source>, \"steps\": [<Step>, ...]}}: it selects start nodes \
         (source), then transforms the row stream with a linear pipeline of steps, each operating \
         on the row's CURRENT node.\n\
         \n\
         Source (choose one):\n\
         - \"ScanAll\"                      every node in the plane\n\
         - {{\"ScanLabel\": \"<Label>\"}}       every node with that label\n\
         - {{\"SeekKeys\": [\"<key>\", ...]}}   specific nodes by external key\n\
         \n\
         Step (zero or more, in order):\n\
         - {{\"Expand\": {{\"dir\": \"Out\"|\"In\"|\"Both\", \"edge_type\": \"<TYPE>\"|null}}}}  1-hop to neighbours\n\
         - {{\"ExpandVar\": {{\"dir\": ..., \"edge_type\": ...|null, \"min\": <int>, \"max\": <int>}}}}  min..max hops\n\
         - {{\"Filter\": <Expr>}}            keep rows whose current node matches\n\
         - \"Distinct\"                     dedupe by node\n\
         - {{\"Sort\": [{{\"expr\": <Expr>, \"descending\": true|false}}, ...]}}\n\
         - {{\"Skip\": <int>}} / {{\"Limit\": <int>}}\n\
         \n\
         Expr (for Filter/Sort):\n\
         - {{\"Property\": \"<key>\"}}         the node's value for a property (Null if absent)\n\
         - {{\"Literal\": <Value>}}          a constant\n\
         - {{\"HasLabel\": \"<Label>\"}}       true if the node has that label\n\
         - {{\"Compare\": {{\"op\": \"Eq\"|\"Ne\"|\"Lt\"|\"Le\"|\"Gt\"|\"Ge\", \"lhs\": <Expr>, \"rhs\": <Expr>}}}}\n\
         - {{\"Logic\": {{\"op\": \"And\"|\"Or\", \"lhs\": <Expr>, \"rhs\": <Expr>}}}}\n\
         - {{\"Not\": <Expr>}} / {{\"IsNull\": <Expr>}}\n\
         - {{\"Arith\": {{\"op\": \"Add\"|\"Sub\"|\"Mul\"|\"Div\", \"lhs\": <Expr>, \"rhs\": <Expr>}}}}\n\
         Value: {{\"Int\": 2020}} | {{\"Float\": 1.5}} | {{\"Str\": \"text\"}} | {{\"Bool\": true}} | \"Null\"\n\
         \n\
         Rules:\n\
         - Use ONLY the labels, properties, and edge types in SCHEMA below, matching their exact case.\n\
         - Reference a SPECIFIC named entity with {{\"SeekKeys\": [\"<name>\"]}}: an entity's key is \
           its canonical name as written (Chinese stays Chinese). Do NOT ScanLabel+Filter to find one \
           entity by name; identity lives in the key.\n\
         - Match a relationship to a SPECIFIC edge_type from SCHEMA (e.g. 任职/works at → EMPLOYED_AT) \
           and follow its direction; use edge_type null ONLY for a generic \"any connection\".\n\
         - When the question names a result KIND (e.g. 公司/companies), add {{\"Filter\": {{\"HasLabel\": \
           \"<Label>\"}}}} after the Expand.\n\
         - Read-only: never invent write operations. Do NOT use vector/similarity operators.\n\
         - A Filter's Expr must yield a Bool (top-level Compare/HasLabel/Logic/Not/IsNull).\n\
         \n\
         Example:\n\
         Q: \"which companies does bob work at\"\n\
         {{\"plan\":{{\"source\":{{\"SeekKeys\":[\"bob\"]}},\"steps\":[{{\"Expand\":{{\"dir\":\"Out\",\"edge_type\":\"EMPLOYED_AT\"}}}},{{\"Filter\":{{\"HasLabel\":\"Company\"}}}}]}}}}\n\
         \n\
         SCHEMA (plane has {} nodes, {} edges):\n{}",
        catalog.node_count,
        catalog.edge_count,
        schema_summary(catalog),
    )
}

/// A compact, model-readable schema: each label's scalar properties (with their
/// dominant observed type) and each edge type's `src→dst` connectivity.
/// `_`-prefixed provenance properties are hidden — they're digest bookkeeping.
fn schema_summary(catalog: &CatalogSnapshot) -> String {
    let mut s = String::new();
    s.push_str("Labels:\n");
    if catalog.labels.is_empty() {
        s.push_str("  (none)\n");
    }
    for (label, stats) in &catalog.labels {
        let props: Vec<String> = stats
            .properties
            .iter()
            .filter(|(name, _)| !name.starts_with('_'))
            .map(|(name, ps)| {
                let ty = ps
                    .types
                    .iter()
                    .max_by_key(|(_, c)| **c)
                    .map(|(t, _)| format!("{t:?}"))
                    .unwrap_or_else(|| "?".into());
                format!("{name}:{ty}")
            })
            .collect();
        let props = if props.is_empty() {
            "(no properties)".to_string()
        } else {
            props.join(", ")
        };
        s.push_str(&format!("- {label} ({} nodes): {props}\n", stats.count));
    }
    s.push_str("Edge types:\n");
    if catalog.edge_types.is_empty() {
        s.push_str("  (none)\n");
    }
    for (ty, stats) in &catalog.edge_types {
        let conns: Vec<String> = stats
            .connections
            .iter()
            .map(|c| format!("{}→{}", c.src_label, c.dst_label))
            .collect();
        let conns = if conns.is_empty() {
            "(unknown endpoints)".to_string()
        } else {
            conns.join(", ")
        };
        s.push_str(&format!("- {ty}: {conns}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use dr_strange_core::{Database, PropDesc, PropValue, Properties};

    use crate::provider::MockProvider;

    fn paper(year: i64, title: &str) -> Properties {
        [
            ("year".to_string(), PropDesc::new(PropValue::Int(year))),
            ("title".to_string(), PropDesc::new(PropValue::Str(title.into()))),
            ("_model".to_string(), PropDesc::new(PropValue::Str("gpt".into()))),
        ]
        .into_iter()
        .collect()
    }

    fn seeded() -> Database {
        let db = Database::in_memory().unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        txn.create_node(&["Paper"], paper(2019, "old")).unwrap();
        txn.create_node(&["Paper"], paper(2021, "new")).unwrap();
        txn.create_node(&["Paper"], paper(2023, "newer")).unwrap();
        txn.create_node(&["Author"], Properties::new()).unwrap();
        txn.commit().unwrap();
        db
    }

    const PLAN_2020: &str = r#"{"source":{"ScanLabel":"Paper"},"steps":[
        {"Filter":{"Compare":{"op":"Ge","lhs":{"Property":"year"},"rhs":{"Literal":{"Int":2020}}}}}]}"#;

    #[test]
    fn prompt_grounds_and_steers() {
        let db = seeded();
        let cat = db.plane("startup").unwrap().catalog().unwrap();
        let p = system_prompt(&cat, false);
        assert!(p.contains("- Paper (3 nodes): title:Str, year:Int"));
        assert!(!p.contains("_model")); // provenance hidden
        assert!(p.contains("SeekKeys"));
        assert!(p.contains("identity lives in the key"));
        assert!(p.contains("SPECIFIC edge_type"));
        assert!(p.contains("HasLabel"));
        // Tool protocol only appears when tools are enabled.
        assert!(!p.contains("find_edge"));
        assert!(system_prompt(&cat, true).contains("find_entity"));
    }

    #[test]
    fn schema_only_runs_the_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec![PLAN_2020.to_string()], 4);
        let res = ask(&chat, None, &plane, "papers from 2020 on", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 1);
        assert_eq!(res.nodes.len(), 2);
    }

    #[test]
    fn tool_call_then_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        // Turn 1: the model searches; turn 2: it emits a (wrapped) plan.
        let chat = MockProvider::new(
            vec![
                r#"{"tool":"find_edge","query":"citation"}"#.to_string(),
                format!(r#"{{"plan": {PLAN_2020}}}"#),
            ],
            4,
        );
        // Same mock is the embedder (its mock vectors make find_edge harmless
        // on this edge-less graph). The loop should run the tool then the plan.
        let res = ask(&chat, Some(&chat), &plane, "recent papers", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 2, "one tool turn, then the plan turn");
        assert_eq!(res.nodes.len(), 2);
    }

    #[test]
    fn repairs_after_a_bad_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec!["not json at all".to_string(), PLAN_2020.to_string()], 4);
        let res = ask(&chat, None, &plane, "recent papers", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 2);
        assert_eq!(res.nodes.len(), 2);
    }

    #[test]
    fn dry_run_appends_limit_without_executing() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec![PLAN_2020.to_string()], 4);
        let opts = AskOptions {
            dry_run: true,
            ..Default::default()
        };
        let res = ask(&chat, None, &plane, "recent papers", &opts).unwrap();
        assert!(!res.ran && res.nodes.is_empty());
        assert!(matches!(res.plan.steps.last(), Some(Step::Limit(100))));
    }

    #[test]
    fn gives_up_after_the_step_budget() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec!["still not json".to_string()], 4);
        let opts = AskOptions {
            max_attempts: 2,
            ..Default::default()
        };
        let err = ask(&chat, None, &plane, "anything", &opts).unwrap_err();
        assert!(err.to_string().contains("after 2 steps"));
    }
}
