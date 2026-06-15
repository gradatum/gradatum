//! Trust context for authenticated request handling.
//!
//! Mandatory enum carried by every protected handler via Axum `Extension`.
//! No handler reads `Authorization` directly: extraction lives in
//! `gradatum-server::middleware::TrustExtractor`.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Identifies the origin and trust level of an incoming request.
///
/// Passed as `Extension<TrustContext>` to every protected Axum handler.
/// Extraction from HTTP headers lives in `gradatum-server::middleware`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TrustContext {
    /// Unauthenticated request — access denied on all protected routes.
    Unauthenticated,
    /// JWT bearer token presented via `Authorization: Bearer <token>`.
    BearerToken {
        /// Key ID (`kid` JWT claim) enabling key rotation.
        kid: String,
        /// Expected audience — must be `"gradatum"` for this service.
        aud: String,
        /// Subject — bearer identity (agent ID, user ID, etc.).
        sub: String,
        /// Granted scopes (`"read"`, `"write"`, `"service"`, …).
        scopes: Vec<String>,
        /// Target tenant. Value `"main"` for the root tenant (default).
        tenant_id: String,
    },
    /// mTLS client — certificate verified by the TLS layer.
    Mtls {
        /// Common Name extracted from the client certificate.
        cn: String,
        /// SHA-256 fingerprint of the client certificate (32 bytes).
        fingerprint_sha256: [u8; 32],
    },
    /// Studio session (admin UI) — interactive authentication.
    Studio {
        /// Email or identifier of the Studio user.
        user: String,
        /// Scope of the Studio session.
        scope: StudioScope,
        /// Deadline for a privilege elevation (`sudo`-like step-up).
        step_up_until: Option<SystemTime>,
    },
}

/// Access levels for a Studio session (admin UI).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StudioScope {
    /// Read-only — exploration, audit, search.
    ReadOnly,
    /// Operator — read and write, no admin.
    Operator,
    /// Administrator — full access including token management.
    Admin,
}

impl TrustContext {
    /// Returns `true` for any variant other than `Unauthenticated`.
    pub fn is_authenticated(&self) -> bool {
        !matches!(self, TrustContext::Unauthenticated)
    }

    /// Returns `true` if the bearer token carries the `"service"` scope.
    ///
    /// Service agents (static mcp-stub, backend services, etc.) receive a JWT with
    /// `scope: ["service"]` — eligible for the long TTL tier.
    pub fn is_service_bearer(&self) -> bool {
        matches!(
            self,
            TrustContext::BearerToken { scopes, .. } if scopes.iter().any(|s| s == "service")
        )
    }

    /// Returns the `tenant_id` if present in the context.
    ///
    /// Returns `None` for variants without a tenant (`Unauthenticated`, `Mtls`, `Studio`).
    /// Only `BearerToken` carries the tenant identifier.
    pub fn tenant_id(&self) -> Option<&str> {
        match self {
            TrustContext::BearerToken { tenant_id, .. } => Some(tenant_id.as_str()),
            _ => None,
        }
    }
}

/// Trait used by the middleware to extract [`TrustContext`] from HTTP request parts.
///
/// Concrete implementations live in `gradatum-server::middleware`.
/// Separation allows testing extraction independently of handlers.
#[async_trait::async_trait]
pub trait TrustExtractor: Send + Sync {
    /// Extracts the trust context from HTTP request headers/parts.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError::Missing`] if no credential is present,
    /// [`TrustError::InvalidBearer`] if the JWT is malformed or expired,
    /// [`TrustError::InvalidMtls`] if the client certificate is invalid.
    async fn extract(&self, parts: &http::request::Parts) -> Result<TrustContext, TrustError>;
}

/// Errors that can occur during trust context extraction.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// No credential present in the request.
    #[error("missing authentication credentials")]
    Missing,
    /// JWT bearer token is invalid (malformed, expired, or wrong signature).
    #[error("invalid bearer token: {0}")]
    InvalidBearer(String),
    /// mTLS certificate is invalid or trust chain is not verified.
    #[error("invalid mTLS certificate: {0}")]
    InvalidMtls(String),
}
