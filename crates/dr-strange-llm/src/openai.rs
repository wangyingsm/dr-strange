//! OpenAI-compatible HTTP provider (arch/07 §2). Targets the widely-adopted
//! `/chat/completions` + `/embeddings` wire shape; the base URL is
//! configurable, so the same code hits OpenAI, DeepSeek, Qwen (DashScope
//! compatible-mode), a gateway, or a local `ollama`/`llama.cpp` server (see
//! [`crate::preset`]). Synchronous (`ureq`) — the CLI is sync.
//!
//! One instance = one service + one model, so chat and embeddings can come
//! from different providers (e.g. DeepSeek chat + Qwen embeddings): build two
//! instances and pass each to the role it serves.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::preset::preset;
use crate::provider::{Chat, ChatReply, EmbedReply, Embedder, OutputTruncated};

/// Output-token ceiling for extraction replies. Generous headroom so a chunk's
/// JSON is not cut off mid-object (which yields unparseable output). Chunks are
/// size-bounded upstream, so this is rarely the binding limit; it also fits the
/// preset models' output caps (deepseek-chat / qwen-plus are 8K).
///
/// Reasoning models (e.g. DeepSeek-v4-flash) are handled by `reasoning_effort:
/// "none"` in [`Chat::complete`], not by shrinking this — their thinking tokens
/// used to fill 8192 and truncate the JSON, but disabling reasoning keeps the
/// output to clean structured JSON well under any of these caps.
const MAX_OUTPUT_TOKENS: u32 = 8192;

/// Ceiling on one provider round-trip. Long enough for a slow model writing a
/// full extraction reply, short enough that a hung connection surfaces as an
/// error the caller can retry rather than an indefinite stall.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Total tries for one request, the first included. Reaching a provider over
/// the open internet fails occasionally for reasons that have nothing to do
/// with the request — a dropped handshake, a moment of congestion — and a
/// digest run makes hundreds of them. Without this, one such moment costs a
/// whole run: extraction aborts on the first chunk that errors, and the
/// reconciliation, identity and `ask` passes all propagate.
const MAX_ATTEMPTS: u32 = 4;
/// Wait before the second try; doubles each time, to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Whether an HTTP status is worth trying again. `429` and `5xx` say "not now";
/// every other 4xx is a statement about the request itself — a bad key, a
/// malformed body, a model that does not exist — and repeating it only spends
/// time and money to be told the same thing.
fn worth_retrying(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// How long to wait before try number `attempt` (2 for the first retry).
fn backoff(attempt: u32) -> Duration {
    let steps = attempt.saturating_sub(2);
    INITIAL_BACKOFF
        .saturating_mul(1u32 << steps.min(6))
        .min(MAX_BACKOFF)
}

/// Build an [`OpenAiProvider`] from a provider name (a [`preset`] or a raw base
/// URL) plus optional overrides, reading the API key from the environment. The
/// one place CLI and MCP resolve provider config the same way. `embed` selects
/// the embedding vs chat default model and, when true, requires a model.
pub fn build_provider(
    provider: &str,
    model: Option<&str>,
    url: Option<&str>,
    key_env: Option<&str>,
    embed: bool,
) -> Result<OpenAiProvider> {
    let p = preset(provider);
    let base = url
        .map(str::to_string)
        .or_else(|| p.map(|p| p.base_url.to_string()))
        .ok_or_else(|| anyhow!("unknown provider '{provider}'; give a base URL instead"))?;
    let key_env = key_env.or_else(|| p.map(|p| p.key_env)).unwrap_or("");
    let key = if key_env.is_empty() {
        String::new()
    } else {
        std::env::var(key_env).unwrap_or_default()
    };
    let model = model
        .or_else(|| p.map(|p| if embed { p.embed_model } else { p.chat_model }))
        .unwrap_or("");
    if embed && model.is_empty() {
        bail!(
            "provider '{provider}' has no embedding model — use qwen/openai/ollama, set an embed model, or disable embedding"
        );
    }
    let batch = p.map(|p| p.embed_batch).unwrap_or(64).max(1);
    Ok(OpenAiProvider::new(base, key, model).with_embed_batch(batch))
}

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    /// Max texts per embeddings request. OpenAI accepts large batches; some
    /// providers (DashScope/Qwen) cap it, so it's configurable.
    embed_batch: usize,
    /// Optional `reasoning_effort` sent on chat completions. Reasoning models
    /// (e.g. DeepSeek-v4-flash) emit long thinking tokens that fill the output
    /// cap and truncate the structured JSON dr-strange needs; setting this to
    /// "none" disables them. `None` (the default) sends nothing, keeping
    /// behavior unchanged for providers that don't need it.
    reasoning_effort: Option<String>,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            embed_batch: 256,
            reasoning_effort: None,
        }
    }

    /// Cap embeddings requests at `n` texts each (default 256).
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    /// Set the `reasoning_effort` sent on chat completions ("none" disables
    /// reasoning for models like DeepSeek-v4-flash). Default: not sent.
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    /// The model this instance uses (for provenance).
    pub fn model(&self) -> &str {
        &self.model
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}/{path}", self.base_url);
        for attempt in 1..=MAX_ATTEMPTS {
            // Bounded: ureq applies no timeout of its own, so a provider that
            // accepts the connection and then stops responding would wedge the
            // caller forever — an `ask` loop or a digest run with no way out
            // but Ctrl-C. A generous cap still ends the wait.
            let mut req = ureq::post(&url)
                .timeout(REQUEST_TIMEOUT)
                .set("Content-Type", "application/json");
            if !self.api_key.is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", self.api_key));
            }
            // Cloned per attempt: sending consumes the body.
            let (reason, wait) = match req.send_json(body.clone()) {
                Ok(resp) => {
                    return resp
                        .into_json::<Value>()
                        .context("decoding provider response");
                }
                Err(ureq::Error::Status(code, resp)) => {
                    if !worth_retrying(code) || attempt == MAX_ATTEMPTS {
                        // Surface the provider's error body — the useful part.
                        let detail = resp.into_string().unwrap_or_default();
                        bail!("{path} → HTTP {code}: {}", detail.trim())
                    }
                    // A provider that says how long to wait knows better than
                    // the schedule does.
                    let asked = resp
                        .header("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    (
                        format!("HTTP {code}"),
                        asked
                            .unwrap_or_else(|| backoff(attempt + 1))
                            .min(MAX_BACKOFF),
                    )
                }
                // Transport: no connection, a dropped one, or the timeout above.
                Err(e) => {
                    if attempt == MAX_ATTEMPTS {
                        return Err(anyhow!("{path}: {e}"));
                    }
                    (e.to_string(), backoff(attempt + 1))
                }
            };
            tracing::warn!(
                path,
                attempt,
                of = MAX_ATTEMPTS,
                retry_in_ms = wait.as_millis() as u64,
                reason,
                "provider request failed; retrying",
            );
            std::thread::sleep(wait);
        }
        unreachable!("the final attempt either returns or bails")
    }

    fn embed_once(&self, texts: &[String]) -> Result<EmbedReply> {
        let v = self.post("embeddings", json!({ "model": self.model, "input": texts }))?;
        let data = v["data"]
            .as_array()
            .context("embeddings reply had no `data`")?;
        let vectors = data
            .iter()
            .map(|d| {
                d["embedding"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_f64().map(|f| f as f32))
                            .collect()
                    })
                    .context("embedding entry missing `embedding` array")
            })
            .collect::<Result<Vec<Vec<f32>>>>()?;
        Ok(EmbedReply {
            vectors,
            tokens: v["usage"]["total_tokens"].as_u64().unwrap_or(0),
        })
    }
}

impl OpenAiProvider {
    /// The `chat/completions` request body. Split out from [`Chat::complete`]
    /// so the optional-field logic is assertable without a provider on the
    /// other end of a socket.
    fn chat_body(&self, system: &str, user: &str) -> Value {
        let mut body = json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });
        // Reasoning models (e.g. DeepSeek-v4-flash) emit long thinking tokens
        // that fill the output cap and truncate the structured JSON dr-strange
        // needs. `with_reasoning_effort("none")` opts into disabling them; when
        // unset we send nothing and keep the provider's default behavior.
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = Value::String(effort.clone());
        }
        body
    }
}

impl Chat for OpenAiProvider {
    fn complete(&self, system: &str, user: &str) -> Result<ChatReply> {
        let v = self.post("chat/completions", self.chat_body(system, user))?;
        let choice = &v["choices"][0];
        // A truncated reply (hit the output cap) is unparseable JSON — surface
        // the real cause instead of a confusing "not valid extraction JSON".
        if choice["finish_reason"] == "length" {
            tracing::warn!(
                max_output_tokens = MAX_OUTPUT_TOKENS,
                "provider reply truncated at the output-token limit"
            );
            // Typed, recoverable: digest splits the chunk and retries.
            return Err(anyhow::Error::new(OutputTruncated {
                limit: MAX_OUTPUT_TOKENS,
            }));
        }
        let text = choice["message"]["content"]
            .as_str()
            .context("provider reply had no message content")?
            .to_string();
        Ok(ChatReply {
            text,
            input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        })
    }
}

impl Embedder for OpenAiProvider {
    fn embed(&self, texts: &[String]) -> Result<EmbedReply> {
        let mut vectors = Vec::with_capacity(texts.len());
        let mut tokens = 0;
        for batch in texts.chunks(self.embed_batch) {
            let reply = self.embed_once(batch)?;
            vectors.extend(reply.vectors);
            tokens += reply.tokens;
        }
        Ok(EmbedReply { vectors, tokens })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn only_transient_statuses_are_retried() {
        // "Not now" — worth asking again.
        for s in [429, 500, 502, 503, 504] {
            assert!(worth_retrying(s), "{s} should be retried");
        }
        // Statements about the request itself: repeating it changes nothing,
        // and a wrong key would be retried four times over.
        for s in [400, 401, 403, 404, 422] {
            assert!(!worth_retrying(s), "{s} must not be retried");
        }
    }

    #[test]
    fn reasoning_effort_is_sent_only_when_asked_for() {
        let p = OpenAiProvider::new("http://example.invalid/v1", "k", "m");
        // Unset is not the same as "none": a provider that has no such field
        // must see no such field, or every non-reasoning model gets a request
        // it did not have before.
        assert!(p.chat_body("s", "u").get("reasoning_effort").is_none());

        let p = p.with_reasoning_effort("none");
        assert_eq!(p.chat_body("s", "u")["reasoning_effort"], json!("none"));
        // The rest of the body is untouched by the option.
        assert_eq!(
            p.chat_body("s", "u")["max_tokens"],
            json!(MAX_OUTPUT_TOKENS)
        );
        assert_eq!(p.chat_body("s", "u")["messages"][1]["content"], json!("u"));
    }

    #[test]
    fn backoff_doubles_and_then_stops_growing() {
        assert_eq!(backoff(2), INITIAL_BACKOFF);
        assert_eq!(backoff(3), INITIAL_BACKOFF * 2);
        assert_eq!(backoff(4), INITIAL_BACKOFF * 4);
        // However many attempts a future change allows, the wait stays bounded.
        assert_eq!(backoff(50), MAX_BACKOFF);
        assert!(backoff(4) <= MAX_BACKOFF);
    }

    /// A one-connection-at-a-time HTTP server that answers with each of
    /// `replies` in turn: `(status, body)`. Returns its base URL and a counter
    /// of the requests it actually received.
    fn server(replies: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        std::thread::spawn(move || {
            for (i, (status, body)) in replies.into_iter().enumerate() {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                counter.fetch_add(1, Ordering::Relaxed);
                let mut reader = BufReader::new(&stream);
                // Drain the request head, then the body if one was announced.
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                if len > 0 {
                    std::io::Read::read_exact(&mut reader, &mut vec![0u8; len]).ok();
                }
                let mut stream = &stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
                let _ = i;
            }
        });
        (url, seen)
    }

    fn provider(base_url: String) -> OpenAiProvider {
        OpenAiProvider::new(base_url, String::new(), "m")
    }

    #[test]
    fn a_transient_failure_is_survived() {
        // Two 503s, then the answer — exactly the shape of the connection
        // trouble that killed a live digest run.
        let (url, seen) = server(vec![
            (503, "{\"error\":\"busy\"}"),
            (503, "{\"error\":\"busy\"}"),
            (200, "{\"ok\":true}"),
        ]);
        let got = provider(url).post("x", json!({"a":1})).unwrap();
        assert_eq!(got["ok"], json!(true));
        assert_eq!(seen.load(Ordering::Relaxed), 3, "it tried until it worked");
    }

    #[test]
    fn a_rejected_request_is_not_repeated() {
        // A bad key is a bad key: asking again wastes time and says the same.
        let (url, seen) = server(vec![(401, "{\"error\":\"bad key\"}"); 4]);
        let err = provider(url).post("x", json!({})).unwrap_err().to_string();
        assert!(err.contains("401") && err.contains("bad key"), "{err}");
        assert_eq!(seen.load(Ordering::Relaxed), 1, "asked exactly once");
    }

    #[test]
    fn the_provider_error_survives_exhausted_retries() {
        let (url, seen) = server(vec![
            (500, "{\"error\":\"still down\"}");
            MAX_ATTEMPTS as usize
        ]);
        let err = provider(url).post("x", json!({})).unwrap_err().to_string();
        assert!(
            err.contains("still down"),
            "the last body is reported: {err}"
        );
        assert_eq!(seen.load(Ordering::Relaxed), MAX_ATTEMPTS as usize);
    }
}
