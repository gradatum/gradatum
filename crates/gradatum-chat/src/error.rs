//! Errors for the `gradatum-chat` crate.

use thiserror::Error;

/// Errors that can occur when calling a `Chat` backend.
#[derive(Debug, Error)]
pub enum ChatError {
    /// HTTP error from reqwest (connection, DNS, TLS, …).
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// LLM response does not contain parseable valid JSON.
    #[error("parse failure: {0}")]
    ParseFailure(String),

    /// Backend exceeded its timeout.
    #[error("backend timeout")]
    Timeout,

    /// Circuit breaker is open — cooldown active.
    #[error("circuit open (cooldown active)")]
    CircuitOpen,

    /// Structurally invalid response (wrong field, value out of bounds, …).
    #[error("invalid response: {0}")]
    InvalidResponse(String),
}
