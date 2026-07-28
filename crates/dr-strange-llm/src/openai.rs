//! OpenAI-compatible HTTP provider (arch/07 §2). Targets the widely-adopted
//! `/chat/completions` + `/embeddings` wire shape; the base URL is
//! configurable, so the same code hits OpenAI, DeepSeek, Qwen (DashScope
//! compatible-mode), a gateway, or a local `ollama`/`llama.cpp` server (see
//! [`crate::preset`]). Synchronous (`ureq`) — the CLI is sync.
//!
//! One instance = one service + one model, so chat and embeddings can come
//! from different providers (e.g. DeepSeek chat + Qwen embeddings): build two
//! instances and pass each to the role it serves.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::provider::{Chat, ChatReply, EmbedReply, Embedder};

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    model: String,
    /// Max texts per embeddings request. OpenAI accepts large batches; some
    /// providers (DashScope/Qwen) cap it, so it's configurable.
    embed_batch: usize,
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
        }
    }

    /// Cap embeddings requests at `n` texts each (default 256).
    pub fn with_embed_batch(mut self, n: usize) -> Self {
        self.embed_batch = n.max(1);
        self
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}/{path}", self.base_url);
        let mut req = ureq::post(&url).set("Content-Type", "application/json");
        if !self.api_key.is_empty() {
            req = req.set("Authorization", &format!("Bearer {}", self.api_key));
        }
        match req.send_json(body) {
            Ok(resp) => resp
                .into_json::<Value>()
                .context("decoding provider response"),
            // Surface the provider's error body — the useful part.
            Err(ureq::Error::Status(code, resp)) => {
                let detail = resp.into_string().unwrap_or_default();
                bail!("{path} → HTTP {code}: {}", detail.trim())
            }
            Err(e) => Err(anyhow::anyhow!("{path}: {e}")),
        }
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

impl Chat for OpenAiProvider {
    fn complete(&self, system: &str, user: &str) -> Result<ChatReply> {
        let v = self.post(
            "chat/completions",
            json!({
                "model": self.model,
                "temperature": 0,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ],
            }),
        )?;
        let text = v["choices"][0]["message"]["content"]
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
