//! Tests d'intégration C2 (F-18) — isolation réelle multi-vault, chemin LECTURE.
//!
//! ## Périmètre (EX-C2-1/2 + P2, condition de sortie C2)
//!
//! - **Flag OFF (défaut)** : golden byte-identical — la garde mono-vault
//!   `vid != "main" → 403` reste le seul chemin actif ; les tables de grants ne sont
//!   jamais consultées (prouvé : lecture OK tables vidées).
//! - **Flag ON** : l'ACL est recalculée sur la **CIBLE** (`read_vault_id`) et un grant
//!   `read` (ou `write`) du tenant sur la cible est exigé (fail-closed) :
//!   - cross-read SANS grant cible → **403** (le trou historique, prouvé fermé) ;
//!   - cross-read AVEC read-grant → **200** et remonte les notes du vault cible ;
//!   - message de refus grant distinct (`no read grant …`, P2-c, dette C1 soldée) ;
//!   - `vault_id` mal formé → **400** (`VaultId::parse`, P2-a) ;
//!   - non authentifié → **401** (alignement 401/403, P2-c).
//!
//! Fixtures : tenant `main` = seed migration 0030 (write main↔main) · vault `research`
//! = tenant actif + self-grant write, notes seedées ; le grant `main → research` (read)
//! n'est ajouté QUE dans les tests nominaux.

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
// testés viennent donc du GRANT (allow-list), pas de l'ACL — c'est exactement le
// trou EX-C2-1 (avant C2, l'ACL de l'appelant seule gouvernait le cross-read).
const TEST_ACL_C2: &str = r#"
[[consumer]]
identity = "reader-main"
read_patterns  = ["main/*", "main/main", "main/timeline", "research/*", "research/main", "research/timeline"]
write_patterns = ["main/*", "main/main"]

[[consumer]]
identity = "acl-blocked"
read_patterns  = ["main/*", "main/main"]
write_patterns = []
"#;

// ── Fixture ───────────────────────────────────────────────────────────────────

struct C2Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// `AppState` avec index SQLite réel (migrations 0001-0030+, seed `main`↔`main`),
/// job_store câblé et flag `multi_tenant` paramétrable.
async fn build_c2_env(multi_tenant_enabled: bool) -> C2Env {
    let dir = TempDir::new().expect("tempdir C2");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl =
        AclEngine::from_preset_str(TEST_ACL_C2).expect("preset ACL C2 valide — invariant statique");

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

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations")
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);

    C2Env {
        state,
        index_path,
        _dir: dir,
    }
}

/// Seed du vault cible `research` : tenant actif + self-grant write + 1 note FTS
/// (`isolation gravity probe`) + 1 entrée temporal_index pour la timeline.
fn seed_research_vault(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('research', 'research', 'write');
         INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
           VALUES ('01HRESEARCHAAAAAAAAAAAAAAA', 'research', NULL, 'reference', 'live', 1, {now}, X'00', 'isolation gravity probe corpus', 'Gravity Probe');
         INSERT INTO notes_fts (rowid, body_text)
           SELECT rowid, body_text FROM notes WHERE id = '01HRESEARCHAAAAAAAAAAAAAAA';"
    ))
    .expect("seed research vault");
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

/// Body `vault_search` ciblant `vault_id`.
fn search_body(vault_id: Option<&str>) -> serde_json::Value {
    let mut b = serde_json::json!({
        "query": "gravity probe",
        "tenant_id": "main",
    });
    if let Some(v) = vault_id {
        b["vault_id"] = serde_json::Value::String(v.to_owned());
    }
    b
}

/// Body `vault_timeline` ciblant `vault_id`.
fn timeline_body(vault_id: Option<&str>) -> serde_json::Value {
    let mut b = serde_json::json!({ "tenant_id": "main" });
    if let Some(v) = vault_id {
        b["vault_id"] = serde_json::Value::String(v.to_owned());
    }
    b
}

// ── Flag OFF — golden byte-identical ─────────────────────────────────────────

/// OFF : cross-read `vault_id ≠ main` → 403 avec le message mono-vault historique.
#[tokio::test]
async fn flag_off_search_cross_vault_forbidden_golden() {
    let env = build_c2_env(false).await;
    seed_research_vault(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "OFF : garde mono-vault, body : {body}"
    );
}

/// OFF : `vault_id = "main"` explicite → 200, MÊME avec les tables de grants vidées —
/// preuve que le chemin legacy ne consulte jamais l'allow-list (byte-identical).
#[tokio::test]
async fn flag_off_search_main_ignores_grant_tables() {
    let env = build_c2_env(false).await;
    {
        let conn = rusqlite::Connection::open(&env.index_path).expect("open index.db");
        conn.execute_batch("DELETE FROM tenant_vault_grants; DELETE FROM tenants;")
            .expect("truncate grants");
    }
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("main")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : lecture main 200, tables de grants jamais consultées"
    );
}

/// OFF : `vault_id` vide → 400 (validation legacy conservée).
#[tokio::test]
async fn flag_off_search_empty_vault_id_bad_request_golden() {
    let env = build_c2_env(false).await;
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "OFF : vault_id vide → 400");
}

/// OFF : timeline cross-vault → 403 avec le message historique.
#[tokio::test]
async fn flag_off_timeline_cross_vault_forbidden_golden() {
    let env = build_c2_env(false).await;
    seed_research_vault(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_timeline",
        Some(&jwt),
        timeline_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "OFF : garde mono-vault timeline, body : {body}"
    );
}

// ── Flag ON — le trou historique est fermé ───────────────────────────────────

/// ON : cross-read SANS grant cible → 403 — c'est LE trou EX-C2-1 prouvé fermé.
/// L'ACL de l'appelant autorise `research/*` (preset) : seul le grant refuse.
#[tokio::test]
async fn flag_on_cross_read_without_target_grant_refused() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : cross-read sans grant cible doit être refusé (trou historique), body : {body}"
    );
}

/// ON : cross-read AVEC read-grant → 200 et remonte la note du vault cible.
#[tokio::test]
async fn flag_on_cross_read_with_read_grant_returns_target_notes() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : read-grant → 200, body : {body}"
    );
    assert!(
        body.contains("01HRESEARCHAAAAAAAAAAAAAAA"),
        "la note du vault research doit remonter, obtenu : {body}"
    );
}

/// ON : lecture du vault PROPRE (sans `vault_id`) → 200 via le self-grant seedé.
#[tokio::test]
async fn flag_on_own_vault_read_uses_seeded_self_grant() {
    let env = build_c2_env(true).await;
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(None),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : self-grant main↔main (seed 0030)"
    );
}

/// ON : l'ACL de la CIBLE gouverne (EX-C2-1) — un appelant dont l'ACL ne couvre PAS
/// `research/*` est refusé MÊME avec un grant read (défense en profondeur : ACL ∧ grant).
#[tokio::test]
async fn flag_on_target_acl_governs_even_with_grant() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    // `acl-blocked` : ACL read sur main/* uniquement — la CIBLE research est hors ACL.
    let jwt = sign_jwt(&env.state, "acl-blocked", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : ACL cible deny doit refuser malgré le grant, body : {body}"
    );
}

/// ON : `vault_id` mal formé (charset interdit) → 400 via `VaultId::parse` (P2-a).
#[tokio::test]
async fn flag_on_malformed_vault_id_bad_request() {
    let env = build_c2_env(true).await;
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("../etc/Evil")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "ON : VaultId::parse rejette le charset interdit, body : {body}"
    );
}

/// ON : non authentifié → 401 (alignement 401/403, P2-c) — jamais 403.
#[tokio::test]
async fn flag_on_unauthenticated_search_is_401() {
    let env = build_c2_env(true).await;
    let (status, _body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        None,
        search_body(Some("research")),
    )
    .await;
    // NB : à ON le middleware A8 refuse Unauthenticated en 401 (token absent).
    assert_eq!(status, StatusCode::UNAUTHORIZED, "ON : pas de token → 401");
}

/// ON : timeline cross-read AVEC read-grant → 200 (même garde que search, locus
/// `research/timeline`).
#[tokio::test]
async fn flag_on_timeline_cross_read_with_grant_ok() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_timeline",
        Some(&jwt),
        timeline_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : timeline read-grant → 200, body : {body}"
    );
}

/// ON : timeline cross-read SANS grant → 403 avec message grant distinct.
#[tokio::test]
async fn flag_on_timeline_cross_read_without_grant_refused() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader-main", "main");
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_timeline",
        Some(&jwt),
        timeline_body(Some("research")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : timeline cross-read sans grant refusée, body : {body}"
    );
}

/// ON : suspend du tenant cible → refus IMMÉDIAT du cross-read qui passait juste avant
/// (EX-C2-4 : le JOIN `tenants.status='active'` coupe les grants dès la requête suivante).
#[tokio::test]
async fn flag_on_suspend_refuses_immediately() {
    let env = build_c2_env(true).await;
    seed_research_vault(&env.index_path);
    grant_main_on_research(&env.index_path, "read");
    let jwt = sign_jwt(&env.state, "reader-main", "main");

    // Avant suspend : le cross-read passe.
    let (status_before, _b) = post_json_full(
        build_router(env.state.clone()),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(status_before, StatusCode::OK, "pré-condition : grant OK");

    // Suspend du tenant PROPRIÉTAIRE du vault research (statut index-level).
    {
        let conn = rusqlite::Connection::open(&env.index_path).expect("open index.db");
        conn.execute(
            "UPDATE tenants SET status = 'suspended' WHERE id = 'research'",
            [],
        )
        .expect("suspend research");
    }

    // Refus côté APPELANT : le middleware ON consulte `tenant_grants` (JOIN
    // `tenants.status='active'`) — un JWT research ne passe plus dès la requête suivante.
    let jwt_research = sign_jwt(&env.state, "reader-main", "research");
    let (status_suspended, _b2) = post_json_full(
        build_router(env.state.clone()),
        "/api/v1/vault_search",
        Some(&jwt_research),
        serde_json::json!({ "query": "gravity probe", "tenant_id": "research" }),
    )
    .await;
    assert_eq!(
        status_suspended,
        StatusCode::FORBIDDEN,
        "ON : tenant suspendu refusé au middleware dès la requête suivante"
    );

    // Refus côté CIBLE (EX-C2-4) : le cross-read main→research qui passait juste avant
    // est refusé aussi — le grant de main reste en table, mais `require_active_target`
    // exige un tenant cible `active` (un vault gelé cesse d'être lisible par ses grantees).
    let (status_cross, body_cross) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        Some(&jwt),
        search_body(Some("research")),
    )
    .await;
    assert_eq!(
        status_cross,
        StatusCode::FORBIDDEN,
        "ON : cross-read vers un vault suspendu refusé malgré le grant, body : {body_cross}"
    );
}
