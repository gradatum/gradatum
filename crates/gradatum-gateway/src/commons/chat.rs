//! Types Chat Completion compatibles OpenAI API v1 — vendoring inline.
//!
//! Source originale : bibliothèque partagée privée (module openai::chat).
//! Adapté pour gradatum-gateway : annotations utoipa retirées, feature flags supprimés.

use serde::{Deserialize, Serialize};

/// Rôle de l'auteur d'un message dans la conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message dans une conversation multi-tour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// Crée un message système simple.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Crée un message utilisateur simple.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Crée un message assistant texte simple.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }
}

/// Définition d'un outil exposé au LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

impl ToolDefinition {
    /// Helper de construction rapide.
    pub fn function(function: FunctionDefinition) -> Self {
        Self {
            tool_type: "function".to_string(),
            function,
        }
    }
}

/// Définition d'une fonction exposée au LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema décrivant les paramètres.
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Stratégie de sélection d'outil par le modèle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// Chaînes spéciales : "auto", "none", "required".
    Mode(String),
    /// Forcer une fonction spécifique.
    Function {
        #[serde(rename = "type")]
        tool_type: String,
        function: ForcedFunction,
    },
}

impl ToolChoice {
    pub fn auto() -> Self {
        Self::Mode("auto".to_string())
    }

    pub fn none() -> Self {
        Self::Mode("none".to_string())
    }

    pub fn required() -> Self {
        Self::Mode("required".to_string())
    }
}

/// Helper pour `ToolChoice::Function`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForcedFunction {
    pub name: String,
}

/// Tool call émis par l'assistant dans sa réponse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCallResult,
}

/// Contenu d'un appel de fonction dans un `ToolCall`.
///
/// **IMPORTANT** : `arguments` est une **chaîne JSON** (pas un objet JSON), conforme spec OpenAI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCallResult {
    pub name: String,
    pub arguments: String,
}

/// Corps de la requête POST `/v1/chat/completions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Extension llama.cpp — paramètres injectés dans le template Jinja du modèle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
}

impl ChatCompletionRequest {
    /// Ajoute la liste d'outils disponibles et fixe `tool_choice: "auto"` par défaut.
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        if self.tool_choice.is_none() {
            self.tool_choice = Some(ToolChoice::auto());
        }
        self
    }
}

/// Réponse du endpoint `/v1/chat/completions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Une complétion candidate dans la réponse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Statistiques de consommation tokens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// Détails des tokens prompt (cache, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}
