//! Erreurs API du gateway — format OpenAI-compat.
//!
//! `ApiError` est le type de retour unifié pour tous les handlers Axum.
//! Il implémente `IntoResponse` pour produire des réponses HTTP avec body JSON :
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

/// Erreur retournée par les handlers du gateway.
///
/// `#[non_exhaustive]` : les variants peuvent évoluer sans constituer un breaking change SemVer
/// (ADN 2 — stabilité API publique).
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ApiError {
    /// Alias inconnu — pas dans la table d'aliases du config TOML.
    #[error("model alias '{alias}' not found. Available: {}", available.join(", "))]
    AliasNotFound {
        alias: String,
        available: Vec<String>,
    },

    /// Provider configuré pour l'alias mais absent de la section `[providers]`.
    #[error("provider '{0}' not found in config")]
    ProviderNotFound(String),

    /// Erreur LLM en provenance du backend (réseau, timeout, upstream error, etc.).
    #[error("backend error: {0}")]
    Backend(#[from] LlmError),

    /// Erreur de désérialisation du body de requête.
    #[error("invalid request body: {0}")]
    InvalidBody(String),

    /// Dépassement du cap total de tokens (input + max_tokens > seuil configuré).
    #[error("context length exceeded: {total} tokens > cap {cap}")]
    ContextLengthExceeded { total: u64, cap: u64 },

    /// F-MAJ-2 : trop d'outils dans la requête (limit configurable).
    #[error("tools array exceeds cap: {count} > {max}")]
    TooManyTools { count: usize, max: usize },

    /// Service optionnel non configuré (reranker, embedder local, etc.).
    /// Distinct de Backend/ProviderUnavailable : la fonctionnalité est absente, pas dégradée.
    #[error("service unavailable: {message}")]
    ServiceUnavailable { message: String },
}

/// Corps d'erreur au format OpenAI-compat.
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
    /// Retourne le code HTTP correspondant à l'erreur.
    pub fn status_code(&self) -> u16 {
        match self {
            ApiError::AliasNotFound { .. } => 404,
            ApiError::ProviderNotFound(_) => 500,
            ApiError::InvalidBody(_) => 400,
            ApiError::ContextLengthExceeded { .. } => 413,
            ApiError::TooManyTools { .. } => 400,
            ApiError::ServiceUnavailable { .. } => 503,
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
