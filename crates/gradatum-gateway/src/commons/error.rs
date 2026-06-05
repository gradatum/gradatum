//! Taxonomie d'erreurs LLM — vendoring inline (anti-fuite OSS).
//!
//! Source originale : bibliothèque partagée privée (module error).
//! Adapté pour gradatum-gateway : annotations utoipa retirées, dépendances simplifiées.

use std::fmt;

/// Résultat LLM standard.
pub type LlmResult<T> = Result<T, LlmError>;

/// Erreur unifiée pour les opérations LLM.
///
/// `#[non_exhaustive]` : les variants peuvent évoluer sans constituer un breaking change SemVer
/// (ADN 2 — stabilité API publique).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// Erreur réseau (timeout, connexion refusée, DNS, etc.).
    Network {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Timeout HTTP (distinct de Network — permet retry sans backoff long).
    Timeout { elapsed_secs: f64 },
    /// Code HTTP 4xx/5xx non catégorisé autrement.
    Http { status: u16, body: String },
    /// 400 Bad Request — payload invalide, erreur utilisateur non retryable.
    InvalidRequest { message: String },
    /// 401 Unauthorized — clé API manquante/invalide.
    Unauthorized { message: String },
    /// 403 Forbidden — accès refusé (content filter, geo, etc.).
    Forbidden { message: String },
    /// 404 Not Found — modèle ou endpoint inexistant.
    NotFound { message: String },
    /// 429 Rate Limited — respecter `retry_after_secs` si fourni.
    RateLimited {
        retry_after_secs: Option<u32>,
        message: String,
    },
    /// Quota dépassé (billing) — provider doit être écarté temporairement.
    QuotaExceeded { message: String },
    /// 5xx Upstream — erreur côté provider, retry raisonnable.
    UpstreamError { status: u16, message: String },
    /// Provider complètement indisponible (circuit breaker ouvert, health KO).
    ProviderUnavailable { provider: String, reason: String },
    /// Tous les providers de la fallback chain ont échoué.
    AllProvidersFailed { attempts: Vec<String> },
    /// Erreur de désérialisation du payload (spec non respectée par le provider).
    Serialization { source: serde_json::Error },
    /// Erreur de validation d'un tool_call.
    ToolValidation { tool_name: String, reason: String },
    /// Erreur custom pour les impls providers (cas non standard).
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
    /// Retourne `true` si l'erreur est temporaire et peut être retentée sur le même provider.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            LlmError::Network { .. }
                | LlmError::Timeout { .. }
                | LlmError::RateLimited { .. }
                | LlmError::UpstreamError { .. }
        )
    }

    /// Retourne `true` si l'erreur est un rate-limit (doit respecter retry-after).
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, LlmError::RateLimited { .. })
    }

    /// Retourne `true` si l'erreur est un dépassement de quota.
    pub fn is_quota(&self) -> bool {
        matches!(self, LlmError::QuotaExceeded { .. })
    }

    /// Retourne `true` si l'erreur est côté client (4xx non retryable).
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            LlmError::InvalidRequest { .. }
                | LlmError::Unauthorized { .. }
                | LlmError::Forbidden { .. }
                | LlmError::NotFound { .. }
        )
    }

    /// Retourne `true` si le provider en cours doit être écarté pour la prochaine requête.
    pub fn should_failover(&self) -> bool {
        matches!(
            self,
            LlmError::QuotaExceeded { .. }
                | LlmError::ProviderUnavailable { .. }
                | LlmError::Unauthorized { .. }
                | LlmError::NotFound { .. }
        )
    }

    /// Délai suggéré en secondes avant retry (si applicable).
    pub fn retry_after_secs(&self) -> Option<u32> {
        match self {
            LlmError::RateLimited {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Construit un `LlmError` depuis un status HTTP + body (classification automatique).
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
