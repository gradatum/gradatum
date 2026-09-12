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
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, run_migrations};
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
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
write_patterns = ["main/*", "main/main", "main/jobs", "messaging/identity/agent/*"]

[[consumer]]
identity = "writer-reader"
read_patterns  = ["reader/*", "reader/main"]
write_patterns = ["reader/*", "reader/main", "main/jobs", "messaging/identity/agent/*"]

[[consumer]]
identity = "writer-ghost"
read_patterns  = ["ghost/*", "ghost/main"]
write_patterns = ["ghost/*", "ghost/main", "main/jobs", "messaging/identity/agent/*"]
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

// ── F-144 / C3 — messagerie : message_send écrit, message_poll lit ────────────
//
// Étend les fixtures C1 (main/reader/ghost) à la messagerie inter-agents. Les
// consumers ont TOUS un write_pattern `messaging/identity/agent/*` (C4 Allow) : le
// refus testé ici vient donc du GRANT (flag ON), pas de l'ACL — c'est le point C3.

/// `message_send` : grant READ-ONLY → 403 (l'écriture du canal messagerie exige un
/// grant write, comme tout chemin d'écriture à flag ON).
#[tokio::test]
async fn flag_on_message_send_readonly_grant_refused() {
    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");
    // Pas de tenant_id dans le body : MessageSendRequest est deny_unknown_fields,
    // et le tenant est dérivé du credential (jamais un champ client).
    let body = serde_json::json!({
        "recipient_identity": "identity/agent/bob",
        "subject": "ping",
        "body": "hello",
    });
    let status = post_json(
        build_router(env.state),
        "/api/v1/messages",
        Some(&jwt),
        body,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "message_send grant read-only → 403 (C3)"
    );
}

/// `message_send` : tenant "ghost" (aucun grant, aucune ligne) → 403.
#[tokio::test]
async fn flag_on_message_send_ghost_refused() {
    let env = build_c1_env(true).await;
    // ghost n'est PAS seedé (aucune ligne = refus fail-closed).
    let jwt = sign_jwt(&env.state, "writer-ghost", "ghost");
    let body = serde_json::json!({
        "recipient_identity": "identity/agent/bob",
        "subject": "ping",
        "body": "hello",
    });
    let status = post_json(
        build_router(env.state),
        "/api/v1/messages",
        Some(&jwt),
        body,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "message_send ghost (aucun grant) → 403 (C3)"
    );
}

/// `message_poll` (lecture) : un grant READ-ONLY peut lire sa boîte — aucune erreur,
/// messages vides (rien à voir). Le tenant `ghost`, lui, est refusé 403 dès le
/// middleware (allow-list vide, A8/C1) : il ne voit rien, y compris en messagerie.
#[tokio::test]
async fn flag_on_message_poll_readonly_reader_sees_empty_mailbox() {
    use http_body_util::BodyExt;

    let env = build_c1_env(true).await;
    seed_reader_grant(&env.index_path);
    let jwt = sign_jwt(&env.state, "writer-reader", "reader");

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/messages")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::empty())
        .expect("build GET /messages");

    let resp = build_router(env.state).oneshot(req).await.expect("service");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "message_poll grant read → 200 (lecture autorisée)"
    );
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json poll");
    assert_eq!(
        json["messages"].as_array().map(Vec::len),
        Some(0),
        "boîte vide — aucun message adressé au reader (C2) — body: {json}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// F-63 — recherche multi-vault agrégée (VaultScope) : câblage T4 + ACL T5.
// ═══════════════════════════════════════════════════════════════════════════════
//
// ACL : `mv-reader` peut lire `main/*`, `a/*`, `b/*` — le cross-read n'est donc gouverné
// que par les GRANTS (isole le scope, pas l'ACL). `ghost/*` est volontairement absent.
const TEST_ACL_MV: &str = r#"
[[consumer]]
identity = "mv-reader"
read_patterns  = ["main/*", "main/main", "a/*", "b/*"]
write_patterns = ["main/*", "main/jobs"]
"#;

/// Timestamp figé (corpus reproductible, aucune dépendance à l'horloge).
const MV_TS: i64 = 1_700_000_000_000;
/// Requête FTS commune aux corpus des vaults `a`/`b`/`main`.
const MV_QUERY: &str = "gravity probe";

/// `AppState` (flag ON/OFF paramétrable) avec le preset ACL multi-vault.
async fn build_mv_env(multi_tenant_enabled: bool) -> C1Env {
    let dir = TempDir::new().expect("tempdir MV");
    let index_path = dir.path().join("index.db");
    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_MV).expect("preset ACL MV valide");
    let jobs_pool = QueueDb::open_in_memory().await.expect("jobs pool");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
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
        .expect("SqliteIndex::open MV")
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);
    C1Env {
        state,
        index_path,
        _dir: dir,
    }
}

/// Sème une note FTS recherchable (répond à `MV_QUERY`) dans `vault`.
fn seed_mv_note(index_path: &std::path::Path, id: &str, vault: &str, title: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed MV");
    conn.execute_batch(&format!(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
           VALUES ('{id}', '{vault}', NULL, 'reference', 'live', 1, {MV_TS}, X'00', 'isolation gravity probe corpus', '{title}');
         INSERT INTO notes_fts (rowid, body_text)
           SELECT rowid, body_text FROM notes WHERE id = '{id}';"
    ))
    .expect("seed note FTS MV");
}

/// Enregistre `vault` comme tenant actif (nécessaire à `require_active_target`).
fn register_active_vault(index_path: &std::path::Path, vault: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db register MV");
    conn.execute(
        "INSERT OR IGNORE INTO tenants (id, status, created_at) VALUES (?1, 'active', ?2)",
        rusqlite::params![vault, MV_TS],
    )
    .expect("register active vault MV");
}

/// Ajoute un grant `tenant → vault` en lecture.
fn grant_read(index_path: &std::path::Path, tenant: &str, vault: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db grant MV");
    conn.execute(
        "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access) VALUES (?1, ?2, 'read')",
        rusqlite::params![tenant, vault],
    )
    .expect("grant read MV");
}

/// `POST /api/v1/vault_search` authentifié → `(status, json)`.
async fn search_mv(
    router: axum::Router,
    jwt: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/vault_search")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Projection déterministe d'un item : `(path, vault_id, title, snippet, status)`.
fn item_tuples(json: &serde_json::Value) -> Vec<(String, String, String, String, String)> {
    let mut v: Vec<_> = json["items"]
        .as_array()
        .expect("items tableau")
        .iter()
        .map(|it| {
            (
                it["path"].as_str().unwrap_or_default().to_owned(),
                it["vault_id"].as_str().unwrap_or_default().to_owned(),
                it["title"].as_str().unwrap_or_default().to_owned(),
                it["snippet"].as_str().unwrap_or_default().to_owned(),
                it["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Provenances triées `(path, vault_id)`.
fn mv_provenances(json: &serde_json::Value) -> Vec<(String, String)> {
    let mut v: Vec<_> = json["items"]
        .as_array()
        .expect("items tableau")
        .iter()
        .map(|it| {
            (
                it["path"].as_str().unwrap_or_default().to_owned(),
                it["vault_id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("hit sans vault_id — provenance perdue: {it}"))
                    .to_owned(),
            )
        })
        .collect();
    v.sort();
    v
}

// ── T4 — câblage vault_search_impl multi-vault ────────────────────────────────

/// Non-régression mono : SANS opt-in, la réponse est STRUCTURELLEMENT identique à
/// `multi_tenant` OFF et ON (chemin mono partagé, byte-identical par construction — C-2).
#[tokio::test]
async fn f63_mono_no_optin_off_and_on_structurally_identical() {
    // OFF.
    let off = build_mv_env(false).await;
    seed_mv_note(
        &off.index_path,
        "01HMAAAAAAAAAAAAAAAAAAAAA1",
        "main",
        "Alpha",
    );
    seed_mv_note(
        &off.index_path,
        "01HMAAAAAAAAAAAAAAAAAAAAA2",
        "main",
        "Beta",
    );
    let jwt_off = sign_jwt(&off.state, "mv-reader", "main");
    let (s_off, r_off) = search_mv(
        build_router(off.state),
        &jwt_off,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main" }),
    )
    .await;

    // ON, sans opt-in (le vault propre "main" a son grant seedé par la migration 0030).
    let on = build_mv_env(true).await;
    seed_mv_note(
        &on.index_path,
        "01HMAAAAAAAAAAAAAAAAAAAAA1",
        "main",
        "Alpha",
    );
    seed_mv_note(&on.index_path, "01HMAAAAAAAAAAAAAAAAAAAAA2", "main", "Beta");
    let jwt_on = sign_jwt(&on.state, "mv-reader", "main");
    let (s_on, r_on) = search_mv(
        build_router(on.state),
        &jwt_on,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main" }),
    )
    .await;

    assert_eq!(s_off, StatusCode::OK);
    assert_eq!(s_on, StatusCode::OK);
    assert_eq!(
        item_tuples(&r_off),
        item_tuples(&r_on),
        "chemin mono sans opt-in : OFF et ON byte-identical (structure)"
    );
    assert_eq!(
        item_tuples(&r_off).len(),
        2,
        "les 2 notes de main remontent"
    );
}

/// `vault_id` (singulier) + `vault_ids` simultanés → `400` (scopes mutuellement exclusifs).
#[tokio::test]
async fn f63_vault_id_and_vault_ids_conflict_is_400() {
    let env = build_mv_env(true).await;
    let jwt = sign_jwt(&env.state, "mv-reader", "main");
    let (status, _) = search_mv(
        build_router(env.state),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "vault_id": "a", "vault_ids": ["b"] }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "vault_id + vault_ids → 400 InvalidInput"
    );
}

/// `all_readable` agrège deux vaults autorisés en un seul flux, chaque hit estampillé de
/// son vault SOURCE (provenance) + enrichissement titre par-vault correct.
#[tokio::test]
async fn f63_all_readable_aggregates_two_vaults_single_stream() {
    let env = build_mv_env(true).await;
    register_active_vault(&env.index_path, "a");
    register_active_vault(&env.index_path, "b");
    grant_read(&env.index_path, "main", "a");
    grant_read(&env.index_path, "main", "b");
    seed_mv_note(&env.index_path, "01HAAAAAAAAAAAAAAAAAAAAAAA", "a", "Alpha");
    seed_mv_note(&env.index_path, "01HBBBBBBBBBBBBBBBBBBBBBBB", "b", "Beta");

    let jwt = sign_jwt(&env.state, "mv-reader", "main");
    let (status, resp) = search_mv(
        build_router(env.state),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "all_readable": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mv_provenances(&resp),
        vec![
            (
                "reference/01HAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
                "a".to_owned()
            ),
            (
                "reference/01HBBBBBBBBBBBBBBBBBBBBBBB".to_owned(),
                "b".to_owned()
            ),
        ],
        "union {{a,b}} en un flux, provenance par vault source"
    );
    // Enrichissement titre par-vault (le bon titre est rattaché au bon vault).
    let titles = item_tuples(&resp);
    assert!(
        titles
            .iter()
            .any(|(_, v, t, _, _)| v == "a" && t == "Alpha"),
        "titre Alpha rattaché au vault a"
    );
    assert!(
        titles.iter().any(|(_, v, t, _, _)| v == "b" && t == "Beta"),
        "titre Beta rattaché au vault b"
    );
}

/// `corpus_match_count` en multi = SOMME sur les vaults lus (Q1 reco a).
#[tokio::test]
async fn f63_corpus_match_count_is_sum_over_vaults() {
    let env = build_mv_env(true).await;
    register_active_vault(&env.index_path, "a");
    register_active_vault(&env.index_path, "b");
    grant_read(&env.index_path, "main", "a");
    grant_read(&env.index_path, "main", "b");
    seed_mv_note(&env.index_path, "01HAAAAAAAAAAAAAAAAAAAAAAA", "a", "Alpha");
    seed_mv_note(&env.index_path, "01HBBBBBBBBBBBBBBBBBBBBBBB", "b", "Beta");

    let jwt = sign_jwt(&env.state, "mv-reader", "main");
    let (status, resp) = search_mv(
        build_router(env.state),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "all_readable": true, "include_corpus_count": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        resp["corpus_match_count"].as_u64(),
        Some(2),
        "1 match par vault (a,b) → somme = 2"
    );
}

// ── T5 — C3 : ACL multi-vault agrégée (exclusion silencieuse, intersection) ───

/// `grant_read` au niveau AGENT (`agent_vault_grants`).
fn grant_agent_read(index_path: &std::path::Path, agent: &str, vault: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db agent grant");
    conn.execute(
        "INSERT INTO agent_vault_grants (agent_id, vault_id, access) VALUES (?1, ?2, 'read')",
        rusqlite::params![agent, vault],
    )
    .expect("grant agent read");
}

/// C3/C-2 — `vault_ids=[a, ghost]`, grant sur `a` seul → `a` uniquement, `ghost` exclu
/// SILENCIEUX (200, jamais 403), et AUCUNE différence observable entre « vault non
/// autorisé mais existant » et « vault inexistant » (pas d'oracle d'existence).
#[tokio::test]
async fn f63_unauthorized_vault_excluded_silently_no_existence_oracle() {
    let env = build_mv_env(true).await;
    // `a` : autorisé (grant main→a). `ghost` : existe + a une note, mais AUCUN grant main.
    register_active_vault(&env.index_path, "a");
    register_active_vault(&env.index_path, "ghost");
    grant_read(&env.index_path, "main", "a");
    seed_mv_note(&env.index_path, "01HAAAAAAAAAAAAAAAAAAAAAAA", "a", "Alpha");
    seed_mv_note(
        &env.index_path,
        "01HGGGGGGGGGGGGGGGGGGGGGGG",
        "ghost",
        "GhostSecret",
    );

    let jwt = sign_jwt(&env.state, "mv-reader", "main");

    // (1) ghost EXISTE mais non autorisé.
    let (s1, r1) = search_mv(
        build_router(env.state.clone()),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "vault_ids": ["a", "ghost"] }),
    )
    .await;
    // (2) même requête avec un vault totalement INEXISTANT.
    let (s2, r2) = search_mv(
        build_router(env.state.clone()),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "vault_ids": ["a", "voidvault"] }),
    )
    .await;

    assert_eq!(s1, StatusCode::OK, "vault non autorisé → jamais 403, 200");
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        mv_provenances(&r1),
        vec![(
            "reference/01HAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            "a".to_owned()
        )],
        "seul `a` remonte ; la note de `ghost` n'est jamais divulguée"
    );
    assert_eq!(
        mv_provenances(&r1),
        mv_provenances(&r2),
        "no-oracle : « existant non autorisé » et « inexistant » donnent le MÊME résultat"
    );
}

/// C3 — intersection agent : un grant agent plus restrictif que le grant tenant borne
/// l'agrégat à `min(tenant, agent)`. Tenant lit {a,b} ; agent ne couvre que {a} → `b`
/// est exclu de l'union `all_readable` bien qu'un grant tenant l'autorise.
#[tokio::test]
async fn f63_agent_grant_narrows_aggregate() {
    let env = build_mv_env(true).await;
    register_active_vault(&env.index_path, "a");
    register_active_vault(&env.index_path, "b");
    grant_read(&env.index_path, "main", "a");
    grant_read(&env.index_path, "main", "b");
    // Agent `mv-reader` : grants sur son vault propre `main` (pour franchir le middleware)
    // et sur `a` — mais PAS sur `b`. L'intersection exclut donc `b`.
    grant_agent_read(&env.index_path, "mv-reader", "main");
    grant_agent_read(&env.index_path, "mv-reader", "a");
    seed_mv_note(&env.index_path, "01HAAAAAAAAAAAAAAAAAAAAAAA", "a", "Alpha");
    seed_mv_note(&env.index_path, "01HBBBBBBBBBBBBBBBBBBBBBBB", "b", "Beta");

    let jwt = sign_jwt(&env.state, "mv-reader", "main");
    let (status, resp) = search_mv(
        build_router(env.state),
        &jwt,
        serde_json::json!({ "query": MV_QUERY, "tenant_id": "main", "all_readable": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mv_provenances(&resp),
        vec![(
            "reference/01HAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            "a".to_owned()
        )],
        "intersection min(tenant{{a,b}}, agent{{a}}) = {{a}} — b exclu malgré le grant tenant"
    );
}
