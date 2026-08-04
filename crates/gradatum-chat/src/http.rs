//! HTTP OpenAI-compatible backend — `/v1/chat/completions`.
//!
//! Compatible with any OpenAI-compat backend (`/v1/chat/completions`): llama.cpp, vLLM, Ollama, etc.
//!
//! ## Robust parsing
//!
//! The LLM may prefix its JSON response with a text preamble (e.g. `"Here is the JSON:\n{…}"`).
//! The implementation first attempts a direct parse, then falls back to a regex extraction
//! of the first `{…}` block if the direct parse fails.

use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::chat_trait::{Chat, ChatBackend, CuratorContext, CuratorVerdict};
use crate::error::ChatError;

// --- Defaults ---

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_TOKENS: u32 = 256;
const DEFAULT_MODEL: &str = "qwen3.6-35b-a3b-q4-k-xl";

/// Extracts the first JSON block `{…}` from a string (handles LLM preamble).
fn re_json_block() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{[^{}]*\}").expect("re_json_block is a valid literal pattern"))
}

/// System prompt injected to enforce a structured JSON response.
const SYSTEM_PROMPT: &str = "You are a curator. Classify the note. \
Reply ONLY in JSON: \
{\"status\":\"live|pending-review|staging|garbage\",\
\"confidence\":0.0-1.0,\
\"reason\":\"...\"}";

// --- DTO OpenAI response ---

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: String,
}

/// Internal DTO — expected JSON verdict from the LLM.
#[derive(Debug, Deserialize)]
struct LlmVerdict {
    status: String,
    confidence: f32,
    reason: String,
}

/// Converts a status string (LLM output) to `NoteStatus`.
fn parse_status(s: &str) -> Result<NoteStatus, ChatError> {
    match s.trim().to_lowercase().as_str() {
        "live" => Ok(NoteStatus::Live),
        "pending-review" | "pending_review" => Ok(NoteStatus::PendingReview),
        "staging" => Ok(NoteStatus::Staging),
        "garbage" => Ok(NoteStatus::Garbage),
        "draft" => Ok(NoteStatus::Draft),
        "deprecated" => Ok(NoteStatus::Deprecated),
        other => Err(ChatError::InvalidResponse(format!(
            "unknown LLM status: {other:?} (expected: live|pending-review|staging|garbage)"
        ))),
    }
}

/// Parses the LLM response text into an `LlmVerdict`.
///
/// ## Strategy
///
/// 1. Attempts a direct JSON parse of the trimmed content (fast path).
/// 2. On failure, falls back to a regex extraction of the first `{…}` block
///    to handle LLM preamble (e.g. `"Here is the JSON:\n{…}"`).
///
/// ## Limitation
///
/// The fallback regex `\{[^{}]*\}` matches **flat JSON only** — it does not handle
/// nested objects (e.g. `{"key":{"nested":"value"}}`). The curator prompt enforces
/// a flat schema (`status`, `confidence`, `reason` — all scalar values), so nested
/// JSON is not expected and this limitation does not affect normal operation.
/// If the LLM produces nested JSON despite the prompt, the direct parse (step 1)
/// succeeds regardless; only the regex fallback (step 2) is limited to flat structures.
fn extract_verdict(content: &str) -> Result<LlmVerdict, ChatError> {
    // Tentative directe
    if let Ok(v) = serde_json::from_str::<LlmVerdict>(content.trim()) {
        return Ok(v);
    }

    // Extraction regex du premier bloc JSON plat (pas de nesting — voir doc ci-dessus).
    let caps = re_json_block()
        .find(content)
        .ok_or_else(|| ChatError::ParseFailure(format!("no JSON block found in: {content:?}")))?;

    serde_json::from_str::<LlmVerdict>(caps.as_str()).map_err(|e| {
        ChatError::ParseFailure(format!(
            "invalid extracted JSON: {e} — content: {:?}",
            caps.as_str()
        ))
    })
}

/// HTTP OpenAI-compatible backend.
///
/// # Example
///
/// ```rust,no_run
/// use gradatum_chat::http::HttpChat;
///
/// let chat = HttpChat::new("http://localhost:8080/v1/chat/completions", "my-model")
///     .with_timeout(std::time::Duration::from_secs(15))
///     .with_max_tokens(128);
/// ```
pub struct HttpChat {
    client: reqwest::Client,
    /// Full URL of the `/v1/chat/completions` endpoint.
    endpoint: String,
    /// Model identifier passed in the `model` field of the request body.
    model: String,
    /// Configured timeout — retained to reconstruct the client via `with_timeout`.
    // NOTE: intentionally stored even though not read directly (used by with_timeout).
    #[allow(dead_code)]
    timeout: Duration,
    /// Maximum number of tokens in the LLM response.
    max_tokens: u32,
}

impl HttpChat {
    /// Creates an `HttpChat` with default values.
    ///
    /// - timeout: 30 seconds
    /// - max_tokens: 256
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        let timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("the default reqwest configuration is always valid");
        Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            timeout,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    /// Replaces the timeout (rebuilds the internal client).
    #[must_use]
    pub fn with_timeout(self, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("timeout Duration is always valid for reqwest");
        Self {
            client,
            timeout,
            ..self
        }
    }

    /// Replaces the maximum number of tokens.
    #[must_use]
    pub fn with_max_tokens(self, n: u32) -> Self {
        Self {
            max_tokens: n,
            ..self
        }
    }
}

impl Default for HttpChat {
    fn default() -> Self {
        Self::new("http://localhost:8080/v1/chat/completions", DEFAULT_MODEL)
    }
}

#[async_trait]
impl Chat for HttpChat {
    async fn classify_curator(
        &self,
        note: &Note,
        _context: &CuratorContext,
    ) -> Result<CuratorVerdict, ChatError> {
        // Sérialise la note comme contenu utilisateur (JSON compact).
        let note_content = serde_json::to_string(note)
            .map_err(|e| ChatError::InvalidResponse(format!("note serialization failed: {e}")))?;

        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": note_content}
            ],
            "max_tokens": self.max_tokens,
            "temperature": 0.0
        });

        let response = self.client.post(&self.endpoint).json(&body).send().await?;

        // Propagation des erreurs HTTP non-2xx
        let response = response.error_for_status()?;

        let openai_resp: OpenAiResponse = response.json().await?;

        let content = openai_resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ChatError::InvalidResponse("empty OpenAI response (0 choices)".into()))?
            .message
            .content;

        let verdict = extract_verdict(&content)?;

        // Validation confiance
        if !(0.0..=1.0).contains(&verdict.confidence) {
            return Err(ChatError::InvalidResponse(format!(
                "confidence out of bounds: {} (expected 0.0-1.0)",
                verdict.confidence
            )));
        }

        let proposed_status = parse_status(&verdict.status)?;

        Ok(CuratorVerdict {
            proposed_status,
            confidence: verdict.confidence,
            reason: verdict.reason,
            backend: ChatBackend::Http,
        })
    }

    fn backend_kind(&self) -> ChatBackend {
        ChatBackend::Http
    }
}
