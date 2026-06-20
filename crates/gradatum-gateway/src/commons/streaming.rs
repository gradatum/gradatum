//! SSE streaming types for `/v1/chat/completions` — inline vendored.
//!
//! Original source: private shared library (`openai::streaming` module).
//! Adapted for gradatum-gateway: utoipa annotations removed.

use serde::{Deserialize, Serialize};

/// An SSE chunk received in response to a streaming `/v1/chat/completions` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

/// A completion delta inside an SSE chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Incremental content of a chunk.
///
/// The first chunk carries only `role`; subsequent chunks carry `content`
/// or `tool_calls`. The final chunk (non-null `finish_reason`) may have empty `content`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

/// Fragment of a tool call in a streaming chunk.
///
/// Progressive format: only `index` is always present. The first delta contains
/// `id`, `type`, and `function.name`; subsequent deltas carry only `function.arguments`.
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

/// Function fragment inside a tool call delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkToolCallFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}
