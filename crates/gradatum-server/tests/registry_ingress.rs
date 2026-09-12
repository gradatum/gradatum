//! Integration tests — fleet heartbeat ingress (F-156 P6b).
//!
//! Pattern: drive `gradatum_server::registry_ingress::build_ingress_router` directly via
//! `tower::ServiceExt::oneshot` (no TCP bind), injecting a `ConnectInfo` extension (required
//! by the rate-limit layer), exactly like `tests/internal_api.rs`.
//!
//! Covered:
//! - a valid heartbeat → 204 + the member appears in the store + accepted metric;
//! - 401 without a token and with the wrong token (wrong scope);
//! - the six allowlisted service names are accepted exactly;
//! - an unknown service → 400 + `unknown_service` metric;
//! - an over-long field → 400 + `field_too_long` metric;
//! - a body over the 8 KiB cap → 413 + `oversized_body` metric;
//! - a flood → some 429 + `rate_limited` metric.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_server::metrics::HeartbeatRejectLabel;
use gradatum_server::registry_ingress::{HEARTBEAT_ROUTE, build_ingress_router};
use gradatum_server::state::AppState;
use secrecy::SecretString;
use serde_json::json;
use tower::ServiceExt;

const TOKEN: &str = "heartbeat-token-abcdefghijklmnop-0123456789";
const WRONG_TOKEN: &str = "internal-admin-token-scope-mismatch-0000000";

/// Builds a fresh state + router (each test gets its own rate-limit bucket).
fn env() -> (AppState, axum::Router) {
    let state = AppState::new().with_heartbeat_ingress_token(SecretString::from(TOKEN.to_owned()));
    let router = build_ingress_router(state.clone());
    (state, router)
}

/// A heartbeat POST with a `ConnectInfo` extension (loopback) and the given token/body.
fn heartbeat_req(token: Option<&str>, body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(HEARTBEAT_ROUTE)
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("X-Gradatum-Heartbeat", format!("Bearer {t}"));
    }
    builder
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            54321,
        )))
        .body(body)
        .expect("building heartbeat request")
}

fn json_body(service: &str) -> Body {
    Body::from(json!({ "service": service, "version": "2.3.0", "pid": 4242 }).to_string())
}

fn rejected(state: &AppState, reason: &'static str) -> u64 {
    state
        .metrics
        .registry_heartbeat_rejected
        .get_or_create(&HeartbeatRejectLabel { reason })
        .get()
}

#[tokio::test]
async fn valid_heartbeat_registers_and_is_counted() {
    let (state, router) = env();
    let resp = router
        .oneshot(heartbeat_req(Some(TOKEN), json_body("worker")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let report = state.registry.report();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].service, "worker");
    assert_eq!(report.entries[0].pid, 4242);
    assert_eq!(state.metrics.registry_heartbeat_accepted.get(), 1);
}

#[tokio::test]
async fn missing_token_is_401_and_counted() {
    let (state, router) = env();
    let resp = router
        .oneshot(heartbeat_req(None, json_body("worker")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(state.registry.report().entries.is_empty());
    assert_eq!(rejected(&state, "unauthorized"), 1);
}

#[tokio::test]
async fn wrong_scope_token_is_401_and_counted() {
    // A token that is valid elsewhere (internal/admin) but not the heartbeat secret.
    let (state, router) = env();
    let resp = router
        .oneshot(heartbeat_req(Some(WRONG_TOKEN), json_body("worker")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(state.registry.report().entries.is_empty());
    assert_eq!(rejected(&state, "unauthorized"), 1);
}

#[tokio::test]
async fn fail_closed_when_no_token_configured() {
    // State without a heartbeat token ⇒ every request rejected, even with a bearer.
    let state = AppState::new();
    let router = build_ingress_router(state.clone());
    let resp = router
        .oneshot(heartbeat_req(Some(TOKEN), json_body("worker")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(rejected(&state, "unauthorized"), 1);
}

#[tokio::test]
async fn all_six_allowlisted_services_are_accepted() {
    for service in [
        "server",
        "worker",
        "gateway",
        "engine-agent-main",
        "engine-curator",
        "engine-embed",
    ] {
        let (_state, router) = env();
        let resp = router
            .oneshot(heartbeat_req(Some(TOKEN), json_body(service)))
            .await
            .expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::NO_CONTENT,
            "service {service} must be accepted exactly"
        );
    }
}

#[tokio::test]
async fn unknown_service_is_400_and_counted() {
    let (state, router) = env();
    let resp = router
        .oneshot(heartbeat_req(Some(TOKEN), json_body("rogue-daemon")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(state.registry.report().entries.is_empty());
    assert_eq!(rejected(&state, "unknown_service"), 1);
}

#[tokio::test]
async fn oversized_field_is_400_and_counted() {
    let (state, router) = env();
    // version far over MAX_FIELD_LEN (128) but the whole body stays under the 8 KiB body cap.
    let long_version = "v".repeat(300);
    let body =
        Body::from(json!({ "service": "worker", "version": long_version, "pid": 1 }).to_string());
    let resp = router
        .oneshot(heartbeat_req(Some(TOKEN), body))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(rejected(&state, "field_too_long"), 1);
}

#[tokio::test]
async fn oversized_body_is_413_and_counted() {
    let (state, router) = env();
    // A body well over the 8 KiB cap (a valid-looking but huge build_sha).
    let huge = "x".repeat(16 * 1024);
    let body = Body::from(
        json!({ "service": "worker", "version": "2.3.0", "build_sha": huge, "pid": 1 }).to_string(),
    );
    let resp = router
        .oneshot(heartbeat_req(Some(TOKEN), body))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(rejected(&state, "oversized_body"), 1);
}

#[tokio::test]
async fn flood_is_rate_limited_and_counted() {
    let (state, router) = env();
    // Burst is 20; a tight loop of 40 valid requests from one IP must trip the limiter.
    let mut too_many = 0u32;
    for _ in 0..40 {
        let resp = router
            .clone()
            .oneshot(heartbeat_req(Some(TOKEN), json_body("worker")))
            .await
            .expect("oneshot");
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            too_many += 1;
        }
    }
    assert!(too_many > 0, "a 40-request burst must produce some 429s");
    assert_eq!(
        rejected(&state, "rate_limited"),
        u64::from(too_many),
        "every 429 must be counted rate_limited"
    );
}
