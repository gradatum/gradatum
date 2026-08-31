//! Tests d'intégration C3a (F-45) — clôture du gate flag-ON hérité de l'audit C2.
//!
//! ## Périmètre
//!
//! - **P1-1** : `vault_archives_list` intégré au modèle d'isolation —
//!   - flag ON : routé par `effective_read_vault` (ACL cible + grant read + cible
//!     active, fail-closed) ;
//!   - flag OFF : garde mono-vault `vault_filter ≠ "main"` → 403 (parité
//!     `vault_search`/`vault_timeline`) ;
//!   - sur les DEUX chemins le filtre registre est épinglé au vault vérifié —
//!     `vault_filter = None` ne signifie plus « scan tous vaults ».
//! - **P2-1** : cross-read vers un vault **soft-deleted** refusé (search, timeline,
//!   archives) + isolation cross-vault du listing d'archives.
//!
//! Fixture : même topologie que `isolation_c2.rs` (tenant `main` seed migration 0030,
//! vault `research` actif + self-grant, grant `main → research` ajouté au cas par cas).

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

// ── Preset ACL ────────────────────────────────────────────────────────────────
//
// `reader-main` a l'ACL Read sur `main/*` ET `research/*` : à flag ON, les refus
// testés viennent du GRANT ou du STATUT de la cible, jamais de l'ACL — c'est le
// modèle EX-C2-1 (l'ACL de l'appelant seule ne gouverne plus le cross-read).
const TEST_ACL_C3A: &str = r#"
[[consumer]]
identity = "reader-main"
read_patterns  = ["main/*", "main/main", "main/timeline", "research/*", "research/main", "research/timeline"]
write_patterns = ["main/*", "main/main"]
"#;

// ── Fixture ───────────────────────────────────────────────────────────────────

struct C3aEnv {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// `AppState` avec Vault réel (le listing d'archives passe par `state.vault`) dont
/// l'index SQLite (migrations complètes, seed `main`↔`main`) est PARTAGÉ avec
/// `state.search` ; job_store câblé et flag `multi_tenant` paramétrable.
async fn build_c3a_env(multi_tenant_enabled: bool) -> C3aEnv {
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let dir = TempDir::new().expect("tempdir C3a");
    let vault_dir = dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );
    // Chemin canonique de l'index du vault — cible des seeds rusqlite.
    let index_path = gradatum_core::paths::vault_dir_index_path(&vault_dir);

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_C3A)
        .expect("preset ACL C3a valide — invariant statique");

    let jobs_pool = QueueDb::open_in_memory()
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

    C3aEnv {
        state,
        index_path,
        _dir: dir,
    }
}

/// Seed du vault cible `research` : tenant actif + self-grant write + 1 note FTS +
/// 1 archive au registre. Une archive `main` est aussi seedée pour prouver
/// l'isolation du listing (chaque vault ne voit que ses lignes).
fn seed_vaults_with_archives(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('research', 'research', 'write');
         INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
           VALUES ('01HRESEARCHAAAAAAAAAAAAAAA', 'research', NULL, 'reference', 'live', 1, {now}, X'00', 'isolation gravity probe corpus', 'Gravity Probe');
         INSERT INTO notes_fts (rowid, body_text)
           SELECT rowid, body_text FROM notes WHERE id = '01HRESEARCHAAAAAAAAAAAAAAA';
         INSERT INTO archive_index (note_id, vault_id, section, title, archive_path, archived_at, gc_due)
           VALUES ('01HARCMAINAAAAAAAAAAAAAAAA', 'main', 'reference', 'Main archived',
                   '.archive/main/01HARCMAINAAAAAAAAAAAAAAAA.md', {now}, {gc});
         INSERT INTO archive_index (note_id, vault_id, section, title, archive_path, archived_at, gc_due)
           VALUES ('01HARCRESEARCHAAAAAAAAAAAA', 'research', 'reference', 'Research archived',
                   '.archive/research/01HARCRESEARCHAAAAAAAAAAAA.md', {now}, {gc});",
        gc = now + 60 * 24 * 3600 * 1000,
    ))
    .expect("seed vaults + archives");
}

/// Ajoute le grant cross-vault `main → research` avec l'accès donné (`read`/`write`).
fn grant_main_on_research(index_path: &std::path::Path, access: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db grant");
    conn.execute(
        "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES ('main', 'research', ?1)",
        rusqlite::params![access],
    )
    .expect("grant main→research");
}

/// Passe le tenant `research` au statut donné (`suspended` / `deleted`).
fn set_research_status(index_path: &std::path::Path, status: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db status");
    conn.execute(
        "UPDATE tenants SET status = ?1 WHERE id = 'research'",
        rusqlite::params![status],
    )
    .expect("update research status");
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

/// POST JSON authentifié → `(status, body_string)`.
async fn post_json_full(
    router: axum::Router,
    uri: &str,
    jwt: Option<&str>,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(token) = jwt {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = builder
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Body `vault_archives_list` ciblant `vault_filter`.
fn archives_body(vault_filter: Option<&str>) -> serde_json::Value {
    let mut b = serde_json::json!({ "tenant_id": "main" });
    if let Some(v) = vault_filter {
        b["vault_filter"] = serde_json::Value::String(v.to_owned());
    }
    b
}

// ── P1-1 — flag OFF : garde mono-vault (parité search/timeline) ──────────────

/// OFF : `vault_filter = "research"` → 403 mono-vault (le trou « filtre brut passé
/// au registre » est fermé aussi à OFF).
#[tokio::test]
async fn flag_off_archives_cross_vault_forbidden() {
    let env = build_c3a_env(false).await;
    seed_vaults_with_archives(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "OFF cross-listing (garde mono-vault) : {body}"
    );
}

/// OFF : `vault_filter` vide → 400 (validation frontière, parité search).
#[tokio::test]
async fn flag_off_archives_empty_vault_filter_bad_request() {
    let env = build_c3a_env(false).await;
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(Some("")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "OFF : filtre vide → 400");
}

/// OFF : `vault_filter = None` est ÉPINGLÉ au vault propre — les lignes du registre
/// appartenant à un autre vault ne fuient plus (avant le fix : scan tous vaults).
#[tokio::test]
async fn flag_off_archives_none_filter_pinned_to_own_vault() {
    let env = build_c3a_env(false).await;
    seed_vaults_with_archives(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "OFF listing propre : {body}");
    assert!(
        body.contains("Main archived"),
        "l'archive du vault propre doit apparaître : {body}"
    );
    assert!(
        !body.contains("Research archived"),
        "l'archive d'un vault tiers ne doit PAS fuir (pinning) : {body}"
    );
}

// ── P1-1 — flag ON : effective_read_vault (grant + active + ACL cible) ───────

/// ON : cross-listing SANS grant sur la cible → 403 `no read grant` (fail-closed).
#[tokio::test]
async fn flag_on_archives_cross_vault_without_grant_forbidden() {
    let env = build_c3a_env(true).await;
    seed_vaults_with_archives(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON sans grant → 403 fail-closed : {body}"
    );
}

/// ON : cross-listing AVEC read-grant → 200 et remonte les archives du vault CIBLE
/// uniquement (le filtre est épinglé à la cible vérifiée).
#[tokio::test]
async fn flag_on_archives_cross_vault_with_grant_lists_target_only() {
    let env = build_c3a_env(true).await;
    seed_vaults_with_archives(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(Some("research")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ON avec grant : {body}");
    assert!(
        body.contains("Research archived"),
        "les archives de la cible doivent apparaître : {body}"
    );
    assert!(
        !body.contains("Main archived"),
        "les archives d'un autre vault ne doivent pas fuir : {body}"
    );
}

/// ON : `vault_filter = None` → vault propre du tenant, jamais un scan global.
#[tokio::test]
async fn flag_on_archives_none_filter_pinned_to_own_vault() {
    let env = build_c3a_env(true).await;
    seed_vaults_with_archives(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ON listing propre : {body}");
    assert!(body.contains("Main archived"), "vault propre : {body}");
    assert!(
        !body.contains("Research archived"),
        "pinning au vault propre — pas de scan global : {body}"
    );
}

/// ON : `vault_filter` mal formé → 400 (`VaultId::parse`, frontière non fiable).
#[tokio::test]
async fn flag_on_archives_malformed_vault_filter_bad_request() {
    let env = build_c3a_env(true).await;
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_archives_list",
        Some(&jwt),
        archives_body(Some("../evil")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ON : filtre mal formé → 400"
    );
}

// ── P2-1 — cross-read vers un vault soft-DELETED (search / timeline / archives) ──

/// ON : un vault soft-deleted cesse d'être lisible IMMÉDIATEMENT, y compris par un
/// tenant détenteur d'un grant read — sur les TROIS surfaces de lecture cross-vault.
#[tokio::test]
async fn flag_on_cross_read_to_deleted_vault_forbidden_all_surfaces() {
    let env = build_c3a_env(true).await;
    seed_vaults_with_archives(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    set_research_status(&env.index_path, "deleted");
    let jwt = sign_jwt(&env.state, "reader-main", "main");

    let surfaces: [(&str, serde_json::Value); 3] = [
        (
            "/api/v1/vault_search",
            serde_json::json!({ "query": "gravity probe", "tenant_id": "main", "vault_id": "research" }),
        ),
        (
            "/api/v1/vault_timeline",
            serde_json::json!({ "tenant_id": "main", "vault_id": "research" }),
        ),
        (
            "/api/v1/vault_archives_list",
            archives_body(Some("research")),
        ),
    ];

    for (uri, body) in surfaces {
        let (status, resp) =
            post_json_full(build_router(env.state.clone()), uri, Some(&jwt), body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{uri} vers vault deleted doit être 403 : {resp}"
        );
    }
}
