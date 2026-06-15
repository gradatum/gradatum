//! Axum middlewares: JWT authentication and rate limiting.
//!
//! ## JWT auth middleware
//!
//! Verifies the Ed25519 bearer JWT via `JwtService`, then checks revocation.
//!
//! ### Extraction and revocation logic
//!
//! 1. `Authorization: Bearer <token>` header absent → `TrustContext::Unauthenticated`.
//! 2. Header present, `JwtService::verify(token)` succeeds →
//!    revocation check: `state.revocation.is_revoked(&jti)` with a 200 ms timeout.
//!    - Token revoked → immediate 401 (not injected into extensions).
//!    - Store error or timeout → 401 **fail-closed** + `tracing::error!` (never panic or 500).
//!    - Valid, non-revoked token → `TrustContext::BearerToken` injected.
//! 3. Header present, verify fails (exp/kid/sig) →
//!    `TrustContext::Unauthenticated` (the handler returns 401).
//!
//! ### Fail-closed policy
//!
//! The revocation check is fail-closed: any store error (I/O, SQLite, timeout)
//! results in a 401, never a silent pass-through. Rationale: revocation is a
//! security mechanism — a degraded store must not allow access with a revoked token
//! (e.g. a compromised token whose revocation the operator requested immediately).
//!
//! `TrustContext` is injected via `request.extensions_mut().insert(trust)`
//! before calling `next.run(request)`.
//!
//! ## Rate-limiting middleware
//!
//! [`build_warden_layer`] builds a [`WardenLayer`] (the `gradatum-warden` crate)
//! from [`RateLimitConfig`].
//!
//! ### Loopback bypass: real implementation
//!
//! The loopback bypass in `gradatum_warden::WardenService` calls `inner.call(req)`
//! directly for loopback IPs — the business handler returns its real body.
//!
//! ### Mounting order on the rate-limited router:
//! ```text
//! incoming request
//!   → WardenLayer (gradatum-warden)
//!       if loopback + bypass_loopback: inner.call(req) direct → real handler body
//!       if IP deny-filtered: 403
//!       if rate limit exceeded: 429 + retry-after
//!       otherwise: inner.call(req) → real handler body
//!   → business handler
//! ```

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::config::RateLimitConfig;
use crate::state::AppState;
use gradatum_warden::{WardenConfig, WardenLayer};

// ─── Rate limiting ───────────────────────────────────────────────────────────

/// Builds a [`WardenLayer`] from the server [`RateLimitConfig`].
///
/// Returns `None` if `!cfg.enabled` (no rate limiting applied).
///
/// The [`WardenLayer`] handles:
/// - Real loopback bypass (`inner.call(req)` called directly — real handler body returned)
/// - CIDR IP filtering (empty by default = allow all)
/// - Per-IP rate limiting via a governor token bucket
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

/// Timeout for the JWT revocation check (fail-closed if exceeded).
///
/// 200 ms: well above local SQLite latency (<1 ms p99),
/// short enough not to block requests when the store is degraded.
const REVOCATION_CHECK_TIMEOUT: Duration = Duration::from_millis(200);

/// Uniform JSON error response for middleware 401 replies.
#[derive(Serialize)]
struct MiddlewareError {
    error: &'static str,
}

/// Builds a uniform 401 JSON response.
fn unauthorized(msg: &'static str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(MiddlewareError { error: msg }),
    )
        .into_response()
}

/// Builds a uniform 403 JSON response (tenant rejection).
fn forbidden(msg: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(MiddlewareError { error: msg }),
    )
        .into_response()
}

/// Axum middleware that extracts and validates the bearer JWT, then checks revocation.
///
/// Sequence:
/// 1. `extract_trust` parses the header and verifies the JWT signature (sync).
/// 2. If the token is valid (`BearerToken`), the jti is extracted and
///    `state.revocation.is_revoked(&jti)` is called with a 200 ms timeout
///    (fail-closed: store error or timeout → 401).
/// 3. The resulting `TrustContext` is injected into the request extensions.
///
/// Returns a 401 response directly if:
/// - the token is revoked,
/// - the revocation store returns an error,
/// - the check exceeds the 200 ms timeout.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let (trust, maybe_jti) = extract_trust(&state, &request);

    // Check de révocation uniquement pour les tokens valides (BearerToken).
    // Pour Unauthenticated : le handler métier retournera 401 — pas de check nécessaire.
    if let Some(jti) = maybe_jti {
        match tokio::time::timeout(
            REVOCATION_CHECK_TIMEOUT,
            state.revocation.is_revoked(jti.as_str()),
        )
        .await
        {
            Ok(Ok(true)) => {
                tracing::debug!(jti = %jti, "token JWT révoqué — 401");
                return unauthorized("token révoqué");
            }
            Ok(Ok(false)) => {
                // Token valide et non révoqué — continuer.
            }
            Ok(Err(e)) => {
                tracing::error!(
                    err = %e,
                    "revocation store error — fail-closed (401)"
                );
                return unauthorized("erreur de vérification du token — réessayer plus tard");
            }
            Err(_timeout) => {
                tracing::error!(
                    timeout_ms = REVOCATION_CHECK_TIMEOUT.as_millis(),
                    "revocation check timeout — fail-closed (401)"
                );
                return unauthorized("erreur de vérification du token — réessayer plus tard");
            }
        }
    }

    // SÉCU P0 cross-tenant (Lot 2) — garde middleware centrale, defense-in-depth.
    // Tant que le vault est mono-physique "main", aucun `BearerToken` tenant ≠ "main"
    // ne doit atteindre un handler. Même si un tel JWT venait à exister (clé legacy
    // antérieure au gate Lot 1, ou compromission de clé de signature), il est refusé
    // ici, AVANT toute logique métier. Restrictive-only : zéro impact tenant "main".
    if !tenant_is_authorized(&trust) {
        tracing::warn!(
            tenant = ?trust.tenant_id(),
            "middleware: TrustContext refusé (tenant ≠ main, invariant mono-vault) — 403"
        );
        return forbidden("tenant non supporté (mono-vault v0.4.x)");
    }

    request.extensions_mut().insert(trust);
    next.run(request).await
}

/// Authorises a [`TrustContext`] to reach vault handlers, enforcing the mono-vault
/// `"main"` tenant invariant.
///
/// **Exhaustive match without `_`**: any new `TrustContext` variant must explicitly
/// declare its authorisation (fail-safe by construction).
///
/// Rules:
/// - `BearerToken { tenant_id, .. }`: allowed only if `tenant_id == "main"`.
/// - `Unauthenticated`: allowed to pass through (the business handler returns 401).
/// - `Mtls` / `Studio`: carry no tenant — must not access vault by locus tenant.
///   Allowed through here; their vault access refusal is handled downstream by the ACL
///   (`tenant_id()` returns `None`). No catch-all `_`.
fn tenant_is_authorized(trust: &gradatum_core::trust::TrustContext) -> bool {
    use gradatum_core::trust::TrustContext;
    match trust {
        TrustContext::BearerToken { tenant_id, .. } => tenant_id == "main",
        TrustContext::Unauthenticated => true,
        TrustContext::Mtls { .. } => true,
        TrustContext::Studio { .. } => true,
    }
}

/// Extracts a [`TrustContext`] from HTTP headers via `JwtService`.
///
/// Returns a tuple `(TrustContext, Option<jti>)`:
/// - `jti` is `Some` only when the token is valid (`BearerToken`) — used
///   by `auth_middleware` for the async revocation check.
/// - `jti` is `None` for `Unauthenticated` (no revocation check needed).
///
/// Logic:
/// - No `Authorization` header → `(Unauthenticated, None)`.
/// - Non-`Bearer` header → `(Unauthenticated, None)`.
/// - Empty token → `(Unauthenticated, None)`.
/// - Verify OK → `(BearerToken { kid, aud, sub, scopes }, Some(jti))`.
/// - Verify fails (kid/aud/exp/sig) → `(Unauthenticated, None)` (logged at DEBUG, not ERROR —
///   invalid attempts are not server errors).
fn extract_trust(
    state: &AppState,
    request: &Request<Body>,
) -> (gradatum_core::trust::TrustContext, Option<String>) {
    let header_value = match request.headers().get(axum::http::header::AUTHORIZATION) {
        Some(v) => v,
        None => return (gradatum_core::trust::TrustContext::Unauthenticated, None),
    };

    let raw = match header_value.to_str() {
        Ok(s) => s,
        Err(_) => {
            tracing::debug!("Authorization header contient des octets non-UTF-8 — ignoré");
            return (gradatum_core::trust::TrustContext::Unauthenticated, None);
        }
    };

    let token = match raw.strip_prefix("Bearer ") {
        Some(t) if !t.is_empty() => t,
        _ => return (gradatum_core::trust::TrustContext::Unauthenticated, None),
    };

    match state.jwt.verify(token) {
        Ok(claims) => {
            tracing::debug!(
                sub = %claims.sub,
                tenant = %claims.tenant_id,
                "JWT vérifié avec succès"
            );
            let jti = claims.jti.clone();
            let trust = gradatum_core::trust::TrustContext::BearerToken {
                kid: state.jwt.kid().to_string(),
                aud: claims.aud,
                sub: claims.sub,
                scopes: claims.scopes,
                tenant_id: claims.tenant_id,
            };
            (trust, Some(jti))
        }
        Err(e) => {
            tracing::debug!(err = %e, "JWT invalide — TrustContext::Unauthenticated");
            (gradatum_core::trust::TrustContext::Unauthenticated, None)
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use gradatum_auth::jwt::TokenScope;
    use gradatum_auth::revocation::{RevocationError, RevocationStore};

    use crate::state::AppState;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Crée un `AppState` de test avec un `JwtService` éphémère + `InMemoryRevocationStore`.
    fn make_state() -> AppState {
        AppState::new()
    }

    // ── Lot 2 : tenant_is_authorized (match exhaustif) ─────────────────────────

    #[test]
    fn tenant_authorized_bearer_main_ok() {
        use gradatum_core::trust::TrustContext;
        let trust = TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["write".into()],
            tenant_id: "main".into(),
        };
        assert!(super::tenant_is_authorized(&trust));
    }

    #[test]
    fn tenant_authorized_bearer_non_main_denied() {
        use gradatum_core::trust::TrustContext;
        let trust = TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["write".into()],
            tenant_id: "evil".into(),
        };
        assert!(!super::tenant_is_authorized(&trust));
    }

    #[test]
    fn tenant_authorized_unauthenticated_passes_through() {
        use gradatum_core::trust::TrustContext;
        // Unauthenticated traverse — le handler répond 401 (pas un refus tenant).
        assert!(super::tenant_is_authorized(&TrustContext::Unauthenticated));
    }

    #[test]
    fn tenant_authorized_mtls_and_studio_pass_through() {
        use gradatum_core::trust::{StudioScope, TrustContext};
        // Mtls/Studio ne portent pas de tenant → refus vault géré en aval par l'ACL.
        let mtls = TrustContext::Mtls {
            cn: "client".into(),
            fingerprint_sha256: [0u8; 32],
        };
        let studio = TrustContext::Studio {
            user: "admin".into(),
            scope: StudioScope::ReadOnly,
            step_up_until: None,
        };
        assert!(super::tenant_is_authorized(&mtls));
        assert!(super::tenant_is_authorized(&studio));
    }

    /// Crée un `AppState` avec un store de révocation injecté.
    fn make_state_with_revocation(store: Arc<dyn RevocationStore>) -> AppState {
        let mut state = AppState::new();
        state.revocation = store;
        state
    }

    /// Signe un token de test.
    fn sign_token(state: &AppState, sub: &str) -> (String, String) {
        let token = state
            .jwt
            .sign(sub, &["read".to_string()], TokenScope::Service, "main")
            .expect("sign doit réussir avec une clé éphémère valide");
        // Extraire le jti depuis les claims (vérification round-trip)
        let claims = state
            .jwt
            .verify(&token)
            .expect("verify immédiat ne peut pas échouer");
        (token, claims.jti)
    }

    /// Handler de test minimal — retourne 200 OK.
    async fn handler_ok() -> StatusCode {
        StatusCode::OK
    }

    /// Construit un router de test avec `auth_middleware`.
    fn test_router(state: AppState) -> Router {
        Router::new()
            .route("/test", get(handler_ok))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::auth_middleware,
            ))
            .with_state(state)
    }

    /// Envoie une requête GET /test avec un bearer optionnel, retourne le StatusCode.
    async fn send_request(router: Router, bearer: Option<&str>) -> StatusCode {
        let mut builder = Request::builder().method("GET").uri("/test");
        if let Some(token) = bearer {
            builder = builder.header("Authorization", format!("Bearer {token}"));
        }
        let req = builder
            .body(Body::empty())
            .expect("request builder invariant");
        let resp = router
            .oneshot(req)
            .await
            .expect("handler ne doit pas paniquer");
        resp.status()
    }

    // ── Test 1 : token valide non révoqué → next appelé → 200 ────────────────

    #[tokio::test]
    async fn test_valid_token_not_revoked_passes() {
        let state = make_state();
        let (token, _jti) = sign_token(&state, "user-test");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
    }

    // ── Test 2 : token révoqué → 401 ─────────────────────────────────────────

    #[tokio::test]
    async fn test_revoked_token_returns_401() {
        let state = make_state();
        let (token, jti) = sign_token(&state, "user-revoked");

        // Révoquer le token dans le store.
        let exp = SystemTime::now() + Duration::from_secs(86400);
        state
            .revocation
            .revoke(&jti, exp)
            .await
            .expect("revoke doit réussir sur InMemoryRevocationStore");

        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ── Test 3 : store retourne Err → 401 fail-closed ─────────────────────────

    /// Store de révocation qui retourne toujours une erreur.
    struct AlwaysErrorStore;

    #[async_trait::async_trait]
    impl RevocationStore for AlwaysErrorStore {
        async fn is_revoked(&self, _jti: &str) -> Result<bool, RevocationError> {
            Err(RevocationError::Sqlite(sqlx::Error::RowNotFound))
        }

        async fn revoke(&self, _jti: &str, _exp: SystemTime) -> Result<(), RevocationError> {
            Ok(())
        }

        async fn gc(&self) -> Result<usize, RevocationError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_store_error_returns_401_fail_closed() {
        let error_store = Arc::new(AlwaysErrorStore);
        let state = make_state_with_revocation(error_store);
        let (token, _jti) = sign_token(&state, "user-store-err");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ── Test 4 : timeout → 401 fail-closed ────────────────────────────────────

    /// Store de révocation qui dépasse le timeout (attend 300 ms > REVOCATION_CHECK_TIMEOUT=200ms).
    struct SlowStore;

    #[async_trait::async_trait]
    impl RevocationStore for SlowStore {
        async fn is_revoked(&self, _jti: &str) -> Result<bool, RevocationError> {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(false)
        }

        async fn revoke(&self, _jti: &str, _exp: SystemTime) -> Result<(), RevocationError> {
            Ok(())
        }

        async fn gc(&self) -> Result<usize, RevocationError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn test_timeout_returns_401_fail_closed() {
        let slow_store = Arc::new(SlowStore);
        let state = make_state_with_revocation(slow_store);
        let (token, _jti) = sign_token(&state, "user-slow");
        let router = test_router(state);
        let status = send_request(router, Some(&token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
