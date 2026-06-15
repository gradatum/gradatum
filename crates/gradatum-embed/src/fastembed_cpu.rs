//! Local embedder via fastembed (ONNX CPU).
//!
//! **Requires the `fastembed-cpu` feature** — disabled by default.
//! See the crate's `Cargo.toml` for activation instructions.
//!
//! Uses the `bge-small-en-v1.5` model (384 dimensions, English only).
//! Weights (~150 MB) are downloaded to `~/.cache/fastembed/` on first use.
//!
//! ## Poisoned `Mutex` behaviour
//!
//! `TextEmbedding::embed` takes `&self` in fastembed 4.6.0, so the `Mutex` is
//! only needed to satisfy `Send`. A poisoned `Mutex` is unrecoverable:
//! the process must be restarted. The `.expect()` on the lock documents
//! this intentional design choice.

use std::sync::Mutex;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

/// CPU-only embedder backed by fastembed ONNX.
///
/// fastembed's `TextEmbedding` is not `Sync` (ONNX inference uses non-thread-safe
/// internal buffers). Wrapping it in a `Mutex` makes the struct `Send + Sync`
/// and usable in multi-threaded contexts.
///
/// Inference is synchronous (blocking). For Axum handlers, use
/// `tokio::task::spawn_blocking`.
pub struct FastEmbedCpu {
    /// ONNX model behind a lock for `Send + Sync`.
    inner: Mutex<TextEmbedding>,
    /// Model identifier returned by `embedder_id()`.
    embedder_id: String,
    /// Number of model dimensions.
    dim: u16,
}

impl FastEmbedCpu {
    /// Builds a `FastEmbedCpu` using the default `bge-small-en-v1.5` model (384d, English only).
    ///
    /// Downloads weights (~150 MB) to `~/.cache/fastembed/` if not already present.
    ///
    /// # Errors
    ///
    /// Returns `EmbedError::Init` if the download or ONNX initialization fails.
    pub fn try_default() -> Result<Self, EmbedError> {
        let inner = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false),
        )
        .map_err(|e| EmbedError::Init(format!("fastembed init: {e}")))?;

        Ok(Self {
            inner: Mutex::new(inner),
            embedder_id: "bge-small-en-v1.5".into(),
            dim: 384,
        })
    }
}

#[async_trait]
impl Embedder for FastEmbedCpu {
    fn embedder_id(&self) -> &str {
        &self.embedder_id
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    /// Computes the embedding for a single text by delegating to `embed_batch`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text]).await?;
        // SAFETY: embed_batch guarantees 1 vector when the input contains 1 text.
        Ok(out
            .pop()
            .expect("embed_batch a retourné exactement 1 vecteur pour 1 texte"))
    }

    /// Computes embeddings for a batch of texts (synchronous, blocking inside tokio).
    ///
    /// The `Mutex` is held for the duration of inference and released immediately after.
    /// A poisoned `Mutex` (panic in another thread during inference) is unrecoverable
    /// — the process must be restarted.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let texts_owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();

        // MUTEX POISON: intentional `.expect()` — see module doc.
        let guard = self
            .inner
            .lock()
            .expect("Mutex FastEmbedCpu non-poisonné : une panique dans un autre thread a corrompu l'état du modèle ONNX, redémarrer le process");

        guard
            .embed(texts_owned, None)
            .map_err(|e| EmbedError::Embed(format!("fastembed: {e}")))
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::FastembedCpu
    }
}
