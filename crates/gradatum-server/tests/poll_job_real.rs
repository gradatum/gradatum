//! Tests E2E `poll_job_real` — `GET /api/v1/jobs/:id` handler réel.
//!
//! Vérifie le comportement du handler jobs::get_job après le remplacement du stub T3 :
//! - Sans bearer → 401 (fix C1 F-16 : auth requise sur la route legacy)
//! - Job enqueuté (auth OK) → poll retourne 200 + status "pending"
//! - Job enqueuté + leased + completed (auth OK) → poll retourne 200 + status "done"
//! - ID inconnu (auth OK) → poll retourne 404
//!
//! # Setup
//!
//! - Routeur minimal avec `trust_stub` qui injecte un `BearerToken` à partir du
//!   header `Authorization: Bearer <sub>` (ou `Unauthenticated` si absent).
//! - `SqliteQueue::in_memory()` — queue réelle avec stockage mémoire isolé par test.
//! - `tower::ServiceExt::oneshot` — pas de TCP, latence nulle.
//!
//! # Auth (fix C1 F-16)
//!
//! La route legacy `GET /api/v1/jobs/{id}` exige désormais un bearer authentifié
//! (`is_authenticated()` → 401 sinon) **et** une ACL Read sur le locus `main/jobs`
//! (→ 403 sinon). Identique au pattern `jobs_v2`/`forget`. Le bearer de test est
//! `test-poll-consumer`, autorisé en lecture sur `main/*` par le preset ACL.
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

/// Identity ACL du consumer de test — doit correspondre à `TEST_ACL_POLL.identity`
/// ET au `sub` injecté par `trust_stub` (l'ACL matche `sub` contre `identity`).
const TEST_POLL_SUB: &str = "test-poll-consumer";

/// Construit un routeur minimal avec trust_stub injectant BearerToken.
///
/// Pas de middleware JWT réel : le `trust_stub` extrait le header
/// `Authorization: Bearer <sub>` et injecte un `BearerToken` (scopes read+write,
/// `tenant_id=main`). Sans header → `Unauthenticated` (→ 401 sur la route legacy
/// désormais protégée, fix C1 F-16).
fn build_poll_router(state: AppState) -> axum::Router {
    use axum::{Router, middleware};
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
                            sub: token.into(),
                            scopes: vec!["read".to_string(), "write".to_string()],
                            tenant_id: "main".into(),
                            jti: None,
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

    // GET /api/v1/jobs/:id → 200 + status "pending" (bearer authentifié + ACL Read OK)
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{job_id}"))
        .header("Authorization", format!("Bearer {TEST_POLL_SUB}"))
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

    // GET /api/v1/jobs/:id → 200 + status "done" (bearer authentifié + ACL Read OK)
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{job_id}"))
        .header("Authorization", format!("Bearer {TEST_POLL_SUB}"))
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

/// GET `/api/v1/jobs/9999999` (auth OK) → 404.
///
/// Régression : le stub T3 retournait toujours 200. Vérifie que le handler réel
/// retourne 404 pour un ID inconnu (après passage de l'auth, fix C1 F-16).
#[tokio::test]
async fn poll_unknown_id_returns_404() {
    let (state, _queue) = build_poll_state().await;
    let router = build_poll_router(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/jobs/9999999")
        .header("Authorization", format!("Bearer {TEST_POLL_SUB}"))
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

/// Fix C1 F-16 — GET `/api/v1/jobs/:id` **sans bearer** → 401 Unauthorized.
///
/// Avant le fix, la route legacy ignorait `trust` (`let _ = &trust;`) et exposait
/// le statut d'un job à quiconque connaissait un `i64` (autoincrément devinable).
/// Désormais, l'absence de bearer authentifié retourne 401 — identique au pattern
/// `jobs_v2`/`forget`. Le job EST enqueué (un `i64` valide existe), donc un 401
/// prouve que l'auth est évaluée AVANT la lecture de la queue (pas un 404 fortuit).
#[tokio::test]
async fn poll_without_bearer_returns_401() {
    let (state, queue) = build_poll_state().await;
    let router = build_poll_router(state);

    // Enqueue un job réel — on veut prouver que le 401 précède la lecture queue,
    // pas qu'on tombe sur un 404 parce que l'ID n'existe pas.
    let job_id = queue
        .enqueue(NewJob {
            tenant_id: "main".to_string(),
            kind: "curate".to_string(),
            payload: b"test payload unauth".to_vec(),
            max_attempts: 3,
        })
        .await
        .expect("enqueue job — invariant test");

    // GET sans header Authorization → trust_stub injecte Unauthenticated.
    let req = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/jobs/{job_id}"))
        .body(Body::empty())
        .expect("build GET request");

    let resp = router
        .oneshot(req)
        .await
        .expect("service GET /api/v1/jobs/:id sans bearer");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /api/v1/jobs/:id sans bearer doit retourner 401 (fix C1 F-16) — \
         job_id={job_id} existe pourtant, donc l'auth précède la lecture queue"
    );
}
