//! Trait `LlmProvider` — vendoring inline.
//!
//! Source originale : bibliothèque partagée privée (module provider).
//! Adapté pour gradatum-gateway : annotations utoipa retirées, Capabilities intégré inline.

use std::pin::Pin;

use async_trait::async_trait;

use serde::{Deserialize, Serialize};

use crate::commons::chat::{ChatCompletionRequest, ChatCompletionResponse};
use crate::commons::error::{LlmError, LlmResult};
use crate::commons::streaming::ChatCompletionChunk;

/// Flux de chunks SSE pour le streaming.
pub type ChatCompletionStream =
    Pin<Box<dyn futures::Stream<Item = LlmResult<ChatCompletionChunk>> + Send>>;

// ---------------------------------------------------------------------------
// Capabilities (vendorisé inline — évite une dépendance crate supplémentaire)
// ---------------------------------------------------------------------------

/// Support du tool-use par le provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUseSupport {
    None,
    PromptGuided,
    Native,
}

/// Mode thinking/reasoning supporté.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    None,
    Switchable,
    Always,
}

/// Descriptor de capabilities d'un provider / modèle.
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

    /// Capabilities minimales pour un provider texte-only sans streaming.
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

/// Trait d'abstraction d'un provider LLM.
///
/// Chaque impl traduit la `ChatCompletionRequest` (format OpenAI-compat canonique)
/// vers le format natif du provider et retourne une réponse normalisée.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Nom du provider (logging, métriques, messages d'erreur).
    fn name(&self) -> &str;

    /// Capabilities déclarées par ce provider.
    fn capabilities(&self) -> &Capabilities;

    /// Exécute une complétion non-streaming.
    async fn complete(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionResponse>;

    /// Exécute une complétion streaming (chunks SSE).
    ///
    /// Par défaut retourne `ProviderUnavailable`. Les providers qui supportent le streaming
    /// doivent override cette méthode.
    async fn stream(&self, _request: ChatCompletionRequest) -> LlmResult<ChatCompletionStream> {
        Err(LlmError::ProviderUnavailable {
            provider: self.name().to_string(),
            reason: "streaming not supported by this provider".to_string(),
        })
    }

    /// Health check rapide. Par défaut : `Ok(())` sans appel réseau.
    async fn health_check(&self) -> LlmResult<()> {
        Ok(())
    }
}
