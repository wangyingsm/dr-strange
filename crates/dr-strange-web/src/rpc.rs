//! JSON-RPC 2.0 framing and dispatch (arch/08 §1, 00-overview §2). This is the
//! project-wide wire protocol — MCP is itself JSON-RPC 2.0, so this backend is
//! the first draft of the eventual network server, not a bespoke one-off.
//!
//! [`handle`] is pure and synchronous: bytes in, an optional response `Value`
//! out. The HTTP/WebSocket layers ([`crate::server`]) run it on a blocking
//! task (the core is sync) and never touch the protocol rules themselves,
//! which keeps this fully unit-testable without a running server.

use serde_json::{Value, json};

use crate::auth::{Access, Auth};
use crate::methods::{self, Ctx};

// ---- error model ----------------------------------------------------------

/// A JSON-RPC 2.0 error object. Reserved codes are from the spec; application
/// errors (a bad plane name, a malformed plan) use the server-error code
/// `-32000` so clients can tell "you asked wrong" from "the protocol broke".
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(-32600, msg)
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("method not found: {method}"))
    }
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }
    /// Application-level failure surfaced by the core (server-error range).
    pub fn server(msg: impl Into<String>) -> Self {
        Self::new(-32000, msg)
    }
    /// A method was called without a valid credential. The whole surface is
    /// authenticated, so this covers reads too. JSON-RPC has no standard auth
    /// code; `-32001` sits in the app server-error band and is distinct from
    /// `-32000` so clients (and the SDKs) can detect an auth failure — the wire
    /// analogue of HTTP 401.
    pub fn unauthorized() -> Self {
        Self::new(
            -32001,
            "unauthorized: set DRSG_TOKEN and send it as `Authorization: Bearer <token>` (WebSocket: `?token=<token>`)",
        )
    }

    fn to_value(&self) -> Value {
        let mut obj = json!({ "code": self.code, "message": self.message });
        if let Some(data) = &self.data {
            obj["data"] = data.clone();
        }
        obj
    }
}

fn error_response(id: Value, err: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "error": err.to_value(), "id": id })
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "result": result, "id": id })
}

// ---- entry point ----------------------------------------------------------

/// Parses and dispatches one JSON-RPC message (single or batch). Returns the
/// response `Value`, or `None` when nothing is owed to the caller — a batch of
/// only notifications, or a single notification (a request with no `id`).
pub fn handle(ctx: &Ctx<'_>, auth: &Auth<'_>, body: &[u8]) -> Option<Value> {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        // Parse errors carry a null id per spec §5.1.
        Err(e) => {
            return Some(error_response(
                Value::Null,
                &RpcError::new(-32700, format!("parse error: {e}")),
            ));
        }
    };

    match parsed {
        Value::Array(items) => {
            if items.is_empty() {
                return Some(error_response(
                    Value::Null,
                    &RpcError::invalid_request("empty batch"),
                ));
            }
            let responses: Vec<Value> = items
                .into_iter()
                .filter_map(|item| handle_single(ctx, auth, item))
                .collect();
            // An all-notification batch produces no response at all.
            if responses.is_empty() {
                None
            } else {
                Some(Value::Array(responses))
            }
        }
        obj @ Value::Object(_) => handle_single(ctx, auth, obj),
        _ => Some(error_response(
            Value::Null,
            &RpcError::invalid_request("request must be an object or array"),
        )),
    }
}

/// One request object → its response, or `None` for a notification (no `id`).
/// A notification never yields a response even when it errors (spec §4.1).
fn handle_single(ctx: &Ctx<'_>, auth: &Auth<'_>, msg: Value) -> Option<Value> {
    let Value::Object(map) = msg else {
        return Some(error_response(
            Value::Null,
            &RpcError::invalid_request("request must be an object"),
        ));
    };

    // Presence of `id`, not its value, distinguishes a call from a
    // notification — `"id": null` is still a call (answered with a null id).
    let is_notification = !map.contains_key("id");
    let id = map.get("id").cloned().unwrap_or(Value::Null);

    let dispatch = || -> Result<Value, RpcError> {
        if map.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(RpcError::invalid_request(
                "missing or bad `jsonrpc` version",
            ));
        }
        let method = map
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_request("missing `method`"))?;
        let params = map.get("params").cloned().unwrap_or(Value::Null);
        dispatch_method(ctx, auth, method, params)
    };

    let result = dispatch();
    if is_notification {
        return None;
    }
    Some(match result {
        Ok(value) => ok_response(id, value),
        Err(err) => error_response(id, &err),
    })
}

/// Dispatch one method. Every arm declares the [`Access`] it requires via the
/// `guarded!` macro, so a method cannot be added without classifying it — an
/// ungated write can't ship by omission. The gate runs before the handler, so
/// an unauthorized caller never reaches the core.
fn dispatch_method(
    ctx: &Ctx<'_>,
    auth: &Auth<'_>,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    /// `guarded!(<access>, <handler-call>)` — enforce the access level, then run.
    macro_rules! guarded {
        ($access:expr, $call:expr) => {{
            if !auth.allows($access) {
                return Err(RpcError::unauthorized());
            }
            $call
        }};
    }

    match method {
        "db.stats" => guarded!(Access::Read, methods::db_stats(ctx)),
        "db.catalog" => guarded!(Access::Read, methods::db_catalog(ctx)),
        "plane.list" => guarded!(Access::Read, methods::plane_list(ctx)),
        "plane.catalog" => guarded!(Access::Read, methods::plane_catalog(ctx, params)),
        "node.get" => guarded!(Access::Read, methods::node_get(ctx, params)),
        "plane.neighbors" => guarded!(Access::Read, methods::plane_neighbors(ctx, params)),
        "plane.search" => guarded!(Access::Read, methods::plane_search(ctx, params)),
        "plane.query" => guarded!(Access::Read, methods::plane_query(ctx, params)),
        "plane.find" => guarded!(Access::Read, methods::plane_find(ctx, params)),
        "graph.seed" => guarded!(Access::Read, methods::graph_seed(ctx, params)),
        "graph.expand" => guarded!(Access::Read, methods::graph_expand(ctx, params)),
        // `digest.run` writes nothing, but it spends the server's provider
        // credentials (the LLM + embedding calls), so it's a privileged op.
        "digest.run" => guarded!(Access::Write, methods::digest_run(ctx, params)),
        "digest.write" => guarded!(Access::Write, methods::digest_write(ctx, params)),
        other => Err(RpcError::method_not_found(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dr_strange_core::{Database, Properties};

    /// An in-memory DB with one node ("alice") on the startup plane.
    fn seeded() -> Database {
        let db = Database::in_memory().unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        txn.create_node_with_key("alice", &["Person"], Properties::new())
            .unwrap();
        txn.commit().unwrap();
        db
    }

    /// An in-memory DB with alice —KNOWS→ bob on the startup plane; returns
    /// the two node ids so tests need not assume the id base.
    fn seeded_graph() -> (Database, u64, u64) {
        let db = Database::in_memory().unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let alice = txn
            .create_node_with_key("alice", &["Person"], Properties::new())
            .unwrap();
        let bob = txn
            .create_node_with_key("bob", &["Person"], Properties::new())
            .unwrap();
        txn.create_edge(alice, bob, "KNOWS", Properties::new())
            .unwrap();
        txn.commit().unwrap();
        (db, alice.0, bob.0)
    }

    fn call(db: &Database, body: &str) -> Option<Value> {
        let ctx = Ctx { db, db_path: None };
        handle(&ctx, &Auth::allow_all(), body.as_bytes())
    }

    /// Dispatch `body` under an explicit authorizer + credentials (for the auth
    /// gate tests).
    fn call_as(db: &Database, auth: &Auth<'_>, body: &str) -> Option<Value> {
        let ctx = Ctx { db, db_path: None };
        handle(&ctx, auth, body.as_bytes())
    }

    /// Extract the error code from a (single) response.
    fn err_code(resp: &Value) -> i64 {
        resp["error"]["code"].as_i64().unwrap()
    }

    #[test]
    fn parse_error_has_null_id() {
        let db = seeded();
        let resp = call(&db, "{ not json").unwrap();
        assert_eq!(err_code(&resp), -32700);
        assert!(resp["id"].is_null());
    }

    #[test]
    fn non_object_request_is_invalid() {
        let db = seeded();
        let resp = call(&db, "123").unwrap();
        assert_eq!(err_code(&resp), -32600);
    }

    #[test]
    fn empty_batch_is_invalid() {
        let db = seeded();
        let resp = call(&db, "[]").unwrap();
        assert_eq!(err_code(&resp), -32600);
    }

    #[test]
    fn missing_jsonrpc_version_is_invalid() {
        let db = seeded();
        let resp = call(&db, r#"{"method":"db.stats","id":1}"#).unwrap();
        assert_eq!(err_code(&resp), -32600);
    }

    #[test]
    fn unknown_method_is_not_found() {
        let db = seeded();
        let resp = call(&db, r#"{"jsonrpc":"2.0","method":"nope","id":1}"#).unwrap();
        assert_eq!(err_code(&resp), -32601);
    }

    #[test]
    fn notification_yields_no_response() {
        let db = seeded();
        // No `id` field ⇒ notification ⇒ nothing owed, even for a real method.
        assert!(call(&db, r#"{"jsonrpc":"2.0","method":"db.stats"}"#).is_none());
    }

    #[test]
    fn notification_errors_are_still_silent() {
        let db = seeded();
        assert!(call(&db, r#"{"jsonrpc":"2.0","method":"nope"}"#).is_none());
    }

    #[test]
    fn db_stats_counts() {
        let db = seeded();
        let resp = call(&db, r#"{"jsonrpc":"2.0","method":"db.stats","id":7}"#).unwrap();
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["result"]["nodes"], 1);
        assert!(resp["result"]["planes"].as_u64().unwrap() >= 1);
        assert_eq!(resp["result"]["persistent"], false);
    }

    #[test]
    fn plane_list_includes_startup() {
        let db = seeded();
        let resp = call(&db, r#"{"jsonrpc":"2.0","method":"plane.list","id":1}"#).unwrap();
        let arr = resp["result"].as_array().unwrap();
        assert!(arr.iter().any(|p| p["name"] == "startup"));
    }

    #[test]
    fn node_get_by_key() {
        let db = seeded();
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"node.get","params":{"plane":"startup","key":"alice"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(resp["result"]["external_key"], "alice");
        assert_eq!(resp["result"]["labels"][0], "Person");
    }

    #[test]
    fn node_get_without_selector_is_invalid_params() {
        let db = seeded();
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"node.get","params":{"plane":"startup"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(err_code(&resp), -32602);
    }

    #[test]
    fn unknown_plane_is_server_error() {
        let db = seeded();
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.catalog","params":{"plane":"ghost"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(err_code(&resp), -32000);
    }

    #[test]
    fn batch_returns_one_response_per_call() {
        let db = seeded();
        let resp = call(
            &db,
            r#"[{"jsonrpc":"2.0","method":"db.stats","id":1},
                {"jsonrpc":"2.0","method":"plane.list","id":2}]"#,
        )
        .unwrap();
        assert_eq!(resp.as_array().unwrap().len(), 2);
    }

    #[test]
    fn plane_find_matches_key_and_label_case_insensitively() {
        let db = seeded(); // one node: key "alice", label "Person"

        // Substring of the key, wrong case — still a hit, flagged as a key match.
        let hit = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.find","params":{"plane":"startup","q":"ALI"},"id":1}"#,
        )
        .unwrap();
        let nodes = hit["result"]["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["external_key"], "alice");
        assert_eq!(nodes[0]["match"], "key");
        assert_eq!(hit["result"]["truncated"], false);

        // Label substring, different case.
        let by_label = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.find","params":{"plane":"startup","q":"pers"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(by_label["result"]["nodes"][0]["match"], "label: Person");

        // No match.
        let miss = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.find","params":{"plane":"startup","q":"zzz"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(miss["result"]["nodes"].as_array().unwrap().len(), 0);
        assert_eq!(miss["result"]["edges"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn semantic_find_falls_back_to_text_when_unavailable() {
        let db = seeded(); // "alice", no embeddings
        // deepseek has no embedding model, so semantic can't run (no network
        // hit) — the request must fall back to the text scan with a note.
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.find","params":{"plane":"startup","q":"ali","semantic":true,"provider":"deepseek"},"id":1}"#,
        )
        .unwrap();
        assert_eq!(resp["result"]["mode"], "text");
        assert!(
            resp["result"]["note"]
                .as_str()
                .unwrap()
                .contains("semantic unavailable")
        );
        assert_eq!(resp["result"]["nodes"][0]["external_key"], "alice");
    }

    #[test]
    fn plane_find_matches_edges_by_type() {
        let (db, alice, bob) = seeded_graph(); // alice -KNOWS-> bob

        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"plane.find","params":{"plane":"startup","q":"know"},"id":1}"#,
        )
        .unwrap();
        // No node has "know" in it; the KNOWS edge does.
        assert_eq!(resp["result"]["nodes"].as_array().unwrap().len(), 0);
        let edges = resp["result"]["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["type"], "KNOWS");
        assert_eq!(edges[0]["match"], "type");
        assert_eq!(edges[0]["src"], alice);
        assert_eq!(edges[0]["dst"], bob);
    }

    #[test]
    fn graph_seed_returns_induced_subgraph() {
        let (db, _, _) = seeded_graph();
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"graph.seed","params":{"plane":"startup"},"id":1}"#,
        )
        .unwrap();
        let r = &resp["result"];
        assert_eq!(r["nodes"].as_array().unwrap().len(), 2);
        // The alice→bob edge is induced (both endpoints in the set).
        assert_eq!(r["edges"].as_array().unwrap().len(), 1);
        assert_eq!(r["edges"][0]["type"], "KNOWS");
        assert_eq!(r["total"], 2);
        assert_eq!(r["truncated"], false);
    }

    #[test]
    fn graph_seed_respects_limit_and_reports_total() {
        let (db, _, _) = seeded_graph();
        let resp = call(
            &db,
            r#"{"jsonrpc":"2.0","method":"graph.seed","params":{"plane":"startup","limit":1},"id":1}"#,
        )
        .unwrap();
        let r = &resp["result"];
        assert_eq!(r["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(r["total"], 2);
        assert_eq!(r["truncated"], true);
    }

    #[test]
    fn graph_expand_returns_neighbor_and_edge() {
        let (db, alice, bob) = seeded_graph();
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"graph.expand","params":{{"plane":"startup","id":{alice},"direction":"out"}},"id":1}}"#
        );
        let resp = call(&db, &body).unwrap();
        let r = &resp["result"];
        assert_eq!(r["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(r["nodes"][0]["id"], bob);
        assert_eq!(r["edges"].as_array().unwrap().len(), 1);
        assert_eq!(r["total"], 1);
    }

    // ---- auth gate --------------------------------------------------------

    use crate::auth::{Credentials, SharedToken};

    /// An empty-payload `digest.write` — writes zero nodes/edges, so it reaches
    /// the handler iff the auth gate lets it through.
    const EMPTY_WRITE: &str = r#"{"jsonrpc":"2.0","method":"digest.write","params":{"plane":"startup","nodes":[],"edges":[]},"id":1}"#;

    fn native(bearer: Option<&str>) -> Credentials {
        Credentials {
            bearer: bearer.map(String::from),
            local_ui: false,
        }
    }
    fn browser(bearer: Option<&str>) -> Credentials {
        Credentials {
            bearer: bearer.map(String::from),
            local_ui: true,
        }
    }

    #[test]
    fn write_denied_for_native_client_without_token() {
        let db = seeded();
        let token = SharedToken::new(Some("s3cret".into()));
        let auth = Auth::new(&token, native(None));
        let resp = call_as(&db, &auth, EMPTY_WRITE).unwrap();
        assert_eq!(err_code(&resp), -32001);
    }

    #[test]
    fn write_allowed_with_correct_token() {
        let db = seeded();
        let token = SharedToken::new(Some("s3cret".into()));
        let auth = Auth::new(&token, native(Some("s3cret")));
        let resp = call_as(&db, &auth, EMPTY_WRITE).unwrap();
        // Reaches the handler: a real (empty) write result, not an auth error.
        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        assert_eq!(resp["result"]["nodes_written"], 0);
    }

    #[test]
    fn reads_are_gated_when_a_token_is_configured() {
        let db = seeded();
        let token = SharedToken::new(Some("s3cret".into()));
        const STATS: &str = r#"{"jsonrpc":"2.0","method":"db.stats","id":1}"#;

        // No credential → even a read is refused.
        let denied = call_as(&db, &Auth::new(&token, native(None)), STATS).unwrap();
        assert_eq!(err_code(&denied), -32001);

        // With the token → the read goes through.
        let ok = call_as(&db, &Auth::new(&token, native(Some("s3cret"))), STATS).unwrap();
        assert_eq!(ok["result"]["nodes"], 1);
    }

    #[test]
    fn native_write_denied_when_no_token_configured() {
        // With no DRSG_TOKEN set, a programmatic client still can't write —
        // only the same-origin browser UI can (see below).
        let db = seeded();
        let open = SharedToken::new(None);
        let auth = Auth::new(&open, native(None));
        let resp = call_as(&db, &auth, EMPTY_WRITE).unwrap();
        assert_eq!(err_code(&resp), -32001);
    }

    #[test]
    fn local_ui_write_allowed_when_no_token_configured() {
        let db = seeded();
        let open = SharedToken::new(None);
        let auth = Auth::new(&open, browser(None));
        let resp = call_as(&db, &auth, EMPTY_WRITE).unwrap();
        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        assert_eq!(resp["result"]["nodes_written"], 0);
    }

    #[test]
    fn all_notification_batch_yields_nothing() {
        let db = seeded();
        assert!(
            call(
                &db,
                r#"[{"jsonrpc":"2.0","method":"db.stats"},{"jsonrpc":"2.0","method":"plane.list"}]"#,
            )
            .is_none()
        );
    }
}
