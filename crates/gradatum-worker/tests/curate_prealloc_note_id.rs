//! Tests TDD item C — `handle_curate` honore le `spec.note_id` préalloué.
//!
//! ## Couverture
//!
//! | Test | Comportement validé |
//! |---|---|
//! | `handle_curate_vault_write_honors_prealloc_note_id` | La note créée est lisible à l'ULID préalloué |
//!
//! ## Architecture
//!
//! - Pattern identique à `curate_embed_chaining.rs` (CurateFixture + SqliteQueueStore in-memory)
//! - `spec.note_id` préalloué avant l'appel → `read_note(NoteId(prealloc))` doit retourner Ok

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use gradatum_core::{
    CurateSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    identity::NoteId, scope::VaultId,
};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::handle_curate;
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use tempfile::TempDir;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers (calqués sur curate_embed_chaining.rs)
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué.
async fn test_store() -> SqliteQueueStore {
    let db = QueueDb::open_in_memory().await.expect("pool in-memory");
    apply_sqlite_pragmas(&db).await.expect("pragmas");
    run_migrations(&db).await.expect("migrations");
    SqliteQueueStore::new(db)
}

/// Fixture : vault + index partagés.
struct CurateFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    _tmp: TempDir,
}

impl CurateFixture {
    async fn new() -> Self {
        let tmp = TempDir::new().expect("TempDir");
        let vault = Arc::new(
            Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
                .await
                .expect("Vault::create"),
        );
        let index = vault.index().clone();
        CurateFixture {
            vault,
            index,
            _tmp: tmp,
        }
    }
}

/// Construit un `GradatumJob` vault_write avec `note_id` préalloué fourni.
///
/// Préfixe `[DECISIONS]` → heuristique confidence ≥ 0.8 → Admitted direct.
fn curate_job_vault_write(prealloc: Ulid, body: &str, tenant_id: &str) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: prealloc,
                    tenant_id: tenant_id.to_string(),
                    title: Some("[DECISIONS] Titre préalloué item C".to_string()),
                    body: Some(body.to_string()),
                    ..Default::default()
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::High,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Running,
                created_at: now,
                started_at: Some(now),
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test — handle_curate honore spec.note_id préalloué (item C)
// ─────────────────────────────────────────────────────────────────────────────

/// Après traitement du job curate vault_write, la note doit être lisible à l'ULID
/// préalloué dans `spec.note_id`. Garantit write-time id == stored id.
///
/// Régression : avant le fix, `write_note` générait un nouvel ULID → read_note(prealloc) = 404.
#[tokio::test]
async fn handle_curate_vault_write_honors_prealloc_note_id() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn gradatum_core::QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let prealloc = Ulid::generate();
    let job = curate_job_vault_write(
        prealloc,
        "## Titre\nbody suffisant pour le curator.",
        "main",
    );

    let out = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("handle_curate");

    // La note doit avoir été créée (Admitted ou Pending — les deux honorent note_id)
    assert!(
        !out.notes_created.is_empty(),
        "handle_curate doit créer la note pour un job vault_write — output={out:?}"
    );

    // La note doit être lisible à l'ULID préalloué
    let read = fixture.vault.read_note(NoteId(prealloc)).await;
    assert!(
        read.is_ok(),
        "la note doit être lisible à l'ULID préalloué — err={:?}",
        read.err()
    );
    assert_eq!(
        read.unwrap().id,
        NoteId(prealloc),
        "l'id stocké doit être l'id préalloué"
    );
}
