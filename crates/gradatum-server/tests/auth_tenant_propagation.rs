//! Tests d'intégration AUTH-T8 — propagation tenant_id dans le JWT + invariant mono-vault.
//!
//! ## Mitigation P0 cross-tenant (2026-06-12) — RÉÉCRITURE
//!
//! Le vault est mono-physique "main". L'ancien modèle multi-tenant "best effort"
//! (clés `staging` → JWT `staging` → ACL `staging/main`) constituait la faille P0 :
//! aucune réconciliation body↔claims, et `/auth/exchange` mintait des JWT non-main.
//!
//! Le nouvel invariant (Lots 1+2+3) :
//! - `/auth/exchange` REFUSE 403 toute clé tenant ≠ "main" (Lot 1, gate à la source).
//! - Le middleware REFUSE 403 tout `BearerToken` tenant ≠ "main" (Lot 2, defense-in-depth).
//! - Les handlers dérivent le tenant effectif du JWT et refusent 403 un body
//!   `tenant_id` divergent (Lot 3).
//!
//! Les anciens tests qui exchangaient des clés `staging` testaient le comportement
//! vulnérable — ils asserent désormais le refus 403.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
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
    use axum::{Router, middleware, routing::get};
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

// ── Test 2 : clé staging → exchange refusé 403 (invariant mono-vault) ─────────

/// Une clé API tenant="staging" ne peut plus être échangée (P0 cross-tenant Lot 1).
/// L'ancien comportement (JWT staging émis) était la faille.
#[tokio::test]
async fn staging_key_exchange_returns_403() {
    // La création directe d'une clé staging est désormais refusée (Lot 6) : on crée
    // une clé "main" puis on la mute en SQL pour simuler une clé legacy "staging",
    // afin d'isoler le test du gate /auth/exchange (Lot 1).
    let dir = TempDir::new().expect("tempdir");
    let api_keys_path = dir.path().join("api_keys.sqlite");
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_TENANT).expect("preset ACL");
    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("api_keys store init");

    let material = state
        .api_keys
        .create(
            "writer-staging",
            vec!["read".into(), "write".into()],
            "main".into(),
            None,
        )
        .await
        .expect("create api key main");

    let pool = SqlitePoolOptions::new()
        .connect(&format!("sqlite://{}", api_keys_path.display()))
        .await
        .expect("open api_keys sqlite");
    sqlx::query("UPDATE api_keys SET tenant_id = 'staging' WHERE prefix = ?")
        .bind(&material.prefix)
        .execute(&pool)
        .await
        .expect("mutate tenant to staging");
    pool.close().await;

    let router = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build exchange request");

    let resp = router.oneshot(req).await.expect("service /auth/exchange");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "clé tenant='staging' → exchange refusé 403 (mono-vault)"
    );
}

// ── Test 3 : clé main → exchange OK, tenant propagé "main" ────────────────────

/// La clé main reste pleinement fonctionnelle (zéro breaking pour clients "main").
#[tokio::test]
async fn main_key_exchange_carries_tenant_main() {
    let (state, _dir) = build_tenant_test_state().await;
    let jwt_service = state.jwt.clone();

    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let router = build_tenant_router(state);
    let jwt_main = exchange_key(router, &material_main.secret).await;

    let claims_main = jwt_service.verify(&jwt_main).expect("JWT main vérifiable");
    assert_eq!(
        claims_main.tenant_id, "main",
        "JWT main : tenant_id doit être 'main', obtenu: '{}'",
        claims_main.tenant_id
    );
    assert_eq!(claims_main.sub, "writer-main");
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

// ── Test 5 : vault_write JWT main + body tenant="staging" → 403 (Lot 3) ───────

/// Vrai vecteur P0 : un client légitime "main" envoie un body `tenant_id="staging"`
/// pour tenter de cibler un locus arbitraire. Le handler dérive le tenant du JWT
/// et REFUSE 403 le body divergent (Lot 3). L'attaque ne peut plus construire un
/// locus `staging/...`.
#[tokio::test]
async fn vault_write_main_jwt_with_divergent_body_tenant_returns_403() {
    let (state, _dir) = build_tenant_test_state().await;

    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let router = build_tenant_router(state.clone());
    let jwt_main = exchange_key(router, &material_main.secret).await;

    let router2 = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {jwt_main}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[P0] Tentative locus arbitraire",
                "body": "body tenant_id divergent du JWT.",
                "tenant_id": "staging"  // diverge du JWT main → 403 (Lot 3)
            }))
            .expect("sérialisation body"),
        ))
        .expect("build divergent-body request");

    let resp = router2
        .oneshot(req)
        .await
        .expect("service vault_write divergent body");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_write JWT main + body tenant='staging' → 403 (tenant dérivé du JWT)"
    );
}

// ── Test 6 : middleware deny d'un JWT non-main forgé (Lot 2, defense-in-depth) ─

/// Defense-in-depth : même si un JWT tenant ≠ "main" venait à exister (ex. clé
/// legacy avant Lot 1, ou rotation de clé de signature compromise), le MIDDLEWARE
/// le refuse 403 avant d'atteindre tout handler authentifié.
///
/// Le JWT est forgé directement via `JwtService::sign` (contourne /auth/exchange)
/// pour isoler la couche middleware.
#[tokio::test]
async fn middleware_denies_forged_non_main_jwt() {
    let (state, _dir) = build_tenant_test_state().await;

    // Forger un JWT tenant="staging" SANS passer par /auth/exchange (Lot 1 le refuserait).
    let forged = state
        .jwt
        .sign(
            "writer-staging",
            &["write".to_string()],
            TokenScope::Service,
            "staging",
        )
        .expect("sign forged staging JWT");

    let router = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {forged}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[P0] JWT staging forgé",
                "body": "Le middleware doit refuser ce bearer non-main.",
                "tenant_id": "staging"
            }))
            .expect("sérialisation body"),
        ))
        .expect("build forged-jwt request");

    let resp = router
        .oneshot(req)
        .await
        .expect("service vault_write forged");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "JWT non-main forgé → 403 au niveau middleware (defense-in-depth)"
    );
}

// ── Test 7 : vault_restore JWT main + body tenant_id divergent → 403 (Lot 3 P1) ─

/// ACL permissive : `writer-main` autorisé en écriture sur `main/*` ET `staging/*`.
/// Sert à discriminer la couche effective_tenant (P1) de la couche ACL :
/// - Sans fix P1 : handler utilise locus `staging/main` → ACL Allow → vault 500 (PlaceholderRegistry)
/// - Avec fix P1 : effective_tenant refuse 403 avant ACL
const TEST_ACL_PERMISSIVE: &str = r#"
[[consumer]]
identity = "writer-main"
read_patterns  = ["main/*", "staging/*"]
write_patterns = ["main/*", "staging/*"]
"#;

/// `vault_restore` est une opération d'écriture (CoW) qui doit dériver le locus du
/// tenant JWT, pas du body. Ce test vérifie le correctif P1 : un body
/// `tenant_id="staging"` avec un JWT main doit retourner 403 (effective_tenant),
/// et non 500 (PlaceholderRegistry) comme sans le correctif.
///
/// L'ACL permissive (`staging/*` autorisé) garantit que c'est bien la garde
/// `effective_tenant` qui retourne 403, pas l'ACL.
#[tokio::test]
async fn vault_restore_main_jwt_with_divergent_body_tenant_returns_403() {
    let dir = TempDir::new().expect("tempdir vault_restore divergent");
    let api_keys_path = dir.path().join("api_keys.sqlite");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_PERMISSIVE)
        .expect("preset ACL permissive valide — invariant statique");

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_api_keys_path(&api_keys_path)
        .await
        .expect("SqliteApiKeyStore init — invariant test");

    let material_main = state
        .api_keys
        .create("writer-main", vec!["write".into()], "main".into(), None)
        .await
        .expect("create api key main");

    let router = build_tenant_router(state.clone());
    let jwt_main = exchange_key(router, &material_main.secret).await;

    let router2 = build_tenant_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_restore")
        .header("Authorization", format!("Bearer {jwt_main}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "note_id": "01JT00000000000000000000A1",
                "ts_ms": 1700000000000_i64,
                "tenant_id": "staging"  // diverge du JWT main → 403 par effective_tenant (P1 fix)
            }))
            .expect("sérialisation body"),
        ))
        .expect("build vault_restore divergent-body request");

    let resp = router2
        .oneshot(req)
        .await
        .expect("service vault_restore divergent body");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "vault_restore JWT main + body tenant='staging' + ACL permissive → 403 (effective_tenant, pas ACL)"
    );
}
