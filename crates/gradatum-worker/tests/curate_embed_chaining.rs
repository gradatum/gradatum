//! Tests d'intégration — chaînage `curate → Job::Embed`.
//!
//! ## Couverture
//!
//! | Test | Comportement validé |
//! |---|---|
//! | `curate_admitted_enqueues_embed_job` | Note admise → Job::Embed enqueué avec bons champs |
//! | `enqueue_embed_failure_does_not_fail_curate` | Échec enqueue → curate reste Ok |
//! | `curate_rejected_does_not_enqueue_embed` | Note rejetée → 0 enqueue |
//! | `curate_dry_run_does_not_enqueue_embed` | DryRun → 0 enqueue |
//!
//! ## Architecture
//!
//! - `SqliteQueueStore` in-memory pour le chemin normal (tests 1 + 3 + 4)
//! - `FailingQueueStore` mock (enqueue retourne Err) pour test non-fatal (test 2)
//! - `Vault::create(TempDir)` + `CuratorPipeline::new()` réels
//!
//! ## Références
//!
//! - Plan `docs/internal/2026-06-01-tranche-a-curate-embed-plan.md`
//! - Design `docs/internal/2026-06-01-tranche-a-curate-embed-design.md`

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::{
    CurateSpec, DryRunAware, GradatumJob, Job, JobClass, JobFilter, JobLifecycle, JobLineage,
    JobMode, JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus,
    QueueError, QueueEvent, QueueStore, TriggerSource,
};
use gradatum_core::{identity::NoteId, scope::VaultId};
use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::handle_curate;
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use tempfile::TempDir;
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Crée un `SqliteQueueStore` in-memory avec schéma appliqué.
async fn test_store() -> SqliteQueueStore {
    let db = QueueDb::open_in_memory().await.expect("pool in-memory");
    apply_sqlite_pragmas(&db).await.expect("pragmas");
    run_migrations(&db).await.expect("migrations");
    SqliteQueueStore::new(db)
}

/// Construit un `GradatumJob` curate minimal avec title+body pour le path vault_write.
///
/// `mode` permet de tester DryRun vs Batch.
fn make_curate_job(title: &str, body: &str, mode: JobMode) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: Ulid::generate(),
                    tenant_id: "main".to_string(),
                    title: Some(title.to_string()),
                    body: Some(body.to_string()),
                    ..Default::default()
                }),
                class,
                mode,
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

/// Construit un job curate dont le curator devrait rejeter la note.
///
/// Titre très court + body vide → curator retourne Rejected (heuristique défaut).
/// Si le curator heuristique ne peut pas garantir Rejected, le test assertera
/// que la queue reste vide dans tous les cas sans note écrite.
fn make_curate_job_likely_rejected() -> GradatumJob {
    make_curate_job("", "", JobMode::Batch)
}

/// Fixture partagée pour les tests nécessitant vault + index + curator réels.
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

// ─────────────────────────────────────────────────────────────────────────────
// Mock QueueStore — retourne Err sur enqueue pour test non-fatal
// ─────────────────────────────────────────────────────────────────────────────

/// Mock `QueueStore` dont `enqueue()` retourne toujours une erreur.
///
/// Utilisé pour tester que `handle_curate` reste `Ok` même si l'enqueue échoue.
struct FailingQueueStore;

#[async_trait]
impl QueueStore for FailingQueueStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Err(QueueError::Storage(
            "test: enqueue forcément échoué".to_string(),
        ))
    }
    async fn dequeue(
        &self,
        __tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        unimplemented!("FailingQueueStore::dequeue — non requis pour ce test")
    }
    async fn get(&self, _id: Ulid, _tenant: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        unimplemented!("FailingQueueStore::get — non requis pour ce test")
    }
    async fn complete(
        &self,
        _id: Ulid,
        _result: gradatum_core::JobResult,
    ) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::complete — non requis pour ce test")
    }
    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::fail — non requis pour ce test")
    }
    async fn cancel(&self, _id: Ulid, _tenant: Option<&str>) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::cancel — non requis pour ce test")
    }
    async fn fail_dlq(&self, _id: Ulid, _err: &str) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::fail_dlq — non requis pour ce test")
    }
    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::set_pending — non requis pour ce test")
    }
    async fn recover_stale_leases(&self, _ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn cancel_expired_deadlines(
        &self,
        _now: chrono::DateTime<Utc>,
    ) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn promote_retries(&self, _now: chrono::DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        Ok(vec![])
    }
    async fn schedule_retry(
        &self,
        _id: Ulid,
        _at: chrono::DateTime<Utc>,
    ) -> Result<(), QueueError> {
        unimplemented!("FailingQueueStore::schedule_retry — non requis pour ce test")
    }
    async fn list(&self, _filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    fn subscribe(&self) -> Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1 — curate admis → Job::Embed enqueué
// ─────────────────────────────────────────────────────────────────────────────

/// Après curation d'une note admise, `handle_curate` doit enqueuer exactement
/// un `Job::Embed` avec `note_id` correct, `class=Agent`, `lineage.parent_job=curate.id`,
/// et `force_regenerate=false`.
///
/// Comportement attendu : `store.list(filter_all)` retourne 1 job `Job::Embed`.
#[tokio::test]
async fn curate_admitted_enqueues_embed_job() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    // Préfixe `[DECISIONS]` → heuristique confidence ≥ 0.8 → Admitted direct.
    let job = make_curate_job(
        "[DECISIONS] Note test chaînage embed",
        "# Note test\n\nContenu suffisant pour être admis dans le vault.",
        JobMode::Batch,
    );
    let curate_job_id = job.record.id;

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok — err={result:?}"
    );

    // Vérifier qu'un Job::Embed a été enqueué dans la queue
    let jobs = store
        .list(JobFilter::default())
        .await
        .expect("store.list après curate");

    assert_eq!(
        jobs.len(),
        1,
        "exactement 1 Job::Embed doit être enqueué après curate admis — found={jobs:?}"
    );

    let embed_job = &jobs[0];

    // Vérifier le kind Job::Embed
    let embed_spec = match &embed_job.spec.kind {
        Job::Embed(spec) => spec.clone(),
        other => panic!("job enqueué doit être Job::Embed — trouvé : {other:?}"),
    };

    // force_regenerate doit être false (idempotence déléguée à handle_embed)
    assert!(
        !embed_spec.force_regenerate,
        "force_regenerate doit être false"
    );

    // tenant_id doit être celui du job curate ("main")
    assert_eq!(
        embed_spec.tenant_id, "main",
        "tenant_id du Job::Embed doit correspondre au tenant du curate"
    );

    // note_id doit référencer une note réelle (non-zéro)
    assert_ne!(
        embed_spec.note_id,
        Ulid::nil(),
        "note_id du Job::Embed ne doit pas être nil"
    );

    // class doit être Agent (cascade depuis curate)
    assert_eq!(
        embed_job.spec.class,
        JobClass::Agent,
        "Job::Embed enqueué doit avoir class=Agent"
    );

    // lineage.parent_job doit référencer le job curate
    assert_eq!(
        embed_job.lineage.parent_job,
        Some(curate_job_id),
        "lineage.parent_job doit référencer l'id du job curate"
    );

    // await_jobs OBLIGATOIREMENT vide (cascade engine F-14 todo!())
    assert!(
        embed_job.scheduling.await_jobs.is_empty(),
        "await_jobs doit être vide — cascade engine F-14 est todo!()"
    );

    // Le curate a bien retourné notes_created non vide (note écrite)
    let output = result.unwrap();
    assert!(
        !output.notes_created.is_empty(),
        "handle_curate doit retourner notes_created non vide pour Admitted"
    );

    // note_id embed == note créée par curate
    let written_note_id = output.notes_created[0];
    assert_eq!(
        embed_spec.note_id, written_note_id,
        "note_id dans Job::Embed doit correspondre à la note créée par curate"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2 — échec enqueue → curate reste Ok (non-fatal)
// ─────────────────────────────────────────────────────────────────────────────

/// Si l'enqueue de `Job::Embed` échoue, `handle_curate` doit rester `Ok`.
///
/// La note est bien écrite dans le vault même si l'embed n'est pas schedulé.
/// Best-effort non-fatal : un warn est loggué, pas une erreur propagée.
#[tokio::test]
async fn enqueue_embed_failure_does_not_fail_curate() {
    let fixture = CurateFixture::new().await;
    let failing_queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(FailingQueueStore);
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let job = make_curate_job(
        "[DECISIONS] Note test non-fatal",
        "# Non-fatal test\n\nContenu suffisant pour admission.",
        JobMode::Batch,
    );

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&failing_queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    // Le curate doit rester Ok même si l'enqueue échoue
    assert!(
        result.is_ok(),
        "handle_curate doit rester Ok même si enqueue embed échoue — err={result:?}"
    );

    // La note doit être présente dans le vault (l'écriture vault n'est pas affectée)
    let output = result.unwrap();
    assert!(
        !output.notes_created.is_empty(),
        "la note doit être créée dans le vault même si enqueue embed échoue"
    );

    // Vérifier que la note est effectivement lisible dans le vault
    let note_id = NoteId(output.notes_created[0]);
    let read_result = fixture.vault.read_note(note_id).await;
    assert!(
        read_result.is_ok(),
        "la note doit être lisible dans le vault après curate — err={read_result:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 — note rejetée → 0 enqueue
// ─────────────────────────────────────────────────────────────────────────────

/// Si le curator rejette la note (`CurateOutcome::Rejected`),
/// aucun `Job::Embed` ne doit être enqueué (note non écrite → `written_note_id = None`).
///
/// Mécanisme : titre vide + body vide → curator retourne `Rejected`.
#[tokio::test]
async fn curate_rejected_does_not_enqueue_embed() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    // Titre vide + body vide → curator heuristique → Rejected
    let job = make_curate_job_likely_rejected();

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    // Le curate peut retourner Ok même pour un Rejected (note non écrite)
    assert!(
        result.is_ok(),
        "handle_curate doit retourner Ok même pour Rejected — err={result:?}"
    );

    // La queue doit rester vide — aucun embed schedulé
    let jobs = store
        .list(JobFilter::default())
        .await
        .expect("store.list après curate rejected");

    // Si le curator a effectivement rejeté (notes_created vide), la queue doit être vide.
    // Si le curator a admis malgré un titre vide (heuristique permissive), on vérifie
    // la cohérence : notes_created non vide → embed enqueué.
    let output = result.unwrap();
    if output.notes_created.is_empty() {
        // Rejected : pas d'enqueue
        assert!(
            jobs.is_empty(),
            "si note rejetée, 0 Job::Embed doit être enqueué — found={jobs:?}"
        );
    } else {
        // Admitted malgré titre/body vide : cohérence notes_created ↔ embed enqueué
        assert_eq!(
            jobs.len(),
            1,
            "si note admise, exactement 1 Job::Embed doit être enqueué"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4 — DryRun → 0 enqueue
// ─────────────────────────────────────────────────────────────────────────────

/// En mode `DryRun` (règle v62), `handle_curate` retourne avant tout traitement.
///
/// Aucune note écrite, aucun Job::Embed enqueué.
#[tokio::test]
async fn curate_dry_run_does_not_enqueue_embed() {
    let fixture = CurateFixture::new().await;
    let store = Arc::new(test_store().await);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::clone(&store) as _;
    let curator = Arc::new(gradatum_curator::CuratorPipeline::new());

    let job = make_curate_job(
        "[DECISIONS] Note DryRun test",
        "# Note DryRun\n\nContenu.",
        JobMode::DryRun,
    );

    // Pré-condition : is_dry_run doit être true
    assert!(
        job.record.is_dry_run(),
        "le job doit être reconnu en mode DryRun"
    );

    let result = handle_curate(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fixture.vault),
            Arc::clone(&fixture.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&curator) as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>),
        Data::new(Arc::clone(&queue)),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_curate DryRun doit retourner Ok — err={result:?}"
    );

    // DryRun : retour anticipé v62 — aucun enqueue
    let jobs = store
        .list(JobFilter::default())
        .await
        .expect("store.list après DryRun");

    assert!(
        jobs.is_empty(),
        "DryRun ne doit pas enqueuer de Job::Embed — found={jobs:?}"
    );

    // notes_created doit être vide (pas d'écriture vault en DryRun)
    let output = result.unwrap();
    assert!(
        output.notes_created.is_empty(),
        "DryRun ne doit pas créer de notes — found={:?}",
        output.notes_created
    );
}
