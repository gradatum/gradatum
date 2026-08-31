//! T9 (A3-handlers) — câblage read-path des 5 handlers `target_vault()` → vault effectif.
//!
//! ## Périmètre (RÈGLE READ-PATH OFF-GATING)
//!
//! Chaque handler GET non body-scopé (`dashboard`, `review`, `project-map`, `jobs`,
//! `system`) résout son vault via [`resolve_read_vault`], gaté sur `multi_tenant.enabled` :
//!
//! - **Flag OFF (défaut, LIVE)** : chemin legacy inline **byte-identical** — ACL Read sur
//!   `main/<section>`, AUCUN grant consulté. Prouvé : succès MÊME tables de grants vidées,
//!   JAMAIS `403 Forbidden` (verrou anti-régression LIVE).
//! - **Flag ON** : `effective_read_vault` sur le vault PROPRE du principal JWT (ACL cible +
//!   grant read + statut actif). Les lectures data sont scopées sur le vault effectif
//!   (`dashboard`/`review`/`project-map`) ou l'ACL/grant est enforced (`jobs`/`system`,
//!   store global / données scopées par principal).
//!
//! Le régime `multi_tenant.enabled = true` est LOCAL au harnais (flip INTERDIT LIVE).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

// `reader` a l'ACL Read sur `main/*` ET `vault-b/*` : à ON, un refus vient donc du GRANT
// (allow-list), jamais de l'ACL — on isole le comportement read-path câblé par T9.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "reader"
read_patterns  = ["main/*", "main/main", "vault-b/*", "vault-b/main"]
write_patterns = ["main/*", "main/main", "vault-b/*", "vault-b/main"]
"#;

struct Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// `AppState` avec index SQLite réel (migrations 0001-0030+, seed `main`↔`main`), job_store
/// câblé et flag `multi_tenant` paramétrable.
async fn build_env(multi_tenant_enabled: bool) -> Env {
    let dir = TempDir::new().expect("tempdir T9");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL T9 valide");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs");
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
        .expect("SqliteIndex::open — migrations")
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);

    Env {
        state,
        index_path,
        _dir: dir,
    }
}

/// Enregistre le vault secondaire `vault-b` : tenant actif + self-grant write (couvre read).
fn seed_vault_b_registration(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed vault-b");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('vault-b', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('vault-b', 'vault-b', 'write');"
    ))
    .expect("seed vault-b registration");
}

/// Sème une note dans `vault` (colonne `notes`) — suffit pour count/list (pas de FTS requis).
fn seed_note(
    index_path: &std::path::Path,
    ulid: &str,
    vault: &str,
    section: &str,
    status: &str,
    title: &str,
    body: &str,
) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed note");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
         VALUES (?1, ?2, NULL, ?3, ?4, 1, ?5, X'00', ?6, ?7)",
        rusqlite::params![ulid, vault, section, status, now, body, title],
    )
    .expect("seed note");
}

/// Vide les tables de grants (verrou OFF : le legacy ne doit jamais les consulter).
fn truncate_grants(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db truncate");
    conn.execute_batch("DELETE FROM tenant_vault_grants; DELETE FROM tenants;")
        .expect("truncate grants");
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

fn sign_jwt(state: &AppState, sub: &str, tenant: &str) -> String {
    state
        .jwt
        .sign(
            sub,
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            tenant,
        )
        .expect("sign JWT test")
}

/// GET authentifié → `(status, body_string)`.
async fn get_full(router: axum::Router, uri: &str, jwt: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = jwt {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).expect("build request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ── dashboard ────────────────────────────────────────────────────────────────

/// OFF : `/dashboard` réussit tables de grants VIDES → 200, jamais 403 (byte-identical LIVE).
#[tokio::test]
async fn dashboard_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    seed_note(
        &env.index_path,
        "01HMAINDASHAAAAAAAAAAAAAAA",
        "main",
        "decisions",
        "live",
        "M",
        "corps",
    );
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), "/api/v1/dashboard", Some(&jwt)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : dashboard 200 tables grants vidées (aucun grant consulté), body : {body}"
    );
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
    assert!(
        body.contains("\"live\":1"),
        "OFF : compte main (1 live), body : {body}"
    );
}

/// ON : `/dashboard` d'un principal `vault-b` compte les notes de vault-b, pas de main.
#[tokio::test]
async fn dashboard_scoped_to_effective_vault() {
    let env = build_env(true).await;
    seed_vault_b_registration(&env.index_path);
    // main : 1 'deprecated' — vault-b : 2 'live'. Un dashboard scopé vault-b voit 2 live, 0 deprecated.
    seed_note(
        &env.index_path,
        "01HMAINX000000000000000000",
        "main",
        "decisions",
        "deprecated",
        "M",
        "c",
    );
    seed_note(
        &env.index_path,
        "01HVAULTB0000000000000001A",
        "vault-b",
        "decisions",
        "live",
        "B1",
        "c",
    );
    seed_note(
        &env.index_path,
        "01HVAULTB0000000000000002B",
        "vault-b",
        "reference",
        "live",
        "B2",
        "c",
    );
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(build_router(env.state), "/api/v1/dashboard", Some(&jwt)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : dashboard vault-b 200, body : {body}"
    );
    assert!(
        body.contains("\"live\":2"),
        "ON : 2 live de vault-b, body : {body}"
    );
    assert!(
        !body.contains("deprecated"),
        "ON : la note 'deprecated' de main NE doit PAS fuiter dans vault-b, body : {body}"
    );
}

// ── review ───────────────────────────────────────────────────────────────────

/// OFF : `/review` réussit tables de grants VIDES → 200, jamais 403.
#[tokio::test]
async fn review_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), "/api/v1/review", Some(&jwt)).await;
    assert_eq!(status, StatusCode::OK, "OFF : review 200, body : {body}");
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
}

/// ON : `/review` d'un principal `vault-b` remonte la note pending-review de vault-b, pas de main.
#[tokio::test]
async fn review_scoped_to_effective_vault() {
    let env = build_env(true).await;
    seed_vault_b_registration(&env.index_path);
    // ULID Crockford-valides (list_review_queue rejette les non-ULID).
    seed_note(
        &env.index_path,
        "01HA0000000000000000000004",
        "main",
        "decisions",
        "pending-review",
        "MREV",
        "c",
    );
    seed_note(
        &env.index_path,
        "01HB0000000000000000000003",
        "vault-b",
        "decisions",
        "pending-review",
        "BREV",
        "c",
    );
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(build_router(env.state), "/api/v1/review", Some(&jwt)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : review vault-b 200, body : {body}"
    );
    assert!(
        body.contains("01HB0000000000000000000003"),
        "ON : la note review de vault-b remonte, body : {body}"
    );
    assert!(
        !body.contains("01HA0000000000000000000004"),
        "ON : la note review de main NE doit PAS fuiter dans vault-b, body : {body}"
    );
}

// ── project-map ──────────────────────────────────────────────────────────────

/// OFF : `/project-map/export-features` réussit tables vidées → 200, jamais 403.
#[tokio::test]
async fn project_map_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(
        build_router(env.state),
        "/api/v1/project-map/export-features",
        Some(&jwt),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : project-map 200, body : {body}"
    );
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
}

/// ON : `/project-map/export-features` d'un principal `vault-b` expose la feature de vault-b.
#[tokio::test]
async fn project_map_scoped_to_effective_vault() {
    let env = build_env(true).await;
    seed_vault_b_registration(&env.index_path);
    // Carte-feature project-map (wikilinks typés forcés — format `project_map_feature_entries`).
    seed_note(
        &env.index_path,
        "01HB0000000000000000000004",
        "vault-b",
        "project-map",
        "live",
        "Feature vault-b",
        "[[feature:F-999]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
         [[release:released]] [[version:gradatum/v1.0.0]]",
    );
    // Feature dans main qui ne doit PAS apparaître pour vault-b.
    seed_note(
        &env.index_path,
        "01HA0000000000000000000005",
        "main",
        "project-map",
        "live",
        "Feature main",
        "[[feature:F-111]] [[project:gradatum]] [[status:DONE]] [[kind:FEATURE]] \
         [[release:released]] [[version:gradatum/v1.0.0]]",
    );
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(
        build_router(env.state),
        "/api/v1/project-map/export-features",
        Some(&jwt),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : project-map vault-b 200, body : {body}"
    );
    assert!(
        body.contains("F-999"),
        "ON : feature vault-b présente, body : {body}"
    );
    assert!(
        !body.contains("F-111"),
        "ON : la feature de main NE doit PAS fuiter dans vault-b, body : {body}"
    );
}

// ── jobs (store global — l'ACL/grant est câblé, pas le vault des data) ─────────

/// OFF : `/jobs` réussit tables vidées → 200, jamais 403 (byte-identical).
#[tokio::test]
async fn jobs_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), "/api/v1/jobs", Some(&jwt)).await;
    assert_eq!(status, StatusCode::OK, "OFF : jobs 200, body : {body}");
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
}

/// ON : `/jobs` d'un principal `vault-b` avec self-grant → 200 (ACL cible + grant enforced).
/// Contrôle négatif : sans grant, la lecture serait refusée (fermeture du hardcode `main/jobs`).
#[tokio::test]
async fn jobs_scoped_to_effective_vault() {
    let env = build_env(true).await;
    seed_vault_b_registration(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(build_router(env.state), "/api/v1/jobs", Some(&jwt)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : jobs vault-b 200 via ACL `vault-b/jobs` + self-grant, body : {body}"
    );
}

// ── system ───────────────────────────────────────────────────────────────────

/// OFF : `/system/scheduled` réussit tables vidées → 200, jamais 403.
#[tokio::test]
async fn system_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(
        build_router(env.state),
        "/api/v1/system/scheduled",
        Some(&jwt),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : system/scheduled 200, body : {body}"
    );
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
}

/// ON : `/system/scheduled` d'un principal `vault-b` avec self-grant → 200 (ACL/grant câblé).
#[tokio::test]
async fn system_scoped_to_effective_vault() {
    let env = build_env(true).await;
    seed_vault_b_registration(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(
        build_router(env.state),
        "/api/v1/system/scheduled",
        Some(&jwt),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : system/scheduled vault-b 200 via ACL `vault-b/dashboard` + self-grant, body : {body}"
    );
}
