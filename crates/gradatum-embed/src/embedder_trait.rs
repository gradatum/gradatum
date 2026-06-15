//! `Embedder` trait and associated public types.

use async_trait::async_trait;

use crate::error::EmbedError;

/// Underlying backend type — used for monitoring and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbedBackend {
    /// Local inference via fastembed (ONNX CPU).
    FastembedCpu,
    /// HTTP call to an OpenAI-compatible backend.
    Http,
    /// Noop — returns zero vectors (tests / disabled embedding).
    Noop,
}

/// Main trait for any text embedder.
///
/// Implementations must be `Send + Sync` to be used in Axum handlers or
/// multi-threaded tokio workers.
///
/// # Dimension contract
///
/// `dim()` must remain stable for the entire lifetime of the instance.
/// Two embedders with different dimensions cannot be used against the same
/// SQLite index without a migration.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embedding model identifier (e.g. `"bge-small-en-v1.5"`, `"bge-m3"`).
    fn embedder_id(&self) -> &str;

    /// Number of dimensions in the produced vectors.
    fn dim(&self) -> u16;

    /// Computes the embedding for a single text.
    ///
    /// Delegates to `embed_batch` with a batch of size 1 by default.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// Computes embeddings for a batch of texts.
    ///
    /// The order of the returned vectors matches the order of the input texts.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Underlying backend type (for monitoring / logging / routing).
    fn backend_kind(&self) -> EmbedBackend;
}
