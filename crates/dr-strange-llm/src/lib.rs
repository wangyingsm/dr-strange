//! dr-strange-llm — everything that talks to a language or embedding model
//! (arch/07). Sits strictly above the core: `dr-strange-core` never calls a
//! model and never sees an API key, and stays fully usable without this crate.
//!
//! Design rules (arch/07 §2): **proposals, not mutations** — the digest
//! pipeline returns a value the caller inspects and then explicitly writes;
//! **provenance on everything** written (source, model, run id); a minimal
//! provider abstraction ([`Chat`] + [`Embedder`]) with a plain-HTTP
//! OpenAI-compatible implementation and a deterministic mock for tests.
//!
//! Still TODO (arch/07 §1, v1.5): entity-resolution proposals.

mod ask;
mod digest;
mod openai;
mod preset;
mod provider;

pub use ask::{AskOptions, AskResult, ask};
pub use digest::{
    CandidateSource, DigestEdge, DigestNode, DigestOptions, DigestReport, DigestResult,
    ExistingEntity, PlaneCandidates, digest,
};
pub use openai::{OpenAiProvider, build_provider};
pub use preset::{PRESET_NAMES, ProviderPreset, preset};
pub use provider::{Chat, ChatReply, EmbedReply, Embedder, MockProvider, OutputTruncated};
