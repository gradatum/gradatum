//! gradatum-server library API for integration tests.
//!
//! T8 expose `api_v1` et `stubs` pour les tests d'intégration.
//! T9 expose `middleware` pour les tests vérifiant l'extraction JWT.
//! T10 expose `health` pour le test d'intégration `health.rs`.
//! T10 (P2.0b) expose `audit_jsonl` pour `JsonlFileSink` (caveat C4).
//! AUTH-T5 expose `auth_routes` pour le handler `/auth/exchange`.
//! W1 (Task W1 Phase 2.1.1) expose `build_rate_limit_test_app` pour les tests E2E rate limit.
pub mod api_v1;
pub mod audit_jsonl;
pub mod auth_routes;
pub mod config;
pub mod event_log_store;
pub mod health;
pub mod metrics;
pub mod metrics_proxy;
pub mod middleware;
pub mod state;
pub mod stubs;

/// Construit un router Axum minimal pour les tests E2E du rate limiting.
///
/// Route exposée : `GET /ping` → 200 "pong" (pas d'authentification).
/// La pile middleware utilise [`gradatum_warden::WardenLayer`] identique à la prod.
///
/// # Bypass loopback
///
/// Contrairement à l'ancienne implémentation `tower_governor` (error_handler retournait
/// `Body::empty()`), le warden appelle directement `inner.call(req)` pour les IPs loopback.
/// Le body retourné est le "pong" réel du handler.
///
/// # Usage dans les tests
///
/// Injecter `ConnectInfo<SocketAddr>` dans les extensions de la requête via
/// `req.extensions_mut().insert(ConnectInfo(addr))` avant `tower::ServiceExt::oneshot`.
///
/// # Remarques
///
/// - Si `rl.enabled == false` : aucun rate limiting, `/ping` répond toujours 200.
/// - Si `rl.exempt_localhost == false` : les connexions loopback sont rate-limitées.
/// - Pour tester le throttle depuis un test local, utiliser `exempt_localhost=false`.
pub fn build_rate_limit_test_app(rl: &config::RateLimitConfig) -> axum::Router {
    use axum::{routing::get, Router};

    async fn ping_handler() -> &'static str {
        "pong"
    }

    let base = Router::new().route("/ping", get(ping_handler));

    match crate::middleware::build_warden_layer(rl) {
        Some(warden) => base.layer(warden),
        None => base,
    }
}
