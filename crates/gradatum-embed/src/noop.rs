//! `Noop` embedder — returns zero vectors.
//!
//! Used in tests and when embedding is disabled.

use async_trait::async_trait;

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

/// Embedder that returns `vec![0.0; dim]` for every text.
///
/// Performs no computation — useful for unit tests that do not require real
/// embeddings, or to disable embedding without changing the calling code's
/// signature.
pub struct Noop {
    /// Number of dimensions in the returned vectors.
    pub dim: u16,
}

impl Noop {
    /// Creates a `Noop` that returns zero vectors of `dim` dimensions.
    pub fn new(dim: u16) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl Embedder for Noop {
    fn embedder_id(&self) -> &str {
        "noop"
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0; self.dim as usize])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.0; self.dim as usize]).collect())
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_embed_returns_zero_vector() {
        let e = Noop::new(384);
        let v = e.embed("hello").await.unwrap();
        assert_eq!(v.len(), 384);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[tokio::test]
    async fn noop_embed_batch_preserves_order() {
        let e = Noop::new(4);
        let texts = vec!["a", "b", "c"];
        let result = e.embed_batch(&texts).await.unwrap();
        assert_eq!(result.len(), 3);
        for row in &result {
            assert_eq!(row.len(), 4);
        }
    }

    #[test]
    fn noop_metadata() {
        let e = Noop::new(768);
        assert_eq!(e.embedder_id(), "noop");
        assert_eq!(e.dim(), 768);
        assert_eq!(e.backend_kind(), EmbedBackend::Noop);
    }
}
