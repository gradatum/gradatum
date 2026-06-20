//! Tests E2E W1 rate limiting (gradatum-warden).
//!
//! Vérifie trois scénarios clés :
//! 1. `burst_within_limit_ok_with_real_body` — burst requêtes depuis IP non-loopback → tous 200 + body "pong" réel.
//! 2. `burst_exceeded_returns_429_with_retry_after` — (burst+1)e requête → 429 + header `retry-after`.
//! 3. `localhost_exempt_returns_real_handler_body` — 50 GET loopback avec `bypass_loopback=true`
//!    → tous 200 + body "pong" réel (test critique : prouve que warden n'écrase pas le body).
//!
//! # Stratégie d'injection ConnectInfo
//!
//! `ConnectInfo<SocketAddr>` est injecté manuellement dans les extensions de chaque
//! requête via `req.extensions_mut().insert(ConnectInfo(peer))` avant l'appel
//! `tower::ServiceExt::oneshot`. [`WardenService`] lit cette extension directement.
//!
//! Cette approche évite de démarrer un vrai serveur TCP.
//!
//! # IP utilisées
//!
//! - Non-loopback : `192.0.2.1:12345` (TEST-NET-1, RFC 5737 — garantie non-routable).
//! - Loopback : `127.0.0.1:12345`.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use gradatum_server::config::RateLimitConfig;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ── Constantes IP de test ─────────────────────────────────────────────────────

/// IP non-loopback pour simuler une connexion externe.
/// RFC 5737 TEST-NET-1 : garantie non-routable, jamais assignée publiquement.
const EXTERNAL_IP: &str = "192.0.2.1:12345";

/// IP loopback pour les tests de bypass loopback.
const LOOPBACK_IP: &str = "127.0.0.1:12345";

// ── Helper ────────────────────────────────────────────────────────────────────

/// Construit une requête GET /ping avec `ConnectInfo` injecté dans les extensions.
fn ping_req(peer: &str) -> Request<Body> {
    let addr: SocketAddr = peer
        .parse()
        .unwrap_or_else(|e| panic!("parse SocketAddr '{}' : {}", peer, e));
    let mut req = Request::builder()
        .uri("/ping")
        .method("GET")
        .body(Body::empty())
        .expect("construire requête GET /ping — ne peut pas échouer");
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Burst requêtes depuis IP non-loopback → tous 200 OK + body "pong" réel non-vide.
///
/// Vérifie :
/// - StatusCode::OK pour chaque requête dans le burst.
/// - Body non-vide (bytes.len() > 0) — warden ne remplace pas le body handler.
/// - Body parseable comme UTF-8 (cohérence basic).
#[tokio::test]
async fn burst_within_limit_ok_with_real_body() {
    let rl = RateLimitConfig {
        enabled: true,
        per_minute: 600,
        burst: 10,
        exempt_localhost: false,
    };
    let app = gradatum_server::build_rate_limit_test_app(&rl);

    for i in 1u32..=10 {
        let resp = app
            .clone()
            .oneshot(ping_req(EXTERNAL_IP))
            .await
            .unwrap_or_else(|e| panic!("requête {i}/10 GET /ping échouée : {e}"));

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "requête {i}/10 : attendu 200 OK (dans le burst), obtenu {}",
            resp.status()
        );

        // Vérifier que le body est le "pong" réel du handler — pas Body::empty().
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("collect body requête {i}/10 : {e}"))
            .to_bytes();
        assert!(
            !bytes.is_empty(),
            "requête {i}/10 : body ne doit pas être vide"
        );
        let body_str = std::str::from_utf8(&bytes)
            .unwrap_or_else(|e| panic!("body requête {i}/10 non-UTF-8 : {e}"));
        assert_eq!(
            body_str, "pong",
            "requête {i}/10 : body attendu 'pong', obtenu '{body_str}'"
        );
    }
}

/// (burst+1)e GET depuis IP non-loopback après avoir brûlé le burst → 429 + `retry-after`.
#[tokio::test]
async fn burst_exceeded_returns_429_with_retry_after() {
    let rl = RateLimitConfig {
        enabled: true,
        per_minute: 600,
        burst: 10,
        exempt_localhost: false,
    };
    let app = gradatum_server::build_rate_limit_test_app(&rl);

    // Brûler le burst complet (10 requêtes autorisées).
    for i in 1u32..=10 {
        let resp = app
            .clone()
            .oneshot(ping_req(EXTERNAL_IP))
            .await
            .unwrap_or_else(|e| panic!("requête de chauffe {i}/10 échouée : {e}"));
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "requête de chauffe {i}/10 : attendu 200, obtenu {}",
            resp.status()
        );
    }

    // 11e requête : doit être bloquée.
    let resp_11 = app
        .clone()
        .oneshot(ping_req(EXTERNAL_IP))
        .await
        .expect("11e requête GET /ping — oneshot doit retourner un Result");

    assert_eq!(
        resp_11.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "11e requête : attendu 429 Too Many Requests, obtenu {}",
        resp_11.status()
    );

    assert!(
        resp_11.headers().contains_key("retry-after"),
        "réponse 429 doit contenir le header 'retry-after', headers : {:?}",
        resp_11.headers()
    );
}

/// 50 GET loopback avec `bypass_loopback=true` et burst=1 → tous 200 + body "pong" réel.
///
/// Test critique : prouve que le warden appelle le handler inner pour les IPs loopback
/// et retourne son body réel (pas `Body::empty()` synthétique de l'ancien error_handler).
///
/// Avec burst=1, sans bypass, la 2e requête retournerait 429.
/// Le test valide que les 50 requêtes passent sans throttle grâce au bypass loopback,
/// ET que le body retourné est celui du handler.
#[tokio::test]
async fn localhost_exempt_returns_real_handler_body() {
    let rl = RateLimitConfig {
        enabled: true,
        per_minute: 60,
        burst: 1, // Burst volontairement minimal : sans bypass, la 2e requête serait 429.
        exempt_localhost: true,
    };
    let app = gradatum_server::build_rate_limit_test_app(&rl);

    for i in 1u32..=50 {
        let resp = app
            .clone()
            .oneshot(ping_req(LOOPBACK_IP))
            .await
            .unwrap_or_else(|e| panic!("requête loopback {i}/50 échouée : {e}"));

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "requête loopback {i}/50 : attendu 200 OK (bypass), obtenu {}",
            resp.status()
        );

        // Vérifier body réel (pas Body::empty() de l'ancien error_handler tower_governor).
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("collect body loopback {i}/50 : {e}"))
            .to_bytes();
        assert!(
            !bytes.is_empty(),
            "requête loopback {i}/50 : body ne doit pas être vide (bypass loopback doit appeler le handler réel)"
        );
        let body_str = std::str::from_utf8(&bytes)
            .unwrap_or_else(|e| panic!("body loopback {i}/50 non-UTF-8 : {e}"));
        assert_eq!(
            body_str, "pong",
            "requête loopback {i}/50 : body attendu 'pong', obtenu '{body_str}'"
        );
    }
}
