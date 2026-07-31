//! Natural-language querying (ROADMAP §3): turn an English question into a
//! [`LogicalPlan`] the engine runs.
//!
//! The model is grounded with the plane's catalog (its labels, properties, and
//! edge types) plus a compact spec of the plan JSON, then emits a plan as JSON.
//! We deserialize it (a `LogicalPlan` has no write operators, so it is
//! **read-only by construction**), run it, and — on a parse or execution error
//! — feed the error back for a bounded number of repair attempts. The result
//! carries the plan that ran and its rows; `dry_run` returns the validated plan
//! without executing.

use anyhow::{Result, bail};
use dr_strange_core::{CatalogSnapshot, LogicalPlan, NodeRecord, PlaneHandle, Step};

use crate::provider::Chat;

/// Knobs for [`ask`].
#[derive(Debug, Clone, Copy)]
pub struct AskOptions {
    /// Total model attempts, including repairs (default 3).
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
            max_attempts: 3,
            dry_run: false,
            limit: 100,
        }
    }
}

/// The outcome of an [`ask`]: the plan that ran (or would run), how many model
/// attempts it took, and the result rows (empty when `dry_run`).
#[derive(Debug)]
pub struct AskResult {
    pub plan: LogicalPlan,
    pub attempts: u32,
    pub nodes: Vec<NodeRecord>,
    pub ran: bool,
}

/// Translate `question` into a [`LogicalPlan`] over `plane` and (unless
/// `dry_run`) run it, repairing on failure up to `opts.max_attempts`.
pub fn ask(
    chat: &dyn Chat,
    plane: &PlaneHandle<'_>,
    question: &str,
    opts: &AskOptions,
) -> Result<AskResult> {
    let catalog = plane
        .catalog()
        .map_err(|e| anyhow::anyhow!("reading the plane catalog: {e}"))?;
    let system = system_prompt(&catalog);
    let question = question.trim();
    let mut user = format!("Question: {question}\nReturn the plan JSON.");
    let attempts = opts.max_attempts.max(1);
    let mut last_err = String::new();

    for attempt in 1..=attempts {
        let reply = chat.complete(&system, &user)?;
        let json = extract_json(&reply.text).to_string();
        match serde_json::from_str::<LogicalPlan>(&json) {
            Ok(mut plan) => {
                ensure_limit(&mut plan, opts.limit);
                if opts.dry_run {
                    return Ok(AskResult {
                        plan,
                        attempts: attempt,
                        nodes: Vec::new(),
                        ran: false,
                    });
                }
                match plane.query_from_plan(plan.clone()).nodes() {
                    Ok(nodes) => {
                        return Ok(AskResult {
                            plan,
                            attempts: attempt,
                            nodes,
                            ran: true,
                        });
                    }
                    Err(e) => last_err = format!("running that plan failed: {e}"),
                }
            }
            Err(e) => last_err = format!("that was not valid plan JSON: {e}"),
        }
        // Feed the failure back for the next attempt.
        user = format!(
            "Question: {question}\nYour previous answer was:\n{json}\nIt failed — {last_err}\n\
             Return a corrected plan as JSON only.",
        );
    }
    bail!("couldn't produce a runnable plan after {attempts} attempts: {last_err}")
}

/// Append a final `Limit` cap when the plan declares none, so an
/// unbounded-looking question can't dump the whole plane.
fn ensure_limit(plan: &mut LogicalPlan, limit: u64) {
    if limit == 0 {
        return;
    }
    if !plan.steps.iter().any(|s| matches!(s, Step::Limit(_))) {
        plan.push(Step::Limit(limit));
    }
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

/// The system prompt: the plan-JSON grammar + the plane's schema. Kept explicit
/// because the model must match the serde shape exactly (externally-tagged
/// enums), and grounded in the catalog so it uses real labels/props/edges.
fn system_prompt(catalog: &CatalogSnapshot) -> String {
    format!(
        "You translate a natural-language question about a graph into a QUERY PLAN as strict JSON. \
         Reply with ONLY the JSON object — no prose, no markdown fences.\n\
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
         - To reference a SPECIFIC named entity (a person, company, product, …), select it with \
           {{\"SeekKeys\": [\"<name>\"]}}: an entity's key IS its canonical name exactly as written \
           (a Chinese name stays Chinese, e.g. \"王滢\"). Do NOT ScanLabel then Filter on a property \
           to find one entity by name — identity lives in the key; there is usually no name property.\n\
         - Read-only: there are no write operations — never invent any.\n\
         - Do NOT use vector/similarity operators; you cannot produce embeddings.\n\
         - A Filter's Expr must yield a Bool (top-level Compare/HasLabel/Logic/Not/IsNull).\n\
         - Follow edge_type directions per SCHEMA (e.g. WROTE: Author→Paper means dir Out from an Author).\n\
         \n\
         Examples:\n\
         Q: \"papers from 2020 or later, newest first, top 10\"\n\
         {{\"source\":{{\"ScanLabel\":\"Paper\"}},\"steps\":[{{\"Filter\":{{\"Compare\":{{\"op\":\"Ge\",\"lhs\":{{\"Property\":\"year\"}},\"rhs\":{{\"Literal\":{{\"Int\":2020}}}}}}}}}},{{\"Sort\":[{{\"expr\":{{\"Property\":\"year\"}},\"descending\":true}}]}},{{\"Limit\":10}}]}}\n\
         Q: \"who does alice know\"\n\
         {{\"source\":{{\"SeekKeys\":[\"alice\"]}},\"steps\":[{{\"Expand\":{{\"dir\":\"Out\",\"edge_type\":\"KNOWS\"}}}}]}}\n\
         \n\
         SCHEMA (plane has {} nodes, {} edges):\n{}",
        catalog.node_count,
        catalog.edge_count,
        schema_summary(catalog),
    )
}

/// A compact, model-readable schema: each label's scalar properties (with their
/// dominant observed type) and each edge type's `src→dst` connectivity.
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
            // Hide `_`-prefixed provenance metadata (e.g. _model/_source/_run):
            // it's digest bookkeeping, not a queryable attribute, and the model
            // otherwise mistakes it for a name field.
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
            // A digest-style provenance property, which must NOT leak into the
            // schema shown to the model.
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
    fn prompt_grounds_in_the_catalog() {
        let db = seeded();
        let cat = db.plane("startup").unwrap().catalog().unwrap();
        let p = system_prompt(&cat);
        // Real properties show; the `_model` provenance property is hidden.
        assert!(p.contains("- Paper (3 nodes): title:Str, year:Int"));
        assert!(!p.contains("_model"));
        assert!(p.contains("- Author"));
        assert!(p.contains("Read-only"));
        // Named-entity lookups are steered to SeekKeys, not property filters.
        assert!(p.contains("SeekKeys"));
        assert!(p.contains("identity lives in the key"));
    }

    #[test]
    fn runs_the_generated_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec![PLAN_2020.to_string()], 4);
        let res = ask(&chat, &plane, "papers from 2020 on", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 1);
        assert!(res.ran);
        assert_eq!(res.nodes.len(), 2); // 2021 + 2023
        assert!(res.nodes.iter().all(|n| matches!(
            n.properties.get("year").map(|p| &p.value),
            Some(PropValue::Int(y)) if *y >= 2020
        )));
    }

    #[test]
    fn repairs_after_a_bad_plan() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        // First reply is junk; the loop feeds the error back and the second is valid.
        let chat = MockProvider::new(vec!["not json at all".to_string(), PLAN_2020.to_string()], 4);
        let res = ask(&chat, &plane, "recent papers", &AskOptions::default()).unwrap();
        assert_eq!(res.attempts, 2);
        assert_eq!(res.nodes.len(), 2);
    }

    #[test]
    fn dry_run_validates_without_executing() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec![PLAN_2020.to_string()], 4);
        let opts = AskOptions {
            dry_run: true,
            ..Default::default()
        };
        let res = ask(&chat, &plane, "recent papers", &opts).unwrap();
        assert!(!res.ran);
        assert!(res.nodes.is_empty());
        // The safety Limit was appended (the model's plan had none).
        assert!(matches!(res.plan.steps.last(), Some(Step::Limit(100))));
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let db = seeded();
        let plane = db.plane("startup").unwrap();
        let chat = MockProvider::new(vec!["still not json".to_string()], 4);
        let opts = AskOptions {
            max_attempts: 2,
            ..Default::default()
        };
        let err = ask(&chat, &plane, "anything", &opts).unwrap_err();
        assert!(err.to_string().contains("after 2 attempts"));
    }
}
