//! Tests d'intégration — GET /health (T10).
//!
//! Vérifie que le handler retourne un payload JSON complet avec les 10 champs
//! sans authentification (RFC-0003 §8 — endpoint unauthenticated).
//!
//! # Pattern de test
//!
//! Un serveur Axum est démarré sur un port éphémère avec `AppState::default()`.
//! `/health` est monté directement, sans middleware auth — identique à `build_router`.

use std::net::SocketAddr;
use std::time::Duration;

use gradatum_server::state::AppState;
use reqwest::StatusCode;
use serde_json::Value;

// ── Helper ────────────────────────────────────────────────────────────────────

/// Démarre un serveur de test minimaliste avec uniquement `/health`.
///
/// Reproduit le montage de `build_router` : `/health` hors middleware auth.
async fn start_health_server() -> SocketAddr {
    use axum::{middleware, routing::get, Router};
    use gradatum_server::{api_v1, health};

    async fn trust_stub(
        mut req: axum::http::Request<axum::body::Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        use gradatum_core::trust::TrustContext;
        req.extensions_mut().insert(TrustContext::Unauthenticated);
        next.run(req).await
    }

    let state = AppState::default();
    let app = Router::new()
        // /health monté avant le layer middleware — pas d'auth requise.
        .route("/health", get(health::handler))
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port éphémère — doit réussir sur localhost");
    let addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale — listener actif");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serveur de test health arrêté proprement");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    addr
}

/// Client reqwest sans retry, timeout 5s.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("construction client HTTP — pas de TLS custom")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// GET /health — 200 OK sans authentification.
#[tokio::test]
async fn health_no_auth_required() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/health doit retourner 200 sans bearer (unauthenticated RFC-0003 §8)"
    );
}

/// GET /health — payload JSON avec les 10 champs D1 présents et typés correctement.
#[tokio::test]
async fn health_returns_full_payload() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    assert_eq!(resp.status(), StatusCode::OK, "/health doit retourner 200");

    let body: Value = resp.json().await.expect("corps JSON valide depuis /health");

    // ── Champ 1 : status ──────────────────────────────────────────────────────
    let status = body
        .get("status")
        .expect("champ 'status' présent dans /health");
    assert!(
        status.is_string(),
        "'status' doit être une string, obtenu : {status}"
    );
    let status_str = status.as_str().unwrap();
    assert!(
        status_str == "ok" || status_str == "degraded",
        "'status' doit valoir \"ok\" ou \"degraded\", obtenu : \"{status_str}\""
    );

    // ── Champ 2 : version ─────────────────────────────────────────────────────
    let version = body
        .get("version")
        .expect("champ 'version' présent dans /health");
    assert!(version.is_string(), "'version' doit être une string");
    assert!(
        !version.as_str().unwrap().is_empty(),
        "'version' ne doit pas être vide"
    );

    // ── Champ 3 : build_sha ───────────────────────────────────────────────────
    let build_sha = body
        .get("build_sha")
        .expect("champ 'build_sha' présent dans /health");
    assert!(build_sha.is_string(), "'build_sha' doit être une string");
    assert!(
        !build_sha.as_str().unwrap().is_empty(),
        "'build_sha' ne doit pas être vide"
    );

    // ── Champ 4 : uptime_secs ─────────────────────────────────────────────────
    let uptime = body
        .get("uptime_secs")
        .expect("champ 'uptime_secs' présent dans /health");
    assert!(
        uptime.is_u64() || uptime.is_number(),
        "'uptime_secs' doit être un nombre entier non-négatif, obtenu : {uptime}"
    );
    // Doit être >= 0 (u64 implicitement). Borne haute raisonnable : < 3600s pour un test.
    let uptime_val = uptime.as_u64().expect("'uptime_secs' convertible en u64");
    assert!(
        uptime_val < 3600,
        "'uptime_secs' trop grand pour un test (>= 3600s) : {uptime_val}"
    );

    // ── Champ 5 : tenant_count ────────────────────────────────────────────────
    let tenant_count = body
        .get("tenant_count")
        .expect("champ 'tenant_count' présent dans /health");
    assert!(
        tenant_count.is_u64() || tenant_count.is_number(),
        "'tenant_count' doit être un entier non-négatif"
    );
    // Stub T10 : 0 attendu.
    assert_eq!(
        tenant_count.as_u64().unwrap_or(u64::MAX),
        0,
        "'tenant_count' doit être 0 (stub T10)"
    );

    // ── Champ 6 : locus_count ─────────────────────────────────────────────────
    let locus_count = body
        .get("locus_count")
        .expect("champ 'locus_count' présent dans /health");
    assert!(
        locus_count.is_u64() || locus_count.is_number(),
        "'locus_count' doit être un entier non-négatif"
    );
    // Stub T10 : 0 attendu.
    assert_eq!(
        locus_count.as_u64().unwrap_or(u64::MAX),
        0,
        "'locus_count' doit être 0 (stub T10)"
    );

    // ── Champ 7 : queue_depth ─────────────────────────────────────────────────
    let queue_depth = body
        .get("queue_depth")
        .expect("champ 'queue_depth' présent dans /health");
    assert!(
        queue_depth.is_u64() || queue_depth.is_number(),
        "'queue_depth' doit être un entier non-négatif"
    );

    // ── Champ 8 : queue_oldest_age_secs ──────────────────────────────────────
    let queue_oldest = body
        .get("queue_oldest_age_secs")
        .expect("champ 'queue_oldest_age_secs' présent dans /health");
    assert!(
        queue_oldest.is_u64() || queue_oldest.is_number(),
        "'queue_oldest_age_secs' doit être un entier non-négatif"
    );

    // ── Champ 9 : sqlite_wal_size_bytes ───────────────────────────────────────
    let wal_size = body
        .get("sqlite_wal_size_bytes")
        .expect("champ 'sqlite_wal_size_bytes' présent dans /health");
    assert!(
        wal_size.is_u64() || wal_size.is_number(),
        "'sqlite_wal_size_bytes' doit être un entier non-négatif"
    );

    // ── Champ 10 : started_at ─────────────────────────────────────────────────
    let started_at = body
        .get("started_at")
        .expect("champ 'started_at' présent dans /health");
    assert!(started_at.is_string(), "'started_at' doit être une string");
    let started_at_str = started_at.as_str().unwrap();
    // Validation format RFC3339 minimal : doit contenir 'T' et '+' ou 'Z'.
    assert!(
        started_at_str.contains('T'),
        "'started_at' ne ressemble pas à du RFC3339 (pas de 'T') : \"{started_at_str}\""
    );
    assert!(
        started_at_str.contains('+') || started_at_str.ends_with('Z'),
        "'started_at' ne ressemble pas à du RFC3339 (pas de timezone) : \"{started_at_str}\""
    );
}

/// GET /health — status "ok" quand les stubs retournent 0 (queue vide).
#[tokio::test]
async fn health_status_ok_with_stub_zeros() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let body: Value = resp.json().await.expect("corps JSON valide");
    let status = body["status"].as_str().expect("'status' string");
    assert_eq!(
        status, "ok",
        "status doit être \"ok\" quand queue_depth=0 et queue_oldest_age_secs=0"
    );
}

/// GET /health — Content-Type application/json.
#[tokio::test]
async fn health_content_type_json() {
    let addr = start_health_server().await;
    let resp = client()
        .get(format!("http://{}/health", addr))
        .send()
        .await
        .expect("requête GET /health");

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("Content-Type header présent")
        .to_str()
        .expect("Content-Type valide UTF-8");
    assert!(
        content_type.contains("application/json"),
        "Content-Type doit contenir 'application/json', obtenu : \"{content_type}\""
    );
}
