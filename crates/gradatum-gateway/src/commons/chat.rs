//! OpenAI API v1 chat completion types — inline vendored.
//!
//! Original source: private shared library (`openai::chat` module).
//! Adapted for gradatum-gateway: utoipa annotations removed, feature flags stripped.
//!
//! # Multimodal support
//!
//! `Message.content` accepts two forms conforming to the OpenAI spec:
//! - `MessageContent::Text(String)` — plain text (legacy form, backward-compatible)
//! - `MessageContent::Parts(Vec<ContentPart>)` — array of parts (text + images)
//!
//! When serialized, `Text` produces a plain JSON `String` identical to the old
//! representation — text-only backends remain transparent.
//!
//! **Security**: the `url` value of an `ImageUrl` must NEVER be logged
//! (base64 ~1 MiB/image). Log only the count or types of parts.

use serde::{Deserialize, Serialize};

/// Role of a message author in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a message — plain text or an array of multimodal parts.
///
/// Two JSON forms accepted (OpenAI spec):
/// - `"text"` — deserialized as `Text(String)`
/// - `[{"type":"text","text":"..."}, {"type":"image_url","image_url":{"url":"..."}}]` — deserialized as `Parts`
///
/// `#[serde(untagged)]` enables transparent deserialization of both forms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text — serialized as a JSON string (backward-compatible with text backends).
    Text(String),
    /// Array of multimodal parts (text + images).
    Parts(Vec<ContentPart>),
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl MessageContent {
    /// Returns `true` if at least one `ImageUrl` part is present.
    ///
    /// Used by the vision gate in the chat handler.
    #[must_use]
    pub fn has_image(&self) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Parts(parts) => parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
        }
    }

    /// Returns the raw text (sum of text parts, or the string for `Text`).
    ///
    /// Used for token counting and text-only extractions.
    #[must_use]
    pub fn text_content(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Parts(parts) => parts
                .iter()
                .filter_map(|p| {
                    if let ContentPart::Text { text } = p {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Counts the number of `ImageUrl` parts in this content.
    ///
    /// Used by the token counter for image cost estimation.
    #[must_use]
    pub fn image_count(&self) -> usize {
        match self {
            Self::Text(_) => 0,
            Self::Parts(parts) => parts
                .iter()
                .filter(|p| matches!(p, ContentPart::ImageUrl { .. }))
                .count(),
        }
    }
}

/// Part of a multimodal message.
///
/// `#[serde(tag = "type", rename_all = "snake_case")]`: deserializes by the `"type"` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Text part.
    Text { text: String },
    /// Image part — base64 data URL or HTTP URL.
    ///
    /// **Security**: NEVER log `image_url.url` (base64 ~1 MiB/image).
    /// Log only the type or count of parts.
    ImageUrl { image_url: ImageUrlDetail },
}

/// Image URL detail in a multimodal message.
///
/// **Security**: `url` may contain a base64-encoded image (~1 MiB) — never include in logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageUrlDetail {
    pub url: String,
}

/// Message in a multi-turn conversation.
///
/// `content` accepts both OpenAI forms: `String` (plain text) or
/// `[{"type":"text"|"image_url",...}]` (multimodal).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    /// Creates a simple plain-text system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Creates a simple plain-text user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Creates a simple plain-text assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// Returns `true` if this message contains at least one image part.
    #[must_use]
    pub fn has_image(&self) -> bool {
        self.content.has_image()
    }
}

/// Tool definition exposed to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

impl ToolDefinition {
    /// Convenience constructor.
    pub fn function(function: FunctionDefinition) -> Self {
        Self {
            tool_type: "function".to_string(),
            function,
        }
    }
}

/// Function definition exposed to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing the parameters.
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Tool selection strategy for the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// Special mode strings: `"auto"`, `"none"`, `"required"`.
    Mode(String),
    /// Forces a specific function.
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

/// Helper struct for `ToolChoice::Function`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForcedFunction {
    pub name: String,
}

/// Tool call emitted by the assistant in its response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCallResult,
}

/// Function call payload inside a `ToolCall`.
///
/// **Important**: `arguments` is a **JSON string** (not a JSON object), conforming to the OpenAI spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCallResult {
    pub name: String,
    pub arguments: String,
}

/// Request body for `POST /v1/chat/completions`.
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
    /// Top-k sampling — keep only the `k` most probable tokens (llama.cpp / vLLM).
    ///
    /// Omitted from the forwarded body when `None` (the backend applies its own
    /// launch default, e.g. `--top-k`), so this is additive and non-breaking.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Min-p sampling — drop tokens below `min_p × p(top token)` (llama.cpp / vLLM).
    ///
    /// Omitted from the forwarded body when `None` (backend launch default applies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f32>,
    /// Presence penalty — discourages repeating tokens already present.
    ///
    /// Omitted from the forwarded body when `None` (backend launch default applies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// llama.cpp extension — parameters injected into the model's Jinja template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
}

impl ChatCompletionRequest {
    /// Attaches an available tools list and sets `tool_choice: "auto"` as the default.
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        if self.tool_choice.is_none() {
            self.tool_choice = Some(ToolChoice::auto());
        }
        self
    }
}

/// Response from the `/v1/chat/completions` endpoint.
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

/// A candidate completion in the response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Token usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// Prompt token details (cache hits, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}
