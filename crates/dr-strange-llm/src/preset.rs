//! Named provider presets (arch/07 §2). All are OpenAI-compatible endpoints,
//! so [`OpenAiProvider`](crate::OpenAiProvider) speaks to every one; a preset
//! just fills in the base URL, key env var, default models, and embedding
//! batch cap so callers say `--chat deepseek --embed qwen`, not URLs.
//!
//! Endpoints/models here are sensible defaults as of writing; every field is
//! overridable, so if a provider moves an endpoint or renames a model, point
//! the flags at the new one.

/// A provider's connection defaults. Any field may be overridden by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    pub base_url: &'static str,
    pub key_env: &'static str,
    pub chat_model: &'static str,
    /// Empty when the provider has no embeddings endpoint (e.g. DeepSeek).
    pub embed_model: &'static str,
    pub embed_batch: usize,
}

/// Resolve a preset by name (`openai`, `deepseek`, `qwen`, `ollama`), or `None`
/// for an unknown name — the caller then treats it as a raw base URL.
pub fn preset(name: &str) -> Option<ProviderPreset> {
    Some(match name {
        "openai" => ProviderPreset {
            base_url: "https://api.openai.com/v1",
            key_env: "OPENAI_API_KEY",
            chat_model: "gpt-4o-mini",
            embed_model: "text-embedding-3-small",
            embed_batch: 256,
        },
        // DeepSeek: OpenAI-compatible chat; no embeddings endpoint (pair it
        // with a separate embed provider such as `qwen`).
        "deepseek" => ProviderPreset {
            base_url: "https://api.deepseek.com",
            key_env: "DEEPSEEK_API_KEY",
            chat_model: "deepseek-chat",
            embed_model: "",
            embed_batch: 0,
        },
        // Qwen via Alibaba DashScope OpenAI-compatible mode. Use the
        // `dashscope-intl` host outside mainland China (override --embed-url).
        "qwen" => ProviderPreset {
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            key_env: "DASHSCOPE_API_KEY",
            chat_model: "qwen-plus",
            embed_model: "text-embedding-v4",
            embed_batch: 10, // DashScope caps embedding batches
        },
        // Local OpenAI-compatible server (Ollama). No key needed.
        "ollama" => ProviderPreset {
            base_url: "http://localhost:11434/v1",
            key_env: "OLLAMA_API_KEY",
            chat_model: "llama3.1",
            embed_model: "nomic-embed-text",
            embed_batch: 64,
        },
        _ => return None,
    })
}

/// The preset names, for CLI help / validation.
pub const PRESET_NAMES: &[&str] = &["openai", "deepseek", "qwen", "ollama"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_presets_resolve_unknown_is_none() {
        assert_eq!(preset("deepseek").unwrap().chat_model, "deepseek-chat");
        assert_eq!(preset("deepseek").unwrap().embed_model, ""); // no embeddings
        assert_eq!(
            preset("qwen").unwrap().base_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        assert_eq!(preset("qwen").unwrap().embed_batch, 10);
        assert!(preset("nope").is_none());
        // Every advertised name resolves.
        assert!(PRESET_NAMES.iter().all(|n| preset(n).is_some()));
    }
}
