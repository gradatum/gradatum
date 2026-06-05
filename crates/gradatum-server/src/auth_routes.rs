//! Route `/auth/exchange` — échange API key → JWT (Auth Path 2, AUTH-T5).
//!
//! # Flux
//!
//! 1. Client envoie `Authorization: Bearer ak_<secret>` (ou `Authorization: ak_<secret>`)
//! 2. Handler extrait le secret, appelle `state.api_keys.verify(secret)`
//! 3. Si valide : signe un JWT `TokenScope::Service` avec sub=owner, scopes=key.scopes, tenant_id=key.tenant_id
//! 4. Retourne `{ "token": "<jwt>", "ttl_secs": <ttl>, "scopes": [...], "tenant_id": "...", "kid": "..." }`
//!
//! # Sécurité
//!
//! - Monté AVANT le middleware `auth_middleware` (pas de JWT requis pour s'authentifier)
//! - Le secret API key n'est jamais loggué (seul le préfixe est tracé)
//! - Les erreurs de vérification (NotFound + mauvais secret) retournent 401 uniforme
//! - Les clés révoquées retournent 401 (même message — pas de distinction énumération)
//! - Timeout argon2id inhérent à la vérification (~50-200ms selon cost)
//!
//! # Codes d'erreur
//!
//! | Code | Cas |
//! |------|-----|
//! | 400  | Header absent ou format invalide |
//! | 401  | Clé invalide, inconnue ou révoquée |
//! | 500  | Erreur interne SQLite ou signature JWT |

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

/// Réponse de `/auth/exchange` (succès) — spec §2.4 P2.0c-bis (5 champs).
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct ExchangeResponse {
    /// Token JWT signé, utilisable sur `/api/v1/*`.
    pub token: String,
    /// TTL en secondes (R-A1 : 86400s = 24h pour TokenScope::Service).
    pub ttl_secs: u64,
    /// Scopes accordés par la clé API (ex. `["admin"]`).
    pub scopes: Vec<String>,
    /// Tenant ID associé à la clé API (ex. `"main"`).
    pub tenant_id: String,
    /// Identifiant de la clé de signature JWT (kid header).
    pub kid: String,
}

/// Corps de la réponse d'erreur.
#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
}

/// Handler `POST /auth/exchange`.
///
/// Extrait le secret API key depuis le header `Authorization`, vérifie via
/// `state.api_keys.verify()`, signe un JWT service, retourne le token.
///
/// # Header attendu
///
/// - `Authorization: Bearer ak_<32hex>` (format standard)
/// - `Authorization: ak_<32hex>` (format alternatif accepté)
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

/// Extrait le secret API key depuis le header `Authorization`.
///
/// Accepte deux formats :
/// - `Authorization: Bearer ak_<secret>` (standard)
/// - `Authorization: ak_<secret>` (alternatif)
///
/// Retourne `None` si le header est absent ou si le format ne commence pas par `ak_`.
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

/// Construit le routeur `/auth`.
///
/// Monté AVANT le middleware `auth_middleware` dans `build_router`.
/// L'état est injecté via `Router::with_state`.
pub fn router() -> axum::Router<AppState> {
    use axum::{routing::post, Router};
    Router::new().route("/auth/exchange", post(exchange))
}
