//! LLM error taxonomy — inline vendored (OSS-compatible).
//!
//! Original source: private shared library (`error` module).
//! Adapted for gradatum-gateway: utoipa annotations removed, dependencies simplified.

use std::fmt;

/// Standard LLM result type.
pub type LlmResult<T> = Result<T, LlmError>;

/// Unified error type for LLM operations.
///
/// `#[non_exhaustive]`: variants may be added without a SemVer breaking change
/// (public API stability guarantee).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Network error (timeout, connection refused, DNS, etc.).
    Network {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// HTTP timeout (distinct from `Network` — allows retry without long backoff).
    Timeout { elapsed_secs: f64 },
    /// HTTP 4xx/5xx status not covered by a more specific variant.
    Http { status: u16, body: String },
    /// HTTP 400 Bad Request — invalid payload, non-retryable user error.
    InvalidRequest { message: String },
    /// HTTP 401 Unauthorized — missing or invalid API key.
    Unauthorized { message: String },
    /// HTTP 403 Forbidden — access denied (content filter, geo-block, etc.).
    Forbidden { message: String },
    /// HTTP 404 Not Found — model or endpoint does not exist.
    NotFound { message: String },
    /// HTTP 429 Rate Limited — honor `retry_after_secs` when provided.
    RateLimited {
        retry_after_secs: Option<u32>,
        message: String,
    },
    /// Quota exceeded (billing) — provider should be skipped temporarily.
    QuotaExceeded { message: String },
    /// HTTP 5xx Upstream error — provider-side fault, retry is reasonable.
    UpstreamError { status: u16, message: String },
    /// Provider completely unavailable (circuit breaker open, health check failed).
    ProviderUnavailable { provider: String, reason: String },
    /// All providers in the fallback chain failed.
    AllProvidersFailed { attempts: Vec<String> },
    /// Payload deserialization error (provider violates the spec).
    Serialization { source: serde_json::Error },
    /// `tool_call` validation error.
    ToolValidation { tool_name: String, reason: String },
    /// Custom error for provider implementations (non-standard cases).
    Custom { message: String },
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::Network { source } => write!(f, "network error: {}", source),
            LlmError::Timeout { elapsed_secs } => {
                write!(f, "request timed out after {:.1}s", elapsed_secs)
            }
            LlmError::Http { status, body } => write!(f, "http {}: {}", status, body),
            LlmError::InvalidRequest { message } => write!(f, "invalid request: {}", message),
            LlmError::Unauthorized { message } => write!(f, "unauthorized: {}", message),
            LlmError::Forbidden { message } => write!(f, "forbidden: {}", message),
            LlmError::NotFound { message } => write!(f, "not found: {}", message),
            LlmError::RateLimited {
                retry_after_secs,
                message,
            } => match retry_after_secs {
                Some(s) => write!(f, "rate limited (retry after {}s): {}", s, message),
                None => write!(f, "rate limited: {}", message),
            },
            LlmError::QuotaExceeded { message } => write!(f, "quota exceeded: {}", message),
            LlmError::UpstreamError { status, message } => {
                write!(f, "upstream error {}: {}", status, message)
            }
            LlmError::ProviderUnavailable { provider, reason } => {
                write!(f, "provider '{}' unavailable: {}", provider, reason)
            }
            LlmError::AllProvidersFailed { attempts } => {
                write!(f, "all providers failed: {}", attempts.join(", "))
            }
            LlmError::Serialization { source } => {
                write!(f, "serialization error: {}", source)
            }
            LlmError::ToolValidation { tool_name, reason } => {
                write!(f, "tool '{}' validation failed: {}", tool_name, reason)
            }
            LlmError::Custom { message } => write!(f, "{}", message),
        }
    }
}

impl LlmError {
    /// Returns `true` if the error is transient and may be retried against the same provider.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::Network { .. }
                | LlmError::Timeout { .. }
                | LlmError::RateLimited { .. }
                | LlmError::UpstreamError { .. }
        )
    }

    /// Returns `true` if the error is a rate-limit (retry-after must be honored).
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, LlmError::RateLimited { .. })
    }

    /// Returns `true` if the error is a quota exhaustion.
    pub fn is_quota(&self) -> bool {
        matches!(self, LlmError::QuotaExceeded { .. })
    }

    /// Returns `true` if the error is client-side (4xx non-retryable).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            LlmError::InvalidRequest { .. }
                | LlmError::Unauthorized { .. }
                | LlmError::Forbidden { .. }
                | LlmError::NotFound { .. }
        )
    }

    /// Returns `true` if the current provider should be skipped for the next request.
    pub fn should_failover(&self) -> bool {
        matches!(
            self,
            LlmError::QuotaExceeded { .. }
                | LlmError::ProviderUnavailable { .. }
                | LlmError::Unauthorized { .. }
                | LlmError::NotFound { .. }
        )
    }

    /// Suggested delay in seconds before retrying, when applicable.
    pub fn retry_after_secs(&self) -> Option<u32> {
        match self {
            LlmError::RateLimited {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Builds an `LlmError` from an HTTP status code and body (automatic classification).
    pub fn from_http_status(status: u16, body: String) -> Self {
        match status {
            400 => LlmError::InvalidRequest { message: body },
            401 => LlmError::Unauthorized { message: body },
            403 => LlmError::Forbidden { message: body },
            404 => LlmError::NotFound { message: body },
            402 | 413 => LlmError::QuotaExceeded { message: body },
            429 => LlmError::RateLimited {
                retry_after_secs: None,
                message: body,
            },
            500..=599 => LlmError::UpstreamError {
                status,
                message: body,
            },
            _ => LlmError::Http { status, body },
        }
    }
}

impl From<reqwest::Error> for LlmError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            LlmError::Timeout { elapsed_secs: 0.0 }
        } else {
            LlmError::Network {
                source: Box::new(e),
            }
        }
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(source: serde_json::Error) -> Self {
        LlmError::Serialization { source }
    }
}
