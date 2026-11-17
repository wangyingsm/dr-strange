//! Spans out of OpenObserve, where a file exporter is not in the picture.
//!
//! The same fold as a file ([`super::fold`]), reached by a different reader —
//! and the difference is the window. A file holds whatever the exporter
//! flushed; a query holds what a stated interval held, which is the thing a
//! reader of the graph wants to know. So the interval is asked for, recorded
//! on nothing, and reported.
//!
//! The query is OpenObserve's SQL search endpoint over a traces stream, and
//! the column names are its own: `span_id`, `trace_id`, `service_name`,
//! `span_status`, `start_time` in **nanoseconds**, and the parent under
//! `reference_parent_span_id` — a column OpenObserve itself probes for before
//! drawing parent-child edges, because a stream whose schema predates it
//! holds spans and no links. That absence is reported rather than returned as
//! a graph with no edges.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use dr_strange_net::Network;
use serde_json::{Value, json};

use super::Span;

/// What one pull asks for.
#[derive(Debug, Clone)]
pub struct Query {
    /// The instance's base URL, e.g. `https://observe.internal:5080`.
    pub base_url: String,
    /// The organization, `default` on a single-tenant install.
    pub org: String,
    /// The traces stream, `default` unless the collector was told otherwise.
    pub stream: String,
    /// The window's start, unix-epoch microseconds — the unit the endpoint
    /// takes, not the one the spans carry.
    pub start_us: i64,
    /// The window's end, unix-epoch microseconds.
    pub end_us: i64,
    /// Ceiling on rows returned. A pull that reaches it is reported as having
    /// reached it: the window held more than was read.
    pub limit: usize,
}

/// What a pull brought back.
#[derive(Debug, Default)]
pub struct Pulled {
    /// The spans, in the shape the fold takes.
    pub spans: Vec<Span>,
    /// Rows the response held, before any were dropped for want of a service
    /// or a span id.
    pub rows: usize,
    /// Whether the row ceiling was reached, so the window holds more.
    pub capped: bool,
    /// Whether no row carried `reference_parent_span_id`. Either the stream's
    /// schema has no such column, or every span read is a root — the message
    /// says both, because the two cannot be told apart from here.
    pub no_parent_column: bool,
}

/// The seconds in a `30s` / `15m` / `6h` / `7d` window.
///
/// Deliberately small: a window is one unit, because `1h30m` invites a reader
/// to believe the pull is precise about a boundary that sampling already made
/// approximate.
pub fn window(spec: &str) -> Result<Duration> {
    let (count, unit) = spec.split_at(
        spec.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| anyhow!("{spec} names no unit — try 30m, 6h or 7d"))?,
    );
    let count: u64 = count
        .parse()
        .with_context(|| format!("{spec} does not start with a count"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        other => bail!("{other} is not a unit of time — use s, m, h or d"),
    };
    if count == 0 {
        bail!("a window of {spec} holds nothing");
    }
    Ok(Duration::from_secs(count * secs))
}

/// The endpoint a pull posts to.
pub fn url(query: &Query) -> String {
    format!(
        "{}/api/{}/_search?type=traces",
        query.base_url.trim_end_matches('/'),
        query.org
    )
}

/// The request body: the columns the fold needs, and nothing else.
///
/// `SELECT *` would carry every attribute a service ever set, which on a busy
/// stream is most of the bytes for none of the answer.
pub fn body(query: &Query) -> Value {
    json!({
        "query": {
            "sql": format!(
                "SELECT trace_id, span_id, reference_parent_span_id, service_name, \
                 span_status, start_time FROM \"{}\" ORDER BY start_time",
                query.stream.replace('"', "\"\"")
            ),
            "start_time": query.start_us,
            "end_time": query.end_us,
            "from": 0,
            "size": query.limit,
        },
        "search_type": "ui",
    })
}

/// The `Authorization` header value for what the operator put in the
/// environment: an already-encoded token as OpenObserve shows it, or
/// `user:password`, which is encoded here.
///
/// Never a command-line flag either way — an argument is visible to every
/// process on the machine and lands in the shell's history.
pub fn authorization(secret: &str) -> String {
    let secret = secret.trim();
    match secret.contains(':') {
        true => format!("Basic {}", base64(secret.as_bytes())),
        false => format!("Basic {secret}"),
    }
}

/// Ask, and turn the answer into spans.
pub fn pull(query: &Query, secret: &str, net: &Network, timeout: Duration) -> Result<Pulled> {
    let url = url(query);
    let parsed =
        url::Url::parse(&url).with_context(|| format!("{} is not a URL", query.base_url))?;
    let agent: ureq::Agent = net
        .apply(
            ureq::Agent::config_builder(),
            dr_strange_net::Destination::of(&parsed),
        )?
        // The instance's own message is in the body — a bare 400 would say
        // nothing about which column or stream it objected to.
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &authorization(secret))
        .send_json(body(query))
        .map_err(|e| anyhow!("asking {} for spans: {e}", shown(&query.base_url)))?;
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .read_to_string()
        .with_context(|| format!("reading the answer from {}", shown(&query.base_url)))?;
    if status != 200 {
        bail!(
            "{} answered {status}: {}",
            shown(&query.base_url),
            terse(&text)
        );
    }
    spans(&text, query.limit)
}

/// The spans in a search response, against the row ceiling that was asked
/// for.
///
/// The ceiling is a parameter because the response's own `total` is the rows
/// *returned* — a 5-row answer to a query that scanned 120 reports
/// `"total": 5`, as a live instance does. Only the request knows what a full
/// page means.
pub fn spans(text: &str, limit: usize) -> Result<Pulled> {
    let body: Value = serde_json::from_str(text)
        .with_context(|| format!("the answer is not JSON: {}", terse(text)))?;
    let Some(hits) = body.get("hits").and_then(Value::as_array) else {
        bail!(
            "no `hits` in the answer — is this an OpenObserve search endpoint? {}",
            terse(text)
        );
    };
    let mut out = Pulled {
        rows: hits.len(),
        capped: limit > 0 && hits.len() >= limit,
        no_parent_column: true,
        ..Pulled::default()
    };
    for hit in hits {
        if hit
            .get("reference_parent_span_id")
            .is_some_and(|v| !v.is_null())
        {
            out.no_parent_column = false;
        }
        let (Some(id), Some(service)) = (text_of(hit, "span_id"), text_of(hit, "service_name"))
        else {
            continue;
        };
        out.spans.push(Span {
            trace: text_of(hit, "trace_id").unwrap_or_default(),
            id,
            parent: text_of(hit, "reference_parent_span_id"),
            service,
            // Stored in nanoseconds, whatever the window was asked for in.
            start_ms: int_of(hit, "start_time").unwrap_or(0) / 1_000_000,
            error: text_of(hit, "span_status").as_deref() == Some("ERROR"),
        });
    }
    Ok(out)
}

/// A string column, where the row has one.
fn text_of(hit: &Value, name: &str) -> Option<String> {
    match hit.get(name) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// An integer column, quoted or not — a 64-bit nanosecond timestamp does not
/// survive JSON's number type everywhere, so some writers quote it.
fn int_of(hit: &Value, name: &str) -> Option<i64> {
    match hit.get(name) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// The base URL without its credentials, for a message.
fn shown(base_url: &str) -> String {
    match url::Url::parse(base_url) {
        Ok(mut url) if !url.username().is_empty() || url.password().is_some() => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.to_string()
        }
        _ => base_url.to_string(),
    }
}

/// Enough of a body to recognise the problem, not enough to flood a terminal.
fn terse(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(300) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

/// RFC 4648 base64, standard alphabet, padded.
///
/// Fifteen lines against a dependency: this is the only base64 in the tree,
/// and it encodes one short credential.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut word = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * i);
        }
        for i in 0..4 {
            match i <= chunk.len() {
                true => out.push(ALPHABET[(word >> (18 - 6 * i)) as usize & 0x3f] as char),
                false => out.push('='),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
