//! Types Embeddings compatibles OpenAI API v1 — vendoring inline.
//!
//! Source originale : bibliothèque partagée privée (module openai::embeddings).
//! Adapté pour gradatum-gateway : annotations utoipa retirées.

use serde::{Deserialize, Serialize};

/// Input d'une requête d'embedding — accepte une chaîne seule ou un batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

impl EmbeddingInput {
    /// Convertit l'input en vecteur de chaînes — normalise Single et Batch.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            EmbeddingInput::Single(s) => vec![s],
            EmbeddingInput::Batch(v) => v,
        }
    }
}

/// Corps de la requête POST `/v1/embeddings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input: EmbeddingInput,
}

/// Réponse du endpoint `/v1/embeddings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

/// Un vecteur d'embedding pour un texte donné.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub embedding: Vec<f32>,
    pub index: u32,
}

/// Consommation tokens pour une requête d'embedding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}
