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
    CatalogSnapshot, EdgeRecord, LogicalPlan, Metric, NodeRecord, PlaneHandle, PropValue, Source,
    Step,
};
use serde::Serialize;

use crate::provider::{Chat, Embedder};

/// Model turns an [`ask`] gets by default — tool calls, decomposition and
/// repairs included. Twenty because the tool loop spends a turn per
/// sub-question on `find_edge` and another on `find_entity` before it ever
/// plans; a compound question needs most of them.
pub const ASK_DEFAULT_ATTEMPTS: u32 = 20;

/// The `Limit` appended to a plan that declares none, by default.
pub const ASK_DEFAULT_LIMIT: u64 = 100;

/// The most rows any plan run by [`ask`] may return, whatever the model or
/// the caller asked for. A model-emitted `Limit`, the caller's
/// [`AskOptions::limit`], and a projection's `limit` are all clamped to it.
///
/// A hard ceiling rather than a default because the plan is the model's: a
/// document can talk a model into `{"Limit": 1000000000000}`, and `ask` is
/// reachable from the Read tier of the RPC surface. A thousand rows is far
/// past what a natural-language answer is read for; anything larger is a
/// Cypher query.
pub const ASK_MAX_LIMIT: u64 = 1_000;

/// Knobs for [`ask`].
#[derive(Debug, Clone, Copy)]
pub struct AskOptions {
    /// Total model turns, including tool calls and repairs (default
    /// [`ASK_DEFAULT_ATTEMPTS`]).
    pub max_attempts: u32,
    /// Validate + return the plan without executing it.
    pub dry_run: bool,
    /// A safety cap appended as a final `Limit` when the plan has none
    /// (default [`ASK_DEFAULT_LIMIT`]). Clamped to [`ASK_MAX_LIMIT`]; `0`
    /// asks for the maximum, not for no limit — there is no unbounded plan.
    pub limit: u64,
}

impl Default for AskOptions {
    fn default() -> Self {
        Self {
            max_attempts: ASK_DEFAULT_ATTEMPTS,
            dry_run: false,
            limit: ASK_DEFAULT_LIMIT,
        }
    }
}

/// The outcome of an [`ask`]: the plan(s) that ran (or would run), how many
/// model turns it took, and the matched **subgraph** — the union of every
/// plan's nodes and the edges among them, so a compound question ("X's
/// companies AND X's projects") plots as one connected graph. A single-traversal
/// question yields one plan; nodes/edges are empty when `dry_run`.
#[derive(Debug)]
pub struct AskResult {
    pub plans: Vec<LogicalPlan>,
    pub attempts: u32,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
    pub ran: bool,
    /// A per-turn log of the model's tool calls and rejected plans, for
    /// debugging why a plan came out the way it did.
    pub trace: Vec<String>,
}

/// Property the digest pipeline stores node embeddings under; `find_entity`
/// vector-searches it.
const EMBED_PROP: &str = "embedding";
/// Candidates each tool returns.
const TOOL_K: usize = 5;
/// find_edge returns more candidates than find_entity — edge-type names are
/// close in embedding space (IMPLEMENTS vs IMPLEMENTED_IN vs DEVELOPS), so the
/// right one can rank just outside a small top-k.
const EDGE_K: usize = 10;

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
    // A human-readable log of what the model did each turn (tool calls +
    // rejected plans), surfaced for debugging/refinement.
    let mut trace: Vec<String> = Vec::new();
    // How many sub-questions the model declared (via its `asks` decomposition);
    // we then require one plan per sub-question.
    let mut expected: Option<usize> = None;
    // How many find_edge calls the model made — we require one per ask so it
    // actually sees the ranked candidates (and every fitting edge) rather than
    // picking a single literal edge off the schema.
    let mut edge_searches = 0usize;

    for i in 0..steps {
        turns += 1;
        // Reserve the final turn for the plan, so a tool-happy model can't burn
        // the whole budget searching and never answer.
        let is_last = i + 1 == steps;
        let user = if tools && !is_last {
            format!(
                "{transcript}\n\nReply with ONE JSON object — a tool call \
                 ({{\"tool\":…}}) or the final plan ({{\"plan\":…}})."
            )
        } else if tools {
            format!(
                "{transcript}\n\nFINAL TURN — do NOT call tools. Reply with ONLY the plan(s). If the question asked for more than one thing, return one plan per part: {{\"plans\": […]}}; otherwise {{\"plan\": …}}."
            )
        } else {
            format!("{transcript}\n\nReturn the plan JSON.")
        };
        let reply = chat.complete(&system, &user)?;
        let json = extract_json(&reply.text).to_string();

        // First, the model declares its decomposition: {"asks": ["…", "…"]}.
        // Remember the count so we can require one plan per sub-question.
        if tools
            && !is_last
            && let Some(asks) = parse_asks(&json)
        {
            let n = asks.len();
            expected = Some(n);
            trace.push(format!("decompose → {n} ask(s): {}", asks.join(" | ")));
            transcript.push_str(&format!(
                "\n\nYou split the question into {n} sub-question(s): {asks:?}. Now call find_edge on \
                 EACH sub-question's relationship (one at a time), then find_entity for named \
                 entities, then return at least {n} plan(s) — one per (sub-question × fitting edge)."
            ));
            continue;
        }

        // A tool call short-circuits (but not on the final turn): run it, feed
        // the result back, continue.
        if tools
            && !is_last
            && let Some(call) = parse_tool_call(&json)
        {
            if call.tool == "find_edge" {
                edge_searches += 1;
            }
            let result = run_tool(embedder.expect("tools ⇒ embedder"), plane, &catalog, &call);
            let result = result.unwrap_or_else(|e| format!("tool error: {e}"));
            trace.push(format!(
                "{}(\"{}\") → {}",
                call.tool,
                call.query,
                result.chars().take(500).collect::<String>()
            ));
            transcript.push_str(&format!(
                "\n\nYou called {}(\"{}\"):\n{result}",
                call.tool, call.query
            ));
            continue;
        }

        // Otherwise it should be a plan, or several: {"plan": …}, a bare plan
        // object, or {"plans": [ … ]} for a compound question.
        match parse_plans(&json) {
            Ok(mut plans) if !plans.is_empty() => {
                // The grammar first: a plan outside it is sent back as a
                // repair, exactly like one that fails to run.
                if let Err(e) = plans.iter().try_for_each(check_read_only) {
                    last_err = format!("{e}");
                    trace.push(format!("plan rejected: {last_err}"));
                    transcript.push_str(&format!(
                        "\n\nYour previous answer:\n{json}\nIt failed — {last_err}\nTry again."
                    ));
                    continue;
                }
                for p in &mut plans {
                    bound_limits(p, opts.limit);
                }
                // Require a find_edge per sub-question first, so the model saw
                // the ranked candidates (and every fitting edge) instead of
                // picking one literal edge off the schema. Skip on the last turn.
                if let Some(n) = expected
                    && edge_searches < n
                    && !is_last
                {
                    last_err = format!(
                        "before planning you must call find_edge for EACH of the {n} sub-questions' \
                         relationships (you've called it {edge_searches}× — its ranked candidates \
                         reveal every fitting edge, e.g. both IMPLEMENTS and DEVELOPS for \"make\")"
                    );
                    trace.push(format!("plan rejected: {last_err}"));
                    transcript.push_str(&format!(
                        "\n\nYour previous answer:\n{json}\nIt failed — {last_err}\nTry again."
                    ));
                    continue;
                }
                // Enforce the declared decomposition: one plan per sub-question
                // (the model tends to answer only the first otherwise). Skip the
                // check on the final turn — take what we have rather than fail.
                if let Some(n) = expected
                    && plans.len() < n
                    && !is_last
                {
                    last_err = format!(
                        "you split the question into {n} sub-questions but returned only {} plan(s) — \
                         return one plan per sub-question in {{\"plans\":[…]}}",
                        plans.len()
                    );
                    trace.push(format!("plan rejected: {last_err}"));
                    transcript.push_str(&format!(
                        "\n\nYour previous answer:\n{json}\nIt failed — {last_err}\nTry again."
                    ));
                    continue;
                }
                if opts.dry_run {
                    return Ok(AskResult {
                        plans,
                        attempts: turns,
                        nodes: Vec::new(),
                        edges: Vec::new(),
                        ran: false,
                        trace,
                    });
                }
                // Run each plan and union their subgraphs into one graph.
                match run_plans(plane, &plans) {
                    Ok((nodes, edges)) => {
                        return Ok(AskResult {
                            plans,
                            attempts: turns,
                            nodes,
                            edges,
                            ran: true,
                            trace,
                        });
                    }
                    Err(e) => last_err = format!("running the plan(s) failed: {e}"),
                }
            }
            Ok(_) => last_err = "you returned an empty plan list".to_string(),
            Err(e) => last_err = format!("that was not a valid tool call or plan JSON: {e}"),
        }
        trace.push(format!("plan rejected: {last_err}"));
        transcript.push_str(&format!(
            "\n\nYour previous answer:\n{json}\nIt failed — {last_err}\nTry again.",
        ));
    }
    let reason = if last_err.is_empty() {
        "the model kept calling tools without emitting a plan".to_string()
    } else {
        last_err
    };
    bail!("couldn't produce a runnable plan after {turns} steps: {reason}")
}

// ---- tools ----------------------------------------------------------------

struct ToolCall {
    tool: String,
    query: String,
    label: Option<String>,
}

/// Recognize a decomposition `{"asks": ["…", "…"]}` (non-empty). Returns the
/// sub-question list.
fn parse_asks(json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = v.get("asks")?.as_array()?;
    let asks: Vec<String> = arr
        .iter()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    (!asks.is_empty()).then_some(asks)
}

/// Recognize a tool call `{"tool": "...", "query": "...", "label": ...}`.
/// Returns `None` for a plan (which has no `tool` field).
fn parse_tool_call(json: &str) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tool = v.get("tool")?.as_str()?.to_string();
    Some(ToolCall {
        tool,
        query: v
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string(),
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
            let hits = find_edge(embedder, catalog, &call.query, EDGE_K)?;
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

/// The plan grammar `ask` runs — the one its prompt teaches, and nothing the
/// prompt does not.
///
/// Every variant of [`Source`] and [`Step`] is read-only: the algebra has no
/// mutation, which is what makes `ask` safe by construction (arch/07 §3.3).
/// This check exists for the other half of the promise. Both enums are
/// `#[non_exhaustive]`, so the core may grow a variant this loop has never
/// heard of, and a model can already emit the ones the prompt forbids: a
/// `VectorTopK` with an invented query vector, an `Algo` that runs PageRank
/// over the whole plane on a Read-tier RPC call, an `ExpandBeam` nobody asked
/// for. An allowlist rejects all of those today and whatever arrives
/// tomorrow, and the rejection goes back to the model as a repair.
fn check_read_only(plan: &LogicalPlan) -> Result<()> {
    let source_ok = matches!(
        plan.source,
        Source::ScanAll | Source::ScanLabel(_) | Source::SeekIds(_) | Source::SeekKeys(_)
    );
    if !source_ok {
        bail!(
            "the plan's source is not one of ScanAll, ScanLabel, SeekIds or SeekKeys — \
             vector, keyword, hybrid and algorithm sources are not available here"
        );
    }
    for step in &plan.steps {
        let ok = matches!(
            step,
            Step::Expand { .. }
                | Step::ExpandVar { .. }
                | Step::Filter(_)
                | Step::Skip(_)
                | Step::Limit(_)
                | Step::Distinct
                | Step::Sort(_)
        );
        if !ok {
            bail!(
                "the plan uses a step outside Expand, ExpandVar, Filter, Distinct, Sort, Skip \
                 and Limit — vector and similarity operators are not available here"
            );
        }
    }
    Ok(())
}

/// Bound what a plan may return: every `Limit` the model wrote — as a step or
/// on a projection — is clamped to [`ASK_MAX_LIMIT`], and a plan whose
/// *last* step is not a `Limit` gets `requested` appended, clamped the same
/// way (`0` means the maximum). No plan leaves here unbounded.
///
/// The last step, not any step: a `Limit` applies where it stands, and the
/// prompt lists it among steps that go "in order", so a model can write
/// `[Limit(1), ExpandVar(1..50)]` — one seed, then every walk from it. That
/// plan declared a limit and returned an unbounded number of rows. The cap
/// at the end is what the executor's streaming `take` stops on.
fn bound_limits(plan: &mut LogicalPlan, requested: u64) {
    for step in &mut plan.steps {
        if let Step::Limit(n) = step {
            *n = (*n).min(ASK_MAX_LIMIT);
        }
    }
    if let Some(p) = &mut plan.project
        && let Some(n) = &mut p.limit
    {
        *n = (*n).min(ASK_MAX_LIMIT);
    }
    let declared = matches!(plan.steps.last(), Some(Step::Limit(_)));
    if !declared {
        let cap = if requested == 0 {
            ASK_MAX_LIMIT
        } else {
            requested.min(ASK_MAX_LIMIT)
        };
        plan.push(Step::Limit(cap));
    }
}

/// Parse the model's final answer into one or more plans: `{"plans": [ … ]}`
/// (compound question), `{"plan": <p>}`, or a bare plan object.
fn parse_plans(json: &str) -> Result<Vec<LogicalPlan>> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    if let Some(arr) = v.get("plans").and_then(|p| p.as_array()) {
        arr.iter()
            .map(|p| serde_json::from_value::<LogicalPlan>(p.clone()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    } else if let Some(p) = v.get("plan") {
        Ok(vec![serde_json::from_value(p.clone())?])
    } else {
        Ok(vec![serde_json::from_value(v)?])
    }
}

/// Run every plan and union the matched subgraphs (nodes by id, edges by id) —
/// so several traversals from the same entity become one connected graph.
fn run_plans(
    plane: &PlaneHandle<'_>,
    plans: &[LogicalPlan],
) -> Result<(Vec<NodeRecord>, Vec<EdgeRecord>)> {
    use std::collections::BTreeMap;
    let mut nodes: BTreeMap<u64, NodeRecord> = BTreeMap::new();
    let mut edges: BTreeMap<u64, EdgeRecord> = BTreeMap::new();
    for plan in plans {
        let (ns, es) = plane
            .query_from_plan(plan.clone())
            .subgraph()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        for n in ns {
            nodes.entry(n.id.0).or_insert(n);
        }
        for e in es {
            edges.entry(e.id.0).or_insert(e);
        }
    }
    Ok((nodes.into_values().collect(), edges.into_values().collect()))
}

/// Pull the JSON object out of a model reply — tolerate ```json fences and
/// leading/trailing prose.
pub(crate) fn extract_json(raw: &str) -> &str {
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
         - {\"asks\": [\"…\", \"…\"]} → your decomposition (see Flow step 1).\n\
         - {\"plan\": <the LogicalPlan>} or {\"plans\": [<plan>, …]} → your final answer.\n\
         Flow — follow IN ORDER:\n\
         1. DECOMPOSE first: reply {\"asks\": [\"<sub-question 1>\", \"<sub-question 2>\", …]} splitting \
            the question into EVERY distinct thing it asks. Clauses joined by 和/以及/并/、/，/\"and\" are \
            SEPARATE asks (e.g. \"…任职于哪些公司，做了哪些项目，实现了哪些东西\" = THREE asks). A \
            one-part question is one ask.\n\
         2. For EACH ask you MUST call find_edge on its relationship (do NOT pick edges off the \
            schema alone — the ranked candidates reveal every fitting edge, including near-synonyms). \
            Then take EVERY candidate whose src is the entity's label and whose meaning fits the \
            ask's verb: usually one, but a BROAD verb (做/实现/研发/build/make) matches SEVERAL — e.g. \
            \"实现了哪些东西\" from a Person covers BOTH IMPLEMENTS: Person→CryptographicModule AND \
            DEVELOPS: Person→HardwareDevice (both are \"things made\") — take them ALL. Use \
            find_entity for named entities.\n\
         3. Return one plan per (ask × matching edge) in {\"plans\": [ … ]} — at least one plan per \
            ask, MORE when an ask matched several edges — using the EXACT edge types and keys. Never \
            merge two relationships into one pipeline.\n\
         Keep tool queries short.\n"
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
         - A specific edge_type already scopes the result to its target labels (its src→dst in \
           SCHEMA). Add {{\"Filter\": {{\"HasLabel\": \"<Label>\"}}}} ONLY to pick one kind \
           when the edge reaches SEVERAL distinct kinds and the question wants just that one. Do NOT \
           filter when the question's category covers all the edge's targets (e.g. 任职/employed-at → \
           Company AND Organization are both employers → return both, no filter).\n\
         - A plan is a SINGLE linear traversal from one source and CANNOT branch. If the question \
           asks for MORE THAN ONE thing about an entity (clauses joined by 和/以及/并/、/，/\"and\", \
           e.g. \"X's companies AND X's projects\"), you MUST return one plan per sub-question in \
           {{\"plans\": [<planA>, <planB>]}} — each starting from that entity, each with its own \
           edge_type. Their subgraphs are unioned into one graph. NEVER chain the two relationships \
           into a single pipeline (that traverses A→B→C and matches nothing).\n\
         - Read-only: never invent write operations. Do NOT use vector/similarity operators.\n\
         - A Filter's Expr must yield a Bool (top-level Compare/HasLabel/Logic/Not/IsNull).\n\
         \n\
         Examples:\n\
         Q: \"which companies does bob work at\"  (EMPLOYED_AT → Company, Organization are all employers → no label filter)\n\
         {{\"plan\":{{\"source\":{{\"SeekKeys\":[\"bob\"]}},\"steps\":[{{\"Expand\":{{\"dir\":\"Out\",\"edge_type\":\"EMPLOYED_AT\"}}}}]}}}}\n\
         Q: \"which companies does bob work at, and what projects did he do\"  (compound → one plan per part, unioned)\n\
         {{\"plans\":[{{\"source\":{{\"SeekKeys\":[\"bob\"]}},\"steps\":[{{\"Expand\":{{\"dir\":\"Out\",\"edge_type\":\"EMPLOYED_AT\"}}}}]}},{{\"source\":{{\"SeekKeys\":[\"bob\"]}},\"steps\":[{{\"Expand\":{{\"dir\":\"Out\",\"edge_type\":\"WORKS_ON\"}}}}]}}]}}\n\
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
            (
                "title".to_string(),
                PropDesc::new(PropValue::Str(title.into())),
            ),
            (
                "_model".to_string(),
                PropDesc::new(PropValue::Str("gpt".into())),
            ),
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
        let res = ask(
            &chat,
            None,
            &plane,
            "papers from 2020 on",
            &AskOptions::default(),
        )
        .unwrap();
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
        let res = ask(
            &chat,
            Some(&chat),
            &plane,
            "recent papers",
            &AskOptions::default(),
        )
        .unwrap();
        assert_eq!(res.attempts, 2, "one tool turn, then the plan turn");
        assert_eq!(res.nodes.len(), 2);
    }

    #[test]
    fn compound_question_unions_multiple_plans() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        // Two plans: recent papers + all authors. Their subgraphs union.
        let two = format!(
            r#"{{"plans":[{PLAN_2020},{{"source":{{"ScanLabel":"Author"}},"steps":[]}}]}}"#
        );
        let chat = MockProvider::new(vec![two], 4);
        let res = ask(
            &chat,
            None,
            &plane,
            "recent papers and authors",
            &AskOptions::default(),
        )
        .unwrap();
        assert_eq!(res.plans.len(), 2);
        assert_eq!(res.nodes.len(), 3); // 2 papers (≥2020) + 1 author, deduped union
    }

    #[test]
    fn repairs_after_a_bad_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(
            vec!["not json at all".to_string(), PLAN_2020.to_string()],
            4,
        );
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
        assert_eq!(res.plans.len(), 1);
        assert!(matches!(
            res.plans[0].steps.last(),
            Some(Step::Limit(ASK_DEFAULT_LIMIT))
        ));
    }

    /// The plan is the model's, and a document can talk a model into
    /// `Limit(10^12)`. Whatever the model or the caller asks for, no plan
    /// leaves `ask` able to return more than [`ASK_MAX_LIMIT`] rows — and
    /// `limit: 0` now means the ceiling, not "no ceiling".
    #[test]
    fn every_limit_is_clamped_and_no_plan_is_unbounded() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let dry = |plan: &str, limit: u64| {
            let chat = MockProvider::new(vec![plan.to_string()], 4);
            let opts = AskOptions {
                dry_run: true,
                limit,
                ..Default::default()
            };
            ask(&chat, None, &plane, "q", &opts)
                .unwrap()
                .plans
                .remove(0)
        };
        // A model-emitted limit is clamped in place, not appended to.
        let huge = r#"{"source":"ScanAll","steps":[{"Limit":1000000000000}]}"#;
        let plan = dry(huge, 100);
        assert_eq!(plan.steps, vec![Step::Limit(ASK_MAX_LIMIT)]);
        // A modest one is the model's to choose.
        let plan = dry(r#"{"source":"ScanAll","steps":[{"Limit":5}]}"#, 100);
        assert_eq!(plan.steps, vec![Step::Limit(5)]);
        // The caller's cap is clamped too, and zero is the ceiling.
        let plan = dry(PLAN_2020, 5_000);
        assert!(matches!(
            plan.steps.last(),
            Some(Step::Limit(ASK_MAX_LIMIT))
        ));
        let plan = dry(PLAN_2020, 0);
        assert!(matches!(
            plan.steps.last(),
            Some(Step::Limit(ASK_MAX_LIMIT))
        ));
        // A limit that stands before an expansion bounds the seeds, not the
        // rows: the plan still ends in a cap.
        let seeded_walk = r#"{"source":"ScanAll","steps":[{"Limit":1},
            {"ExpandVar":{"dir":"Both","edge_type":null,"min":1,"max":6}}]}"#;
        let plan = dry(seeded_walk, 100);
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0], Step::Limit(1));
        assert!(matches!(plan.steps[2], Step::Limit(100)));
        // A projection's own limit is bounded the same way.
        let projected = r#"{"source":"ScanAll","steps":[],
            "project":{"items":[],"distinct":false,"order_by":[],"skip":null,"limit":99999}}"#;
        let plan = dry(projected, 100);
        assert_eq!(plan.project.unwrap().limit, Some(ASK_MAX_LIMIT));
    }

    /// The prompt forbids vector, keyword and algorithm sources and the
    /// similarity steps; a model that emits one anyway is sent back to try
    /// again rather than run — PageRank over the plane is not an answer to a
    /// question, and on a shared server it is a cost anyone with Read can
    /// impose.
    #[test]
    fn a_plan_outside_the_grammar_is_rejected_and_repaired() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let algo = r#"{"source":{"Algo":{"label":null,"algo":{"PageRank":{"damping":0.85,"max_iters":20,"tolerance":0.001}}}},"steps":[]}"#;
        let keyword = r#"{"source":{"KeywordTopK":{"label":"Paper","property":"title","query":"x","k":5}},"steps":[]}"#;
        let beam = r#"{"source":"ScanAll","steps":[{"FrontierTopK":{"property":"embedding","query":[0.1],"metric":"Cosine","k":5}}]}"#;
        let chat = MockProvider::new(
            vec![
                algo.to_string(),
                keyword.to_string(),
                beam.to_string(),
                PLAN_2020.to_string(),
            ],
            4,
        );
        let res = ask(&chat, None, &plane, "recent papers", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 4, "three rejections, then the plan");
        assert_eq!(res.nodes.len(), 2);
        assert_eq!(
            res.trace.iter().filter(|t| t.contains("rejected")).count(),
            3,
            "{:?}",
            res.trace
        );
        // Nothing outside the grammar ever ran or was returned.
        assert!(res.plans.iter().all(|p| check_read_only(p).is_ok()));
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
