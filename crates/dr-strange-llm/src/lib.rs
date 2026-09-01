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
pub mod document;
mod identity;
mod openai;
pub mod preprocess;
mod preset;
mod provider;
mod reconcile;
mod refine;
mod vectorize;

pub use ask::{AskOptions, AskResult, ask};
pub use digest::{
    ApplyStats, CandidateSource, DigestEdge, DigestMode, DigestNode, DigestOptions, DigestReport,
    DigestResult, ExistingEntity, PlaneCandidates, SOURCE_MARKER, digest, embeddable_text,
    entity_text, fact_text,
};
pub use document::to_markdown;
pub use identity::IdentityReport;
pub use openai::{OpenAiProvider, build_provider};
/// The official plugin catalog — data fetched from the extensions repository,
/// not a constant of this binary.
#[cfg(feature = "plugins")]
pub use preprocess::{
    CATALOG_DOWNLOAD_CAP, CATALOG_URL, CONTRACT_VERSION, Catalog, CatalogSource, Compat, Fetched,
    HOST_VERSION, OfficialPlugin, Pick, cached_catalog, load_catalog, load_catalog_within,
    refresh_cache,
};
pub use preprocess::{
    CommitDelta, FactsAndPlane, GitDir, Host, IgnorePolicy, LocalFiles, PluginConfig, Plugins,
    Preprocessed, Preprocessor, SyncStats, fold, git_dir, resync, route_document, route_paths,
    route_repository, route_tree, stamp_run, sync_paths,
};
#[cfg(feature = "plugins")]
pub use preprocess::{InstalledPlugin, Limits, PluginStore, WasmPlugin};
/// Reading a repository's history beside its code — see [`preprocess::repo`].
pub use preprocess::{PLANE_SUFFIX as GIT_PLANE_SUFFIX, REPO_PLUGIN, plane_name as git_plane_name};
pub use preprocess::{WriteStats as GitWriteStats, write_history};
pub use preset::{PRESET_NAMES, ProviderPreset, preset};
pub use provider::{Chat, ChatReply, EmbedReply, Embedder, MockProvider, OutputTruncated};
pub use reconcile::ReconcileReport;
pub use refine::RefineReport;
pub use vectorize::{VectorizeStats, semantic_search, vectorize_plane};
