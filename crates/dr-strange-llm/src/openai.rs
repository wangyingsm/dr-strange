//! OpenAI-compatible HTTP provider (arch/07 §2). Targets the widely-adopted
//! `/chat/completions` + `/embeddings` wire shape; the base URL is
//! configurable, so the same code hits OpenAI, DeepSeek, Qwen (DashScope
//! compatible-mode), a gateway, or a local `ollama`/`llama.cpp` server (see
//! [`crate::preset`]). Synchronous (`ureq`) — the CLI is sync.
//!
//! One instance = one service + one model, so chat and embeddings can come
//! from different providers (e.g. DeepSeek chat + Qwen embeddings): build two
//! instances and pass each to the role it serves.

use std::sync::{Condvar, Mutex};
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

/// Embeddings are small, fast requests — a stalled endpoint should fail a
/// `search` in seconds, not hold it for the chat-completion budget above.
const EMBED_TIMEOUT: Duration = Duration::from_secs(60);

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

/// Clean requests a throttled provider must serve before it is allowed one
/// more in flight. Deliberately slow: climbing back costs a refusal every time
/// it overshoots, and a digest run would rather be a little under the limit
/// than repeatedly at it.
const CLIMB_AFTER: u32 = 24;

/// How many requests this provider keeps in flight, learned from the provider's
/// own refusals.
///
/// A 429 is two different complaints wearing one status code. *Too fast* is
/// answered by waiting, which [`backoff`] does. *Too many at once* is not: every
/// worker that waits comes back through the same crowded doorway, in step with
/// the others it was refused with, and the run dies with attempts to spare —
/// which is exactly what an eight-worker digest does to an account whose
/// concurrency limit is five.
///
/// So the ceiling drops when the provider objects and climbs back when it stops
/// objecting. A run settles at whatever the account actually allows, without an
/// operator having to know the number or an unrelated default having to guess
/// it.
struct Throttle {
    gate: Mutex<Gate>,
    room: Condvar,
}

struct Gate {
    /// Requests allowed in flight. [`usize::MAX`] until the provider objects:
    /// an account that never refuses is never held back.
    limit: usize,
    in_flight: usize,
    /// Bumped on every cut, and stamped on each permit. One refusal per crowd,
    /// not one per member of it — eight workers refused together would
    /// otherwise cut the limit eight times over.
    epoch: u64,
    /// The most the limit may climb back to. Never the crowd size that was
    /// refused: that number is known to be too many.
    ceiling: usize,
    /// Clean requests since the last cut.
    ok: u32,
}

/// A request's place in flight, counted while it is out and returned on drop.
struct Permit<'a> {
    throttle: &'a Throttle,
    epoch: u64,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut gate = self.throttle.gate.lock().unwrap();
        gate.in_flight -= 1;
        drop(gate);
        self.throttle.room.notify_one();
    }
}

impl Throttle {
    fn new() -> Self {
        Self {
            gate: Mutex::new(Gate {
                limit: usize::MAX,
                in_flight: 0,
                epoch: 0,
                ceiling: usize::MAX,
                ok: 0,
            }),
            room: Condvar::new(),
        }
    }

    /// Wait for room, then take it.
    fn enter(&self) -> Permit<'_> {
        let mut gate = self.gate.lock().unwrap();
        while gate.in_flight >= gate.limit {
            gate = self.room.wait(gate).unwrap();
        }
        gate.in_flight += 1;
        Permit {
            throttle: self,
            epoch: gate.epoch,
        }
    }

    /// The provider refused this request for crowding: halve what is allowed.
    ///
    /// The crowd size is observed rather than parsed — providers word the
    /// refusal differently and some do not name a number at all, but every one
    /// of them refuses while this many requests are out.
    fn refused(&self, permit: &Permit<'_>) {
        let mut gate = self.gate.lock().unwrap();
        if permit.epoch != gate.epoch {
            return; // already cut for the crowd this request was part of
        }
        let crowd = gate.limit.min(gate.in_flight).max(1);
        gate.limit = (crowd / 2).max(1);
        gate.ceiling = gate.ceiling.min(crowd.saturating_sub(1)).max(1);
        gate.epoch += 1;
        gate.ok = 0;
    }

    /// The provider served this request: earn back a slot, eventually.
    fn allowed(&self) {
        let mut gate = self.gate.lock().unwrap();
        if gate.limit >= gate.ceiling {
            return; // never throttled, or already back at the ceiling
        }
        gate.ok += 1;
        if gate.ok >= CLIMB_AFTER {
            gate.ok = 0;
            gate.limit += 1;
            drop(gate);
            self.room.notify_one();
        }
    }

    /// What is allowed in flight right now — `None` while nothing has been
    /// refused. For tests and for the log line that reports a cut.
    #[cfg(test)]
    fn limit(&self) -> Option<usize> {
        let limit = self.gate.lock().unwrap().limit;
        (limit != usize::MAX).then_some(limit)
    }
}

/// A little noise on every wait, so a crowd refused together does not return
/// together — the retry schedule is deterministic, and workers that started as
/// one batch would otherwise stay in step through every attempt.
fn jittered(wait: Duration) -> Duration {
    let spread = (wait.as_millis() as u64 / 4).max(1);
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    wait + Duration::from_millis(noise % spread)
}

/// How long to wait before try number `attempt` (2 for the first retry).
fn backoff(attempt: u32) -> Duration {
    let steps = attempt.saturating_sub(2);
    INITIAL_BACKOFF
        .saturating_mul(1u32 << steps.min(6))
        .min(MAX_BACKOFF)
}

/// A provider name that is a base URL rather than a preset. Scheme-bearing is
/// the whole test: preset names are bare words, and anything else was already
/// an error, so this cannot reclassify a name that used to resolve.
fn looks_like_url(provider: &str) -> bool {
    provider.contains("://")
}

/// Build an [`OpenAiProvider`] from a provider name (a [`preset`] or a raw base
/// URL) plus optional overrides, reading the API key from the environment. The
/// one place CLI and MCP resolve provider config the same way. `embed` selects
/// the embedding vs chat default model and, when true, requires a model.
///
/// A raw base URL has no preset behind it, so it carries no default key env and
/// no default model: pass `key_env` when the endpoint needs a key, and `model`
/// (always, for embeddings — an empty embedding model is rejected below).
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
        // The name itself may be the base URL — what this function's own doc
        // line and [`preset`]'s ("the caller then treats it as a raw base URL")
        // have always said, and what the CLI help and the MCP tool schemas
        // advertise. Only the explicit `url` override implemented it, so every
        // caller that had no such override — `drsg ask`, query-time embedding,
        // `digest.run` over RPC, and the MCP tools — answered a URL with
        // "unknown provider ...; give a base URL instead", advice those
        // surfaces gave no way to follow.
        .or_else(|| looks_like_url(provider).then(|| provider.to_string()))
        .ok_or_else(|| anyhow!("unknown provider '{provider}'; give a base URL instead"))?;
    let key_env = key_env.or_else(|| p.map(|p| p.key_env)).unwrap_or("");
    // A missing key is not an error *here* — providers are built eagerly for
    // runs that may never call them (`--no-embed`, dry runs). It is recorded,
    // and the first actual request fails fast with the variable's name
    // instead of going to the network keyless and stalling.
    let (key, missing) = if key_env.is_empty() {
        (String::new(), None)
    } else {
        match std::env::var(key_env) {
            Ok(k) if !k.trim().is_empty() => (k, None),
            _ => (String::new(), Some(key_env.to_string())),
        }
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
    let mut provider = OpenAiProvider::new(base, key, model).with_embed_batch(batch);
    provider.missing_key_env = missing;
    Ok(provider)
}

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    /// Max texts per embeddings request. OpenAI accepts large batches; some
    /// providers (DashScope/Qwen) cap it, so it's configurable.
    embed_batch: usize,
    /// The key environment variable that was expected but not set — the
    /// first request bails fast with its name instead of stalling keyless.
    missing_key_env: Option<String>,
    /// Optional `reasoning_effort` sent on chat completions. Reasoning models
    /// (e.g. DeepSeek-v4-flash) emit long thinking tokens that fill the output
    /// cap and truncate the structured JSON dr-strange needs; setting this to
    /// "none" disables them. `None` (the default) sends nothing, keeping
    /// behavior unchanged for providers that don't need it.
    reasoning_effort: Option<String>,
    /// In-flight ceiling, learned from the provider's refusals. Shared by every
    /// thread holding this instance, which is how a digest run's workers come
    /// to agree on a limit none of them was told.
    throttle: Throttle,
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
            missing_key_env: None,
            reasoning_effort: None,
            throttle: Throttle::new(),
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
        self.post_with_timeout(path, body, REQUEST_TIMEOUT)
    }

    fn post_with_timeout(&self, path: &str, body: Value, timeout: Duration) -> Result<Value> {
        // A key the environment never provided fails here, in microseconds,
        // with the variable's name — not at the far end of a network stall.
        if let Some(env) = &self.missing_key_env {
            bail!(
                "environment variable {env} is not set — the provider at {} \
                 needs a key; export it (never put it in a config file)",
                self.base_url
            );
        }
        let url = format!("{}/{path}", self.base_url);
        for attempt in 1..=MAX_ATTEMPTS {
            // Bounded: ureq applies no timeout of its own, so a provider that
            // accepts the connection and then stops responding would wedge the
            // caller forever — an `ask` loop or a digest run with no way out
            // but Ctrl-C. A generous cap still ends the wait.
            let mut req = ureq::post(&url)
                .timeout(timeout)
                .set("Content-Type", "application/json");
            if !self.api_key.is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", self.api_key));
            }
            // Held across the send only: a request waiting out its backoff is
            // not in flight, and holding its slot would keep the crowd that
            // caused the refusal at full size.
            let permit = self.throttle.enter();
            // Cloned per attempt: sending consumes the body.
            let (reason, wait) = match req.send_json(body.clone()) {
                Ok(resp) => {
                    self.throttle.allowed();
                    return resp
                        .into_json::<Value>()
                        .context("decoding provider response");
                }
                Err(ureq::Error::Status(code, resp)) => {
                    if code == 429 {
                        self.throttle.refused(&permit);
                    }
                    if !worth_retrying(code) || attempt == MAX_ATTEMPTS {
                        // Surface the provider's error body — the useful part.
                        let detail = resp.into_string().unwrap_or_default();
                        if code == 429 {
                            bail!(
                                "{path} → HTTP {code}: {}\n\
                                 refused on all {MAX_ATTEMPTS} attempts, down to \
                                 {} request(s) in flight — the account allows less \
                                 than this run asks for. Lower `concurrency` \
                                 (`--concurrency`, `[digest] concurrency`, or the \
                                 `concurrency` field on `digest.run`).",
                                detail.trim(),
                                self.throttle.gate.lock().unwrap().limit,
                            )
                        }
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
            drop(permit);
            let wait = jittered(wait);
            tracing::warn!(
                path,
                attempt,
                of = MAX_ATTEMPTS,
                retry_in_ms = wait.as_millis() as u64,
                in_flight_limit = self.throttle.gate.lock().unwrap().limit,
                reason,
                "provider request failed; retrying",
            );
            std::thread::sleep(wait);
        }
        unreachable!("the final attempt either returns or bails")
    }

    fn embed_once(&self, texts: &[String]) -> Result<EmbedReply> {
        let v = self.post_with_timeout(
            "embeddings",
            json!({ "model": self.model, "input": texts }),
            EMBED_TIMEOUT,
        )?;
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

    #[test]
    fn a_provider_name_that_is_a_url_becomes_the_base_url() {
        // The contract the doc line above has always stated. Every caller with
        // no explicit `url` override depends on it: `drsg ask`, query-time
        // embedding, `digest.run` over RPC, and the MCP tools.
        let p = build_provider("http://127.0.0.1:1234/v1", Some("m"), None, None, false).unwrap();
        assert_eq!(p.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(p.model(), "m");
        // No preset behind it, so no key env is invented and no key is read.
        assert_eq!(p.api_key, "");
    }

    #[test]
    fn presets_and_explicit_urls_keep_precedence() {
        // A preset name resolves to the preset exactly as before.
        let p = build_provider("openai", None, None, None, false).unwrap();
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.model(), "gpt-4o-mini");
        // An explicit `url` still wins over a URL-shaped name.
        let p = build_provider(
            "http://name/v1",
            Some("m"),
            Some("http://override/v1"),
            None,
            false,
        )
        .unwrap();
        assert_eq!(p.base_url, "http://override/v1");
    }

    #[test]
    fn a_bare_unknown_name_is_still_an_error() {
        // Carrying a scheme is the whole test, so a mistyped preset keeps its
        // error instead of being taken for a hostname.
        let Err(e) = build_provider("opanai", None, None, None, false) else {
            panic!("a bare unknown name must not resolve to a provider");
        };
        assert!(e.to_string().contains("unknown provider"), "{e}");
    }

    #[test]
    fn a_url_provider_still_needs_an_embedding_model() {
        // A preset supplies a default embedding model; a bare URL cannot, so
        // the existing guard has to fire rather than send an empty model.
        let Err(e) = build_provider("http://127.0.0.1:1234/v1", None, None, None, true) else {
            panic!("an embedding provider with no model must be rejected");
        };
        assert!(e.to_string().contains("no embedding model"), "{e}");
    }

    #[test]
    fn a_missing_key_fails_fast_at_first_use_not_at_the_network() {
        // Build stays lenient (a --no-embed run never calls the provider),
        // but the first request must bail with the variable's name before
        // any network I/O.
        let p = build_provider(
            "http://127.0.0.1:9/v1",
            Some("m"),
            None,
            Some("DRSG_TEST_KEY_THAT_IS_NEVER_SET"),
            true,
        )
        .expect("build is lazy about keys");
        let Err(e) = p.embed_once(&["x".into()]) else {
            panic!("a keyless request to a key-wanting provider must fail");
        };
        let msg = e.to_string();
        assert!(
            msg.contains("DRSG_TEST_KEY_THAT_IS_NEVER_SET") && msg.contains("not set"),
            "{msg}"
        );
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

    /// A server that answers everything but refuses with 429 whenever more than
    /// `allowed` requests are in flight at once — an account's concurrency
    /// limit, which is the one refusal waiting cannot fix. Returns its base URL
    /// and a count of the refusals it issued.
    fn crowded_server(allowed: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let refusals = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&refusals);
        std::thread::spawn(move || {
            let live = Arc::new(AtomicUsize::new(0));
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let live = Arc::clone(&live);
                let refusals = Arc::clone(&counted);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
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
                    // Counted while the request is being served, so requests
                    // that overlap are seen to overlap.
                    let crowd = live.fetch_add(1, Ordering::SeqCst) + 1;
                    std::thread::sleep(Duration::from_millis(30));
                    let (status, body) = if crowd > allowed {
                        refusals.fetch_add(1, Ordering::SeqCst);
                        (
                            429,
                            "{\"error\":{\"message\":\"Too many requests. Your current concurrency exceeds your concurrency limit.\"}}",
                        )
                    } else {
                        (200, "{\"ok\":true}")
                    };
                    live.fetch_sub(1, Ordering::SeqCst);
                    let mut stream = &stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.flush();
                });
            }
        });
        (url, refusals)
    }

    /// The bug this fixes: eight workers against an account that allows three.
    /// Backing off answers *too fast*; it does not answer *too many at once*,
    /// because every worker that waits returns to the same crowded doorway.
    ///
    /// What is pinned here is the learning — without it the provider ends the
    /// run having drawn no conclusion from a wall of refusals, whatever luck
    /// the retry schedule had — and that the work still finishes.
    #[test]
    fn a_crowding_refusal_shrinks_what_is_kept_in_flight() {
        const WORKERS: usize = 8;
        const EACH: usize = 4; // several apiece, so the crowd is sustained
        let (url, refusals) = crowded_server(3);
        let p = provider(url);
        std::thread::scope(|s| {
            for _ in 0..WORKERS {
                s.spawn(|| {
                    for _ in 0..EACH {
                        p.post("x", json!({}))
                            .expect("a crowded provider must still finish the work");
                    }
                });
            }
        });
        assert!(
            refusals.load(Ordering::SeqCst) > 0,
            "the server must actually have been crowded, or this proves nothing"
        );
        let limit = p
            .throttle
            .limit()
            .expect("a refusal must leave a learned ceiling");
        assert!(
            limit < WORKERS,
            "the provider must keep fewer in flight than the crowd it was refused for, got {limit}"
        );
    }

    #[test]
    fn a_crowd_is_cut_once_and_climbs_back_no_further_than_it_should() {
        let t = Throttle::new();
        assert_eq!(t.limit(), None, "nothing is held back until something is");

        let (a, b, c, d) = (t.enter(), t.enter(), t.enter(), t.enter());
        t.refused(&a);
        assert_eq!(t.limit(), Some(2), "four in flight, refused → half of them");
        // The other three of that crowd are refused too; one refusal is what
        // the crowd earns, not one per member.
        t.refused(&b);
        t.refused(&c);
        t.refused(&d);
        assert_eq!(t.limit(), Some(2));
        drop((a, b, c, d));

        // The climb back is earned, slowly, and stops below the crowd size that
        // was refused — that number is known to be too many.
        for _ in 0..CLIMB_AFTER * 4 {
            t.allowed();
        }
        assert_eq!(t.limit(), Some(3), "climbs to the ceiling and no further");
    }

    #[test]
    fn a_wait_is_jittered_so_a_crowd_does_not_return_in_step() {
        let base = Duration::from_millis(800);
        // Never shorter than asked, never more than a quarter longer.
        for _ in 0..50 {
            let w = jittered(base);
            assert!(w >= base && w < base + base / 4, "{w:?}");
        }
        // A wait too short to divide still returns something valid.
        assert!(jittered(Duration::from_millis(1)) >= Duration::from_millis(1));
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
