//! gradatum-server library API for integration tests.
//!
//! Exposes `api_v1` and `stubs` for integration tests.
//! Exposes `middleware` for tests that verify JWT extraction.
//! Exposes `health` for the `health.rs` integration test.
//! Exposes `audit_jsonl` for `JsonlFileSink`.
//! Exposes `auth_routes` for the `/auth/exchange` handler.
//! Exposes `build_rate_limit_test_app` for rate-limit E2E tests.
//!
//! These modules are internal service plumbing, exposed only for this crate's
//! own integration tests. They are hidden from the rendered documentation and
//! are **not** a stable public API (this crate is a service binary, not a
//! reusable library).
#[doc(hidden)]
pub mod api_v1;
#[doc(hidden)]
pub mod audit_job;
#[doc(hidden)]
pub mod audit_jsonl;
#[doc(hidden)]
pub mod auth_routes;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod context;
#[doc(hidden)]
pub mod curated_metrics;
#[doc(hidden)]
pub mod event_log_store;
#[doc(hidden)]
pub mod health;
#[doc(hidden)]
pub mod internal;
#[doc(hidden)]
pub mod mcp_usage;
#[doc(hidden)]
pub mod metrics;
#[doc(hidden)]
pub mod metrics_proxy;
#[doc(hidden)]
pub mod middleware;
#[doc(hidden)]
pub mod note_usage_store;
#[doc(hidden)]
pub mod privacy_observer;
#[doc(hidden)]
pub mod privacy_rescan;
#[doc(hidden)]
pub mod proactive_recall;
#[doc(hidden)]
pub mod proactive_recall_store;
#[doc(hidden)]
pub mod proactive_surface_store;
#[doc(hidden)]
pub mod read_usage_store;
#[doc(hidden)]
pub mod registry_ingress;
pub mod registry_store;
#[doc(hidden)]
pub mod review_promote;
#[doc(hidden)]
pub mod scheduled_tasks;
#[doc(hidden)]
pub mod session_trace_store;
#[doc(hidden)]
pub mod state;
#[doc(hidden)]
pub mod stubs;
#[doc(hidden)]
pub mod studio;
#[doc(hidden)]
pub mod telemetry_flush;

/// The fake sidecar the personal-data mount tests drive.
///
/// Included, not copied: the mount tests and the privacy crate's client tests speak the
/// same protocol, and a second copy of the wire would be a second description of it.
///
/// Declared from the crate root because `#[path]` is resolved against the directory of
/// the file it is written in, and the mount tests live inside an inline module of
/// `state.rs`, whose directory (`src/state/`) exists in the module tree and not on disk —
/// any relative path written there fails at the first traversal. The crate root is the
/// one place in this crate whose directory is real, and this is a test-only item: the
/// integration tests reach the same file from `tests/`, where the base is real too.
#[cfg(test)]
#[path = "../../gradatum-privacy/tests/sidecar_fake/mod.rs"]
pub(crate) mod sidecar_fake;

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
    use axum::{Router, routing::get};

    async fn ping_handler() -> &'static str {
        "pong"
    }

    let base = Router::new().route("/ping", get(ping_handler));

    match crate::middleware::build_warden_layer(rl) {
        Some(warden) => base.layer(warden),
        None => base,
    }
}
