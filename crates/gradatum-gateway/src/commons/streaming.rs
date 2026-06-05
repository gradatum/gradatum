//! Types streaming SSE pour `/v1/chat/completions` — vendoring inline.
//!
//! Source originale : bibliothèque partagée privée (module openai::streaming).
//! Adapté pour gradatum-gateway : annotations utoipa retirées.

use serde::{Deserialize, Serialize};

/// Un chunk SSE reçu en réponse d'un appel streaming à `/v1/chat/completions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

/// Un delta de complétion dans un chunk SSE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Contenu incrémental d'un chunk.
///
/// Le premier chunk porte uniquement `role` ; les suivants portent `content`
/// ou `tool_calls`. Le chunk final (`finish_reason` non-nul) peut avoir `content` vide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

/// Fragment d'un tool call dans un chunk streaming.
///
/// Format progressif : seul `index` est toujours présent. Le premier delta contient
/// `id`, `type`, `function.name` ; les suivants ne contiennent que `function.arguments`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkToolCall {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub tool_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<ChunkToolCallFunction>,
}

/// Fragment de fonction dans un tool call delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkToolCallFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}
