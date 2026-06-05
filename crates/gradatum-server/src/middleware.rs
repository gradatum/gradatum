//! Middlewares Axum : authentification JWT + rate limiting.
//!
//! ## Middleware auth JWT
//!
//! Remplace `trust_stub` (T8) par la vraie vérification Ed25519 via [`JwtService`].
//!
//! ### Logique d'extraction
//!
//! 1. Header `Authorization: Bearer <token>` absent → [`TrustContext::Unauthenticated`].
//! 2. Header présent, `JwtService::verify(token)` OK →
//!    [`TrustContext::BearerToken`] construit depuis les claims JWT.
//! 3. Header présent, verify échoue (exp/kid/sig) →
//!    [`TrustContext::Unauthenticated`] (le handler retournera 401).
//!
//! Le [`TrustContext`] est injecté via `request.extensions_mut().insert(trust)`
//! avant de passer la main à `next.run(request)`.
//!
//! ## Middleware rate limiting (W1 — Phase 2.1.1)
//!
//! [`build_warden_layer`] construit un [`WardenLayer`] (`gradatum-warden` crate L0)
//! à partir de [`RateLimitConfig`].
//!
//! ### Bypass loopback : implémentation réelle
//!
//! Le bypass loopback dans [`gradatum_warden::WardenService`] appelle directement
//! `inner.call(req)` pour les IPs loopback — le handler métier retourne son body réel.
//! Contrairement à l'ancienne implémentation `tower_governor` (error_handler terminait
//! la chaîne avec `Body::empty()`), les clients loopback reçoivent le body complet.
//!
//! ### Ordre de montage sur le router rate-limité :
//! ```text
//! requête entrante
//!   → WardenLayer (gradatum-warden)
//!       si loopback + bypass_loopback : inner.call(req) direct → body réel handler
//!       si IP filtrée deny : 403
//!       si rate limit dépassé : 429 + retry-after
//!       sinon : inner.call(req) → body réel handler
//!   → handler métier
//! ```

use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::config::RateLimitConfig;
use crate::state::AppState;
use gradatum_warden::{WardenConfig, WardenLayer};

// ─── Rate limiting ───────────────────────────────────────────────────────────

/// Construit le [`WardenLayer`] depuis la [`RateLimitConfig`] server.
///
/// Retourne `None` si `!cfg.enabled` (aucun rate limiting appliqué).
///
/// Le [`WardenLayer`] gère :
/// - bypass loopback réel (appel `inner.call(req)` direct — body handler retourné)
/// - filtrage IP CIDR (vide par défaut = tout autorisé)
/// - rate limit per-IP via governor token bucket
pub fn build_warden_layer(cfg: &RateLimitConfig) -> Option<WardenLayer> {
    if !cfg.enabled {
        return None;
    }
    let warden_cfg = WardenConfig {
        enabled: true,
        rate_limit_per_minute: cfg.per_minute,
        rate_limit_burst: cfg.burst,
        bypass_loopback: cfg.exempt_localhost,
        ip_allow: vec![],
        ip_deny: vec![],
    };
    Some(WardenLayer::new(warden_cfg).expect(
        "config warden invalide — per_minute et burst doivent être > 0, \
         garantis par RateLimitConfig::default() (60, 10)",
    ))
}

// ─── Auth JWT ────────────────────────────────────────────────────────────────

/// Middleware Axum qui extrait et valide le bearer JWT.
///
/// Insère un [`TrustContext`] dans les extensions de chaque requête :
/// - [`TrustContext::BearerToken`] si le JWT est valide,
/// - [`TrustContext::Unauthenticated`] si absent ou invalide.
///
/// Les handlers lisent `Extension<TrustContext>` et appellent `.is_authenticated()`
/// pour retourner 401 si l'authentification échoue.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let trust = extract_trust(&state, &request);
    request.extensions_mut().insert(trust);
    next.run(request).await
}

/// Extrait le [`TrustContext`] depuis les headers HTTP via `JwtService`.
///
/// Logique :
/// - Pas de header `Authorization` → `Unauthenticated`.
/// - Header non-`Bearer` → `Unauthenticated`.
/// - Token vide → `Unauthenticated`.
/// - Verify OK → `BearerToken { kid, aud, sub, scopes }`.
/// - Verify KO (kid/aud/exp/sig) → `Unauthenticated` (loggé en DEBUG, pas ERROR —
///   les tentatives invalides ne sont pas des erreurs serveur).
fn extract_trust(state: &AppState, request: &Request<Body>) -> gradatum_core::trust::TrustContext {
    let header_value = match request.headers().get(axum::http::header::AUTHORIZATION) {
        Some(v) => v,
        None => return gradatum_core::trust::TrustContext::Unauthenticated,
    };

    let raw = match header_value.to_str() {
        Ok(s) => s,
        Err(_) => {
            tracing::debug!("Authorization header contient des octets non-UTF-8 — ignoré");
            return gradatum_core::trust::TrustContext::Unauthenticated;
        }
    };

    let token = match raw.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => return gradatum_core::trust::TrustContext::Unauthenticated,
    };

    match state.jwt.verify(token) {
        Ok(claims) => {
            tracing::debug!(
                sub = %claims.sub,
                tenant = %claims.tenant_id,
                "JWT vérifié avec succès"
            );
            gradatum_core::trust::TrustContext::BearerToken {
                kid: state.jwt.kid().to_string(),
                aud: claims.aud,
                sub: claims.sub,
                scopes: claims.scopes,
                tenant_id: claims.tenant_id,
            }
        }
        Err(e) => {
            tracing::debug!(err = %e, "JWT invalide — TrustContext::Unauthenticated");
            gradatum_core::trust::TrustContext::Unauthenticated
        }
    }
}
