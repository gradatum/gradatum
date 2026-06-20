//! Tests d'intégration AUTH-T8 — flux E2E complet : API key → JWT → vault_write → poll.
//!
//! Vérifie la chaîne d'authentification end-to-end :
//! 1. `SqliteApiKeyStore.create(owner, scopes, tenant)` → secret `ak_xxx`
//! 2. `POST /auth/exchange` Bearer `ak_xxx` → 200 + JWT
//! 3. `POST /api/v1/vault_write` Bearer JWT + body → 202 + job_id
//! 4. `GET /api/v1/jobs/<job_id>` → 200 + statut JSON (route auth validée)
//! 5. Assertions : `tenant_id="main"` propagé dans les claims JWT, scope check OK
//!
//! # Setup
//!
//! Le routeur de test inclut `/auth/exchange` (hors middleware JWT) ET
//! `/api/v1/*` (sous middleware JWT). C'est la même topologie que `main.rs`.
//! L'ACL autorise le consumer dont l'identité = `key.owner` à écrire sur `main/*`.
//!
//! Le worker curator n'est pas câblé dans ce test — la queue utilise `SqliteQueue::in_memory()`.
//! `GET /api/v1/jobs/<id>` retourne `status: "pending"`.
//! Ce comportement est intentionnel : ce test valide la couche auth, pas le pipeline curator.
//!

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_queue::SqliteQueue;
use gradatum_server::auth_routes::ExchangeResponse;
use gradatum_server::state::AppState;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tower::ServiceExt;

// ── Preset ACL de test ────────────────────────────────────────────────────────

/// Preset ACL autorisant `e2e-auth-writer` à lire et écrire sur tous les loci `main/*`.
///
/// L'identité ACL correspond au `owner` de la clé API (`sub` du JWT émis).
/// Le locus évalué par `vault_write` est `"main/main"` (format `{tenant_id}/main`).
const TEST_ACL_E2E: &str = r#"
[[consumer]]
identity = "e2e-auth-writer"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Helpers de setup ──────────────────────────────────────────────────────────

/// Construit un `AppState` de test complet :
/// - `SqliteApiKeyStore` réel sur fichier temporaire
/// - `SqliteQueue` in-memory
/// - `AclEngine` avec `TEST_ACL_E2E`
/// - `JwtService` éphémère (clé Ed25519 générée à chaque test — isolation totale)
///
/// Retourne l'état et le `TempDir` à conserver pour la durée du test.
async fn build_e2e_state() -> (AppState, TempDir) {
    let dir = TempDir::new().expect("tempdir e2e");
    let api_keys_path = dir.path().join("api_keys.sqlite");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_E2E)
        .expect("preset ACL e2e valide — invariant statique");

    let queue = Arc::new(
        SqliteQueue::in_memory()
            .await
            .expect("SqliteQueue::in_memory() — invariant test"),
    );

    // Phase 1.2 : vault_write utilise state.job_store — câbler un SqliteQueueStore in-memory.
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

/// Construit le routeur de test complet :
/// - Routes `/api/v1/*` protégées par le middleware JWT réel (`auth_middleware`)
/// - Route `/auth/exchange` hors middleware (port de l'API key → JWT)
/// - Route `/health` non protégée
///
/// Topologie identique à `main.rs::build_router` — aucune dépendance au stub.
fn build_e2e_router(state: AppState) -> axum::Router {
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

/// Lit le body complet d'une réponse Axum (jusqu'à 64 KB).
async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), 1024 * 64)
        .await
        .expect("lecture body réponse")
        .to_vec()
}

// ── Test 1 : flux nominal complet API key → JWT → vault_write → poll ─────────

/// Flux E2E nominal : créer clé → exchange → vault_write → poll job.
///
/// Vérifie :
/// 1. `/auth/exchange` retourne 200 + JWT valide
/// 2. Claims JWT : `sub = "e2e-auth-writer"`, `tenant_id = "main"`, `scopes ⊇ ["admin"]`
/// 3. `POST /api/v1/vault_write` avec JWT → 202 + `job_id`
/// 4. `GET /api/v1/jobs/<job_id>` → 200 + JSON avec `job_id` correct
#[tokio::test]
async fn e2e_api_key_to_jwt_to_vault_write_to_poll() {
    let (state, _dir) = build_e2e_state().await;
    let jwt_service = state.jwt.clone();
    let router = build_e2e_router(state.clone());

    // Étape 1 : créer une clé API (owner = identité ACL, tenant = "main").
    let material = state
        .api_keys
        .create(
            "e2e-auth-writer",
            vec!["admin".into()],
            "main".into(),
            Some("clé de test E2E auth_e2e_full_flow".into()),
        )
        .await
        .expect("create api key e2e");

    // Étape 2 : POST /auth/exchange → JWT.
    let exchange_req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build exchange request");

    let exchange_resp = router
        .clone()
        .oneshot(exchange_req)
        .await
        .expect("service /auth/exchange");

    assert_eq!(
        exchange_resp.status(),
        StatusCode::OK,
        "échange API key valide → 200"
    );

    let exchange_body = read_body(exchange_resp).await;
    let exchange_parsed: ExchangeResponse =
        serde_json::from_slice(&exchange_body).expect("parse ExchangeResponse");

    // Vérifier les claims JWT avant d'utiliser le token.
    let claims = jwt_service
        .verify(&exchange_parsed.token)
        .expect("le JWT émis par /auth/exchange doit être vérifiable");

    assert_eq!(
        claims.sub, "e2e-auth-writer",
        "sub JWT doit correspondre à l'owner de la clé"
    );
    assert_eq!(
        claims.tenant_id, "main",
        "tenant_id JWT doit correspondre au tenant de la clé"
    );
    assert!(
        claims.scopes.contains(&"admin".to_string()),
        "scopes JWT doivent contenir 'admin', obtenu: {:?}",
        claims.scopes
    );
    assert!(exchange_parsed.ttl_secs > 0, "ttl_secs doit être positif");

    // Étape 3 : POST /api/v1/vault_write avec le JWT obtenu → 202 + job_id.
    //
    // Note : le handler vault_write utilise `req.tenant_id` du body pour l'évaluation
    // ACL (locus = "main/main"). Le JWT est vérifié par le middleware auth_middleware
    // et injecte `TrustContext::BearerToken { sub: "e2e-auth-writer", tenant_id: "main" }`.
    let write_req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {}", exchange_parsed.token))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "[AUTH-T8] Note de test E2E flux complet",
                "body": "Validation du flux API key → JWT → vault_write → poll.",
                "tags": ["auth-t8", "test"],
                "section_hint": "debug",
                "tenant_id": "main"
            }))
            .expect("sérialisation body vault_write"),
        ))
        .expect("build vault_write request");

    let write_resp = router
        .clone()
        .oneshot(write_req)
        .await
        .expect("service /api/v1/vault_write");

    assert_eq!(
        write_resp.status(),
        StatusCode::ACCEPTED,
        "vault_write avec JWT valide → 202 Accepted"
    );

    let write_body = read_body(write_resp).await;
    let write_parsed: serde_json::Value =
        serde_json::from_slice(&write_body).expect("parse vault_write response");

    // Phase 1.2 : vault_write retourne un ULID string (bridge job_store gradatum_jobs).
    let job_id = write_parsed["job_id"]
        .as_str()
        .expect("job_id doit être une string ULID (Phase 1.2 bridge job_store)");
    assert!(!job_id.is_empty(), "job_id ne doit pas être vide");
    assert_eq!(
        write_parsed["status"].as_str(),
        Some("queued"),
        "status doit être 'queued'"
    );
    let poll_url = write_parsed["poll_url"]
        .as_str()
        .expect("poll_url doit être présent");
    assert!(
        poll_url.starts_with("/api/v1/jobs/"),
        "poll_url doit commencer par /api/v1/jobs/, obtenu: {poll_url}"
    );

    // Étape 4 : GET <poll_url> → 200 + JobRecord JSON (route /jobs/{id}/v2).
    //
    // Phase 1.2 : poll_url = /api/v1/jobs/{ulid}/v2 (handler get_job_v2).
    // Ce test valide uniquement que la route est accessible après auth réussie.
    let jwt_token = &exchange_parsed.token;
    let poll_req = Request::builder()
        .method("GET")
        .uri(poll_url)
        .header("Authorization", format!("Bearer {}", jwt_token))
        .body(Body::empty())
        .expect("build poll request");

    let poll_resp = router
        .oneshot(poll_req)
        .await
        .expect("service GET poll_url");

    assert_eq!(
        poll_resp.status(),
        StatusCode::OK,
        "GET poll_url → 200 — job_id={job_id}"
    );

    let poll_body = read_body(poll_resp).await;
    let poll_parsed: serde_json::Value =
        serde_json::from_slice(&poll_body).expect("parse poll response");

    // get_job_v2 retourne JobRecord JSON complet — id + lifecycle + spec.
    assert_eq!(
        poll_parsed["id"].as_str(),
        Some(job_id),
        "id dans la réponse poll doit correspondre au job enqueued"
    );
    assert!(
        poll_parsed.get("lifecycle").is_some(),
        "réponse poll doit contenir lifecycle"
    );
}

// ── Test 2 : vault_write avec JWT invalide (non émis par /auth/exchange) → 401 ─

/// Rejection d'un JWT non valide sur `/api/v1/vault_write`.
///
/// Vérifie que le middleware JWT rejette un token signé avec une clé différente.
#[tokio::test]
async fn e2e_vault_write_with_invalid_jwt_returns_401() {
    let (state, _dir) = build_e2e_state().await;
    let router = build_e2e_router(state);

    // Signer un JWT avec un SERVICE JwtService différent (clé éphémère différente).
    let autre_jwt_service = JwtService::new_ephemeral();
    let faux_token = autre_jwt_service
        .sign(
            "e2e-auth-writer",
            &["admin".into()],
            gradatum_auth::jwt::TokenScope::Service,
            "main",
        )
        .expect("signer faux token — clé éphémère valide");

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {faux_token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "Note avec JWT contrefait",
                "body": "Test sécurité — doit être rejeté par le middleware.",
                "tenant_id": "main"
            }))
            .expect("sérialisation body"),
        ))
        .expect("build request JWT contrefait");

    let resp = router
        .oneshot(req)
        .await
        .expect("service vault_write JWT invalide");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "JWT signé avec une clé différente → 401"
    );
}

// ── Test 3 : vault_write sans JWT → 401 ──────────────────────────────────────

/// Appel à `/api/v1/vault_write` sans aucun header Authorization → 401.
///
/// Vérifie que le middleware JWT protège correctement la route.
#[tokio::test]
async fn e2e_vault_write_without_jwt_returns_401() {
    let (state, _dir) = build_e2e_state().await;
    let router = build_e2e_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "Note sans auth",
                "body": "Test sécurité — doit être rejeté.",
                "tenant_id": "main"
            }))
            .expect("sérialisation body"),
        ))
        .expect("build request sans auth");

    let resp = router
        .oneshot(req)
        .await
        .expect("service vault_write sans auth");

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "vault_write sans header Authorization → 401"
    );
}

// ── Test 4 : vault_write avec JWT ACL-denied → 403 ───────────────────────────

/// JWT valide mais consumer non configuré dans l'ACL → 403 Forbidden.
///
/// Vérifie que l'ACL default-deny s'applique correctement pour un consumer inconnu.
#[tokio::test]
async fn e2e_vault_write_with_unknown_consumer_returns_403() {
    let (state, _dir) = build_e2e_state().await;

    // Signer un JWT avec un `sub` inconnu de l'ACL (owner différent de "e2e-auth-writer").
    let token = state
        .jwt
        .sign(
            "consumer-inconnu-de-lacl",
            &["admin".into()],
            gradatum_auth::jwt::TokenScope::Service,
            "main",
        )
        .expect("signer JWT consumer inconnu");

    let router = build_e2e_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_write")
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "title": "Note consumer inconnu",
                "body": "Test ACL default-deny.",
                "tenant_id": "main"
            }))
            .expect("sérialisation body"),
        ))
        .expect("build request consumer inconnu");

    let resp = router
        .oneshot(req)
        .await
        .expect("service vault_write consumer inconnu");

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "JWT valide mais consumer inconnu de l'ACL → 403"
    );
}

// ── Test 5 : TTL ttl_secs cohérent avec TokenScope::Service ──────────────────

/// Vérification que `ttl_secs` est cohérent avec le scope Service (TTL long).
///
/// Un `JwtService::new_ephemeral()` utilise ttl_service_secs = 86400 (24h).
/// La spec R-A1 exige TTL Service = 86400s. Spec §2.4 E2 fix : champ renommé `ttl_secs`.
#[tokio::test]
async fn e2e_exchange_expires_in_matches_service_ttl() {
    let (state, _dir) = build_e2e_state().await;
    let expected_ttl = state.jwt.ttl_service_secs();
    let router = build_e2e_router(state.clone());

    let material = state
        .api_keys
        .create("e2e-auth-writer", vec!["read".into()], "main".into(), None)
        .await
        .expect("create api key ttl test");

    let req = Request::builder()
        .method("POST")
        .uri("/auth/exchange")
        .header("Authorization", format!("Bearer {}", material.secret))
        .body(Body::empty())
        .expect("build exchange request TTL");

    let resp = router.oneshot(req).await.expect("service exchange TTL");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = read_body(resp).await;
    let parsed: ExchangeResponse =
        serde_json::from_slice(&body).expect("parse ExchangeResponse TTL");

    assert_eq!(
        parsed.ttl_secs, expected_ttl,
        "ttl_secs doit correspondre au TTL Service configuré ({expected_ttl}s)"
    );
    // R-A1 : TTL Service = 86400s.
    // Le TTL éphémère de test peut être différent si JwtService::new_ephemeral change —
    // on vérifie la cohérence plutôt qu'une valeur hardcodée.
    assert!(parsed.ttl_secs > 0, "ttl_secs doit être positif");
}
