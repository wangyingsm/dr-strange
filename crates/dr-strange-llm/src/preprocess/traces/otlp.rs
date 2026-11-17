//! Reading an OTLP/JSON trace export.
//!
//! OTLP/JSON is proto-JSON with two deviations the specification states, and
//! both of them are where a reader written from the proto file alone goes
//! wrong: `traceId` and `spanId` are hex strings, *not* the base64 the
//! standard mapping would give, and enum fields are integers, with the names
//! forbidden. Unknown fields must be ignored, which serde does by default.
//!
//! The file exporter writes JSON Lines — one document per line, batched by
//! whatever the collector flushed, with no ordering guarantee. So a line is
//! not a trace and a parent span may arrive on a later line than its child,
//! and nothing here links spans: that is the fold's job, over every document
//! read.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::Span;

/// OTLP's `ERROR` status code.
const STATUS_ERROR: i64 = 2;

/// The attribute that names the service a span came from.
const SERVICE_NAME: &str = "service.name";

/// What one export yielded, and what it could not say.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// The spans, in the order read.
    pub spans: Vec<Span>,
    /// How many OTLP documents the text held — one, or one per line.
    pub documents: usize,
    /// Spans whose resource named no `service.name`. They are dropped: a span
    /// that cannot say which service emitted it cannot take part in an edge
    /// between services.
    pub anonymous: usize,
}

/// Read an OTLP/JSON trace export: one document, or JSON Lines.
///
/// Fails when nothing in `text` holds `resourceSpans`, rather than reporting
/// an empty read — any JSON object parses as an export whose spans are absent,
/// and "0 spans" is the wrong answer to the wrong file.
pub fn read(text: &str) -> Result<Read> {
    let mut out = Read::default();
    match serde_json::from_str::<TracesData>(text) {
        Ok(doc) => take(doc, &mut out),
        Err(whole) => {
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let doc: TracesData = serde_json::from_str(line)
                    .with_context(|| format!("line {}", n + 1))
                    .with_context(|| {
                        format!("not one OTLP/JSON document either ({whole}), nor JSON Lines")
                    })?;
                take(doc, &mut out);
            }
        }
    }
    if out.documents == 0 {
        bail!("no `resourceSpans` here — is this an OTLP/JSON trace export?");
    }
    Ok(out)
}

/// Add one document's spans to the read, counting the document only where it
/// held the field that makes it an export.
fn take(doc: TracesData, out: &mut Read) {
    let Some(resources) = doc.resource_spans else {
        return;
    };
    out.documents += 1;
    for resource in resources {
        let service = resource.resource.and_then(|r| r.service());
        for scope in resource.scope_spans {
            for span in scope.spans {
                match &service {
                    Some(service) => out.spans.push(span.into_span(service)),
                    None => out.anonymous += 1,
                }
            }
        }
    }
}

/// A number proto-JSON may write quoted — `uint64` always is — or an enum a
/// producer wrote as its name although the specification forbids it.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Int(i64),
    Text(String),
}

impl Scalar {
    /// The integer it holds, parsing a quoted one.
    fn int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            Self::Text(t) => t.parse().ok(),
        }
    }

    /// Whether it is OTLP's `ERROR`, by number or by the forbidden name.
    fn is_error(&self) -> bool {
        match self {
            Self::Int(n) => *n == STATUS_ERROR,
            Self::Text(t) => t == "STATUS_CODE_ERROR",
        }
    }
}

/// The top level of an export, and of `ExportTraceServiceRequest`, which carry
/// the same field.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TracesData {
    #[serde(default)]
    resource_spans: Option<Vec<ResourceSpans>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans {
    #[serde(default)]
    resource: Option<Resource>,
    #[serde(default)]
    scope_spans: Vec<ScopeSpans>,
}

#[derive(Debug, Deserialize)]
struct Resource {
    #[serde(default)]
    attributes: Vec<KeyValue>,
}

impl Resource {
    /// The `service.name` attribute, where it is a string.
    fn service(self) -> Option<String> {
        self.attributes
            .into_iter()
            .find(|a| a.key == SERVICE_NAME)
            .and_then(|a| a.value.string_value)
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct KeyValue {
    key: String,
    #[serde(default)]
    value: AnyValue,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnyValue {
    #[serde(default)]
    string_value: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeSpans {
    #[serde(default)]
    spans: Vec<RawSpan>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSpan {
    #[serde(default)]
    trace_id: String,
    #[serde(default)]
    span_id: String,
    #[serde(default)]
    parent_span_id: String,
    #[serde(default)]
    start_time_unix_nano: Option<Scalar>,
    #[serde(default)]
    status: Option<Status>,
}

impl RawSpan {
    /// The span this states, attributed to `service`.
    fn into_span(self, service: &str) -> Span {
        Span {
            trace: self.trace_id.to_ascii_lowercase(),
            id: self.span_id.to_ascii_lowercase(),
            parent: Some(self.parent_span_id)
                .filter(|p| !p.is_empty())
                .map(|p| p.to_ascii_lowercase()),
            service: service.to_string(),
            start_ms: self
                .start_time_unix_nano
                .as_ref()
                .and_then(Scalar::int)
                .unwrap_or_default()
                / 1_000_000,
            error: self
                .status
                .and_then(|s| s.code)
                .is_some_and(|c| c.is_error()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    code: Option<Scalar>,
}

#[cfg(test)]
mod tests;
