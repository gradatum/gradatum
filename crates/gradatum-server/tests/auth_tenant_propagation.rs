//! Tests d'intégration AUTH-T8 — propagation tenant_id dans le JWT + ACL filter.
//!
//! Vérifie que le `tenant_id` de la clé API est correctement propagé dans les claims JWT :
//! 1. Créer 2 clés API : `ak_main` (tenant="main") et `ak_staging` (tenant="staging")
//! 2. `/auth/exchange` avec chaque clé → JWT main vs JWT staging
//! 3. Assertions sur les claims JWT : `tenant_id` correct pour chaque token
//! 4. `vault_write` avec chaque JWT → 202 Accepted (ACL autorisée pour les deux tenants)
//! 5. Scope déféré Phase 2.1 : filter list par tenant_id — documenté ici
//!
//! # Périmètre alpha.5 Phase 2.0c
//!
//! - ACL filter par `tenant_id` dans les handlers read/list : DÉFÉRÉ Phase 2.1.
//!   La route `vault_write` utilise `req.tenant_id` du body pour l'évaluation ACL,
//!   mais les handlers read (`vault_list`, `vault_read`) ne filtrent pas encore
//!   les résultats par tenant_id du JWT. Ce filtrage est câblé en Phase 2.1 avec
//!   le store vault multi-tenant réel.
//!
//! - Ce test valide UNIQUEMENT la propagation JWT.tenant_id et le flow auth.
//!   Les assertions sur filter list par tenant sont marquées `TODO Phase 2.1`.
//!

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_db_sqlite::{run_migrations, SqliteQueueStore};
use gradatum_queue::SqliteQueue;
use gradatum_server::auth_routes::ExchangeResponse;
use gradatum_server::state::AppState;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL autorisant deux consumers distincts :
/// - `writer-main` : écriture sur `main/*`
/// - `writer-staging` : écriture sur `staging/*` et `main/*`
///
/// Note : le handler `vault_write` évalue l'ACL sur `{req.tenant_id}/main`.
/// Pour tenant="main" : locus = "main/main" → autorisé pour `writer-main`
/// Pour tenant="staging" : locus = "staging/main" → autorisé pour `writer-staging`
const TEST_ACL_TENANT: &str = r#"
[[consumer]]
identity = "writer-main"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]

[[consumer]]
identity = "writer-staging"
read_patterns  = ["staging/*", "staging/main"]
write_patterns = ["staging/*", "staging/main"]
"#;

// ── Helpers de setup ──────────────────────────────────────────────────────────

/// Construit un `AppState` de test avec deux consumers ACL + SqliteApiKeyStore.
async fn build_tenant_test_state() -> (AppState, TempDir) {
    let dir = TempDir::new().expect("tempdir tenant propagation");
    let api_keys_path = dir.path().join("api_keys.sqlite");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_TENANT)
        .expect("preset ACL tenant valide — invariant statique");

    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory() — invariant test"),
    );

    // Phase 1.2 : vault_write bridge vers job_store (gradatum_jobs) — nécessaire pour 202.
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(queue as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("SqliteApiKeyStore init — invariant test");

    (state, dir)
}

/// Construit le routeur de test complet (même topologie que main.rs).
fn build_tenant_router(state: AppState) -> axum::Router {
    use axum::{middleware, routing::get, Router};
    use gradatum_server::health;

    let authed = Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ));

    let unauthed = Router::new()
        .route("/health", get(health::handler))
        .merge(gradatum_server::auth_routes::router());

    authed.merge(unauthed).with_state(state)
}

/// Échange une clé API contre un JWT via le routeur de test.
///
/// Retourne le `token` JWT désérialisé (string).
async fn exchange_key(router: axum::Router, secret: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {secret}"))
        .body(Body::empty())
        .expect("build exchange request");

    let resp = router.oneshot(req).await.expect("service /auth/exchange");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "échange clé API → JWT doit retourner 200"
    );

    let body = axum::body::to_bytes(resp.into_body(), 1024 * 64)
        .await
        .expect("lecture body exchange");
    let parsed: ExchangeResponse = serde_json::from_slice(&body).expect("parse ExchangeResponse");
    parsed.token
}

// ── Test 1 : JWT main porte tenant_id="main" ─────────────────────────────────

/// Vérification que le JWT émis pour une clé tenant="main" porte `tenant_id="main"`.
#[tokio::test]
async fn jwt_main_key_carries_tenant_main() {
    let (state, _dir) = build_tenant_test_state().await;
    let jwt_service = state.jwt.clone();

    // Créer la clé API pour le tenant "main".
    let material_main = state
        .api_keys
        .create(
            "writer-main",
            vec!["read".into(), "write".into()],
            "main".into(),
            Some("clé test tenant main".into()),
        )
        .await
        .expect("create api key main");

    let router = build_tenant_router(state);
    let jwt_main = exchange_key(router, &material_main.secret).await;

    // Vérifier les claims JWT.
    let claims = jwt_service
        .verify(&jwt_main)
        .expect("JWT main doit être vérifiable");

    assert_eq!(
        claims.tenant_id, "main",
        "JWT émis pour clé tenant='main' doit porter tenant_id='main'"
    );
    assert_eq!(
        claims.sub, "writer-main",
        "sub JWT doit correspondre à l'owner de la clé"
    );
    assert!(
        claims.scopes.contains(&"read".to_string()),
        "scopes JWT doivent contenir 'read'"
    );
    assert!(
        claims.scopes.contains(&"write".to_string()),
        "scopes JWT doivent contenir 'write'"
    );
}

// ── Test 2 : JWT staging porte tenant_id="staging" ───────────────────────────

/// Vérification que le JWT émis pour une clé tenant="staging" porte `tenant_id="staging"`.
#[tokio::test]
async fn jwt_staging_key_carries_tenant_staging() {
    let (state, _dir) = build_tenant_test_state().await;
    let jwt_service = state.jwt.clone();

    // Créer la clé API pour le tenant "staging".
    let material_staging = state
        .api_keys
        .create(
            "writer-staging",
            vec!["read".into(), "write".into()],
            "staging".into(),
            Some("clé test tenant staging".into()),
        )
        .await
        .expect("create api key staging");

    let router = build_tenant_router(state);
    let jwt_staging = exchange_key(router, &material_staging.secret).await;

    // Vérifier les claims JWT.
    let claims = jwt_service
        .verify(&jwt_staging)
        .expect("JWT staging doit être vérifiable");

    assert_eq!(
        claims.tenant_id, "staging",
        "JWT émis pour clé tenant='staging' doit porter tenant_id='staging'"
    );
    assert_eq!(
        claims.sub, "writer-staging",
        "sub JWT doit correspondre à l'owner de la clé staging"
    );
}

// ── Test 3 : deux clés → deux JWTs → tenant_id distincts ─────────────────────

/// Vérification que les deux JWTs portent des `tenant_id` distincts.
///
/// Test de non-contamination : le tenant d'une clé ne doit jamais fuir dans le JWT
/// d'une autre clé, même si elles sont créées simultanément dans le même store.
#[tokio::test]
async fn two_keys_produce_distinct_tenant_ids_in_jwt() {
    let (state, _dir) = build_tenant_test_state().await;
    let jwt_service = state.jwt.clone();

    // Créer les deux clés dans le même store.
    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let material_staging = state
        .api_keys
        .create(
            "writer-staging",
            vec!["write".into()],
            "staging".into(),
            None,
        )
        .await
        .expect("create api key staging");

    // Construire le routeur UNE SEULE FOIS pour les deux échanges.
    // `exchange_key` consomme le Router (oneshot) — on doit cloner.
    let router_main = build_tenant_router(state.clone());
    let router_staging = build_tenant_router(state);

    let jwt_main = exchange_key(router_main, &material_main.secret).await;
    let jwt_staging = exchange_key(router_staging, &material_staging.secret).await;

    let claims_main = jwt_service.verify(&jwt_main).expect("JWT main vérifiable");
    let claims_staging = jwt_service
        .verify(&jwt_staging)
        .expect("JWT staging vérifiable");

    // Assertion principale : les tenant_id sont distincts et corrects.
    assert_eq!(
        claims_main.tenant_id, "main",
        "JWT main : tenant_id doit être 'main', obtenu: '{}'",
        claims_main.tenant_id
    );
    assert_eq!(
        claims_staging.tenant_id, "staging",
        "JWT staging : tenant_id doit être 'staging', obtenu: '{}'",
        claims_staging.tenant_id
    );

    // Non-contamination : le tenant_id main n'est pas dans le JWT staging et vice-versa.
    assert_ne!(
        claims_main.tenant_id, claims_staging.tenant_id,
        "les deux JWTs doivent porter des tenant_id distincts"
    );
}

// ── Test 4 : vault_write avec JWT main → 202 (ACL "main/main" autorisée) ──────

/// vault_write avec JWT tenant="main" et body tenant_id="main" → 202 Accepted.
///
/// Le handler évalue ACL sur locus "main/main" avec sub="writer-main".
/// Le preset ACL autorise `writer-main` sur `main/*` → Allow.
#[tokio::test]
async fn vault_write_with_main_jwt_accepted() {
    let (state, _dir) = build_tenant_test_state().await;

    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let router = build_tenant_router(state.clone());
    let jwt_main = exchange_key(router, &material_main.secret).await;

    // POST /api/v1/vault_write avec JWT main.
    let router2 = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {jwt_main}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[AUTH-T8] Note tenant main",
                "body": "Test propagation tenant main.",
                "tenant_id": "main"  // correspond au JWT tenant_id
            }))
            .expect("sérialisation body"),
        ))
        .expect("build vault_write main request");

    let resp = router2
        .oneshot(req)
        .await
        .expect("service vault_write main");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write avec JWT main (tenant='main') → 202 Accepted"
    );
}

// ── Test 5 : vault_write avec JWT staging → 202 (ACL "staging/main" autorisée) ─

/// vault_write avec JWT tenant="staging" et body tenant_id="staging" → 202 Accepted.
///
/// Le handler évalue ACL sur locus "staging/main" avec sub="writer-staging".
/// Le preset ACL autorise `writer-staging` sur `staging/*` → Allow.
///
/// Note (TODO Phase 2.1) : le vault réel ne filtre pas encore les notes par tenant_id.
/// En Phase 2.1, `vault_list` avec JWT tenant="main" ne devra PAS retourner les notes
/// écrites avec JWT tenant="staging". Ce test se limite à valider l'auth flow.
#[tokio::test]
async fn vault_write_with_staging_jwt_accepted() {
    let (state, _dir) = build_tenant_test_state().await;

    let material_staging = state
        .api_keys
        .create(
            "writer-staging",
            vec!["write".into()],
            "staging".into(),
            None,
        )
        .await
        .expect("create api key staging");

    let router = build_tenant_router(state.clone());
    let jwt_staging = exchange_key(router, &material_staging.secret).await;

    // POST /api/v1/vault_write avec JWT staging.
    let router2 = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {jwt_staging}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[AUTH-T8] Note tenant staging",
                "body": "Test propagation tenant staging.",
                "tenant_id": "staging"  // correspond au JWT tenant_id
            }))
            .expect("sérialisation body"),
        ))
        .expect("build vault_write staging request");

    let resp = router2
        .oneshot(req)
        .await
        .expect("service vault_write staging");

    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "vault_write avec JWT staging (tenant='staging') → 202 Accepted"
    );
}

// ── Test 6 : vault_write avec JWT main mais body tenant="staging" → 403 ───────

/// Cross-tenant write : JWT tenant="main" mais body tenant_id="staging" → 403 Forbidden.
///
/// Le handler évalue ACL sur locus "staging/main" avec sub="writer-main".
/// Le preset ACL n'autorise PAS `writer-main` sur `staging/*` → DenyImplicit → 403.
///
/// Ce test valide que le tenant_id du body est effectivement utilisé pour l'ACL,
/// et que le JWT tenant_id ne peut pas contourner l'isolation cross-tenant.
#[tokio::test]
async fn vault_write_cross_tenant_returns_403() {
    let (state, _dir) = build_tenant_test_state().await;

    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let router = build_tenant_router(state.clone());
    let jwt_main = exchange_key(router, &material_main.secret).await;

    // POST /api/v1/vault_write avec JWT main mais body tenant_id="staging".
    // ACL évaluée sur locus "staging/main" avec sub="writer-main" → DenyImplicit.
    let router2 = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {jwt_main}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[AUTH-T8] Tentative cross-tenant",
                "body": "writer-main ne peut pas écrire sur staging.",
                "tenant_id": "staging"  // cross-tenant : non autorisé pour writer-main
            }))
            .expect("sérialisation body"),
        ))
        .expect("build cross-tenant request");

    let resp = router2
        .oneshot(req)
        .await
        .expect("service vault_write cross-tenant");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_write cross-tenant (JWT main, body tenant='staging') → 403 Forbidden"
    );
}
