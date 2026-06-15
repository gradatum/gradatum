//! Route `/auth/exchange` — exchanges an API key for a JWT (bearer auth path).
//!
//! # Flow
//!
//! 1. Client sends `Authorization: Bearer ak_<secret>` (or `Authorization: ak_<secret>`)
//! 2. Handler extracts the secret and calls `state.api_keys.verify(secret)`
//! 3. If valid: signs a `TokenScope::Service` JWT with sub=owner, scopes=key.scopes, tenant_id=key.tenant_id
//! 4. Returns `{ "token": "<jwt>", "ttl_secs": <ttl>, "scopes": [...], "tenant_id": "...", "kid": "..." }`
//!
//! # Security
//!
//! - Mounted BEFORE the `auth_middleware` (no JWT required to authenticate)
//! - The API key secret is never logged (only the prefix is traced)
//! - Verification errors (NotFound + wrong secret) return a uniform 401
//! - Revoked keys return 401 (same message — no enumeration distinction)
//! - Argon2id verification timeout inherent (~50-200 ms depending on cost)
//!
//! # Error codes
//!
//! | Code | Case |
//! |------|------|
//! | 400  | Header absent or invalid format |
//! | 401  | Invalid, unknown, or revoked key |
//! | 500  | Internal SQLite error or JWT signing failure |

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use gradatum_acl_auth::ApiKeyError;
use gradatum_auth::jwt::TokenScope;
use serde::Serialize;

use crate::state::AppState;

/// Success response from `/auth/exchange` — 5 fields.
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct ExchangeResponse {
    /// Signed JWT token, usable on `/api/v1/*`.
    pub token: String,
    /// TTL in seconds (86400 s = 24 h for `TokenScope::Service`).
    pub ttl_secs: u64,
    /// Scopes granted by the API key (e.g. `["admin"]`).
    pub scopes: Vec<String>,
    /// Tenant ID associated with the API key (e.g. `"main"`).
    pub tenant_id: String,
    /// JWT signing key identifier (`kid` header).
    pub kid: String,
}

/// Error response body.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
}

/// Handles `POST /auth/exchange`.
///
/// Extracts the API key secret from the `Authorization` header, verifies it via
/// `state.api_keys.verify()`, signs a service JWT, and returns the token.
///
/// # Expected header
///
/// - `Authorization: Bearer ak_<32hex>` (standard format)
/// - `Authorization: ak_<32hex>` (alternative format also accepted)
pub async fn exchange(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    // Extraction du secret depuis le header Authorization.
    let secret = match extract_api_key_secret(&headers) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error:
                        "Authorization header absent ou format invalide (attendu: Bearer ak_...)",
                }),
            )
                .into_response();
        }
    };

    // Log du préfixe uniquement (jamais le secret complet — sécurité).
    let prefix_display = if secret.len() >= 11 {
        &secret[..11] // "ak_" + 8 chars
    } else {
        &secret[..]
    };
    tracing::debug!(prefix = %prefix_display, "tentative d'échange API key → JWT");

    // Vérification argon2id via le store.
    let key = match state.api_keys.verify(&secret).await {
        Ok(k) => k,
        Err(ApiKeyError::AlreadyRevoked) => {
            tracing::warn!(prefix = %prefix_display, "tentative d'échange avec clé révoquée");
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "clé API invalide ou révoquée",
                }),
            )
                .into_response();
        }
        Err(ApiKeyError::NotFound) => {
            tracing::debug!(prefix = %prefix_display, "clé API non trouvée ou secret incorrect");
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "clé API invalide ou révoquée",
                }),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "erreur interne lors de la vérification API key");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "erreur interne — réessayer plus tard",
                }),
            )
                .into_response();
        }
    };

    // SÉCU P0 cross-tenant (Lot 1) — gate à la source.
    // Tant que le vault est mono-physique "main", aucun JWT ne doit être émis pour
    // un tenant ≠ "main". `/auth/exchange` étant l'UNIQUE émetteur de JWT (Path 2),
    // ce refus garantit qu'aucun bearer non-main ne peut exister dans tout le système
    // (handlers présents ET futurs). Restrictive-only : zéro impact pour les clés "main".
    if key.tenant_id != "main" {
        tracing::warn!(
            owner = %key.owner,
            tenant = %key.tenant_id,
            prefix = %prefix_display,
            "échange refusé : clé API tenant ≠ main (invariant mono-vault, aucun mint JWT)"
        );
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error:
                    "tenant non supporté (mono-vault v0.4.x) — seul le tenant 'main' est autorisé",
            }),
        )
            .into_response();
    }

    // Signature du token JWT service (R-A1 : TTL 86400s).
    let ttl_secs = state.jwt.ttl_service_secs();
    let token = match state
        .jwt
        .sign(&key.owner, &key.scopes, TokenScope::Service, &key.tenant_id)
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "erreur de signature JWT lors de l'échange");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "erreur interne — réessayer plus tard",
                }),
            )
                .into_response();
        }
    };

    tracing::info!(
        owner = %key.owner,
        tenant = %key.tenant_id,
        prefix = %prefix_display,
        "échange API key → JWT réussi"
    );

    (
        StatusCode::OK,
        Json(ExchangeResponse {
            token,
            ttl_secs,
            scopes: key.scopes.clone(),
            tenant_id: key.tenant_id.clone(),
            kid: state.jwt.kid().to_string(),
        }),
    )
        .into_response()
}

/// Extracts the API key secret from the `Authorization` header.
///
/// Accepts two formats:
/// - `Authorization: Bearer ak_<secret>` (standard)
/// - `Authorization: ak_<secret>` (alternative)
///
/// Returns `None` if the header is absent or the value does not start with `ak_`.
fn extract_api_key_secret(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get("Authorization")?.to_str().ok()?;

    // Format 1 : "Bearer ak_..."
    if let Some(rest) = auth.strip_prefix("Bearer ") {
        let trimmed = rest.trim();
        if trimmed.starts_with("ak_") {
            return Some(trimmed.to_string());
        }
    }

    // Format 2 : "ak_..." directement
    let trimmed = auth.trim();
    if trimmed.starts_with("ak_") {
        return Some(trimmed.to_string());
    }

    None
}

/// Builds the `/auth` router.
///
/// Mounted BEFORE the `auth_middleware` in `build_router`.
/// State is injected via `Router::with_state`.
pub fn router() -> axum::Router<AppState> {
    use axum::{routing::post, Router};
    Router::new().route("/auth/exchange", post(exchange))
}
