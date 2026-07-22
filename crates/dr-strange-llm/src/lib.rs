//! dr-strange-llm — everything that talks to a language or embedding model
//! (arch/07). Sits strictly above the core: dr-strange-core never calls a
//! model and never sees an API key.
//!
//! Design rules: proposals, not mutations; provenance on everything written.
//!
//! TODO(M3): `Embedder` trait + HTTP providers, embedding cache.
//! TODO(deferred design session): digest pipeline (documents → graph).
//! TODO(v1.5): entity resolution proposals; NL → plan translation.
