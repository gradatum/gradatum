//! Tests d'intégration C3a (F-45, EX-C3a-1) — enforcement des scopes sur les
//! chemins d'ÉCRITURE.
//!
//! ## Modèle
//!
//! - `multi_tenant.enabled = false` (défaut) : AUCUN enforcement (byte-identical —
//!   un token `["read"]` peut encore écrire, comportement historique du parc).
//! - `enabled = true` : un `BearerToken` doit porter au moins un scope de
//!   `WRITE_SCOPES = {write, admin, service}` pour emprunter un chemin write —
//!   sinon 403, quel que soit son grant. Une clé `["read"]` est donc
//!   **lecture-seule stricte** (pré-requis council accès distant : la clé
//!   connecteur claude.ai sera read-only avant toute exposition C3b).
//!
//! Les chemins de lecture restent ouverts au token read-only (vault_search 200).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use tower::ServiceExt;

// `writer-main` : ACL Read + Write pleines sur main — les refus testés à ON
// viennent du SCOPE du token, jamais de l'ACL ni du grant (seed 0030 main↔main write).
const TEST_ACL_SCOPES: &str = r#"
[[consumer]]
identity = "writer-main"
read_patterns  = ["main/*", "main/main", "main/timeline"]
write_patterns = ["main/*", "main/main", "main/session-log", "main/event-log"]
"#;

struct ScopeEnv {
    state: AppState,
    _dir: TempDir,
}

/// `AppState` avec Vault réel + index partagé (seed migration 0030 `main`↔`main`
/// write) et flag `multi_tenant` paramétrable.
async fn build_scope_env(multi_tenant_enabled: bool) -> ScopeEnv {
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let dir = TempDir::new().expect("tempdir scopes");
    let vault_dir = dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_SCOPES)
        .expect("preset ACL scopes valide — invariant statique");

    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let cfg = ServerConfig {
        multi_tenant: MultiTenantConfig {
            enabled: multi_tenant_enabled,
        },
        ..ServerConfig::default()
    };

    let idx = vault.index().clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault as Arc<dyn gradatum_vault::Registry>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);
    state.search = idx as Arc<dyn gradatum_core::index::Index>;

    ScopeEnv { state, _dir: dir }
}

fn build_router(state: AppState) -> axum::Router {
    use axum::{Router, middleware};

    Router::new()
        .nest("/api/v1", gradatum_server::api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            gradatum_server::middleware::auth_middleware,
        ))
        .with_state(state)
}

/// Signe un JWT `writer-main`/`main` avec les scopes donnés.
fn sign_jwt_scopes(state: &AppState, scopes: &[&str]) -> String {
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
    state
        .jwt
        .sign("writer-main", &scopes, TokenScope::Service, "main")
        .expect("sign JWT test — clé éphémère valide")
}

async fn post_json(
    router: axum::Router,
    uri: &str,
    jwt: &str,
    body: serde_json::Value,
) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    router.oneshot(req).await.expect("service").status()
}

/// Batterie des chemins d'écriture vault-notes : `(uri, body JSON valide)`.
fn write_surfaces() -> Vec<(&'static str, serde_json::Value)> {
    let ulid = "01HFAKEULIDAAAAAAAAAAAAAAA";
    vec![
        (
            "/api/v1/vault_write",
            serde_json::json!({ "title": "t", "body": "b", "tenant_id": "main" }),
        ),
        (
            "/api/v1/vault_downgrade",
            serde_json::json!({ "note_id": ulid, "reason": "test", "tenant_id": "main" }),
        ),
        (
            "/api/v1/vault_restore",
            serde_json::json!({ "note_id": ulid, "ts_ms": 1, "tenant_id": "main" }),
        ),
        (
            "/api/v1/vault_forget",
            serde_json::json!({
                "scope": { "type": "topic", "query": "rien" },
                "dry_run": false,
                "tenant_id": "main"
            }),
        ),
    ]
}

/// Requête authentifiée méthode-aware → `StatusCode` (couvre PATCH pour `/notes/{id}`).
async fn request_scoped(
    router: axum::Router,
    method: &str,
    uri: &str,
    jwt: &str,
    body: serde_json::Value,
) -> StatusCode {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    router.oneshot(req).await.expect("service").status()
}

/// Batterie ÉTENDUE des chemins d'écriture (au-delà de `write_surfaces`) : mutations par
/// ULID (patch/move) + append-only (event-log/session-log) + jobs (create/cancel).
/// `(method, uri, body JSON valide côté DTO — le refus vient du scope, pas du parsing)`.
fn extended_write_surfaces() -> Vec<(&'static str, String, serde_json::Value)> {
    let ulid = ulid::Ulid::generate().to_string();
    vec![
        (
            "PATCH",
            format!("/api/v1/notes/{ulid}"),
            serde_json::json!({ "status_reason": "x" }),
        ),
        (
            "POST",
            format!("/api/v1/notes/{ulid}/move"),
            serde_json::json!({ "locus": "knowledge" }),
        ),
        (
            "POST",
            "/api/v1/event-log".to_owned(),
            serde_json::json!([]),
        ),
        (
            "POST",
            "/api/v1/session-log/trace".to_owned(),
            serde_json::json!({ "ts_ms": 1, "action_type": "plan" }),
        ),
        (
            "POST",
            "/api/v1/jobs".to_owned(),
            serde_json::json!({ "spec": {} }),
        ),
        (
            "POST",
            format!("/api/v1/jobs/{ulid}/cancel"),
            serde_json::json!({}),
        ),
        (
            "POST",
            "/api/v1/vault_forget".to_owned(),
            serde_json::json!({
                "scope": { "type": "topic", "query": "rien" },
                "dry_run": false,
                "tenant_id": "main"
            }),
        ),
    ]
}

// ── ON : token read-only strictement lecture-seule ───────────────────────────

/// ON : un token `["read"]` est refusé (403) sur CHAQUE chemin d'écriture
/// vault-notes, avant toute mutation — enforcement du scope, pas de l'ACL.
#[tokio::test]
async fn flag_on_read_only_token_refused_on_all_write_paths() {
    let env = build_scope_env(true).await;
    let jwt = sign_jwt_scopes(&env.state, &["read"]);
    for (uri, body) in write_surfaces() {
        let status = post_json(build_router(env.state.clone()), uri, &jwt, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{uri} avec token read-only doit être 403 (EX-C3a-1)"
        );
    }
}

/// ON : le token read-only est refusé (403) sur CHAQUE chemin d'écriture ÉTENDU —
/// mutations par ULID (patch/move), append-only (event-log/session-log), jobs
/// (create/cancel), forget. Le refus est celui du SCOPE (write manquant), avant toute
/// mutation — clôt la couverture partielle (P1 security review).
#[tokio::test]
async fn flag_on_read_only_token_refused_on_extended_write_paths() {
    let env = build_scope_env(true).await;
    let jwt = sign_jwt_scopes(&env.state, &["read"]);
    for (method, uri, body) in extended_write_surfaces() {
        let status =
            request_scoped(build_router(env.state.clone()), method, &uri, &jwt, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} avec token read-only doit être 403 (EX-C3a-1)"
        );
    }
}

/// ON : le même token read-only LIT normalement (vault_search 200) — la clé
/// lecture-seule stricte est utilisable, pas bloquée globalement.
#[tokio::test]
async fn flag_on_read_only_token_can_still_read() {
    let env = build_scope_env(true).await;
    let jwt = sign_jwt_scopes(&env.state, &["read"]);
    let status = post_json(
        build_router(env.state.clone()),
        "/api/v1/vault_search",
        &jwt,
        serde_json::json!({ "query": "anything", "tenant_id": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "lecture 200 avec token read-only");
}

/// ON : un token portant `write` passe le gate scope — `vault_write` aboutit
/// (202, le grant seed `main`↔`main` write couvre l'écriture).
#[tokio::test]
async fn flag_on_write_scope_token_can_write() {
    let env = build_scope_env(true).await;
    let jwt = sign_jwt_scopes(&env.state, &["read", "write"]);
    let status = post_json(
        build_router(env.state.clone()),
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({ "title": "t", "body": "b", "tenant_id": "main" }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "scope write → 202 enqueue");
}

/// ON : `service` et `admin` sont des scopes autorisant l'écriture (parc
/// historique : mcp-stub/service, clés opérateur/admin).
#[tokio::test]
async fn flag_on_service_and_admin_scopes_can_write() {
    for scope in ["service", "admin"] {
        let env = build_scope_env(true).await;
        let jwt = sign_jwt_scopes(&env.state, &[scope]);
        let status = post_json(
            build_router(env.state.clone()),
            "/api/v1/vault_write",
            &jwt,
            serde_json::json!({ "title": "t", "body": "b", "tenant_id": "main" }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "scope '{scope}' doit autoriser l'écriture (202)"
        );
    }
}

// ── OFF : byte-identical, aucun enforcement ──────────────────────────────────

/// OFF : un token `["read"]` écrit encore (202) — l'enforcement de scope est
/// STRICTEMENT gated par le flag (aucun changement de comportement du parc).
#[tokio::test]
async fn flag_off_read_only_token_still_writes_byte_identical() {
    let env = build_scope_env(false).await;
    let jwt = sign_jwt_scopes(&env.state, &["read"]);
    let status = post_json(
        build_router(env.state.clone()),
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({ "title": "t", "body": "b", "tenant_id": "main" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "OFF : aucun enforcement de scope (byte-identical)"
    );
}
