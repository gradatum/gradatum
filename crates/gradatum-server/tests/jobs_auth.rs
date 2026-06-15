//! Tests authz F-16 — auth obligatoire sur les endpoints `/api/v1/jobs`.
//!
//! # Contexte (F-16.1)
//!
//! Avant ce fix, les 5 handlers `jobs_v2.rs` recevaient `Extension<TrustContext>`
//! mais ne le consommaient jamais : aucun `is_authenticated()`, aucun check ACL.
//! `create_job` étant devenu un endpoint **destructeur** depuis F-16.1 (Purge
//! réelle `dry_run:false`, injection Curate), c'était un trou authz (atténué par
//! le bind loopback, mais l'authz applicative doit être explicite et cohérente
//! avec le reste de `api_v1`).
//!
//! L'ancien module `jobs_auth.rs` (flag fantôme `require_jwt_jobs_endpoint`)
//! avait été vidé — ce qui laissait précisément ces endpoints sans aucune
//! couverture de test d'auth. Ce module reconstruit la couverture réelle.
//!
//! # Matrice testée
//!
//! | Endpoint | Op ACL | sans bearer | bearer valide + ACL OK | bearer valide + ACL deny |
//! |----------|--------|-------------|------------------------|--------------------------|
//! | POST `/jobs`         | Write | **401** | 202 | 403 |
//! | GET  `/jobs`         | Read  | **401** | 200 | — |
//! | GET  `/jobs/:id/v2`  | Read  | **401** | (200/404) | — |
//! | POST `/jobs/:id/cancel` | Write | **401** | — | — |
//! | GET  `/jobs/:id/events` | Read  | **401** | — | — |
//! | GET  `/jobs/:id` (legacy i64) | Read | **401** | (404 si absent) | 403 |
//!
//! Le scénario nominal (202/200 avec auth) est couvert en détail dans
//! `jobs_api_integration.rs` ; ici on cible la **fermeture du trou** : 401/403.
//!
//! # Fix C1 F-16 — route legacy `GET /api/v1/jobs/{id}` (i64)
//!
//! La dernière route jobs non-sécurisée : elle recevait `trust` mais l'ignorait
//! (`let _ = &trust;`), exposant le statut d'un job à quiconque connaissait un
//! `i64` AUTOINCREMENT (devinable). C'est le `poll_url` de `vault_downgrade` —
//! sécurisée (pas supprimée). Tests legacy ci-dessous.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{middleware, Router};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::TokenScope;
use gradatum_db_sqlite::{apply_sqlite_pragmas, run_migrations, SqliteQueueStore};
use gradatum_server::{api_v1, middleware::auth_middleware, state::AppState};
use sqlx::SqlitePool;
use std::sync::Arc;
use tower::ServiceExt;
use ulid::Ulid;

/// Identité du consumer de test — doit matcher l'`identity` du preset ACL.
const TEST_IDENTITY: &str = "jobs-auth-tester";

/// Preset ACL autorisant read+write sur `main/*` (donc `main/jobs`).
const ACL_ALLOW: &str = r#"
[[consumer]]
identity = "jobs-auth-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// Preset ACL où le consumer existe mais N'A AUCUN droit (read ni write).
/// Un token signé pour cette identité est authentifié mais l'ACL refuse → 403.
const ACL_DENY: &str = r#"
[[consumer]]
identity = "jobs-auth-tester"
read_patterns  = []
write_patterns = []
"#;

async fn make_test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory invariant");
    apply_sqlite_pragmas(&pool)
        .await
        .expect("pragmas WAL invariant");
    run_migrations(&pool).await.expect("migrations invariant");
    pool
}

/// Construit `(state, token)` avec store SQLite in-memory + preset ACL fourni.
async fn build_state(acl_preset: &str) -> (AppState, String) {
    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));
    let mut state = AppState::new().with_job_store(store, pool);
    let acl = AclEngine::from_preset_str(acl_preset).expect("preset ACL valide");
    state.acl = Arc::new(acl);
    let token = state
        .jwt
        .sign(
            TEST_IDENTITY,
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test");
    (state, token)
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Body POST `/jobs` valide (Curate sur un note_id réel) — déclencherait un 202
/// si l'auth passe. Sert à prouver qu'un non-authentifié est arrêté AVANT.
fn valid_create_body() -> serde_json::Value {
    serde_json::json!({
        "spec": { "kind": { "type": "Curate", "data": { "note_id": Ulid::new().to_string() } } }
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Cœur du fix — sans auth → 401 (et NON 202/400). Doit échouer AVANT le patch.
// ─────────────────────────────────────────────────────────────────────────────

/// (a) POST `/jobs` SANS bearer → 401 (pas 202, pas 400).
///
/// Test pivot : avant le fix, ce POST (body valide + Idempotency-Key) renvoyait
/// 202 et enqueuait réellement un job destructeur sans aucune authentification.
#[tokio::test]
async fn create_job_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Content-Type", "application/json")
        .header("Idempotency-Key", "auth-401-no-bearer")
        .body(Body::from(
            serde_json::to_vec(&valid_create_body()).unwrap(),
        ))
        .expect("build POST sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /jobs sans bearer DOIT être 401 (trou authz F-16 fermé)"
    );
}

/// (a') POST `/jobs` avec bearer GARBAGE (JWT invalide) → 401.
#[tokio::test]
async fn create_job_with_invalid_jwt_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Content-Type", "application/json")
        .header("Idempotency-Key", "auth-401-bad-jwt")
        .header("authorization", "Bearer not.a.valid.jwt")
        .body(Body::from(
            serde_json::to_vec(&valid_create_body()).unwrap(),
        ))
        .expect("build POST bad jwt");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /jobs avec JWT invalide → 401"
    );
}

/// (b) POST `/jobs` avec bearer valide + ACL OK → 202 (comportement F-16.1 préservé).
#[tokio::test]
async fn create_job_with_auth_is_202() {
    let (state, token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Content-Type", "application/json")
        .header("Idempotency-Key", "auth-202-ok")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&valid_create_body()).unwrap(),
        ))
        .expect("build POST authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "POST /jobs authentifié + ACL Write → 202 (F-16.1 préservé)"
    );
}

/// (b') POST `/jobs` avec bearer valide mais ACL Write refusée → 403.
#[tokio::test]
async fn create_job_with_auth_but_acl_deny_is_403() {
    let (state, token) = build_state(ACL_DENY).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Content-Type", "application/json")
        .header("Idempotency-Key", "auth-403-deny")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(
            serde_json::to_vec(&valid_create_body()).unwrap(),
        ))
        .expect("build POST acl-deny");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "POST /jobs authentifié mais ACL Write refusée → 403"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Le même trou existait sur les 4 autres handlers — couverture 401 cohérente.
// ─────────────────────────────────────────────────────────────────────────────

/// GET `/jobs` (list) SANS bearer → 401 (lecture aussi protégée).
#[tokio::test]
async fn list_jobs_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs")
        .body(Body::empty())
        .expect("build GET list sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /jobs sans bearer → 401"
    );
}

/// GET `/jobs` avec bearer valide + ACL Read → 200 (preuve que le check Read
/// ne bloque pas un consommateur légitime).
#[tokio::test]
async fn list_jobs_with_auth_is_200() {
    let (state, token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build GET list authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /jobs authentifié + ACL Read → 200"
    );
}

/// GET `/jobs/:id/v2` (detail) SANS bearer → 401 (avant même la résolution ULID).
#[tokio::test]
async fn get_job_detail_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{}/v2", Ulid::new()))
        .body(Body::empty())
        .expect("build GET detail sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /jobs/:id/v2 sans bearer → 401 (avant 404)"
    );
}

/// POST `/jobs/:id/cancel` SANS bearer → 401 (write protégé).
#[tokio::test]
async fn cancel_job_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/jobs/{}/cancel", Ulid::new()))
        .body(Body::empty())
        .expect("build POST cancel sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /jobs/:id/cancel sans bearer → 401 (avant 404)"
    );
}

/// GET `/jobs/:id/events` (SSE) SANS bearer → 401.
#[tokio::test]
async fn job_events_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{}/events", Ulid::new()))
        .body(Body::empty())
        .expect("build GET events sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /jobs/:id/events sans bearer → 401 (avant 404)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Fix C1 F-16 — route legacy `GET /api/v1/jobs/{id}` (i64, poll_url downgrade).
//
// La route legacy lit `state.queue` (queue legacy), pas `state.job_store`.
// Le harness ici n'injecte pas de queue réelle → `PlaceholderQueue::get` renvoie
// `Ok(None)` → 404 quand l'auth passe. Cela suffit pour discriminer 401/403/404 :
// - sans bearer → 401 AVANT lecture queue (trou C1 fermé) ;
// - bearer valide + ACL deny → 403 ;
// - bearer valide + ACL ok → 404 (preuve que l'auth ne bloque pas un légitime).
// ─────────────────────────────────────────────────────────────────────────────

/// GET `/jobs/{id}` legacy SANS bearer → 401 (fix C1 — avant toute lecture queue).
#[tokio::test]
async fn legacy_get_job_without_auth_is_401() {
    let (state, _token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/42")
        .body(Body::empty())
        .expect("build GET legacy sans bearer");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /jobs/{{id}} legacy sans bearer → 401 (trou C1 F-16 fermé)"
    );
}

/// GET `/jobs/{id}` legacy avec bearer valide mais ACL Read refusée → 403.
#[tokio::test]
async fn legacy_get_job_with_auth_but_acl_deny_is_403() {
    let (state, token) = build_state(ACL_DENY).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/42")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build GET legacy acl-deny");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "GET /jobs/{{id}} legacy authentifié mais ACL Read refusée → 403"
    );
}

/// GET `/jobs/{id}` legacy avec bearer valide + ACL Read OK → 404 (job absent).
///
/// Preuve que l'auth ne bloque pas un consommateur légitime : la requête traverse
/// l'authz et atteint la lecture queue (`PlaceholderQueue` → None → 404). Le
/// comportement nominal 200 est couvert par `poll_job_real::poll_pending_returns_pending`.
#[tokio::test]
async fn legacy_get_job_with_auth_ok_reaches_queue_404() {
    let (state, token) = build_state(ACL_ALLOW).await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/42")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("build GET legacy authed");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "GET /jobs/{{id}} legacy authentifié + ACL Read → atteint la queue → 404 (job absent)"
    );
}
