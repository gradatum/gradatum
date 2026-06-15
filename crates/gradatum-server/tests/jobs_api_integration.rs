//! Tests d'intégration F-16 — API jobs.
//!
//! Couvre les 9 scénarios critiques :
//!
//! 1. `list_jobs_empty`          : GET /api/v1/jobs → 200 [] (store vide)
//! 2. `get_job_not_found`        : GET /api/v1/jobs/:id/v2 → 404 (ID inconnu)
//! 3. `get_job_bad_ulid`         : GET /api/v1/jobs/:id/v2 → 400 (ULID invalide)
//! 4. `create_job_no_idempotency_key` : POST /api/v1/jobs (sans header) → 501 (pool non câblé Bronze)
//! 5. `cancel_job_not_found`     : POST /api/v1/jobs/:id/cancel → 404
//! 6. `cancel_job_conflict_running` : POST /api/v1/jobs/:id/cancel (Running) → 409
//! 7. `e12_regression_get_after_dequeue` : enqueue → dequeue → GET v2 → status=Running (fix E-12)
//! 8. `list_jobs_cursor_pagination` : liste paginée (cursor-based)
//! 9. `list_jobs_filter_status`  : filtre par statut
//!
//! # F-16.1 — fix stub E-13 (désérialisation du JobKind réel)
//!
//! Avant : `create_job` forçait tout job → `Job::Curate(note_id aléatoire)` → DLQ.
//! Tests ajoutés (préfixe `create_job_*`) prouvant le fix :
//! - Curate honore le `note_id` client · Distill/Purge ne sont plus forcés en Curate
//! - ReIndex (handler stub) et kinds non routés → 400 · Curate sans note_id → 400
//! - Idempotency-Key rejoué → même job_id (régression idempotence préservée)
//!
//! # Architecture des tests
//!
//! - `AppState` construit avec `SqliteQueueStore` in-memory injecté via `with_job_store`.
//! - `NoopQueueStore` par défaut pour les tests ne nécessitant pas de store réel.
//! - Pattern `tower::ServiceExt::oneshot` (identique aux autres tests du workspace).
//!
//! # Références
//!
//! - F-16 — API jobs · F-16.1 — fix E-13
//! - fix E-12 §11

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

// ─────────────────────────────────────────────────────────────────────────────
// Auth de test (fix authz F-16)
//
// Les endpoints jobs exigent désormais un bearer JWT authentifié + ACL sur
// `main/jobs` (read pour list/detail/events, write pour create/cancel).
// Les helpers ci-dessous signent un token autorisé et l'injectent dans chaque
// requête — le scénario "sans auth → 401" est couvert dans `jobs_auth.rs`.
// ─────────────────────────────────────────────────────────────────────────────

/// Identité du consumer de test — doit matcher l'`identity` du preset ACL.
const TEST_IDENTITY: &str = "jobs-tester";

/// Preset ACL autorisant `jobs-tester` en read+write sur `main/*` (donc `main/jobs`).
const TEST_ACL: &str = r#"
[[consumer]]
identity = "jobs-tester"
read_patterns  = ["main/*"]
write_patterns = ["main/*"]
"#;

/// Remplace l'ACL deny-all par défaut (`AppState::new`) par le preset de test.
///
/// Le champ `acl` est `pub` — réassignation directe (même pattern que `state.search`
/// dans les tests dashboard). Le `JwtService` éphémère de `AppState::new` est conservé.
fn with_test_auth(mut state: AppState) -> AppState {
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL de test valide");
    state.acl = Arc::new(acl);
    state
}

/// Signe un bearer token autorisé pour le consumer de test.
fn test_token(state: &AppState) -> String {
    state
        .jwt
        .sign(
            TEST_IDENTITY,
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT de test")
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqlitePool` in-memory avec migrations 006+007+008 appliquées.
///
/// Distinct de `test_pool()` dans `queue_store_sqlite.rs` — ce helper est
/// public pour être partagé entre les tests d'intégration du crate server.
async fn make_test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("pool in-memory invariant");
    apply_sqlite_pragmas(&pool)
        .await
        .expect("pragmas WAL invariant");
    run_migrations(&pool)
        .await
        .expect("migrations 006+007+008 invariant");
    pool
}

/// Construit un `AppState` de test avec `SqliteQueueStore` in-memory + auth de test.
///
/// Injecte `job_store` et `jobs_pool` via `with_job_store`, puis remplace l'ACL
/// deny-all par le preset autorisant `jobs-tester` (fix authz F-16).
async fn build_state_with_job_store() -> AppState {
    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));
    with_test_auth(AppState::new().with_job_store(store, pool))
}

/// Construit un `AppState` de test avec un store déjà câblé + auth de test.
///
/// Variante de [`build_state_with_job_store`] pour les tests qui pré-remplissent
/// le store (enqueue/dequeue) avant de construire le state.
fn state_with_store(store: Arc<SqliteQueueStore>, pool: SqlitePool) -> AppState {
    with_test_auth(AppState::new().with_job_store(store, pool))
}

/// Construit le routeur de test avec `auth_middleware` actif.
///
/// `auth_middleware` est obligatoire — les handlers extraient `Extension<TrustContext>`.
///
/// Note : signer le token de test AVANT cet appel (`test_token(&state)`) car
/// `state` est consommé ici. Le bearer s'injecte dans chaque requête via
/// [`with_bearer`].
fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Ajoute l'en-tête `Authorization: Bearer <token>` à un `RequestBuilder`.
///
/// Centralise l'injection d'auth pour tous les tests (fix authz F-16) — évite
/// de répéter le format du header sur chaque requête.
fn with_bearer(builder: axum::http::request::Builder, token: &str) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {token}"))
}

/// Construit un `JobRecord` Pending minimal pour les tests.
fn make_test_job() -> gradatum_core::JobRecord {
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
        JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, RetryBackoff, TriggerSource,
    };

    let now = Utc::now();
    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            class: JobClass::Agent,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::default_for(&JobClass::Agent),
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// Test 1 — GET /api/v1/jobs → 200 liste vide.
///
/// Prouve que l'endpoint est accessible et retourne une liste vide
/// quand le store ne contient aucun job.
#[tokio::test]
async fn list_jobs_empty() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(Request::builder().method("GET").uri("/api/v1/jobs"), &token)
        .body(Body::empty())
        .expect("build GET /api/v1/jobs");

    let resp = router.oneshot(req).await.expect("service GET /api/v1/jobs");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /api/v1/jobs store vide → 200"
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON");
    assert_eq!(
        json["items"].as_array().expect("items est un array").len(),
        0,
        "liste vide → items = []"
    );
    assert!(json["next_cursor"].is_null(), "pas de cursor si liste vide");
}

/// Test 2 — GET /api/v1/jobs/:id/v2 → 404 (ULID inconnu).
#[tokio::test]
async fn get_job_not_found() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let unknown_id = Ulid::new();
    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/jobs/{}/v2", unknown_id)),
        &token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs/:id/v2");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "ULID inconnu → 404");
}

/// Test 3 — GET /api/v1/jobs/:id/v2 → 400 (ULID invalide).
#[tokio::test]
async fn get_job_bad_ulid() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri("/api/v1/jobs/NOT_A_VALID_ULID/v2"),
        &token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs/invalid/v2");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "ULID invalide → 400"
    );
}

/// Test 4 — POST /api/v1/jobs sans Idempotency-Key → 400.
///
/// L'absence du header Idempotency-Key est obligatoire — retourne 400.
/// Note : avec `jobs_pool` câblé, l'absence de header → 400 (pas 501).
#[tokio::test]
async fn create_job_no_idempotency_key() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": "Curate" }
    });
    let req = with_bearer(
        Request::builder()
            .method("POST")
            .uri("/api/v1/jobs")
            .header("Content-Type", "application/json"),
        &token,
    )
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .expect("build POST /api/v1/jobs");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "POST /api/v1/jobs sans Idempotency-Key → 400"
    );
}

/// Test 5 — POST /api/v1/jobs/:id/cancel → 404 (ID inconnu).
#[tokio::test]
async fn cancel_job_not_found() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let unknown_id = Ulid::new();
    let req = with_bearer(
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/jobs/{}/cancel", unknown_id)),
        &token,
    )
    .body(Body::empty())
    .expect("build POST /api/v1/jobs/:id/cancel");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "cancel ID inconnu → 404"
    );
}

/// Test 6 — POST /api/v1/jobs/:id/cancel → 409 Conflict (job Running).
///
/// Injecte un job directement dans le store, le met en Running via dequeue(),
/// puis tente de l'annuler → 409 Conflict (job Running ne peut pas être annulé).
#[tokio::test]
async fn cancel_job_conflict_running() {
    use gradatum_core::QueueStore;

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    // Enqueue un job puis le dequeue (le met en Running)
    let record = make_test_job();
    let job_id = store.enqueue(record).await.expect("enqueue invariant test");

    let _ = store.dequeue().await.expect("dequeue invariant test");

    // Vérifier que le job est bien Running avant le test
    let fetched = store
        .get(job_id)
        .await
        .expect("get après dequeue")
        .expect("job doit exister");
    assert_eq!(
        fetched.lifecycle.status,
        gradatum_core::JobStatus::Running,
        "job doit être Running après dequeue"
    );

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/jobs/{}/cancel", job_id)),
        &token,
    )
    .body(Body::empty())
    .expect("build POST cancel Running");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "cancel job Running → 409 Conflict (invariant v81 L5656-5660)"
    );
}

/// Test 7 — Régression E-12 : GET /api/v1/jobs/:id/v2 après dequeue → status=Running.
///
/// Avant le fix E-12, get() retournait le payload stale (status=Pending).
/// Ce test garantit la régression : après dequeue, le statut lu via l'API
/// doit être Running (colonnes SQL autoritatives, pas le payload BLOB stale).
#[tokio::test]
async fn e12_regression_get_after_dequeue() {
    use gradatum_core::QueueStore;

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    let record = make_test_job();
    let job_id = store.enqueue(record).await.expect("enqueue E-12 test");

    // Dequeue met le statut SQL en Running MAIS le payload BLOB reste Pending (optimisation)
    let _ = store.dequeue().await.expect("dequeue E-12 test");

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/jobs/{}/v2", job_id)),
        &token,
    )
    .body(Body::empty())
    .expect("build GET jobs/:id/v2 E-12");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(resp.status(), StatusCode::OK, "GET jobs/:id/v2 → 200");

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body E-12");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON E-12");

    // Fix E-12 : le statut doit être Running (lu depuis SQL, pas le BLOB)
    assert_eq!(
        json["lifecycle"]["status"],
        serde_json::json!("Running"),
        "get() après dequeue DOIT retourner Running — régression E-12"
    );
}

/// Test 8 — Pagination cursor-based : liste de jobs avec cursor.
///
/// Insère 5 jobs, liste avec limit=3 → next_cursor présent.
/// Utilise le cursor pour la page 2 → 2 jobs restants, pas de cursor suivant.
#[tokio::test]
async fn list_jobs_cursor_pagination() {
    use gradatum_core::QueueStore;

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    // Insère 5 jobs
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = store
            .enqueue(make_test_job())
            .await
            .expect("enqueue pagination test");
        ids.push(id);
    }

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    // Page 1 : limit=3 → 3 items + next_cursor
    let req = with_bearer(
        Request::builder().method("GET").uri("/api/v1/jobs?limit=3"),
        &token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs?limit=3");

    let resp = router.clone().oneshot(req).await.expect("service page 1");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body page 1");
    let page1: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON page 1");

    assert_eq!(
        page1["items"].as_array().expect("items page 1").len(),
        3,
        "page 1 : 3 items"
    );
    let cursor = page1["next_cursor"]
        .as_str()
        .expect("next_cursor présent page 1");

    // Page 2 : cursor → 2 items restants, pas de cursor
    let req2 = with_bearer(
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/jobs?limit=3&cursor={}", cursor)),
        &token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs?limit=3&cursor=...");

    let resp2 = router.oneshot(req2).await.expect("service page 2");
    assert_eq!(resp2.status(), StatusCode::OK);

    let body2 = axum::body::to_bytes(resp2.into_body(), usize::MAX)
        .await
        .expect("lire body page 2");
    let page2: serde_json::Value = serde_json::from_slice(&body2).expect("parse JSON page 2");

    assert_eq!(
        page2["items"].as_array().expect("items page 2").len(),
        2,
        "page 2 : 2 items restants"
    );
    assert!(
        page2["next_cursor"].is_null(),
        "page 2 : pas de cursor (dernière page)"
    );
}

/// Test 9 — Filtre par statut : GET /api/v1/jobs?status=pending.
///
/// Insère 2 jobs Pending + 1 job qui passe en Done.
/// Filtre status=pending → 2 résultats.
#[tokio::test]
async fn list_jobs_filter_status() {
    use gradatum_core::{JobResult, QueueStore};

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    // Insère 3 jobs Pending
    for _ in 0..3 {
        store
            .enqueue(make_test_job())
            .await
            .expect("enqueue filter test");
    }

    // Dequeue 1 et le complète → Done
    if let Some(running) = store.dequeue().await.expect("dequeue filter test") {
        let result = JobResult {
            success: true,
            duration_ms: 10,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        store
            .complete(running.id, result)
            .await
            .expect("complete filter test");
    }

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    // Filtre status=pending → 2 jobs (le 3ème est Done)
    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri("/api/v1/jobs?status=pending"),
        &token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs?status=pending");

    let resp = router.oneshot(req).await.expect("service filtre status");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body filtre");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON filtre");

    let items = json["items"].as_array().expect("items est array");
    assert_eq!(
        items.len(),
        2,
        "status=pending → 2 jobs (le job Done est exclu)"
    );

    // Vérifie que tous les items ont le statut Pending
    for item in items {
        assert_eq!(
            item["lifecycle"]["status"],
            serde_json::json!("Pending"),
            "chaque item filtré doit avoir status=Pending"
        );
    }

    // Note : router consommé par oneshot — créer un nouveau pool pour ce sous-test
    // (le state a déjà été consommé, tester via le store directement)
    let pool2 = make_test_pool().await;
    let store2 = Arc::new(SqliteQueueStore::new(pool2.clone()));
    // Réinsère et complète pour ce sous-test
    for _ in 0..2 {
        store2
            .enqueue(make_test_job())
            .await
            .expect("enqueue done test");
    }
    let r = store2.dequeue().await.expect("dequeue done test").unwrap();
    store2
        .complete(
            r.id,
            JobResult {
                success: true,
                duration_ms: 5,
                cost_usd: None,
                result_note: None,
                conflict_payload: None,
            },
        )
        .await
        .expect("complete done test");

    let state2 = state_with_store(store2, pool2);
    let token2 = test_token(&state2);
    let router2 = build_router(state2);
    let req_done = with_bearer(
        Request::builder()
            .method("GET")
            .uri("/api/v1/jobs?status=done"),
        &token2,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs?status=done");
    let resp_done = router2
        .oneshot(req_done)
        .await
        .expect("service done filter");
    assert_eq!(resp_done.status(), StatusCode::OK);
    let body_done = axum::body::to_bytes(resp_done.into_body(), usize::MAX)
        .await
        .expect("lire body done");
    let json_done: serde_json::Value = serde_json::from_slice(&body_done).expect("parse JSON done");
    assert_eq!(
        json_done["items"].as_array().expect("items done").len(),
        1,
        "status=done → 1 job"
    );
}

/// Construit un `JobRecord` Pending dont l'`id` ULID et le `created_at` dérivent
/// de `dt` — id monotone corrélé à la date (F-37 studio jobs page : tri + plage).
fn make_test_job_at(dt: chrono::DateTime<chrono::Utc>) -> gradatum_core::JobRecord {
    let mut r = make_test_job();
    r.id = Ulid::from_datetime(dt.into());
    r.lifecycle.created_at = dt;
    r
}

/// Test 10 — F-37 `?order=desc&limit=5` → 5 jobs les plus récents, newest-first.
#[tokio::test]
async fn list_jobs_order_desc_newest_first() {
    use chrono::{Duration, Utc};
    use gradatum_core::QueueStore;

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    // 7 jobs à T+0..6 minutes → ids[6] le plus récent.
    let base = Utc::now() - Duration::hours(1);
    let mut ids = Vec::new();
    for i in 0..7 {
        let r = make_test_job_at(base + Duration::minutes(i));
        ids.push(r.id);
        store.enqueue(r).await.expect("enqueue order desc");
    }

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri("/api/v1/jobs?order=desc&limit=5"),
        &token,
    )
    .body(Body::empty())
    .expect("build GET ?order=desc&limit=5");

    let resp = router.oneshot(req).await.expect("service order desc");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body order desc");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON order desc");
    let items = json["items"].as_array().expect("items array");
    assert_eq!(items.len(), 5, "limit=5 → 5 items");

    // Les 5 plus récents, dans l'ordre décroissant : ids[6], 5, 4, 3, 2.
    let got: Vec<String> = items
        .iter()
        .map(|it| it["id"].as_str().expect("id str").to_string())
        .collect();
    let expected: Vec<String> = [ids[6], ids[5], ids[4], ids[3], ids[2]]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(got, expected, "DESC = 5 plus récents, newest first");
    assert!(
        json["next_cursor"].is_string(),
        "has_more (7 > 5) → next_cursor présent"
    );
}

/// Test 11 — F-37 `?created_after=..&created_before=..` isole une plage (un jour).
#[tokio::test]
async fn list_jobs_created_range() {
    use chrono::{Duration, Utc};
    use gradatum_core::QueueStore;

    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));

    // 4 jobs à T+0,1,2,3 minutes.
    let base = Utc::now() - Duration::hours(2);
    let mut ids = Vec::new();
    let mut dates = Vec::new();
    for i in 0..4 {
        let dt = base + Duration::minutes(i);
        let r = make_test_job_at(dt);
        ids.push(r.id);
        dates.push(dt);
        store.enqueue(r).await.expect("enqueue range");
    }

    let state = state_with_store(store, pool);
    let token = test_token(&state);
    let router = build_router(state);

    // Plage exclusive (after = dates[0], before = dates[3]) → ne capte que ids[1], ids[2].
    let uri = format!(
        "/api/v1/jobs?created_after={}&created_before={}",
        urlencoding(&dates[0].to_rfc3339()),
        urlencoding(&dates[3].to_rfc3339()),
    );
    let req = with_bearer(Request::builder().method("GET").uri(uri), &token)
        .body(Body::empty())
        .expect("build GET range");

    let resp = router.oneshot(req).await.expect("service range");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body range");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("parse JSON range");
    let got: Vec<String> = json["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|it| it["id"].as_str().expect("id str").to_string())
        .collect();
    let expected: Vec<String> = [ids[1], ids[2]].iter().map(ToString::to_string).collect();
    assert_eq!(got, expected, "plage exclusive isole l'intérieur");
}

/// Test 12 — F-37 `?order=bogus` → 400 Bad Request (ordre inconnu rejeté).
#[tokio::test]
async fn list_jobs_bad_order_400() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri("/api/v1/jobs?order=sideways"),
        &token,
    )
    .body(Body::empty())
    .expect("build GET ?order=sideways");

    let resp = router.oneshot(req).await.expect("service bad order");
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "order inconnu → 400"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests F-16.1 — fix stub E-13 (désérialisation du JobKind réel)
//
// Avant F-16.1 : create_job forçait TOUT job → Job::Curate(note_id aléatoire)
// → worker read_note échoue → DLQ. Ces tests prouvent que le JobKind demandé
// est honoré, qu'un note_id client est respecté, et qu'un kind sans handler
// réel est rejeté en 400 (PAS de DLQ silencieux).
// ─────────────────────────────────────────────────────────────────────────────

/// Helper : POST /api/v1/jobs avec un body + Idempotency-Key + bearer, renvoie (status, json).
async fn post_create_job(
    router: Router,
    body: serde_json::Value,
    idempotency_key: &str,
    token: &str,
) -> (StatusCode, serde_json::Value) {
    let req = with_bearer(
        Request::builder()
            .method("POST")
            .uri("/api/v1/jobs")
            .header("Content-Type", "application/json")
            .header("Idempotency-Key", idempotency_key),
        token,
    )
    .body(Body::from(
        serde_json::to_vec(&body).expect("serialize body"),
    ))
    .expect("build POST /api/v1/jobs");
    let resp = router
        .oneshot(req)
        .await
        .expect("service POST /api/v1/jobs");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Helper : GET /api/v1/jobs/:id/v2 + bearer → JobRecord JSON.
async fn get_job_detail(router: Router, id: &str, token: &str) -> (StatusCode, serde_json::Value) {
    let req = with_bearer(
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/jobs/{id}/v2")),
        token,
    )
    .body(Body::empty())
    .expect("build GET /api/v1/jobs/:id/v2");
    let resp = router.oneshot(req).await.expect("service GET v2");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("lire body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Test F-16.1 (a) — POST Curate {note_id réel} → job Curate sur CETTE note.
///
/// Cœur du fix E-13 : avant, le note_id était aléatoire (`Ulid::new()`) → DLQ
/// garanti. Après, le note_id fourni par le client est honoré dans le JobSpec.
#[tokio::test]
async fn create_job_curate_honors_client_note_id() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state.clone());

    let note_id = Ulid::new().to_string();
    let body = serde_json::json!({
        "spec": { "kind": { "type": "Curate", "data": { "note_id": note_id } } }
    });

    let (status, json) = post_create_job(router, body, "f16-curate-noteid", &token).await;
    assert_eq!(status, StatusCode::ACCEPTED, "POST Curate valide → 202");
    let job_id = json["id"].as_str().expect("id renvoyé").to_string();
    assert_eq!(json["idempotent"], serde_json::json!(false));

    // Le JobRecord enqueued doit porter Job::Curate sur CE note_id (pas un ULID aléatoire).
    let router = build_router(state);
    let (status, detail) = get_job_detail(router, &job_id, &token).await;
    assert_eq!(status, StatusCode::OK, "GET v2 → 200");
    assert_eq!(
        detail["spec"]["kind"]["type"],
        serde_json::json!("Curate"),
        "kind doit être Curate (pas forcé arbitrairement)"
    );
    assert_eq!(
        detail["spec"]["kind"]["data"]["note_id"],
        serde_json::json!(note_id),
        "note_id doit être celui fourni par le client (fix E-13)"
    );
}

/// Test F-16.1 (b) — POST Distill → job Distill enqueued (PAS forcé en Curate).
#[tokio::test]
async fn create_job_distill_is_distill_not_curate() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state.clone());

    let body = serde_json::json!({
        "spec": { "kind": { "type": "Distill", "data": { "scope": "VaultWide" } } }
    });

    let (status, json) = post_create_job(router, body, "f16-distill", &token).await;
    assert_eq!(status, StatusCode::ACCEPTED, "POST Distill valide → 202");
    let job_id = json["id"].as_str().expect("id renvoyé").to_string();

    let router = build_router(state);
    let (status, detail) = get_job_detail(router, &job_id, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["spec"]["kind"]["type"],
        serde_json::json!("Distill"),
        "Distill ne doit PAS être forcé en Curate (fix E-13)"
    );
}

/// Test F-16.1 (b') — POST Purge → job Purge enqueued (handler réel).
#[tokio::test]
async fn create_job_purge_is_purge() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state.clone());

    let body = serde_json::json!({
        "spec": { "kind": { "type": "Purge", "data": { "mode": "Lifecycle" } } }
    });

    let (status, json) = post_create_job(router, body, "f16-purge", &token).await;
    assert_eq!(status, StatusCode::ACCEPTED, "POST Purge valide → 202");
    let job_id = json["id"].as_str().expect("id").to_string();

    let router = build_router(state);
    let (_, detail) = get_job_detail(router, &job_id, &token).await;
    assert_eq!(detail["spec"]["kind"]["type"], serde_json::json!("Purge"));
    // dry_run par défaut = true (PurgeSpec prudent).
    assert_eq!(
        detail["spec"]["kind"]["data"]["dry_run"],
        serde_json::json!(true),
        "PurgeSpec.dry_run défaut prudent = true"
    );
}

/// Test F-16.1 (c1) — POST ReIndex (handler stub `not implemented`) → 400.
///
/// ReIndex est routé vers un worker apalis mais `handle_reindex` retourne
/// toujours Err(Business) en v0.4.x. L'exposer comme déclenchable produirait
/// un échec systématique → on le rejette honnêtement en 400.
#[tokio::test]
async fn create_job_reindex_stub_rejected_400() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": { "type": "ReIndex", "data": "FtsOnly" } }
    });

    let (status, _) = post_create_job(router, body, "f16-reindex-stub", &token).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ReIndex (handler stub) → 400, PAS de job enqueued"
    );
}

/// Test F-16.1 (c2) — POST kind sans worker apalis (Backup) → 400.
#[tokio::test]
async fn create_job_unsupported_kind_rejected_400() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": "Backup" }
    });

    let (status, _) = post_create_job(router, body, "f16-backup-unsupported", &token).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "kind sans handler worker → 400"
    );
}

/// Test F-16.1 (c3) — POST Curate sans note_id → 400 (désérialisation échoue,
/// note_id est obligatoire pour cibler une note réelle).
#[tokio::test]
async fn create_job_curate_missing_note_id_rejected_400() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": { "type": "Curate", "data": {} } }
    });

    let (status, _) = post_create_job(router, body, "f16-curate-no-note", &token).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "Curate sans note_id → 400 (cible obligatoire)"
    );
}

/// Test F-16.1 (c4) — POST kind inconnu/garbage → 400 (pas de panic).
#[tokio::test]
async fn create_job_garbage_kind_rejected_400() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": "TotallyBogusKind" }
    });

    let (status, _) = post_create_job(router, body, "f16-garbage", &token).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "kind inconnu → 400");
}

/// Test F-16.1 (d) — Idempotency-Key rejoué → même job_id (pas de doublon).
#[tokio::test]
async fn create_job_idempotency_key_replay_same_id() {
    let state = build_state_with_job_store().await;
    let token = test_token(&state);
    let note_id = Ulid::new().to_string();
    let body = serde_json::json!({
        "spec": { "kind": { "type": "Curate", "data": { "note_id": note_id } } }
    });

    let router = build_router(state.clone());
    let (status1, json1) = post_create_job(router, body.clone(), "f16-idem-replay", &token).await;
    assert_eq!(status1, StatusCode::ACCEPTED);
    let id1 = json1["id"].as_str().expect("id1").to_string();
    assert_eq!(json1["idempotent"], serde_json::json!(false));

    let router = build_router(state);
    let (status2, json2) = post_create_job(router, body, "f16-idem-replay", &token).await;
    assert_eq!(status2, StatusCode::OK, "rejeu → 200 (idempotent)");
    let id2 = json2["id"].as_str().expect("id2").to_string();
    assert_eq!(json2["idempotent"], serde_json::json!(true));
    assert_eq!(id1, id2, "même clé → même job_id (pas de doublon)");
}

/// Encode minimal pour query string (RFC3339 contient `:` et `+`).
fn urlencoding(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}
