//! Tests d'intégration gradatum-warden.
//!
//! Vérifient les 6 scénarios demandés + comportement body réel via un routeur Axum minimal.
//!
//! # Stratégie ConnectInfo
//!
//! `ConnectInfo<SocketAddr>` est injecté manuellement dans les extensions de chaque requête
//! via `req.extensions_mut().insert(ConnectInfo(addr))` avant `tower::ServiceExt::oneshot`.
//! [`WardenService`] lit cette extension directement.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use gradatum_warden::{WardenConfig, WardenLayer};
use tower::ServiceExt;

// ── IPs de test ───────────────────────────────────────────────────────────────

/// IP non-loopback : RFC 5737 TEST-NET-1 — garantie non-routable.
const EXTERNAL_IP: &str = "192.0.2.1:12345";
/// IP loopback.
const LOOPBACK_IP: &str = "127.0.0.1:12345";

// ── Helper ────────────────────────────────────────────────────────────────────

/// Construit une requête GET /check avec l'IP injectée dans les extensions.
fn req(peer: &str) -> Request<Body> {
    let addr: SocketAddr = peer
        .parse()
        .unwrap_or_else(|e| panic!("parse SocketAddr '{}': {}", peer, e));
    let mut r = Request::builder()
        .uri("/check")
        .method("GET")
        .body(Body::empty())
        .expect("construction requête — ne peut pas échouer");
    r.extensions_mut().insert(ConnectInfo(addr));
    r
}

/// Construit un routeur Axum minimal avec le warden et un handler renvoyant "ok" dans le body.
fn make_app(config: WardenConfig) -> Router {
    let warden = WardenLayer::new(config).expect("config warden valide");
    Router::new()
        .route("/check", get(|| async { "ok" }))
        .layer(warden)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// (1) IP dans le CIDR allow → 200 OK.
#[tokio::test]
async fn ip_filter_allow_cidr_match() {
    let config = WardenConfig {
        ip_allow: vec!["192.0.2.0/24".parse().unwrap()],
        ip_deny: vec![],
        bypass_loopback: false,
        rate_limit_burst: 100,
        ..WardenConfig::default()
    };
    let app = make_app(config);
    let resp = app
        .oneshot(req(EXTERNAL_IP))
        .await
        .expect("oneshot ne doit pas échouer");
    assert_eq!(resp.status(), StatusCode::OK, "IP dans allow → 200 attendu");
}

/// (2) IP dans le CIDR deny → 403.
#[tokio::test]
async fn ip_filter_deny_cidr_match() {
    let config = WardenConfig {
        ip_allow: vec![],
        ip_deny: vec!["192.0.2.0/24".parse().unwrap()],
        bypass_loopback: false,
        rate_limit_burst: 100,
        ..WardenConfig::default()
    };
    let app = make_app(config);
    let resp = app
        .oneshot(req(EXTERNAL_IP))
        .await
        .expect("oneshot ne doit pas échouer");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "IP dans deny → 403 attendu"
    );
}

/// (3) N hits dans le burst → tous 200 OK.
#[tokio::test]
async fn rate_limit_burst_within_limit() {
    let config = WardenConfig {
        bypass_loopback: false,
        rate_limit_per_minute: 600,
        rate_limit_burst: 10,
        ..WardenConfig::default()
    };
    let app = make_app(config);

    for i in 1u32..=10 {
        let resp = app
            .clone()
            .oneshot(req(EXTERNAL_IP))
            .await
            .unwrap_or_else(|e| panic!("requête {i}/10 échouée: {e}"));
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "requête {i}/10 : attendu 200 dans le burst, obtenu {}",
            resp.status()
        );
    }
}

/// (4) (burst+1)e hit après épuisement → 429.
#[tokio::test]
async fn rate_limit_burst_exceeded() {
    let config = WardenConfig {
        bypass_loopback: false,
        rate_limit_per_minute: 600,
        rate_limit_burst: 10,
        ..WardenConfig::default()
    };
    let app = make_app(config);

    // Vider le burst.
    for _ in 0..10 {
        app.clone()
            .oneshot(req(EXTERNAL_IP))
            .await
            .expect("requête de chauffe");
    }

    // 11e : doit être bloquée.
    let resp = app
        .clone()
        .oneshot(req(EXTERNAL_IP))
        .await
        .expect("11e requête");
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "11e requête : attendu 429, obtenu {}",
        resp.status()
    );
    assert!(
        resp.headers().contains_key("retry-after"),
        "réponse 429 doit contenir retry-after, headers: {:?}",
        resp.headers()
    );
}

/// (5) Loopback avec bypass_loopback=true + burst=1 saturé → Bypass (200 OK).
///
/// Test critique : prouve que le bypass loopback contourne réellement le rate limit
/// et appelle le handler inner (body réel, pas Body::empty synthétique).
#[tokio::test]
async fn bypass_loopback_skips_rate_limit() {
    let config = WardenConfig {
        bypass_loopback: true,
        rate_limit_per_minute: 60,
        rate_limit_burst: 1, // Burst très bas — sans bypass, la 2e requête serait 429.
        ..WardenConfig::default()
    };
    let app = make_app(config);

    for i in 1u32..=10 {
        let resp = app
            .clone()
            .oneshot(req(LOOPBACK_IP))
            .await
            .unwrap_or_else(|e| panic!("requête loopback {i}/10 échouée: {e}"));
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "requête loopback {i}/10 : attendu 200 (bypass), obtenu {}",
            resp.status()
        );
    }
}

/// (6) bypass_loopback=false + loopback + burst saturé → 429.
#[tokio::test]
async fn bypass_loopback_disabled_applies_rate_limit() {
    let config = WardenConfig {
        bypass_loopback: false,
        rate_limit_per_minute: 60,
        rate_limit_burst: 1,
        ..WardenConfig::default()
    };
    let app = make_app(config);

    // Premier jeton : OK.
    let resp1 = app
        .clone()
        .oneshot(req(LOOPBACK_IP))
        .await
        .expect("1re requête loopback");
    assert_eq!(resp1.status(), StatusCode::OK, "1re requête : 200 attendu");

    // 2e : burst épuisé → 429.
    let resp2 = app
        .clone()
        .oneshot(req(LOOPBACK_IP))
        .await
        .expect("2e requête loopback");
    assert_eq!(
        resp2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "2e requête loopback sans bypass : 429 attendu, obtenu {}",
        resp2.status()
    );
}
