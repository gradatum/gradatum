//! Error types for the gradatum-embed crate.

use gradatum_core::error::GradatumError;
use thiserror::Error;

/// Converts `EmbedError` into `GradatumError::Inference`.
///
/// The orphan rule allows this `impl` on the `gradatum-embed` side because
/// `gradatum-embed` depends on `gradatum-core`.
///
/// Enables `?` propagation in handlers returning `GradatumError`
/// and provides a single conversion point for `EmbedError -> GradatumError`.
impl From<EmbedError> for GradatumError {
    fn from(err: EmbedError) -> Self {
        // Preserves the rich `Display` of `EmbedError` (variant prefix + message).
        GradatumError::Inference(err.to_string())
    }
}

/// Errors that can occur during embedder initialization or use.
#[derive(Debug, Error)]
pub enum EmbedError {
    /// Embedder initialization error (model load, ONNX init, …).
    #[error("init: {0}")]
    Init(String),

    /// Embedding computation error (inference failure, batch failure, …).
    #[error("embed: {0}")]
    Embed(String),

    /// HTTP error when calling a remote backend.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Unexpected or malformed HTTP response (invalid JSON, missing fields, …).
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Dimension count returned by the backend does not match the expected value.
    /// Guards against silent model swaps on the server side.
    #[error("dim mismatch: expected {expected}, got {got}")]
    DimMismatch {
        /// Expected dimensions (configured by the caller).
        expected: u16,
        /// Dimensions received in the response.
        got: u16,
    },

    /// Vector count in the response does not match the number of input texts.
    ///
    /// A conforming OpenAI-compatible backend must return exactly one embedding
    /// per input text. A mismatch indicates a server defect or a protocol violation
    /// and must not be silently ignored.
    #[error("count mismatch: sent {sent} texts, received {received} embeddings")]
    CountMismatch {
        /// Number of texts sent in the request.
        sent: usize,
        /// Number of embedding vectors received in the response.
        received: usize,
    },

    /// Index field in a response item is not a valid contiguous permutation of `0..n`.
    ///
    /// After sorting by `item.index`, each index must equal its position (0-based).
    /// A duplicate or out-of-bounds index causes a silent vector-to-text misalignment
    /// that is worse than an outright count error — the caller would consume the wrong
    /// embedding for a given text without any indication.
    ///
    /// # Protocol context
    ///
    /// OpenAI-compatible backends guarantee distinct `index` values in `[0, n)`.
    /// This error fires when that invariant is violated post-sort.
    #[error(
        "index mismatch at position {position}: expected index {expected}, got {got} \
         (duplicate or out-of-bounds index in embedding response)"
    )]
    IndexMismatch {
        /// Zero-based position in the sorted array.
        position: usize,
        /// Expected index value (equals `position` for a valid permutation).
        expected: usize,
        /// Actual index value found in the response item.
        got: usize,
    },
}
