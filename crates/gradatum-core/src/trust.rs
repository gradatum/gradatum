//! Trust context for authenticated request handling.
//!
//! Mandatory enum carried by every protected handler via Axum `Extension`.
//! No handler reads `Authorization` directly: extraction lives in
//! `gradatum-server::middleware::TrustExtractor`.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::scope::{AgentId, TenantId};

/// Identifies the origin and trust level of an incoming request.
///
/// Passed as `Extension<TrustContext>` to every protected Axum handler.
/// Extraction from HTTP headers lives in `gradatum-server::middleware`.
///
/// `#[non_exhaustive]`: new variants may be added within the `1.x` line, so downstream
/// matches must carry a fail-closed `_` arm — a future variant must never be silently
/// authorised.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
pub enum TrustContext {
    /// Unauthenticated request — access denied on all protected routes.
    Unauthenticated,
    /// JWT bearer token presented via `Authorization: Bearer <token>`.
    BearerToken {
        /// Key ID (`kid` JWT claim) enabling key rotation.
        kid: String,
        /// Expected audience — must be `"gradatum"` for this service.
        aud: String,
        /// Subject — the credential-borne agent identity, typed [`AgentId`].
        ///
        /// Two server-side origins, never a client-supplied value: the
        /// `api_keys.owner` column read after argon2id verification, or the `sub`
        /// claim of an already signature-verified JWT.
        /// The wire format is unchanged (`#[serde(transparent)]` — serialised as a
        /// bare `String`).
        sub: AgentId,
        /// Granted scopes (`"read"`, `"write"`, `"service"`, …).
        scopes: Vec<String>,
        /// Target tenant (**principal**), taken from the mandatory `tenant_id` JWT
        /// claim, or from the API key record on the direct api-key path.
        /// Value `"main"` for the root tenant (default). Typed as [`TenantId`]: the
        /// principal dimension, distinct from the [`crate::scope::VaultId`] namespace.
        /// The wire format is unchanged (`#[serde(transparent)]` — serialised as a
        /// bare `String`).
        tenant_id: TenantId,
        /// Token instance identifier (`jti` JWT claim).
        ///
        /// `Some` on the JWT path (one unique ULID per minted token, revocable through
        /// the `RevocationStore`); `None` on the direct api-key Bearer path, where the
        /// stable key ULID already lives in `kid`. Carried so that the audit trail can
        /// name the exact token instance behind every event.
        #[serde(default)]
        jti: Option<String>,
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
///
/// `#[non_exhaustive]`: new variants may be added within the `1.x` line, so downstream
/// matches must carry a fail-closed `_` arm (least privilege for any future scope).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
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

    /// Returns `true` if the bearer token carries the given scope.
    ///
    /// **Planned capability — not enforced in `1.0.0`.** No caller in this workspace
    /// invokes this method: it gates nothing, on any request path. Its presence is not
    /// evidence that a scope is required anywhere.
    ///
    /// The scope check actually enforced today is the coarse read/write split of
    /// `gradatum_acl_auth::has_write_scope` (constant `WRITE_SCOPES`), applied by the
    /// server-side tenant guard. This method is the intended primitive for the
    /// finer-grained, per-endpoint scope control that is **not** implemented yet
    /// (e.g. requiring a `"write"` scope on write-path endpoints); handlers enforcing
    /// only authentication today could then be upgraded without signature change.
    pub fn has_scope(&self, scope: &str) -> bool {
        matches!(
            self,
            TrustContext::BearerToken { scopes, .. } if scopes.iter().any(|s| s == scope)
        )
    }

    /// Returns `true` if the bearer token carries the `"service"` scope.
    ///
    /// **Planned capability — not enforced in `1.0.0`.** No caller in this workspace
    /// invokes this method: it grants nothing and selects nothing.
    ///
    /// A long-TTL tier does exist for service agents (static mcp-stub, backend
    /// services, …), but it is **not** derived from this predicate: the server picks the
    /// tier at API-key exchange time, from the requested `scope` field of the exchange
    /// request, not from the scopes already carried by a `TrustContext`. This method is
    /// the intended primitive for a future check that reads the tier back from an
    /// already-issued token.
    pub fn is_service_bearer(&self) -> bool {
        matches!(
            self,
            TrustContext::BearerToken { scopes, .. } if scopes.iter().any(|s| s == "service")
        )
    }

    /// Returns the typed principal [`TenantId`] if present in the context.
    ///
    /// Returns `None` for variants without a tenant (`Unauthenticated`, `Mtls`, `Studio`).
    /// Only `BearerToken` carries the principal identifier. Callers needing the bare
    /// string call [`TenantId::as_str`] on the result (frontier boundary, byte-identical).
    pub fn tenant_id(&self) -> Option<&TenantId> {
        match self {
            TrustContext::BearerToken { tenant_id, .. } => Some(tenant_id),
            _ => None,
        }
    }

    /// Returns the caller's typed [`AgentId`] (JWT `sub` / api-key `owner`).
    ///
    /// Returns `None` for variants without a subject (`Unauthenticated`, `Mtls`, `Studio`).
    /// Used by write-restrictive ACL guards (e.g. the `identity` section) to
    /// derive the caller identity server-side without trusting any client-supplied parameter.
    /// Callers needing the bare string call [`AgentId::as_str`] on the result (frontier
    /// boundary, byte-identical).
    pub fn subject(&self) -> Option<&AgentId> {
        match self {
            TrustContext::BearerToken { sub, .. } => Some(sub),
            _ => None,
        }
    }
}

/// Trait used by the middleware to extract [`TrustContext`] from HTTP request parts.
///
/// **No implementor in the workspace** — `gradatum-server::middleware` extracts its
/// [`TrustContext`] without going through this trait (its only two mentions are in this
/// file). The separation it was meant to provide is not in effect.
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
