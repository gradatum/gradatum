//! Tests adversariaux C3a (F-45) — pré-requis council accès distant
//! (`council/01KXGQ3ZJJ`) : `identity/main` n'est JAMAIS accessible en écriture
//! à une identité distante.
//!
//! ## Modèle prouvé
//!
//! À `multi_tenant.enabled = true`, une identité « distante » (bearer d'un tenant
//! ≠ `main`) ne dispose d'AUCUN chemin d'écriture vers le vault `main` :
//! - le body `tenant_id = "main"` diverge de son JWT → 403 (`effective_tenant`) ;
//! - ses écritures légitimes sont ÉPINGLÉES à son vault propre
//!   (`effective_write_vault`, INV-P1-3 : le vault cible d'une écriture EST le
//!   tenant JWT) ;
//! - forger `sub = "main-agent"` (l'owner privilégié des âmes) ne change rien :
//!   le guard identity est en AVAL du pinning de vault — l'âme `identity/main`
//!   du vault `main` reste hors d'atteinte.
//!
//! EX-C3a-3 (« le test identity non-privilégié reste vert ») est couvert par la
//! suite existante `identity_section.rs`, inchangée.

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

// ACL large pour les deux identités : les refus testés viennent du garde tenant
// (divergence body/JWT, pinning vault), jamais de l'ACL.
const TEST_ACL_IDENTITY: &str = r#"
[[consumer]]
identity = "main-agent"
read_patterns  = ["main/*", "main/main", "research/*", "research/main"]
write_patterns = ["main/*", "main/main", "research/*", "research/main"]

[[consumer]]
identity = "remote-agent"
read_patterns  = ["main/*", "main/main", "research/*", "research/main"]
write_patterns = ["main/*", "main/main", "research/*", "research/main"]
"#;

struct IdEnv {
    state: AppState,
    index_path: std::path::PathBuf,
    _dir: TempDir,
}

async fn build_identity_env() -> IdEnv {
    use gradatum_core::scope::VaultId;
    use gradatum_vault::Vault;

    let dir = TempDir::new().expect("tempdir identity C3a");
    let vault_dir = dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_dir, VaultId::new("main"))
            .await
            .expect("Vault::create — invariant test"),
    );
    let index_path = gradatum_core::paths::vault_dir_index_path(&vault_dir);

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL_IDENTITY)
        .expect("preset ACL identity valide — invariant statique");

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
        multi_tenant: MultiTenantConfig { enabled: true },
        ..ServerConfig::default()
    };

    let idx = vault.index().clone();
    let mut state = AppState::with_jwt_and_acl(jwt, acl)
        .with_vault_arc(vault as Arc<dyn gradatum_vault::Registry>)
        .with_job_store(job_store as Arc<dyn gradatum_core::QueueStore>, jobs_pool)
        .with_server_config(cfg);
    state.search = idx as Arc<dyn gradatum_core::index::Index>;

    // B7 : seed agent_grants pour les identités du preset de test.
    seed_agent_grants(&index_path, &["main-agent", "remote-agent"]);

    IdEnv {
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

/// Provisionne le tenant distant `research` (actif + self-grant write).
fn seed_research(index_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(index_path).expect("open index.db");
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute_batch(&format!(
        "INSERT INTO tenants (id, status, created_at) VALUES ('research', 'active', {now});
         INSERT INTO tenant_vault_grants (tenant_id, vault_id, access)
           VALUES ('research', 'research', 'write');
         -- B9 : agent grant pour remote-agent sur son propre vault
         INSERT INTO agent_vault_grants (agent_id, vault_id, access) VALUES ('remote-agent', 'research', 'write');"
    ))
    .expect("seed research");
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

/// Une identité distante (tenant `research`) ne peut CIBLER le vault `main` sur
/// aucun chemin d'écriture — body `tenant_id = "main"` divergent → 403.
#[tokio::test]
async fn remote_tenant_cannot_target_main_vault_on_write_paths() {
    let env = build_identity_env().await;
    seed_research(&env.index_path);
    let jwt = sign_jwt(&env.state, "remote-agent", "research");

    let surfaces: [(&str, serde_json::Value); 3] = [
        (
            "/api/v1/vault_write",
            serde_json::json!({
                "title": "identity/main", "body": "takeover", "tenant_id": "main",
                "section_hint": "identity"
            }),
        ),
        (
            "/api/v1/vault_downgrade",
            serde_json::json!({
                "note_id": "01HFAKEULIDAAAAAAAAAAAAAAA", "reason": "x", "tenant_id": "main"
            }),
        ),
        (
            "/api/v1/vault_forget",
            serde_json::json!({
                "scope": { "type": "topic", "query": "identity" },
                "dry_run": false,
                "tenant_id": "main"
            }),
        ),
    ];
    for (uri, body) in surfaces {
        let status = post_json(build_router(env.state.clone()), uri, &jwt, body).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{uri} : identité distante ciblant le vault main doit être 403"
        );
    }
}

/// Forger `sub = "main-agent"` (owner privilégié des âmes) ne donne AUCUN accès
/// au vault `main` : le pinning de vault (`effective_tenant`) refuse AVANT le
/// guard identity — l'âme `identity/main` du vault `main` est hors d'atteinte.
#[tokio::test]
async fn forged_main_agent_sub_still_cannot_write_main_identity() {
    let env = build_identity_env().await;
    seed_research(&env.index_path);
    let jwt = sign_jwt(&env.state, "main-agent", "research");

    let status = post_json(
        build_router(env.state.clone()),
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({
            "title": "identity/main", "body": "soul takeover", "tenant_id": "main",
            "section_hint": "identity"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "sub forgé main-agent + tenant distant → 403 (pinning vault avant guard identity)"
    );
}

/// Les écritures légitimes d'une identité distante restent ÉPINGLÉES à son vault
/// propre : `tenant_id = "research"` cohérent avec le JWT → 202 (enqueue), le
/// vault `main` n'est jamais la cible.
#[tokio::test]
async fn remote_tenant_writes_pinned_to_own_vault() {
    let env = build_identity_env().await;
    seed_research(&env.index_path);
    let jwt = sign_jwt(&env.state, "remote-agent", "research");

    let status = post_json(
        build_router(env.state.clone()),
        "/api/v1/vault_write",
        &jwt,
        serde_json::json!({ "title": "note distante", "body": "b", "tenant_id": "research" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "écriture cohérente JWT/body sur le vault propre → 202"
    );
}
