//! Tests d'intégration C1 (F-63) — substrat multi-vault `VaultGrant`.
//!
//! ## Périmètre (EX-C1-3, condition de sortie C1)
//!
//! - **Flag OFF (défaut)** : comportement byte-identical au legacy mono-vault —
//!   les tables `tenants`/`tenant_vault_grants` (migration 0030) ne sont JAMAIS
//!   consultées (prouvé : écriture OK même tables vidées). Le golden global reste
//!   la suite existante, qui tourne intégralement à flag OFF.
//! - **Flag ON** : pour CHAQUE chemin d'écriture (`vault_write`, `vault_forget`,
//!   `vault_downgrade`, `vault_restore`, enqueue `POST /jobs`, archivage F-100
//!   admin delete), une écriture cross-vault non autorisée est REFUSÉE 403 :
//!   tenant sans grant (middleware), grant read-only (write-path), body divergent.
//! - **A8** : `Unauthenticated` re-validé fail-closed dans le chemin lookup (403
//!   au middleware, au lieu du 401 handler du chemin legacy).
//!
//! Fixtures : tenant `main` = seed migration 0030 (write) · `reader` = grant
//! read-only seedé par le test · `ghost` = aucun grant.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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

// ── Preset ACL ────────────────────────────────────────────────────────────────
//
// Chaque consumer a l'ACL Write sur son locus ET sur `main/jobs` (locus fixe de
// `POST /jobs`) : les refus testés à flag ON viennent donc du GRANT, pas de l'ACL.
const TEST_ACL_C1: &str = r#"
[[consumer]]
identity = "writer-main"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main", "main/jobs"]

[[consumer]]
identity = "writer-reader"
read_patterns  = ["reader/*", "reader/main"]
write_patterns = ["reader/*", "reader/main", "main/jobs"]

[[consumer]]
identity = "writer-ghost"
read_patterns  = ["ghost/*", "ghost/main"]
write_patterns = ["ghost/*", "ghost/main", "main/jobs"]
"#;

// ── Fixture ───────────────────────────────────────────────────────────────────

struct C1Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// Construit un `AppState` avec index SQLite réel (migrations 0001-0030 appliquées,
/// seed `main`↔`main` write inclus), job_store câblé, et le flag `multi_tenant`.
async fn build_c1_env(multi_tenant_enabled: bool) -> C1Env {
    let dir = TempDir::new().expect("tempdir C1");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl =
        AclEngine::from_preset_str(TEST_ACL_C1).expect("preset ACL C1 valide — invariant statique");

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

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations 0001-0030")
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);

    C1Env {
        state,
        index_path,
        _dir: dir,
    }
}

/// Seed des tenants de test : `reader` (grant read-only sur son vault) ; `ghost`
/// n'est PAS seedé (aucune ligne = refus fail-closed).
fn seed_reader_grant(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed");
    conn.execute_batch(
        "INSERT INTO tenants (id, status, created_at) VALUES ('reader', 'active', 0);
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('reader', 'reader', 'read');",
    )
    .expect("seed reader read-only");
}

/// Routeur de test — même topologie que `main.rs` (`/api/v1` derrière `auth_middleware`).
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

/// Signe un JWT de test pour `(sub, tenant)` avec scopes read+write.
fn sign_jwt(state: &AppState, sub: &str, tenant: &str) -> String {
    state
        .jwt
        .sign(
            sub,
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign JWT test — clé éphémère valide")
}

/// POST JSON authentifié → status code.
async fn post_json(
    router: axum::Router,
    uri: &str,
    jwt: Option<&str>,
    body: serde_json::Value,
    idempotency_key: Option<&str>,
) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(token) = jwt {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("Idempotency-Key", key);
    }
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    router.oneshot(req).await.expect("service").status()
}

/// Body `vault_write` minimal pour `tenant_id`.
fn write_body(tenant: &str) -> serde_json::Value {
    serde_json::json!({
        "title": "note C1",
        "body": "corps de test C1",
        "tenant_id": tenant,
    })
}

// ── Flag OFF — byte-identical ─────────────────────────────────────────────────

/// Le flag est OFF par défaut (`ServerConfig::default()` et section TOML absente).
#[test]
fn multi_tenant_flag_default_off() {
    assert!(!ServerConfig::default().multi_tenant.enabled);
    let from_empty: MultiTenantConfig =
        serde_json::from_value(serde_json::json!({})).expect("deserialize section vide");
    assert!(!from_empty.enabled, "section absente → enabled=false");
}

/// Flag OFF : `vault_write` main → 202, MÊME avec les tables de grants vidées —
/// preuve que le chemin legacy ne consulte jamais l'allow-list (byte-identical).
#[tokio::test]
async fn flag_off_write_main_ignores_grant_tables() {
    let env = build_c1_env(false).await;

    // Vider les tables 0030 : à flag OFF, cela ne doit RIEN changer.
    {
        let conn = rusqlite::Connection::open(&env.index_path).expect("open index.db");
        conn.execute_batch("DELETE FROM tenant_vault_grants; DELETE FROM tenants;")
            .expect("truncate grants");
    }

    let jwt = sign_jwt(&env.state, "writer-main", "main");
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        Some(&jwt),
        write_body("main"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "flag OFF : écriture main 202, tables de grants jamais consultées"
    );
}

// ── Flag ON — chemin nominal ──────────────────────────────────────────────────

/// Flag ON : le seed `main`↔`main` write (migration 0030) autorise l'écriture main.
#[tokio::test]
async fn flag_on_write_main_seeded_grant_accepted() {
    let env = build_c1_env(true).await;
    let jwt = sign_jwt(&env.state, "writer-main", "main");
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        Some(&jwt),
        write_body("main"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "flag ON : grant write seedé → écriture main acceptée"
    );
}

// ── Flag ON — refus cross-vault par chemin d'écriture (EX-C1-3) ───────────────
//
// Le tenant `reader` détient un grant READ-ONLY sur son vault : il franchit le
// middleware (allow-list non vide) mais CHAQUE chemin d'écriture doit refuser 403
// (grant sans write). C'est le refus « écriture cross-vault non autorisée ».

/// `vault_write` : grant read-only → 403.
#[tokio::test]
async fn flag_on_vault_write_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        Some(&jwt),
        write_body("reader"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "vault_write read-only → 403");
}

/// `vault_forget` : grant read-only → 403.
#[tokio::test]
async fn flag_on_vault_forget_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    let body = serde_json::json!({
        "scope": { "type": "topic", "query": "test", "vault": "reader", "limit": 10 },
        "tenant_id": "reader",
    });
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_forget",
        Some(&jwt),
        body,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "vault_forget read-only → 403"
    );
}

/// `vault_downgrade` : grant read-only → 403.
#[tokio::test]
async fn flag_on_vault_downgrade_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    let body = serde_json::json!({
        "note_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "reason": "obsolete",
        "tenant_id": "reader",
    });
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_downgrade",
        Some(&jwt),
        body,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "vault_downgrade read-only → 403"
    );
}

/// `vault_restore` (history) : grant read-only → 403.
#[tokio::test]
async fn flag_on_vault_restore_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    let body = serde_json::json!({
        "note_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "ts_ms": 1,
        "tenant_id": "reader",
    });
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_restore",
        Some(&jwt),
        body,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "vault_restore read-only → 403"
    );
}

/// Enqueue `POST /jobs` : grant read-only → 403 (avant validation d'idempotence).
#[tokio::test]
async fn flag_on_create_job_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    let body = serde_json::json!({ "spec": { "kind": "Curate" } });
    let status = post_json(
        build_router(env.state),
        "/api/v1/jobs",
        Some(&jwt),
        body,
        Some("c1-idem-key"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "POST /jobs read-only → 403");
}

/// Tenant sans AUCUN grant (`ghost`) : refusé 403 dès le middleware (allow-list vide).
#[tokio::test]
async fn flag_on_tenant_without_grant_refused_at_middleware() {
    let env = build_c1_env(true).await;
    let jwt = sign_jwt(&env.state, "writer-ghost", "ghost");
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        Some(&jwt),
        write_body("ghost"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "tenant sans grant → 403 middleware"
    );
}

/// Body `tenant_id` divergent du JWT : 403 (le tenant est dérivé du JWT, jamais du body).
#[tokio::test]
async fn flag_on_divergent_body_tenant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-main", "main");
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        Some(&jwt),
        write_body("reader"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "body tenant divergent du JWT → 403"
    );
}

/// A8 + P2-c (C2) : `Unauthenticated` reste re-validé FAIL-CLOSED au middleware à
/// flag ON (jamais de handler atteint), mais le statut est **401** depuis C2 —
/// credentials absents = 401, aligné sur le chemin legacy (dette 401/403 soldée).
#[tokio::test]
async fn flag_on_unauthenticated_denied_at_middleware() {
    let env = build_c1_env(true).await;
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        None,
        write_body("main"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "flag ON : Unauthenticated refusé au middleware, statut 401 (A8 + P2-c)"
    );
}

/// Contrôle legacy : à flag OFF, la même requête non authentifiée rend 401 (handler)
/// — le 403 du test précédent est bien un comportement du chemin lookup uniquement.
#[tokio::test]
async fn flag_off_unauthenticated_still_401() {
    let env = build_c1_env(false).await;
    let status = post_json(
        build_router(env.state),
        "/api/v1/vault_write",
        None,
        write_body("main"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "flag OFF : Unauthenticated → 401 handler (byte-identical legacy)"
    );
}

// ── Flag ON — archivage F-100 (admin delete, identité synthétique) ────────────

/// Archivage F-100 (`/internal/v1/admin/delete`) : à flag ON, même l'identité
/// admin synthétique exige un grant write sur le vault cible — `reader` read-only
/// → 403 (l'archivage est une écriture, EX-C1-3).
#[tokio::test]
async fn flag_on_admin_delete_readonly_grant_refused() {
    use secrecy::SecretString;

    const ADMIN_TOKEN: &str = "test-admin-token-c1";

    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let state = env
        .state
        .with_admin_api_token(SecretString::from(ADMIN_TOKEN.to_owned()));
    let router = gradatum_server::internal::build_internal_router(state);

    let body = serde_json::json!({
        "note_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "tenant_id": "reader",
        "dry_run": true,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/internal/v1/admin/delete")
        .header("Content-Type", "application/json")
        .header("X-Gradatum-Admin", format!("Bearer {ADMIN_TOKEN}"))
        .extension(axum::extract::ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            12345,
        )))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build admin delete request");

    let resp = router.oneshot(req).await.expect("service admin delete");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "admin delete sur tenant read-only à flag ON → 403 (archivage = écriture)"
    );
}
