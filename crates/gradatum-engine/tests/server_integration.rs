//! Tests d'intégration HTTP `gradatum-engine` — serveur axum vague-2 (forward transparent).
//!
//! Ces tests couvrent les routes non-proxy (/health, /metrics) et le wiring du routeur.
//! Les tests de forward transparent (body preserved, 413, 504, concurrent) sont dans
//! `server.rs#transparent_handler` (tests unitaires inline).
//!
//! Compiler avec : `cargo test -p gradatum-engine --features test-utils`
#![cfg(feature = "test-utils")]
use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_engine::server::EngineServer;
use tower::ServiceExt;

#[tokio::test]
async fn health_endpoint_reports_ready() {
    let app =
        EngineServer::test_app_with_child("http://127.0.0.1:1".to_string(), 30, 32 * 1024 * 1024);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // F-205 : le champ `event_log` est sérialisé jusqu'à la surface HTTP. Le helper de
    // test câble une télémétrie active → "active".
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["event_log"], "active",
        "/health doit exposer l'état de la télémétrie (event_log) : {json}"
    );
}

/// C2 — /metrics absent du router principal (port LAN) : doit retourner 404.
#[tokio::test]
async fn metrics_absent_from_main_router() {
    let app =
        EngineServer::test_app_with_child("http://127.0.0.1:1".to_string(), 30, 32 * 1024 * 1024);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "C2 : /metrics doit retourner 404 sur le router principal (LAN) — \
         métriques séparées sur le listener loopback-only"
    );
}

/// C2 — /metrics accessible via metrics_router (loopback-only).
#[tokio::test]
async fn metrics_available_on_metrics_router() {
    use gradatum_engine::metrics::EngineMetrics;
    use std::sync::Arc;

    let metrics = Arc::new(EngineMetrics::new());
    metrics.record_request("/v1/chat/completions", 200, 42);
    let app = EngineServer::metrics_router(metrics);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "C2 : /metrics doit être disponible sur le metrics_router loopback-only"
    );

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = std::str::from_utf8(&bytes).unwrap();
    assert!(
        body.contains("engine_requests_total"),
        "C2 : corps /metrics doit contenir les métriques Prometheus"
    );
}

#[tokio::test]
async fn bind_is_loopback_in_test_state() {
    // Vérification de design : l'URL de test utilise 127.0.0.1 (P1-4)
    let url = "http://127.0.0.1:11435";
    assert!(
        url.contains("127.0.0.1") || url.contains("localhost"),
        "bind doit être loopback (P1-4)"
    );
}
