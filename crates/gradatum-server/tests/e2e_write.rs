//! Test E2E T9 — write → enqueue dans le job store actif (`gradatum_jobs`).
//!
//! Valide la frontière HTTP → file de jobs du moteur ACTIF :
//! 1. POST `/api/v1/vault_write` → 202 + job_id (ULID)
//! 2. Le job est enfilé dans `gradatum_jobs` (`SqliteQueueStore`, moteur Apalis)
//! 3. La file legacy `jobs_v2` (`SqliteQueue`) reste vide — `vault_write` ne l'emprunte pas
//!
//! Le traitement effectif du job (curator → vault) est couvert in-process par les tests
//! `gradatum-worker/tests/curate_*` qui appellent `handle_curate` directement. Ce test se
//! borne à la frontière d'enqueue : il ne dépend plus du `Dispatcher` legacy supprimé.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::{JwtService, TokenScope};
use gradatum_core::QueueStore;
use gradatum_core::audit::http::{AuditSink, HttpAuditEvent};
use gradatum_core::scope::VaultId;
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_queue::{Queue as _, SqliteQueue};
use gradatum_server::middleware::auth_middleware;
use gradatum_server::{api_v1, state::AppState};
use gradatum_vault::Vault;
use serde_json::Value;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use ulid::Ulid;

// ── NoopAuditSink local — l'audit HTTP n'est pas l'objet de ce test ───────────

struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn record(&self, _event: HttpAuditEvent) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Constantes ───────────────────────────────────────────────────────────────

const TEST_ACL_WRITE: &str = r#"
[[consumer]]
identity = "test-e2e-writer"
read_patterns = ["**"]
write_patterns = ["**"]
"#;

// ── Client HTTP ─────────────────────────────────────────────────────────────

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("construction client HTTP e2e")
}

// ── Test E2E ─────────────────────────────────────────────────────────────────

/// Test E2E : `vault_write` → 202 + job enfilé dans `gradatum_jobs` (job store actif),
/// file legacy `jobs_v2` restée vide.
#[tokio::test]
async fn write_enqueues_into_active_job_store() {
    // ── Infra partagée : vault + file legacy + job store actif ─────────────────
    let data_dir = TempDir::new().expect("tempdir data e2e");

    // Vault permanent dans data_dir.
    let vault_path = data_dir.path().join("vault");
    let vault = Arc::new(
        Vault::create(&vault_path, VaultId::new("main"))
            .await
            .expect("Vault::create e2e"),
    );

    // File legacy SQLite sur fichier (état `queue` requis par AppState ; jobs_v2).
    let queue_path = data_dir.path().join("queue.db");
    let queue = Arc::new(
        SqliteQueue::new(&queue_path)
            .await
            .expect("SqliteQueue::new e2e"),
    );

    // ── Serveur HTTP ─────────────────────────────────────────────────────────
    let jwt = JwtService::new_ephemeral();
    let bearer = jwt
        .sign(
            "test-e2e-writer",
            &["read".to_string(), "write".to_string()],
            TokenScope::Service,
            "main",
        )
        .expect("bearer e2e");

    let acl = AclEngine::from_preset_str(TEST_ACL_WRITE).expect("ACL e2e");

    // vault_write bridge vers job_store (gradatum_jobs) — nécessaire pour 202.
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("jobs pool in-memory — invariant test e2e_write");
    run_migrations(&jobs_pool)
        .await
        .expect("migrations gradatum_jobs — invariant test e2e_write");
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_queue(Arc::clone(&queue) as Arc<dyn gradatum_queue::Queue>)
        .with_job_store(
            Arc::clone(&job_store) as Arc<dyn gradatum_core::QueueStore>,
            jobs_pool,
        )
        .with_vault_arc(Arc::clone(&vault) as Arc<dyn gradatum_vault::Registry>)
        .with_audit(Arc::new(NoopAuditSink));

    use axum::{Router, middleware};
    let app = Router::new()
        .nest("/api/v1", api_v1::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind e2e");
    let addr = listener.local_addr().expect("addr e2e");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serveur e2e") });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ── Step 1 : POST vault_write ─────────────────────────────────────────────
    let title = "[DEBUG] E2E test bug";
    let resp = client()
        .post(format!("http://{addr}/api/v1/vault_write"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "title": title,
            "body": "Ce test valide la chaîne write→enqueue dans gradatum_jobs.",
            "tenant_id": "main"
        }))
        .send()
        .await
        .expect("POST vault_write e2e");

    assert_eq!(resp.status(), 202, "vault_write doit retourner 202");

    let body: Value = resp.json().await.expect("body 202 e2e");
    let job_id = body["job_id"]
        .as_str()
        .expect("job_id doit être une string ULID");
    assert!(!job_id.is_empty(), "job_id ne doit pas être vide");
    let job_ulid = Ulid::from_string(job_id).expect("job_id doit être un ULID valide");

    // ── Step 2 : le job est dans le job store actif (gradatum_jobs) ────────────
    let stored = job_store
        .get(job_ulid, Some("main"))
        .await
        .expect("lecture job_store");
    assert!(
        stored.is_some(),
        "vault_write doit enfiler le job dans gradatum_jobs (job store actif)"
    );

    // ── Step 3 : la file legacy jobs_v2 reste vide (vault_write ne l'emprunte pas) ──
    let legacy_depth = queue.depth().await.expect("depth file legacy");
    assert_eq!(
        legacy_depth, 0,
        "la file legacy jobs_v2 doit rester vide — vault_write enfile dans gradatum_jobs"
    );
}
