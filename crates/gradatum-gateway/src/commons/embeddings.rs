//! OpenAI API v1 embeddings types — inline vendored.
//!
//! Original source: private shared library (`openai::embeddings` module).
//! Adapted for gradatum-gateway: utoipa annotations removed.

use serde::{Deserialize, Serialize};

/// Input for an embedding request — accepts a single string or a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

impl EmbeddingInput {
    /// Converts the input into a `Vec<String>`, normalizing both `Single` and `Batch`.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            EmbeddingInput::Single(s) => vec![s],
            EmbeddingInput::Batch(v) => v,
        }
    }
}

/// Request body for `POST /v1/embeddings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input: EmbeddingInput,
}

/// Response from the `/v1/embeddings` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

/// An embedding vector for a single input text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub embedding: Vec<f32>,
    pub index: u32,
}

/// Token usage for an embedding request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}
