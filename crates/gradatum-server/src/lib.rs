//! gradatum-server library API for integration tests.
//!
//! Exposes `api_v1` and `stubs` for integration tests.
//! Exposes `middleware` for tests that verify JWT extraction.
//! Exposes `health` for the `health.rs` integration test.
//! Exposes `audit_jsonl` for `JsonlFileSink`.
//! Exposes `auth_routes` for the `/auth/exchange` handler.
//! Exposes `build_rate_limit_test_app` for rate-limit E2E tests.
pub mod api_v1;
pub mod audit_jsonl;
pub mod auth_routes;
pub mod config;
pub mod event_log_store;
pub mod health;
pub mod metrics;
pub mod metrics_proxy;
pub mod middleware;
pub mod session_trace_store;
pub mod state;
pub mod stubs;
pub mod studio;

/// Builds a minimal Axum router for rate-limit E2E tests.
///
/// Exposed route: `GET /ping` → 200 "pong" (no authentication required).
/// The middleware stack uses [`gradatum_warden::WardenLayer`] identical to production.
///
/// # Loopback bypass
///
/// The warden calls `inner.call(req)` directly for loopback IPs
/// (when `bypass_loopback=true`) — the handler returns its real body.
/// No synthetic `Body::empty()` response is produced.
///
/// # Usage in tests
///
/// Inject `ConnectInfo<SocketAddr>` into request extensions via
/// `req.extensions_mut().insert(ConnectInfo(addr))` before `tower::ServiceExt::oneshot`.
///
/// # Notes
///
/// - If `rl.enabled == false`: no rate limiting, `/ping` always responds 200.
/// - If `rl.exempt_localhost == false`: loopback connections are rate-limited.
/// - To test throttling from a local test, use `exempt_localhost=false`.
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
