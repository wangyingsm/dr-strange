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

/// Whether `name` is one of the presets — the question a **remote** surface
/// must ask before handing a provider name to [`build_provider`].
///
/// [`build_provider`] also accepts a raw base URL, which is right for the
/// operator at their own terminal and wrong for a name that arrived over the
/// wire: an HTTP client that will POST to any URL it is given is a
/// server-side request forger, and it reaches whatever the server can reach.
/// A preset resolves only to the fixed endpoint written in this file, so a
/// request restricted to presets can name a provider without naming a host.
///
/// [`build_provider`]: crate::build_provider
pub fn is_preset(name: &str) -> bool {
    preset(name).is_some()
}

/// A provider the operator configured out of band — in `drsg.toml` or on the
/// command line — that a remote request may name in addition to the presets.
/// `key_env` is the variable the operator bound its key to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfiguredProvider<'a> {
    pub name: &'a str,
    pub key_env: Option<&'a str>,
}

/// Why a provider named over the wire was refused — see [`wire_provider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireProviderError {
    /// Not a preset and not the configured provider: a base URL, or a typo.
    NotAllowed { requested: String },
    /// The request tried to pick the environment variable the key is read
    /// from; that choice belongs to the operator.
    ForeignKeyEnv { provider: String, requested: String },
}

impl std::fmt::Display for WireProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowed { requested } => write!(
                f,
                "provider '{requested}' is not allowed over the wire: name a preset ({}) or \
                 the server's configured provider; a base URL is not accepted",
                PRESET_NAMES.join(", ")
            ),
            Self::ForeignKeyEnv {
                provider,
                requested,
            } => write!(
                f,
                "key_env '{requested}' is not accepted over the wire: provider '{provider}' \
                 reads its key from the variable the server configured for it"
            ),
        }
    }
}

impl std::error::Error for WireProviderError {}

/// The one rule every **remote** surface applies before a request-supplied
/// provider reaches [`build_provider`](crate::build_provider): the name must
/// be a preset or exactly the operator-configured provider (never a base URL
/// — that would make the server POST wherever a client says, a server-side
/// request forgery), and the request may not choose which environment
/// variable the key is read from (a request that could name `AWS_SECRET_KEY`
/// as `key_env` would send that secret as a bearer token to whichever
/// allowed provider it named). `requested_key_env` is therefore accepted only
/// when it repeats the provider's own default, and the returned pair is what
/// the caller hands to `build_provider`: the name and the key variable the
/// operator (or the preset) chose. A `None` request names `openai`.
///
/// Shared by the JSON-RPC methods of dr-strange-web and the MCP tools of
/// dr-strange-mcp so the two surfaces cannot drift apart on this.
pub fn wire_provider<'a>(
    requested: Option<&'a str>,
    requested_key_env: Option<&str>,
    configured: Option<ConfiguredProvider<'a>>,
) -> Result<(&'a str, Option<&'a str>), WireProviderError> {
    let name = requested.unwrap_or("openai");
    let default_key_env = match (preset(name), configured) {
        (Some(p), _) => Some(p.key_env),
        (None, Some(c)) if c.name == name => c.key_env,
        _ => {
            return Err(WireProviderError::NotAllowed {
                requested: name.to_string(),
            });
        }
    };
    if let Some(k) = requested_key_env {
        if Some(k) != default_key_env {
            return Err(WireProviderError::ForeignKeyEnv {
                provider: name.to_string(),
                requested: k.to_string(),
            });
        }
    }
    Ok((name, default_key_env))
}

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

    /// The gate a remote surface puts in front of `build_provider`: a preset
    /// is a name, anything else — above all a URL — is not one.
    #[test]
    fn is_preset_admits_names_and_nothing_url_shaped() {
        assert!(PRESET_NAMES.iter().all(|n| is_preset(n)));
        assert!(!is_preset("http://169.254.169.254/latest/meta-data"));
        assert!(!is_preset("https://api.openai.com/v1"));
        assert!(!is_preset("OpenAI"), "names are exact, as `preset` is");
        assert!(!is_preset(""));
    }

    /// The wire rule: presets and the configured provider pass, a URL does
    /// not, and a request never picks the key's environment variable.
    #[test]
    fn wire_provider_admits_presets_and_the_configured_one_with_their_own_key_env() {
        assert_eq!(
            wire_provider(None, None, None),
            Ok(("openai", Some("OPENAI_API_KEY")))
        );
        assert_eq!(
            wire_provider(Some("deepseek"), None, None),
            Ok(("deepseek", Some("DEEPSEEK_API_KEY")))
        );
        // Repeating the default is harmless.
        assert_eq!(
            wire_provider(Some("deepseek"), Some("DEEPSEEK_API_KEY"), None),
            Ok(("deepseek", Some("DEEPSEEK_API_KEY")))
        );
        let configured = ConfiguredProvider {
            name: "http://embed.internal/v1",
            key_env: Some("EMBED_KEY"),
        };
        assert_eq!(
            wire_provider(Some("http://embed.internal/v1"), None, Some(configured)),
            Ok(("http://embed.internal/v1", Some("EMBED_KEY")))
        );
        assert_eq!(
            wire_provider(
                Some("http://169.254.169.254/latest"),
                None,
                Some(configured)
            ),
            Err(WireProviderError::NotAllowed {
                requested: "http://169.254.169.254/latest".into()
            })
        );
        assert_eq!(
            wire_provider(Some("http://169.254.169.254/latest"), None, None),
            Err(WireProviderError::NotAllowed {
                requested: "http://169.254.169.254/latest".into()
            })
        );
        assert_eq!(
            wire_provider(Some("openai"), Some("AWS_SECRET_ACCESS_KEY"), None),
            Err(WireProviderError::ForeignKeyEnv {
                provider: "openai".into(),
                requested: "AWS_SECRET_ACCESS_KEY".into()
            })
        );
        assert_eq!(
            wire_provider(
                Some(configured.name),
                Some("OPENAI_API_KEY"),
                Some(configured)
            ),
            Err(WireProviderError::ForeignKeyEnv {
                provider: configured.name.into(),
                requested: "OPENAI_API_KEY".into()
            })
        );
    }
}
