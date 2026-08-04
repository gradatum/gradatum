//! DTOs for the Anthropic Messages API — inbound request/response types.
//!
//! These types cover the `POST /v1/messages` API surface (Anthropic).
//!
//! # Supported features
//! - Plain text requests and responses
//! - Full tool use (tools[], tool_choice, tool_use blocks, tool_result, image)
//! - Anthropic SSE streaming
//! - Anthropic error envelope, configurable model mapping
//!
//! Reference: <https://docs.anthropic.com/en/api/messages>

use serde::{Deserialize, Serialize};

// ── Requête entrant ────────────────────────────────────────────────────────────

/// Inbound `POST /v1/messages` request in Anthropic Messages API format.
///
/// Unknown JSON fields are ignored (serde default behaviour), so a client using a
/// newer Anthropic field is never rejected with a deserialization error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesRequest {
    /// Model identifier — resolved to an internal alias by the gateway.
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<AnthropicMessage>,
    /// Optional system prompt (plain text or content blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    /// Maximum number of tokens to generate (required by the Anthropic API).
    pub max_tokens: u32,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Cumulative probability cutoff (nucleus sampling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Additional stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// `true` = Anthropic SSE streaming mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Tool definitions exposed to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Tool selection strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// Extended-thinking configuration — accepted for wire compatibility, never read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
    /// Anthropic beta feature opt-ins — accepted for wire compatibility, never read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub betas: Option<Vec<String>>,
    /// Arbitrary caller metadata — accepted for wire compatibility, never read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Definition of a tool exposed to the model.
///
/// Corresponds to `tools[i]` in the Anthropic request. Mapped to an OpenAI
/// `ToolDefinition` during translation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    /// Tool name — unique identifier within the list.
    pub name: String,
    /// Tool description (optional, but recommended).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema of the tool parameters.
    ///
    /// Anthropic names this field `input_schema`; it is mapped to `parameters`
    /// in the OpenAI `FunctionDefinition`.
    pub input_schema: serde_json::Value,
}

/// Tool selection strategy.
///
/// Anthropic wire format:
/// - `{"type": "auto"}` — the model decides
/// - `{"type": "any"}` — the model must call at least one tool
/// - `{"type": "tool", "name": "X"}` — force the named tool
/// - `{"type": "none"}` — no tool at all
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model freely decides whether to use a tool.
    Auto,
    /// The model must call at least one tool (OpenAI equivalent: `"required"`).
    Any,
    /// Force a call to the named tool.
    Tool {
        /// Name of the tool to force.
        name: String,
    },
    /// Disable tools (the model must not call any).
    None,
}

/// A message in an Anthropic conversation.
///
/// `role` is `"user"` or `"assistant"`. `content` accepts either a bare string
/// or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicMessage {
    /// Author role: `"user"` or `"assistant"`.
    pub role: String,
    /// Message content — plain text or a list of blocks.
    pub content: AnthropicContent,
}

/// Content of an Anthropic message — plain text or an array of blocks.
///
/// `#[serde(untagged)]` deserializes both shapes transparently:
/// - `"text"` → `Text(String)`
/// - `[{...}]` → `Blocks(Vec<ContentBlock>)`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicContent {
    /// Plain text (Anthropic short form).
    Text(String),
    /// Array of content blocks (extended form).
    Blocks(Vec<ContentBlock>),
}

impl AnthropicContent {
    /// Returns the concatenation of every text block.
    ///
    /// For a plain `Text` value, returns the string as-is. Non-text blocks
    /// (tool use, tool result, image, thinking) contribute nothing.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// Content block in an Anthropic message.
///
/// Supported variants:
/// - `Text`
/// - `ToolUse`, `ToolResult`, `Image`
/// - `Thinking` (extended thinking — silently ignored)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Text block.
    Text {
        /// Textual content of the block.
        text: String,
    },
    /// Tool-call block generated by the assistant.
    ///
    /// Mapped to `tool_calls[i]` in the OpenAI `Message` with role `assistant`.
    ToolUse {
        /// Unique identifier of this tool call within the current turn.
        id: String,
        /// Name of the called tool.
        name: String,
        /// Arguments passed to the tool (JSON object).
        input: serde_json::Value,
    },
    /// Tool result provided by the user.
    ///
    /// Mapped to an OpenAI `Message` with role `tool` and a `tool_call_id`.
    ToolResult {
        /// Identifier of the tool call this result answers.
        tool_use_id: String,
        /// Result content — plain text or an array of blocks.
        ///
        /// Anthropic accepts either a `String` or a `Vec<ContentBlock>` of text
        /// blocks. Stored as `serde_json::Value` to absorb both shapes; the
        /// translation layer extracts the text.
        content: serde_json::Value,
        /// Whether the tool execution failed (used for error tool results).
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Image block (base64 or URL).
    ///
    /// Mapped to an OpenAI `ContentPart::ImageUrl` with a data-URI `data:<media_type>;base64,<data>`.
    Image {
        /// Image source.
        source: ImageSource,
    },
    /// Extended-thinking block — parsed, then dropped by the translation layer.
    Thinking {
        /// Thinking content (ignored).
        thinking: String,
    },
    /// Block of an unrecognized type — silently dropped during translation.
    ///
    /// Absorbs future Anthropic API variants (for example `"document"` or
    /// `"redacted_thinking"`) so that an unknown block type never turns the
    /// whole request into a 400 deserialization error.
    ///
    /// Note: `#[serde(other)]` on a unit variant works with internally tagged
    /// enums — the fields of the unknown object are dropped.
    #[serde(other)]
    Unknown,
}

/// Source of an image carried by a [`ContentBlock::Image`] block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    /// Source type: `"base64"` or `"url"`.
    #[serde(rename = "type")]
    pub source_type: String,
    /// MIME type of the image (for example `"image/jpeg"`, `"image/png"`).
    ///
    /// Present when `type = "base64"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Base64-encoded image data (when `type = "base64"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Image URL (when `type = "url"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ── DTO count_tokens ──────────────────────────────────────────────────────────

/// Inbound `POST /v1/messages/count_tokens` request in Anthropic Messages API format.
///
/// Structurally identical to [`MessagesRequest`] except that `max_tokens` is **absent**:
/// the Anthropic `count_tokens` API does not require it, unlike `/v1/messages`.
///
/// Keeping a dedicated DTO avoids making `max_tokens` optional on [`MessagesRequest`],
/// which must stay strict for the main route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountTokensRequest {
    /// Model identifier — not used for counting (no provider dispatch happens).
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<AnthropicMessage>,
    /// Optional system prompt (plain text or content blocks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,
    /// Tool definitions exposed to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Extended-thinking configuration — accepted for wire compatibility, never read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,
}

/// Content of a system prompt — plain text or blocks.
///
/// Same shape as [`AnthropicContent`], kept as a distinct type because a system
/// prompt only carries `Text` blocks in practice.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SystemContent {
    /// Plain text.
    Text(String),
    /// Array of blocks (normally `Text` only for a system prompt).
    Blocks(Vec<ContentBlock>),
}

impl SystemContent {
    /// Returns the concatenation of every text block.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

// ── Réponse sortant ────────────────────────────────────────────────────────────

/// Outbound `POST /v1/messages` response in Anthropic format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessagesResponse {
    /// Unique message identifier — prefixed with `msg_`.
    pub id: String,
    /// Object type — always `"message"`.
    #[serde(rename = "type")]
    pub object_type: String,
    /// Author role of the response — always `"assistant"`.
    pub role: String,
    /// Model used, echoed as supplied in the request.
    pub model: String,
    /// Content blocks of the response.
    pub content: Vec<ResponseBlock>,
    /// Reason generation stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Stop sequence that ended generation (when `stop_reason = "stop_sequence"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Token usage (input + output).
    pub usage: AnthropicUsage,
}

/// Content block in an Anthropic response.
///
/// Supported variants: `Text`, `ToolUse`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseBlock {
    /// Generated text block.
    Text {
        /// Text generated by the model.
        text: String,
    },
    /// Tool-call block generated by the model.
    ToolUse {
        /// Unique identifier of the call within this message.
        id: String,
        /// Name of the called tool.
        name: String,
        /// Call arguments (JSON object parsed from the OpenAI argument string).
        input: serde_json::Value,
    },
}

/// Token usage reported in an Anthropic response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicUsage {
    /// Input (prompt) tokens.
    pub input_tokens: u32,
    /// Output (completion) tokens.
    pub output_tokens: u32,
}
