//! OpenAI-compatible HTTP provider (arch/07 §2). Targets the widely-adopted
//! `/chat/completions` + `/embeddings` wire shape; the base URL is
//! configurable, so the same code hits OpenAI, a gateway, or a local
//! `ollama`/`llama.cpp` server. Synchronous (`ureq`) — the CLI is sync.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::provider::{Chat, ChatReply, EmbedReply, Embedder};

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    chat_model: String,
    embed_model: String,
}

impl OpenAiProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        chat_model: impl Into<String>,
        embed_model: impl Into<String>,
    ) -> Self {
        let base = base_url.into();
        Self {
            base_url: base.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            chat_model: chat_model.into(),
            embed_model: embed_model.into(),
        }
    }

    fn post(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}/{path}", self.base_url);
        match ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(body)
        {
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
}

impl Chat for OpenAiProvider {
    fn complete(&self, system: &str, user: &str) -> Result<ChatReply> {
        let v = self.post(
            "chat/completions",
            json!({
                "model": self.chat_model,
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
        let v = self.post(
            "embeddings",
            json!({ "model": self.embed_model, "input": texts }),
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
