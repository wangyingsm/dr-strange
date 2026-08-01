//! Provider abstraction (arch/07 §2): the two seams everything above the core
//! talks to a model through. Deliberately minimal — one chat completion, one
//! embedding call — with plain HTTP implementations elsewhere and a
//! deterministic [`MockProvider`] for offline tests.

use std::sync::Mutex;

use anyhow::Result;

/// A single chat completion (system + user turn) with token accounting.
pub trait Chat {
    fn complete(&self, system: &str, user: &str) -> Result<ChatReply>;
}

pub struct ChatReply {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The provider's reply was cut off at the model's output-token limit. A dense
/// extraction chunk can trigger this; [`crate::digest`] recovers by splitting
/// the chunk and retrying, so it is a typed, recoverable error a caller can
/// match on rather than a plain message.
#[derive(Debug)]
pub struct OutputTruncated {
    pub limit: u32,
}

impl std::fmt::Display for OutputTruncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the model's reply hit the {}-token output limit and was cut off",
            self.limit
        )
    }
}

impl std::error::Error for OutputTruncated {}

/// Batch text → vectors, with token accounting.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<EmbedReply>;
}

pub struct EmbedReply {
    pub vectors: Vec<Vec<f32>>,
    pub tokens: u64,
}

/// A deterministic, offline provider for tests and dry demos. Chat returns
/// canned replies in order (cycling); embeddings are a stable hash of the text
/// so the same text always maps to the same vector.
pub struct MockProvider {
    replies: Vec<String>,
    next: Mutex<usize>,
    dim: usize,
}

impl MockProvider {
    /// `replies` are handed out one per `complete` call (round-robin);
    /// embeddings have dimension `dim`.
    pub fn new(replies: Vec<String>, dim: usize) -> Self {
        Self {
            replies,
            next: Mutex::new(0),
            dim,
        }
    }
}

impl Chat for MockProvider {
    fn complete(&self, _system: &str, user: &str) -> Result<ChatReply> {
        let text = if self.replies.is_empty() {
            "{\"entities\":[],\"relations\":[]}".to_string()
        } else {
            let mut n = self.next.lock().unwrap();
            let r = self.replies[*n % self.replies.len()].clone();
            *n += 1;
            r
        };
        Ok(ChatReply {
            input_tokens: user.len() as u64 / 4,
            output_tokens: text.len() as u64 / 4,
            text,
        })
    }
}

impl Embedder for MockProvider {
    fn embed(&self, texts: &[String]) -> Result<EmbedReply> {
        let vectors = texts.iter().map(|t| mock_vector(t, self.dim)).collect();
        Ok(EmbedReply {
            vectors,
            tokens: texts.iter().map(|t| t.len() as u64 / 4).sum(),
        })
    }
}

/// A deterministic unit vector seeded by the text (SplitMix64 over a simple
/// hash) — stable across runs so tests can assert on it.
fn mock_vector(text: &str, dim: usize) -> Vec<f32> {
    let mut seed = 0xcbf2_9ce4_8422_2325u64; // FNV offset basis
    for b in text.bytes() {
        seed = (seed ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    let mut v: Vec<f32> = (0..dim)
        .map(|_| {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z ^= z >> 27;
            (z as f64 / u64::MAX as f64) as f32 * 2.0 - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    for x in &mut v {
        *x /= norm;
    }
    v
}
