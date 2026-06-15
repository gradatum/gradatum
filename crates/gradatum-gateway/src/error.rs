//! Gateway API errors — OpenAI-compat format.
//!
//! `ApiError` is the unified return type for all Axum handlers.
//! It implements `IntoResponse` to produce HTTP responses with a JSON body:
//!
//! ```json
//! { "error": { "message": "...", "type": "...", "code": "..." } }
//! ```

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use thiserror::Error;

use crate::commons::error::LlmError;

/// Error returned by gateway handlers.
///
/// `#[non_exhaustive]`: variants may be added without a SemVer breaking change
/// (public API stability guarantee).
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ApiError {
    /// Unknown alias — not present in the TOML alias table.
    #[error("model alias '{alias}' not found. Available: {}", available.join(", "))]
    AliasNotFound {
        alias: String,
        available: Vec<String>,
    },

    /// Provider declared for the alias but absent from `[providers]`.
    #[error("provider '{0}' not found in config")]
    ProviderNotFound(String),

    /// LLM error from the backend (network, timeout, upstream error, etc.).
    #[error("backend error: {0}")]
    Backend(#[from] LlmError),

    /// Request body deserialization error.
    #[error("invalid request body: {0}")]
    InvalidBody(String),

    /// Total token cap exceeded (input + `max_tokens` > configured threshold).
    #[error("context length exceeded: {total} tokens > cap {cap}")]
    ContextLengthExceeded { total: u64, cap: u64 },

    /// Too many tools in the request (limit configurable via `max_tools_per_request`).
    #[error("tools array exceeds cap: {count} > {max}")]
    TooManyTools { count: usize, max: usize },

    /// Optional service not configured (reranker, local embedder, etc.).
    /// Distinct from `Backend`/`ProviderUnavailable`: the feature is absent, not degraded.
    #[error("service unavailable: {message}")]
    ServiceUnavailable { message: String },

    /// Multimodal (image) request sent to an alias not declared `vision_capable = true`.
    ///
    /// Vision gate: protects text-only backends from images they cannot process.
    /// HTTP 400 — the client must use a `vision_capable` alias.
    #[error("alias '{alias}' does not support vision requests (vision_capable = false)")]
    VisionNotSupported { alias: String },
}

/// Error body in OpenAI-compat format.
#[derive(serde::Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(serde::Serialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: &'static str,
}

impl ApiError {
    /// Returns the HTTP status code corresponding to this error.
    pub fn status_code(&self) -> u16 {
        match self {
            ApiError::AliasNotFound { .. } => 404,
            ApiError::ProviderNotFound(_) => 500,
            ApiError::InvalidBody(_) => 400,
            ApiError::ContextLengthExceeded { .. } => 413,
            ApiError::TooManyTools { .. } => 400,
            ApiError::ServiceUnavailable { .. } => 503,
            ApiError::VisionNotSupported { .. } => 400,
            ApiError::Backend(llm_err) => match llm_err {
                LlmError::InvalidRequest { .. } => 400,
                LlmError::Unauthorized { .. } => 401,
                LlmError::Forbidden { .. } => 403,
                LlmError::NotFound { .. } => 404,
                LlmError::RateLimited { .. } => 429,
                LlmError::Timeout { .. }
                | LlmError::Network { .. }
                | LlmError::ProviderUnavailable { .. }
                | LlmError::UpstreamError { .. } => 502,
                _ => 500,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_type, code, message) = match &self {
            ApiError::AliasNotFound { alias, available } => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                format!(
                    "Model alias '{}' is not configured. Available aliases: {}",
                    alias,
                    available.join(", ")
                ),
            ),
            ApiError::ProviderNotFound(p) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "provider_not_configured",
                format!("provider '{}' not found in config", p),
            ),
            ApiError::Backend(llm_err) => {
                let status = match llm_err {
                    LlmError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
                    LlmError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
                    LlmError::Forbidden { .. } => StatusCode::FORBIDDEN,
                    LlmError::NotFound { .. } => StatusCode::NOT_FOUND,
                    LlmError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
                    LlmError::Timeout { .. }
                    | LlmError::Network { .. }
                    | LlmError::ProviderUnavailable { .. }
                    | LlmError::UpstreamError { .. } => StatusCode::BAD_GATEWAY,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (status, "server_error", "backend_error", llm_err.to_string())
            }
            ApiError::InvalidBody(msg) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_body",
                msg.clone(),
            ),
            ApiError::ContextLengthExceeded { total, cap } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "context_length_exceeded",
                format!(
                    "Input + max_tokens exceeds {} token cap. Estimated total: {} tokens.",
                    cap, total
                ),
            ),
            ApiError::TooManyTools { count, max } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "too_many_tools",
                format!(
                    "tools array contains {} elements, maximum allowed is {}",
                    count, max
                ),
            ),
            ApiError::ServiceUnavailable { message } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "service_unavailable",
                message.clone(),
            ),
            ApiError::VisionNotSupported { alias } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "vision_not_supported",
                format!(
                    "Alias '{}' does not support vision requests. \
                     Use a vision_capable alias or remove images from the request.",
                    alias
                ),
            ),
        };

        let body = ErrorBody {
            error: ErrorDetail {
                message,
                error_type,
                code,
            },
        };

        (status, Json(body)).into_response()
    }
}
