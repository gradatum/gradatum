//! `LlmProvider` trait — inline vendored.
//!
//! Original source: private shared library (`provider` module).
//! Adapted for gradatum-gateway: utoipa annotations removed, `Capabilities` inlined.

use std::pin::Pin;

use async_trait::async_trait;

use serde::{Deserialize, Serialize};

use crate::commons::chat::{ChatCompletionRequest, ChatCompletionResponse};
use crate::commons::error::{LlmError, LlmResult};
use crate::commons::streaming::ChatCompletionChunk;

/// SSE chunk stream for streaming completions.
pub type ChatCompletionStream =
    Pin<Box<dyn futures::Stream<Item = LlmResult<ChatCompletionChunk>> + Send>>;

// ---------------------------------------------------------------------------
// Capabilities (inline vendored — avoids an extra crate dependency)
// ---------------------------------------------------------------------------

/// Tool-use support level for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUseSupport {
    None,
    PromptGuided,
    Native,
}

/// Thinking/reasoning mode supported by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    None,
    Switchable,
    Always,
}

/// Capability descriptor for a provider or model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tool_use: ToolUseSupport,
    pub streaming: bool,
    pub vision: bool,
    pub thinking: ThinkingMode,
    pub context_max: u32,
    pub structured_output: bool,
    pub prompt_caching: bool,
    pub reasoning_levels: Option<Vec<String>>,
}

impl Capabilities {
    pub fn supports_tool_use(&self) -> bool {
        !matches!(self.tool_use, ToolUseSupport::None)
    }

    pub fn supports_native_tool_use(&self) -> bool {
        matches!(self.tool_use, ToolUseSupport::Native)
    }

    /// Minimal capabilities for a text-only provider without streaming.
    pub fn minimal_text_only(context_max: u32) -> Self {
        Self {
            tool_use: ToolUseSupport::None,
            streaming: false,
            vision: false,
            thinking: ThinkingMode::None,
            context_max,
            structured_output: false,
            prompt_caching: false,
            reasoning_levels: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Trait LlmProvider
// ---------------------------------------------------------------------------

/// Abstraction trait for an LLM provider.
///
/// Each implementation translates a `ChatCompletionRequest` (canonical OpenAI-compat format)
/// into the provider's native format and returns a normalized response.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider name (used in logs, metrics, and error messages).
    fn name(&self) -> &str;

    /// Capabilities declared by this provider.
    fn capabilities(&self) -> &Capabilities;

    /// Executes a non-streaming completion.
    async fn complete(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionResponse>;

    /// Executes a streaming completion (SSE chunks).
    ///
    /// Returns `ProviderUnavailable` by default. Providers that support streaming
    /// must override this method.
    async fn stream(&self, _request: ChatCompletionRequest) -> LlmResult<ChatCompletionStream> {
        Err(LlmError::ProviderUnavailable {
            provider: self.name().to_string(),
            reason: "streaming not supported by this provider".to_string(),
        })
    }

    /// Quick health check. Default: returns `Ok(())` without a network call.
    async fn health_check(&self) -> LlmResult<()> {
        Ok(())
    }
}
