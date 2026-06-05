//! Tests d'intégration F-16 — API jobs v0.2.0 Phase 3.
//!
//! Couvre les 9 scénarios critiques de la spec §6 Phase 3 :
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
//! # Architecture des tests
//!
//! - `AppState` construit avec `SqliteQueueStore` in-memory injecté via `with_job_store`.
//! - `NoopQueueStore` par défaut pour les tests ne nécessitant pas de store réel.
//! - Pattern `tower::ServiceExt::oneshot` (identique aux autres tests du workspace).
//!
//! # Références
//!
//! - spec §6 Phase 3
//! - v81 F-16 §6 L5613-5668
//! - fix E-12 §11

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{middleware, Router};
use gradatum_db_sqlite::{apply_sqlite_pragmas, run_migrations, SqliteQueueStore};
use gradatum_server::{api_v1, middleware::auth_middleware, state::AppState};
use sqlx::SqlitePool;
use std::sync::Arc;
use tower::ServiceExt;
use ulid::Ulid;

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

/// Construit un `AppState` de test avec `SqliteQueueStore` in-memory.
///
/// Injecte `job_store` et `jobs_pool` via `with_job_store`.
/// Le reste du state utilise les valeurs par défaut (JwtService éphémère,
/// ACL deny-all, index in-memory).
async fn build_state_with_job_store() -> AppState {
    let pool = make_test_pool().await;
    let store = Arc::new(SqliteQueueStore::new(pool.clone()));
    AppState::new().with_job_store(store, pool)
}

/// Construit le routeur de test avec `auth_middleware` actif.
///
/// `auth_middleware` est obligatoire — les handlers extraient `Extension<TrustContext>`.
fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
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
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs")
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
    let router = build_router(state);

    let unknown_id = Ulid::new();
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{}/v2", unknown_id))
        .body(Body::empty())
        .expect("build GET /api/v1/jobs/:id/v2");

    let resp = router.oneshot(req).await.expect("service");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "ULID inconnu → 404");
}

/// Test 3 — GET /api/v1/jobs/:id/v2 → 400 (ULID invalide).
#[tokio::test]
async fn get_job_bad_ulid() {
    let state = build_state_with_job_store().await;
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/NOT_A_VALID_ULID/v2")
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
    let router = build_router(state);

    let body = serde_json::json!({
        "spec": { "kind": "Curate" }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/jobs")
        .header("Content-Type", "application/json")
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
    let router = build_router(state);

    let unknown_id = Ulid::new();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/jobs/{}/cancel", unknown_id))
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
/// puis tente de l'annuler → 409 Conflict (invariant v81 L5656-5660).
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

    let state = AppState::new().with_job_store(store, pool);
    let router = build_router(state);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/jobs/{}/cancel", job_id))
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

    let state = AppState::new().with_job_store(store, pool);
    let router = build_router(state);

    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{}/v2", job_id))
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

    let state = AppState::new().with_job_store(store, pool);
    let router = build_router(state);

    // Page 1 : limit=3 → 3 items + next_cursor
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs?limit=3")
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
    let req2 = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs?limit=3&cursor={}", cursor))
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
        };
        store
            .complete(running.id, result)
            .await
            .expect("complete filter test");
    }

    let state = AppState::new().with_job_store(store, pool);
    let router = build_router(state);

    // Filtre status=pending → 2 jobs (le 3ème est Done)
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs?status=pending")
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

    // Vérifier aussi le filtre done
    let req_done = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs?status=done")
        .body(Body::empty())
        .expect("build GET /api/v1/jobs?status=done");

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
            },
        )
        .await
        .expect("complete done test");

    let state2 = AppState::new().with_job_store(store2, pool2);
    let router2 = build_router(state2);
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
