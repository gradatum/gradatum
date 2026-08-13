//! Démonstration end-to-end de l'outil MCP `job_status` (`jobs_v2::job_status_mcp`).
//!
//! Pilote un VRAI `SqliteQueueStore` à travers tous les états d'un job et interroge
//! `job_status_mcp` après chaque transition — le chemin exact du tool LIVE (ACL
//! `main/jobs` → filtre tenant → `store.get` → `JobStatusView::from_record`), sans
//! déployer. Prouve le contrat principal : `terminal` distingue « conclure » de
//! « continuer à poller », l'erreur d'un `Failed`/`DLQ` est surfacée, et le payload
//! de `Conflict` est lisible (état inatteignable en LIVE aujourd'hui, atteignable dès
//! que le verrou optimiste sera câblé — d'où le pilotage direct par le store ici).
//!
//! Flag `multi_tenant = OFF` (défaut LIVE) : le filtre tenant est `None`, ACL Read
//! legacy sur `main/jobs`.

use std::sync::Arc;

use gradatum_acl_policy::AclEngine;
use gradatum_auth::jwt::JwtService;
use gradatum_core::trust::TrustContext;
use gradatum_core::{
    CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
    JobResult, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueStore, RetryBackoff,
    TriggerSource,
};
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use gradatum_server::api_v1::jobs_v2::job_status_mcp;
use gradatum_server::config::{MultiTenantConfig, ServerConfig};
use gradatum_server::state::AppState;
use sqlx::sqlite::SqlitePoolOptions;
use tempfile::TempDir;
use ulid::Ulid;

/// ACL minimale : l'identité `main` (== `sub` du Bearer) lit/écrit `main/*`,
/// ce qui couvre le locus `main/jobs` évalué par `resolve_read_vault` à flag OFF.
const TEST_ACL: &str = r#"
[[consumer]]
identity = "main"
read_patterns  = ["main/*", "main/main"]
write_patterns = ["main/*", "main/main"]
"#;

struct Env {
    state: AppState,
    _dir: TempDir,
}

async fn build_env() -> Env {
    let dir = TempDir::new().expect("tempdir job_status demo");
    let index_path = dir.path().join("index.db");

    let jwt = JwtService::new_ephemeral();
    let acl = AclEngine::from_preset_str(TEST_ACL).expect("preset ACL valide");

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
        multi_tenant: MultiTenantConfig { enabled: false },
        ..ServerConfig::default()
    };

    let state = AppState::with_jwt_and_acl(jwt, acl)
        .with_search_path(&index_path)
        .await
        .expect("SqliteIndex::open — migrations")
        .with_job_store(job_store as Arc<dyn QueueStore>, jobs_pool)
        .with_server_config(cfg);

    Env { state, _dir: dir }
}

/// Bearer authentifié dont `sub = "main"` (l'identité ACL déclarée ci-dessus).
fn trust_main() -> TrustContext {
    TrustContext::BearerToken {
        kid: "test-kid".to_string(),
        aud: "gradatum".to_string(),
        sub: "main".into(),
        scopes: vec!["read".to_string(), "write".to_string()],
        tenant_id: "main".into(),
        jti: None,
    }
}

/// Construit un `JobRecord` `Pending` minimal (Curate), tel qu'enqueué par `vault_write`.
fn pending_job() -> JobRecord {
    let now = chrono::Utc::now();
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            class: JobClass::Api,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by: Some("test".to_string()),
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Transition Pending → Done : `terminal` bascule à `true`, `result_note` surfacé.
#[tokio::test]
async fn job_status_pending_then_done_transition() {
    let env = build_env().await;
    let trust = trust_main();
    let id = env
        .state
        .job_store
        .enqueue(pending_job())
        .await
        .expect("enqueue");

    // Instant T = Pending : transitoire → le consommateur continue à poller.
    let view = job_status_mcp(&env.state, &trust, &id.to_string())
        .await
        .expect("job_status Pending");
    assert_eq!(view.status, JobStatus::Pending);
    assert!(!view.terminal, "Pending est transitoire");
    assert!(view.completed_at.is_none());
    assert!(view.error.is_none());

    // P2-6 (v3) : le worker doit d'abord dequeue le job (Pending → Running via
    // atomic lease) avant de pouvoir le complete. Sans dequeue, le SELECT
    // `WHERE status = 'Running'` rejette le complete (NotFound).
    let _dequeued = env
        .state
        .job_store
        .dequeue(None)
        .await
        .expect("dequeue")
        .expect("le job doit être dequeuable");

    // Le worker termine le job.
    let result_note = Ulid::generate();
    env.state
        .job_store
        .complete(
            id,
            JobResult {
                success: true,
                duration_ms: 12,
                cost_usd: None,
                result_note: Some(result_note),
                conflict_payload: None,
            },
        )
        .await
        .expect("complete");

    // Instant T = Done : terminal → le consommateur conclut.
    let view = job_status_mcp(&env.state, &trust, &id.to_string())
        .await
        .expect("job_status Done");
    assert_eq!(view.status, JobStatus::Done);
    assert!(view.terminal, "Done est terminal");
    assert_eq!(view.result_note, Some(result_note));
    assert!(view.error.is_none());
}

/// `Failed` est **transitoire** (retry en attente) — `terminal == false`, erreur surfacée.
#[tokio::test]
async fn job_status_failed_is_transient_with_error() {
    let env = build_env().await;
    let trust = trust_main();
    let id = env
        .state
        .job_store
        .enqueue(pending_job())
        .await
        .expect("enqueue");

    // P2-7 (v3) : le worker doit d'abord dequeue le job (Pending → Running)
    // avant de pouvoir le fail. Sans dequeue, le SELECT
    // `WHERE status = 'Running'` rejette le fail (NotFound).
    let _dequeued = env
        .state
        .job_store
        .dequeue(None)
        .await
        .expect("dequeue")
        .expect("le job doit être dequeuable");

    env.state
        .job_store
        .fail(id, "boom: curator crashed", 1)
        .await
        .expect("fail");

    let view = job_status_mcp(&env.state, &trust, &id.to_string())
        .await
        .expect("job_status Failed");
    assert_eq!(view.status, JobStatus::Failed);
    assert!(
        !view.terminal,
        "Failed est transitoire : un retry est en attente, ne pas conclure"
    );
    assert_eq!(view.error.as_deref(), Some("boom: curator crashed"));
}

/// `DLQ` : terminal (retries épuisés), erreur surfacée — un `DLQ` muet ne vaut rien.
#[tokio::test]
async fn job_status_dlq_is_terminal_with_error() {
    let env = build_env().await;
    let trust = trust_main();
    let id = env
        .state
        .job_store
        .enqueue(pending_job())
        .await
        .expect("enqueue");

    env.state
        .job_store
        .fail_dlq(id, "dead: max retries reached")
        .await
        .expect("fail_dlq");

    let view = job_status_mcp(&env.state, &trust, &id.to_string())
        .await
        .expect("job_status DLQ");
    assert_eq!(view.status, JobStatus::DLQ);
    assert!(view.terminal, "DLQ est terminal");
    assert_eq!(view.error.as_deref(), Some("dead: max retries reached"));
}

/// `Conflict` : terminal, le payload optimistic-lock est lisible sans ambiguïté.
///
/// Atteignable dès que le verrou optimiste sera câblé (aucun chemin d'écriture ne
/// compare les hash aujourd'hui) — conçu pour ce futur proche, piloté ici via le store.
#[tokio::test]
async fn job_status_conflict_is_terminal_with_payload() {
    let env = build_env().await;
    let trust = trust_main();
    let id = env
        .state
        .job_store
        .enqueue(pending_job())
        .await
        .expect("enqueue");

    let payload = r#"{"current_sha256":"a3f1","attempted_sha256":"b2e0"}"#;
    env.state
        .job_store
        .mark_conflict(id, payload.to_string(), 5)
        .await
        .expect("mark_conflict");

    let view = job_status_mcp(&env.state, &trust, &id.to_string())
        .await
        .expect("job_status Conflict");
    assert_eq!(view.status, JobStatus::Conflict);
    assert!(view.terminal, "Conflict est terminal");
    let conflict = view.conflict.expect("conflict payload présent");
    assert_eq!(conflict["current_sha256"], "a3f1");
    assert_eq!(conflict["attempted_sha256"], "b2e0");
}

/// Un `job_id` mal formé → erreur (400 côté HTTP) ; job absent → erreur (404).
#[tokio::test]
async fn job_status_bad_id_and_missing() {
    let env = build_env().await;
    let trust = trust_main();

    // ULID invalide.
    assert!(
        job_status_mcp(&env.state, &trust, "not-a-ulid")
            .await
            .is_err(),
        "job_id mal formé doit échouer"
    );

    // ULID bien formé mais inexistant.
    let ghost = Ulid::generate().to_string();
    assert!(
        job_status_mcp(&env.state, &trust, &ghost).await.is_err(),
        "job absent doit échouer (404)"
    );
}
