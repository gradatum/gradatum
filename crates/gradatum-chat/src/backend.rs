//! Trait `LlmBackend` and shared types for curator LLM backends.
//!
//! Distinct from [`crate::chat_trait::Chat`], which operates on `Note` values
//! and returns a `CuratorVerdict`. `LlmBackend` is lower-level: it accepts a
//! system prompt plus a user prompt and returns a structured `CuratorDecision` JSON.
//!
//! ## Design
//!
//! All five backends implement this trait:
//! - `HeuristicBackend`: offline, internal regex dispatch
//! - `OpenAiCompatBackend`: OpenAI / llama.cpp / OpenRouter / any OpenAI-compatible host
//! - `OllamaCompatBackend`: native Ollama `/api/chat`
//! - `AnthropicCompatBackend`: Anthropic with prompt caching
//! - `GeminiCompatBackend`: Google Gemini `/v1beta/models/{model}:generateContent`
//!
//! ## Circuit breaker
//!
//! [`crate::circuit_breaker_llm::CircuitBreaker`] wrapping `LlmBackend` with
//! exponential backoff and transparent fallback to `HeuristicBackend`.
//!

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned by an `LlmBackend`.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Network timeout.
    #[error("timeout")]
    Timeout,

    /// Connection refused.
    #[error("connection refused")]
    ConnectionRefused,

    /// DNS failure.
    #[error("dns failure: {0}")]
    DnsFailure(String),

    /// Rate limit (HTTP 429).
    #[error("rate limit (429)")]
    RateLimit,

    /// Server error (5xx).
    #[error("server error ({status}): {body}")]
    ServerError {
        /// HTTP status code of the server response.
        status: u16,
        /// Body of the error response (may be empty).
        body: String,
    },

    /// Authentication error (401/403) — does NOT count toward circuit-breaker failures.
    #[error("auth error (401/403)")]
    AuthError,

    /// Bad request (400) — does NOT count toward circuit-breaker failures.
    #[error("bad request (400): {0}")]
    BadRequest(String),

    /// LLM response parse error.
    #[error("parse: {0}")]
    Parse(String),

    /// Generic HTTP transport error.
    #[error("transport: {0}")]
    Transport(String),
}

impl LlmError {
    /// Returns `true` if this error should be counted by the circuit breaker.
    ///
    /// - `AuthError` and `BadRequest` are permanent configuration errors
    ///   (not self-correcting over time) and do not count.
    /// - `Timeout`, connection, rate limit, and server errors do count.
    pub fn counts_for_circuit(&self) -> bool {
        !matches!(self, LlmError::AuthError | LlmError::BadRequest(_))
    }
}

/// Structured decision returned by the LLM after classifying a note.
///
/// Matches the JSON output expected by the `curator-classifier-v1.md` prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorDecision {
    /// Canonical section among the 11 gradatum sections.
    pub section: String,
    /// Extracted tags (2–5, kebab-case).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Wikilinks detected in the body (`[[NoteTitle]]`).
    #[serde(default)]
    pub wikilinks: Vec<String>,
    /// Title of a potential duplicate, or `null`.
    #[serde(default)]
    pub duplicate_hint: Option<String>,
}

/// Backend-agnostic trait for curator LLM backends.
///
/// All implementations are `Send + Sync + 'static` so they can be
/// wrapped in `Arc<dyn LlmBackend>`.
#[async_trait]
pub trait LlmBackend: Send + Sync + 'static {
    /// Short name identifying the backend (used in logs and metrics).
    fn name(&self) -> &'static str;

    /// Returns `true` if the backend runs locally (no external network call).
    fn is_local(&self) -> bool;

    /// Classifies a note via a system prompt and a user prompt.
    ///
    /// - `system`: system prompt (curator-classifier-v1 system message)
    /// - `user`:   formatted user content (`"Classify this note.\nTitle: ...\nBody: ..."`)
    ///
    /// # Errors
    ///
    /// Returns `LlmError` on failure. The caller (circuit breaker) handles
    /// fallback to `HeuristicBackend`.
    async fn classify(&self, system: &str, user: &str) -> Result<CuratorDecision, LlmError>;
}
