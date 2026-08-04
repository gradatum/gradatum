//! Suite pré-flip — isolation cross-vault AU NIVEAU HANDLER HTTP (consolidation W3/T17).
//!
//! Regroupe, sur un harnais HTTP partagé unique, les preuves d'isolation cross-vault des
//! handlers de lecture historiquement forgés sur `main` :
//!
//! - **A3-lessons (T12)** — [`lessons_recall_non_main_tenant_requires_grant`] +
//!   verrou OFF [`lessons_recall_flag_off_no_grant_required`] ;
//! - **A3-vault_status (T13)** — [`vault_status_scoped_to_caller`] +
//!   verrou OFF [`vault_status_flag_off_unchanged`].
//!
//! Chaque propriété est prouvée en DEUX régimes :
//! - **Flag ON** (`multi_tenant.enabled = true`, LOCAL au harnais — flip INTERDIT LIVE) :
//!   le chemin de lecture est scopé au principal JWT / gaté par grant explicite ;
//! - **Flag OFF** (défaut LIVE) : chemin legacy INCHANGÉ, byte-identical, JAMAIS `403`
//!   (verrous anti-régression LIVE : hook `lesson-recall.sh`, F-60 JIT,
//!   MCP `vault_lessons_recall` ; mono-vault `main`).
//!
//! ## Troisième preuve d'isolation pré-flip (hors handler HTTP)
//!
//! Le routage cross-vault de la promotion (**A1 / T15**) n'est PAS un handler HTTP mais un
//! tick d'arrière-plan (`promote_tick`) ; sa preuve d'isolation vit dans
//! `promote_tick_scoped.rs` (`promote_tick_promotes_in_correct_vault`), dont le harnais
//! `Vault`-natif diffère de celui-ci et est mutualisé avec les verrous de typage Task-19.
//! Elle n'est volontairement PAS recopiée ici pour éviter la duplication du harnais promote ;
//! ce fichier reste la suite d'isolation *handler-level*, cohérente avec le libellé du commit.

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

// `reader` a l'ACL Read/Write sur `main/*` ET `vault-b/*` : à ON, un refus provient donc du
// GRANT (allow-list), jamais de l'ACL — on isole le comportement grant/scoping câblé par T12/T13.
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

async fn build_env(multi_tenant_enabled: bool) -> Env {
    let dir = TempDir::new().expect("tempdir preflip");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL preflip valide");

    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
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

/// Enregistre `vault-b` : tenant actif + self-grant write (couvre le middleware ON et la
/// lecture de son propre vault). N'ouvre AUCUN grant sur `main` — l'accès au vault partagé
/// `main` reste à prouver par un grant dédié.
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

/// Ouvre un grant read de `vault-b` sur le vault partagé `main` (allow-list).
fn grant_read_on_main(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db grant main");
    conn.execute_batch(
        "INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('vault-b', 'main', 'read');",
    )
    .expect("grant vault-b read on main");
}

/// Ouvre un grant read de `vault-b` sur `main` BORNÉ à une section (L3, F-121 — 0040).
///
/// `INSERT OR REPLACE` : la PK `(tenant_id, vault_id)` n'admet qu'une ligne par couple,
/// donc re-scoper un grant existant le REMPLACE (cf. en-tête de la migration 0040).
fn grant_read_on_main_section(index_path: &std::path::Path, section: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db grant section");
    conn.execute(
        "INSERT OR REPLACE INTO tenant_vault_grants (tenant_id, vault_id, access, section)
           VALUES ('vault-b', 'main', 'read', ?1)",
        rusqlite::params![section],
    )
    .expect("grant vault-b read on main scoped");
}

/// Insère une note `live` scopée `(id, vault_id)` — support du comptage `vault_status`.
fn seed_live_note(index_path: &std::path::Path, ulid: &str, vault: &str, body: &str) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed note");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
         VALUES (?1, ?2, NULL, 'decisions', 'live', 1, ?3, X'00', ?4, 'T')",
        rusqlite::params![ulid, vault, now, body],
    )
    .expect("seed live note");
}

/// Vide les tables de grants (verrou OFF : le legacy ne doit jamais les consulter).
fn truncate_grants(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db truncate");
    conn.execute_batch("DELETE FROM tenant_vault_grants; DELETE FROM tenants;")
        .expect("truncate grants");
}
/// Insère un grant agent→vault (B7) pour chaque identité listée — le middleware
/// vérifie `tenant_grants ∩ agent_grants` quand `multi_tenant.enabled = true`.
fn seed_agent_grants(index_path: &std::path::Path, agents: &[&str]) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed agent grants");
    for agent in agents {
        conn.execute(
            "INSERT OR IGNORE INTO agent_vault_grants (agent_id, vault_id, access) VALUES (?1, 'main', 'write')",
            rusqlite::params![agent],
        )
        .expect("seed agent grant");
    }
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

async fn get_full(router: axum::Router, uri: &str, jwt: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::empty())
        .expect("build request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// POST JSON authentifié → `(status, body)` — utilisé par les preuves de contenance L3.
async fn post_json_full(
    router: axum::Router,
    uri: &str,
    jwt: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {jwt}"))
        .body(Body::from(serde_json::to_vec(&body).expect("json body")))
        .expect("build POST request");
    let resp = router.oneshot(req).await.expect("service");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ─────────────────────────────────────────────────────────────────────────────
// A3-lessons (T12) — grant explicite sur le `main/lessons` partagé
// ─────────────────────────────────────────────────────────────────────────────

const LESSONS_URI: &str = "/api/v1/lessons/recall?class=deploy";

/// ON : un principal `vault-b` SANS grant sur `main` est refusé (403) ; AVEC grant → 200.
/// Ferme la fuite cross-tenant de la forge `own_vault_checked("main")` sans grant.
#[tokio::test]
async fn lessons_recall_non_main_tenant_requires_grant() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "reader"]);
    seed_vault_b_registration(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "vault-b");

    // Phase 1 — aucun grant sur `main` : refus fail-closed.
    let (status, body) = get_full(build_router(env.state.clone()), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : lessons cross-tenant sans grant `main` doit être 403, body : {body}"
    );

    // Phase 2 — grant read `vault-b → main` ouvert : lecture du lessons partagé autorisée.
    grant_read_on_main(&env.index_path);
    let (status, body) = get_full(build_router(env.state), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : lessons partagé lisible AVEC grant read sur main, body : {body}"
    );
}

/// ON / L3 (F-121) : un grant read borné à `lessons-learned` ouvre les leçons — et RIEN
/// d'autre de `main`. C'est la propriété que le grant vault-entier ne donnait pas :
/// avant 0040, « lire les leçons de main » impliquait « lire tout main ».
#[tokio::test]
async fn lessons_section_scoped_grant_opens_lessons_only() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "reader"]);
    seed_vault_b_registration(&env.index_path);
    grant_read_on_main_section(&env.index_path, "lessons-learned");
    let jwt = sign_jwt(&env.state, "reader", "vault-b");

    // 1. Les leçons partagées sont lisibles (le grant couvre exactement cette section).
    let (status, body) = get_full(build_router(env.state.clone()), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : grant borné à `lessons-learned` doit ouvrir /lessons/recall, body : {body}"
    );

    // 2. CONTENANCE L3 : le même grant n'ouvre PAS la lecture du vault `main` entier.
    //    `vault_search` exige une portée vault-entier → refus fail-closed (403).
    //    L'ACL du preset autorise `main/*` pour `reader` : le refus vient donc du GRANT.
    let (status, body) = post_json_full(
        build_router(env.state),
        "/api/v1/vault_search",
        &jwt,
        serde_json::json!({ "query": "probe", "tenant_id": "vault-b", "vault_id": "main" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : un grant borné à `lessons-learned` ne doit PAS ouvrir tout `main` (L3), body : {body}"
    );
}

/// ON / L3 (F-121) : un grant read borné à une AUTRE section ne donne pas les leçons.
#[tokio::test]
async fn lessons_grant_scoped_to_other_section_is_refused() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "reader"]);
    seed_vault_b_registration(&env.index_path);
    grant_read_on_main_section(&env.index_path, "decisions");
    let jwt = sign_jwt(&env.state, "reader", "vault-b");

    let (status, body) = get_full(build_router(env.state.clone()), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "ON : grant borné à `decisions` ne doit pas ouvrir les leçons, body : {body}"
    );

    // Élargir le grant à TOUT le vault (section NULL) rétablit l'accès : preuve que le
    // refus vient bien de la PORTÉE, pas d'un autre barreau (tenant actif, ACL, access).
    {
        let conn = rusqlite::Connection::open(&env.index_path).expect("open index.db widen");
        conn.execute_batch(
            "INSERT OR REPLACE INTO tenant_vault_grants (tenant_id, vault_id, access, section)
               VALUES ('vault-b', 'main', 'read', NULL);",
        )
        .expect("widen grant to vault-wide");
    }
    let (status, body) = get_full(build_router(env.state), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : grant vault-entier (section NULL) reste couvrant, body : {body}"
    );
}

/// OFF : `lessons/recall` réussit tables de grants VIDES → 200, JAMAIS 403.
/// Verrou anti-régression LIVE (hook lesson-recall.sh / F-60 / MCP vault_lessons_recall).
#[tokio::test]
async fn lessons_recall_flag_off_no_grant_required() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : lessons 200 tables de grants vidées (aucun grant consulté), body : {body}"
    );
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "OFF : JAMAIS 403 — régression LIVE lesson-recall interdite"
    );
}

/// OFF / L3 (F-121) — verrou byte-identical de la colonne `section` (migration 0040) :
/// une ligne de grant BORNÉE à une autre section (`decisions`), donc non couvrante à ON,
/// n'a AUCUN effet à OFF — le chemin legacy ne consulte jamais l'allow-list. Si un jour
/// le gating OFF venait à consulter la table, ce test virerait au 403 et l'attraperait.
#[tokio::test]
async fn lessons_recall_flag_off_ignores_section_scoped_grant() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    seed_vault_b_registration(&env.index_path);
    grant_read_on_main_section(&env.index_path, "decisions");
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), LESSONS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : la colonne `section` doit rester inerte (aucun grant consulté), body : {body}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// A3-vault_status (T13) — les hardcodes `"main"` routés sur le tenant appelant
// ─────────────────────────────────────────────────────────────────────────────

const STATUS_URI: &str = "/api/v1/vault_status";

/// ON : `vault_status` d'un principal `vault-b` reporte les métadonnées de vault-b, pas de main.
#[tokio::test]
async fn vault_status_scoped_to_caller() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "reader"]);
    seed_vault_b_registration(&env.index_path);
    // main : 1 live — vault-b : 2 live. Le statut scopé vault-b doit compter 2.
    seed_live_note(
        &env.index_path,
        "01HMAIN00000000000000000ST",
        "main",
        "corps-main",
    );
    seed_live_note(
        &env.index_path,
        "01HVAULTB000000000000000S1",
        "vault-b",
        "b-un",
    );
    seed_live_note(
        &env.index_path,
        "01HVAULTB000000000000000S2",
        "vault-b",
        "b-deux",
    );
    let jwt = sign_jwt(&env.state, "reader", "vault-b");
    let (status, body) = get_full(build_router(env.state), STATUS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "ON : vault_status vault-b 200, body : {body}"
    );
    assert!(
        body.contains("\"tenant_id\":\"vault-b\""),
        "ON : tenant_id scopé vault-b, body : {body}"
    );
    assert!(
        body.contains("\"note_count\":2"),
        "ON : 2 notes live de vault-b (pas la note de main), body : {body}"
    );
}

/// OFF : `vault_status` reporte les métadonnées de `main`, tables de grants VIDES → 200,
/// JAMAIS 403 (byte-identical LIVE — mono-vault).
#[tokio::test]
async fn vault_status_flag_off_unchanged() {
    let env = build_env(false).await;
    truncate_grants(&env.index_path);
    seed_live_note(
        &env.index_path,
        "01HMAIN00000000000000000O1",
        "main",
        "corps",
    );
    let jwt = sign_jwt(&env.state, "reader", "main");
    let (status, body) = get_full(build_router(env.state), STATUS_URI, &jwt).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "OFF : vault_status 200 tables de grants vidées (aucun grant consulté), body : {body}"
    );
    assert_ne!(status, StatusCode::FORBIDDEN, "OFF : jamais 403");
    assert!(
        body.contains("\"tenant_id\":\"main\""),
        "OFF : métadonnées de main (hardcode inchangé), body : {body}"
    );
    assert!(
        body.contains("\"note_count\":1"),
        "OFF : 1 note live de main, body : {body}"
    );
}
