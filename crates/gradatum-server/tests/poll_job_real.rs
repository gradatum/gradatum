//! Tests E2E `poll_job_real` — `GET /api/v1/jobs/:id` handler réel (P2.1 Task 7).
//!
//! Vérifie le comportement du handler jobs::get_job après le remplacement du stub T3 :
//! - Job enqueuté → poll retourne 200 + status "pending"
//! - Job enqueuté + leased + completed → poll retourne 200 + status "done"
//! - ID inconnu → poll retourne 404
//!
//! # Setup
//!
//! - Routeur minimal sans middleware d'auth (trust_stub injecte BearerToken directement).
//! - `SqliteQueue::in_memory()` — queue réelle avec stockage mémoire isolé par test.
//! - `tower::ServiceExt::oneshot` — pas de TCP, latence nulle.
//!
//! Régression RT5 : garantit que le handler consulte la queue réelle au lieu de
//! retourner un stub statique.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_queue::{NewJob, Queue, SqliteQueue};
use gradatum_server::state::AppState;
use tower::ServiceExt;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Preset ACL autorisant `test-poll-consumer` à lire et écrire sur `main/*`.
const TEST_ACL_POLL: &str = r#"
[[consumer]]
identity = "test-poll-consumer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

/// Construit un `AppState` de test avec une `SqliteQueue::in_memory()` réelle.
///
/// Retourne `(state, queue_arc)` — le `queue_arc` est conservé pour les opérations
/// directes sur la queue (lease, complete) dans les tests.
async fn build_poll_state() -> (AppState, Arc<SqliteQueue>) {
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_POLL)
        .expect("preset ACL poll valide — invariant statique");

    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory() — invariant test"),
    );

    let state = AppState::with_jwt_and_acl(jwt, acl).with_queue(queue.clone() as Arc<dyn Queue>);

    (state, queue)
}

/// Construit un routeur minimal avec trust_stub injectant BearerToken.
///
/// Pas de middleware JWT réel (test poll ne nécessite pas de vérification de token —
/// l'endpoint `/api/v1/jobs/:id` est sans auth par design).
/// Le trust_stub est présent pour satisfaire l'extracteur `TrustContext` dans les
/// autres handlers du routeur `/api/v1/*`.
fn build_poll_router(state: AppState) -> axum::Router {
    use axum::{middleware, Router};
    use gradatum_core::trust::TrustContext;

    async fn trust_stub(
        mut req: axum::http::Request<Body>,
        next: middleware::Next,
    ) -> axum::response::Response {
        let trust = if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
            if let Ok(val) = auth.to_str() {
                if let Some(token) = val.strip_prefix("Bearer ") {
                    if !token.is_empty() {
                        TrustContext::BearerToken {
                            kid: "test-kid".to_string(),
                            aud: "gradatum".to_string(),
                            sub: token.to_string(),
                            scopes: vec!["read".to_string(), "write".to_string()],
                            tenant_id: "main".to_string(),
                        }
                    } else {
                        TrustContext::Unauthenticated
                    }
                } else {
                    TrustContext::Unauthenticated
                }
            } else {
                TrustContext::Unauthenticated
            }
        } else {
            TrustContext::Unauthenticated
        };
        req.extensions_mut().insert(trust);
        next.run(req).await
    }

    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn(trust_stub))
        .with_state(state)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Enqueue un job → GET `/api/v1/jobs/:id` → 200 + status "pending".
///
/// Vérifie que le handler consulte la queue réelle et retourne les métadonnées
/// du job fraîchement enqueuté.
#[tokio::test]
async fn poll_pending_returns_pending() {
    let (state, queue) = build_poll_state().await;
    let router = build_poll_router(state);

    // Enqueue un job directement via la queue (bypass le handler vault_write
    // qui nécessite un vault câblé — on teste get_job en isolation).
    let job_id = queue
        .enqueue(NewJob {
            tenant_id: "main".to_string(),
            kind: "curate".to_string(),
            payload: b"test payload pending".to_vec(),
            max_attempts: 3,
        })
        .await
        .expect("enqueue job — invariant test");

    // GET /api/v1/jobs/:id → 200 + status "pending"
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{job_id}"))
        .body(Body::empty())
        .expect("build GET request");

    let resp = router
        .oneshot(req)
        .await
        .expect("service GET /api/v1/jobs/:id");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "job enqueuté doit retourner 200"
    );

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 4)
        .await
        .expect("lecture body réponse");
    let json: serde_json::Value =
        serde_json::from_slice(&body).expect("réponse doit être du JSON valide");

    assert_eq!(json["job_id"], job_id, "job_id doit correspondre");
    assert_eq!(
        json["status"], "pending",
        "job fraîchement enqueuté doit être pending"
    );
    assert_eq!(
        json["attempts"], 0,
        "attempts doit être 0 pour un job jamais traité"
    );
    assert!(
        json["last_error"].is_null(),
        "last_error doit être null pour un job pending"
    );
}

/// Enqueue + lease + complete → GET `/api/v1/jobs/:id` → 200 + status "done".
///
/// Vérifie le parcours complet du cycle de vie d'un job :
/// pending → leased → done.
#[tokio::test]
async fn poll_after_complete_returns_done() {
    let (state, queue) = build_poll_state().await;
    let router = build_poll_router(state);

    // Enqueue.
    let job_id = queue
        .enqueue(NewJob {
            tenant_id: "main".to_string(),
            kind: "curate".to_string(),
            payload: b"test payload done".to_vec(),
            max_attempts: 3,
        })
        .await
        .expect("enqueue job — invariant test");

    // Lease (simule le worker qui prend le job).
    let leased = queue
        .lease(&["curate"], Duration::from_secs(30))
        .await
        .expect("lease — invariant test")
        .expect("job doit être disponible pour lease");

    assert_eq!(
        leased.id, job_id,
        "job leasé doit correspondre au job enqueuté"
    );

    // Complete (simule la fin du traitement).
    queue
        .complete(job_id)
        .await
        .expect("complete — invariant test");

    // GET /api/v1/jobs/:id → 200 + status "done"
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{job_id}"))
        .body(Body::empty())
        .expect("build GET request");

    let resp = router
        .oneshot(req)
        .await
        .expect("service GET /api/v1/jobs/:id");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "job complété doit retourner 200"
    );

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 4)
        .await
        .expect("lecture body réponse");
    let json: serde_json::Value =
        serde_json::from_slice(&body).expect("réponse doit être du JSON valide");

    assert_eq!(json["job_id"], job_id, "job_id doit correspondre");
    assert_eq!(
        json["status"], "done",
        "job complété doit avoir status done"
    );
}

/// GET `/api/v1/jobs/9999999` → 404.
///
/// Régression : le stub T3 retournait toujours 200. Vérifie que le handler réel
/// retourne 404 pour un ID inconnu.
#[tokio::test]
async fn poll_unknown_id_returns_404() {
    let (state, _queue) = build_poll_state().await;
    let router = build_poll_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/9999999")
        .body(Body::empty())
        .expect("build GET request");

    let resp = router
        .oneshot(req)
        .await
        .expect("service GET /api/v1/jobs/9999999");

    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "ID inconnu doit retourner 404 — régression stub T3"
    );
}
