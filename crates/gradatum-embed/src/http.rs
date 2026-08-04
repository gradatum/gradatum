//! HTTP embedder targeting an OpenAI-compatible backend (`/v1/embeddings`).
//!
//! Compatible with any OpenAI-compatible embedding server
//! (LM Studio, vllm, llama.cpp server, bge-m3, etc.).
//!
//! ## Dimension validation
//!
//! When `dim > 0`, every response is validated: if the returned dimension count
//! differs from `self.dim`, `EmbedError::DimMismatch` is returned.
//! Guards against silent model swaps on the server side.
//!
//! When `dim == 0` (auto-detect), the dimension would be inferred from the first
//! response. Auto-detect is not implemented by design — explicit `dim > 0` is required
//! to detect silent model swaps server-side via `EmbedError::DimMismatch`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::embedder_trait::{EmbedBackend, Embedder};
use crate::error::EmbedError;

// ── OpenAI embeddings response deserialization structs ────────────────────────

/// Single `data[i]` object from a `/v1/embeddings` response.
#[derive(Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    index: usize,
}

/// Full body of a `/v1/embeddings` response.
#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

// ── Request ───────────────────────────────────────────────────────────────────

/// POST request body for `/v1/embeddings`.
#[derive(serde::Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
}

// ── HttpEmbedder ──────────────────────────────────────────────────────────────

/// HTTP embedder targeting an OpenAI-compatible server.
///
/// # URL example
///
/// `http://your-embed-host:8432/v1/embeddings` — any OpenAI-compatible server.
pub struct HttpEmbedder {
    client: reqwest::Client,
    /// Full URL of the `/v1/embeddings` endpoint.
    endpoint: String,
    /// Model name sent in the request body.
    model: String,
    /// reqwest timeout (client is rebuilt when changed via `with_timeout`).
    timeout: Duration,
    /// Embedder identifier (equals the model name).
    embedder_id: String,
    /// Expected dimension count. 0 = unconfigured (auto-detect not implemented by design —
    /// explicit `dim > 0` is required to guard against silent model swaps).
    dim: u16,
}

impl HttpEmbedder {
    /// Creates an `HttpEmbedder` with a default timeout of 5 seconds.
    ///
    /// # Parameters
    ///
    /// - `endpoint`: full URL (e.g. `"http://your-embed-host:8432/v1/embeddings"`)
    /// - `model`: model name (e.g. `"bge-m3"`)
    /// - `dim`: expected dimensions; use `0` to disable validation.
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>, dim: u16) -> Self {
        let endpoint = endpoint.into();
        let model = model.into();
        let embedder_id = model.clone();
        let timeout = Duration::from_secs(5);
        Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                // SAFETY: the default reqwest configuration cannot fail.
                .expect("reqwest client build with default timeout"),
            endpoint,
            model,
            timeout,
            embedder_id,
            dim,
        }
    }

    /// Overrides the timeout (rebuilds the internal reqwest client).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            // SAFETY: same as `new` — trivial configuration.
            .expect("reqwest client build with custom timeout");
        self
    }

    /// Sends the POST request and deserializes the response.
    ///
    /// Re-sorts embeddings by `index` to preserve input order, as some servers
    /// do not guarantee the order of `data[]`.
    async fn call_endpoint(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let body = EmbedRequest {
            model: &self.model,
            input: texts.to_vec(),
        };

        let resp = self.client.post(&self.endpoint).json(&body).send().await?;

        let status = resp.status();
        if !status.is_success() {
            // Consume the body to produce a more readable error message.
            let body_text = resp.text().await.unwrap_or_default();
            return Err(EmbedError::InvalidResponse(format!(
                "HTTP {status}: {body_text}"
            )));
        }

        let parsed: EmbeddingsResponse = resp
            .json()
            .await
            .map_err(|e| EmbedError::InvalidResponse(format!("JSON deserialization: {e}")))?;

        if parsed.data.is_empty() {
            return Err(EmbedError::InvalidResponse(
                "empty data[] in response".into(),
            ));
        }

        // Sort by index to guarantee input order even when the server reorders items.
        let mut items = parsed.data;
        items.sort_by_key(|item| item.index);

        // Count validation: the server must return exactly one vector per input text.
        // Checked before index and dimension validation to surface the most informative error first.
        if items.len() != texts.len() {
            return Err(EmbedError::CountMismatch {
                sent: texts.len(),
                received: items.len(),
            });
        }

        // Index permutation validation: after sorting, each item.index must equal its position.
        // A duplicate or out-of-bounds index (e.g. [0, 0, 2] instead of [0, 1, 2]) would cause
        // silent vector-to-text misalignment — silently returning the wrong embedding per text.
        // `CountMismatch` above already guarantees `items.len() == texts.len()`, so the valid
        // permutation is exactly `0..items.len()`.
        for (position, item) in items.iter().enumerate() {
            if item.index != position {
                return Err(EmbedError::IndexMismatch {
                    position,
                    expected: position,
                    got: item.index,
                });
            }
        }

        // Dimension validation when configured.
        if self.dim > 0 {
            for item in &items {
                let got = item.embedding.len() as u16;
                if got != self.dim {
                    return Err(EmbedError::DimMismatch {
                        expected: self.dim,
                        got,
                    });
                }
            }
        }

        Ok(items.into_iter().map(|item| item.embedding).collect())
    }
}

#[async_trait]
impl Embedder for HttpEmbedder {
    fn embedder_id(&self) -> &str {
        &self.embedder_id
    }

    fn dim(&self) -> u16 {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text]).await?;
        // SAFETY: embed_batch returns exactly 1 vector for 1 text when the response is valid.
        Ok(out
            .pop()
            .expect("embed_batch returned exactly 1 vector for 1 text"))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.call_endpoint(texts).await
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Http
    }
}
