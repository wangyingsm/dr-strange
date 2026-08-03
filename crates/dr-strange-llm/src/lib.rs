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
mod identity;
mod openai;
mod preset;
mod provider;
mod reconcile;
mod refine;

pub use ask::{AskOptions, AskResult, ask};
pub use digest::{
    CandidateSource, DigestEdge, DigestMode, DigestNode, DigestOptions, DigestReport, DigestResult,
    ExistingEntity, PlaneCandidates, SOURCE_MARKER, digest,
};
pub use identity::IdentityReport;
pub use openai::{OpenAiProvider, build_provider};
pub use preset::{PRESET_NAMES, ProviderPreset, preset};
pub use provider::{Chat, ChatReply, EmbedReply, Embedder, MockProvider, OutputTruncated};
pub use reconcile::ReconcileReport;
pub use refine::RefineReport;
