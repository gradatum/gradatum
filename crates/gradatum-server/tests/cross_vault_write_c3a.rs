//! Tests adversariaux C3a (EX-C3a P0) — isolation cross-vault des MUTATIONS par ULID.
//!
//! ## Menace fermée
//!
//! La table `notes` est physiquement partagée entre vaults (colonne `vault_id`). Avant
//! ce correctif, les mutations ciblées par ULID (`vault_downgrade`, `PATCH /notes/{id}`)
//! filtraient `WHERE id = ?` **sans** `vault_id` : un tenant distant légitime (scope write
//! + self-grant sur SON vault) pouvait muter par ULID une note de N'IMPORTE quel vault.
//!
//! ## Modèle de test
//!
//! L'attaquant est le tenant **légitime** `main` (grants/ACL seedés par la fixture) ; la
//! note-victime appartient au vault tiers `research`. L'attaquant cible la victime par son
//! ULID en déclarant son PROPRE `tenant_id = "main"` — il passe donc `effective_write_vault`,
//! le scope write et le self-grant. Post-fix, la mutation est épinglée `AND vault_id = 'main'`
//! → 0 ligne → `NoteNotFound` (404), identique à un ULID inexistant (pas d'oracle d'existence).

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
use ulid::Ulid;

// `attacker-main` : ACL Read+Write pleines sur `main` — les refus testés viennent du
// FILTRE `vault_id` de la mutation, jamais de l'ACL ni du scope (l'attaquant est légitime).
const TEST_ACL: &str = r#"
[[consumer]]
identity = "attacker-main"
read_patterns  = ["main/*", "main/main", "main/timeline"]
write_patterns = ["main/*", "main/main"]
"#;

struct Env {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

/// `AppState` : Vault réel `main` (seed migration 0030 `main`↔`main` write) + index
/// SQLite PARTAGÉ avec `state.search`, flag `multi_tenant` paramétrable.
async fn build_env(multi_tenant_enabled: bool) -> Env {
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let dir = TempDir::new().expect("tempdir");
    let vault_dir = dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );
    let index_path = gradatum_core::paths::vault_dir_index_path(&vault_dir);

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL valide — invariant statique");

    let jobs_pool = QueueDb::open_in_memory()
        .await
        .expect("jobs pool in-memory");
    run_migrations(&jobs_pool).await.expect("migrations jobs");
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

    Env {
        state,
        index_path,
        _dir: dir,
    }
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

/// Seed une VRAIE note `live` dans le vault tiers `research`. Retourne son ULID.
fn seed_research_note(index_path: &std::path::Path) -> Ulid {
    let victim = Ulid::generate();
    let conn = rusqlite::Connection::open(index_path).expect("open index.db seed");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', ?1)",
        rusqlite::params![now],
    )
    .expect("seed tenant research");
    conn.execute(
        "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
         VALUES (?1, 'research', NULL, 'reference', 'live', 1, ?2, X'00', 'secret research corpus', 'Research Secret')",
        rusqlite::params![victim.to_string(), now],
    )
    .expect("seed research note");
    victim
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

/// JWT `attacker-main` / tenant `main`, scopes read+write (attaquant légitime).
fn sign_attacker(state: &AppState) -> String {
    state
        .jwt
        .sign(
            "attacker-main",
            &["read".to_owned(), "write".to_owned()],
            TokenScope::Service,
            "main",
        )
        .expect("sign JWT test")
}

/// Statut du vault `research` pour la note-victime — prouve l'absence de mutation.
fn research_note_status(index_path: &std::path::Path, victim: &Ulid) -> String {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db check");
    conn.query_row(
        "SELECT status FROM notes WHERE id = ?1 AND vault_id = 'research'",
        rusqlite::params![victim.to_string()],
        |row| row.get(0),
    )
    .expect("note research présente")
}

async fn request(
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

// ── vault_downgrade — chemin `state.search.downgrade_note` (P0 confirmé) ─────────

/// ON : downgrade cross-vault par ULID → 404 `NoteNotFound`, note `research` intacte.
#[tokio::test]
async fn flag_on_downgrade_cross_vault_note_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-main"]);
    let victim = seed_research_note(&env.index_path);
    let jwt = sign_attacker(&env.state);

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_downgrade",
        &jwt,
        serde_json::json!({ "note_id": victim.to_string(), "reason": "pwn", "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "downgrade cross-vault doit être 404 (fail-closed)"
    );
    assert_eq!(
        research_note_status(&env.index_path, &victim),
        "live",
        "la note research NE DOIT PAS avoir été downgradée"
    );
}

/// ON : oracle — un ULID totalement inexistant donne le MÊME 404 que la note cross-vault.
#[tokio::test]
async fn flag_on_downgrade_nonexistent_matches_cross_vault_oracle() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-main"]);
    let _victim = seed_research_note(&env.index_path);
    let jwt = sign_attacker(&env.state);
    let ghost = Ulid::generate();

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_downgrade",
        &jwt,
        serde_json::json!({ "note_id": ghost.to_string(), "reason": "x", "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "ULID inexistant → 404, indistinguable du cross-vault"
    );
}

// ── PATCH /notes/{id} — chemin `state.search.patch_note_status` (P0 confirmé) ────

/// ON : PATCH cross-vault (raison seule → SQL direct) → 404, note `research` intacte.
#[tokio::test]
async fn flag_on_patch_note_cross_vault_note_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-main"]);
    let victim = seed_research_note(&env.index_path);
    let jwt = sign_attacker(&env.state);

    let status = request(
        build_router(env.state.clone()),
        "PATCH",
        &format!("/api/v1/notes/{victim}"),
        &jwt,
        serde_json::json!({ "status_reason": "pwned" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "PATCH cross-vault doit être 404 (fail-closed)"
    );
    assert_eq!(
        research_note_status(&env.index_path, &victim),
        "live",
        "la note research NE DOIT PAS avoir été patchée"
    );
}

// ── move / restore — chemins `state.vault` (déjà scopés par le tenant du Vault) ──

/// ON : move cross-vault → l'opération passe par `vault.move_locus` (lecture scopée au
/// tenant du Vault `main`) → la note `research` est invisible → 404. Défense en profondeur.
#[tokio::test]
async fn flag_on_move_cross_vault_note_is_not_found() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-main"]);
    let victim = seed_research_note(&env.index_path);
    let jwt = sign_attacker(&env.state);

    let status = request(
        build_router(env.state.clone()),
        "POST",
        &format!("/api/v1/notes/{victim}/move"),
        &jwt,
        serde_json::json!({ "locus": "knowledge" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "move cross-vault doit être 404 (scoping Vault tenant)"
    );
}

/// ON : restore cross-vault → `vault.history_restore` lit les snapshots sous
/// `<tenant>/.history/` du Vault `main` — le snapshot de la note `research` est
/// inatteignable → l'opération échoue sans muter. On exige un statut d'ERREUR (non-2xx)
/// et l'intégrité de la note-victime.
#[tokio::test]
async fn flag_on_restore_cross_vault_does_not_mutate() {
    let env = build_env(true).await;
    seed_agent_grants(&env.index_path, &["main", "attacker-main"]);
    let victim = seed_research_note(&env.index_path);
    let jwt = sign_attacker(&env.state);

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_restore",
        &jwt,
        serde_json::json!({ "note_id": victim.to_string(), "ts_ms": 1, "tenant_id": "main" }),
    )
    .await;

    assert!(
        !status.is_success(),
        "restore cross-vault ne doit PAS aboutir (got {status})"
    );
    assert_eq!(
        research_note_status(&env.index_path, &victim),
        "live",
        "la note research NE DOIT PAS avoir été restaurée/écrasée"
    );
}

// ── Byte-identical OFF : la note du vault propre reste mutable ───────────────────

/// OFF : downgrade de la note du VAULT PROPRE (`main`) aboutit — le prédicat
/// `AND vault_id = 'main'` est transparent (aucun changement de comportement du parc).
#[tokio::test]
async fn flag_off_downgrade_own_vault_note_still_succeeds() {
    let env = build_env(false).await;
    // Note dans le vault propre 'main' (seed direct pour un ULID contrôlé).
    let own = Ulid::generate();
    {
        let conn = rusqlite::Connection::open(&env.index_path).expect("open");
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO notes (id, vault_id, locus, section, status, schema_version, created, content_hash, body_text, title)
             VALUES (?1, 'main', NULL, 'reference', 'live', 1, ?2, X'00', 'own corpus', 'Own')",
            rusqlite::params![own.to_string(), now],
        )
        .expect("seed own note");
    }
    let jwt = sign_attacker(&env.state);

    let status = request(
        build_router(env.state.clone()),
        "POST",
        "/api/v1/vault_downgrade",
        &jwt,
        serde_json::json!({ "note_id": own.to_string(), "reason": "legit", "tenant_id": "main" }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "downgrade de la note du vault propre doit aboutir (byte-identical)"
    );
}
