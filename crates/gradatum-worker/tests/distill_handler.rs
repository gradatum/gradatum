//! Tests d'intégration — handler `handle_distill` (F-22 distillation sémantique).
//!
//! # Cas couverts
//!
//! - `dry_run_lists_clusters_without_mutation` : le dry-run liste les clusters
//!   sans écrire de note ni marquer de source.
//! - `real_mode_creates_pending_review_synthesis` : mode réel → note de synthèse
//!   PendingReview, provenance "distilled", derived-from, trust dynamique persisté ;
//!   sources marquées processed=true.
//! - `processed_notes_never_reclustered` : une note déjà processed est exclue
//!   (idempotence — double run sans nouvelle synthèse).
//! - `synthesizer_failure_propagates_as_business_error` : synthèse en échec
//!   (gateway down) → HandlerError (job Failed propre), pas de note partielle.
//! - `vaultwide_refused_in_real_mode` : JobScope::VaultWide refusé hors dry-run.
//! - `notes_without_embedding_skipped` : note sans embedding ignorée silencieusement.

#[path = "test_internal_client.rs"]
mod test_internal_client;

use std::sync::Arc;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::VectorStore as _;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use std::time::Duration;

use gradatum_core::{
    DistillSource, GradatumJob, Job, JobClass, JobFilter, JobLifecycle, JobLineage, JobMode,
    JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueError,
    QueueEvent, QueueStore, TriggerSource, job_kind_str,
};
use gradatum_embed::{EmbedBackend, EmbedError, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use gradatum_worker::apalis_handlers::{
    ClusterSynthesis, DistillSynthesizer, SynthesisError, TemplateSynthesizer, handle_distill,
};
use gradatum_worker::internal_client::InternalClient;
use test_internal_client::TestInternalClient;

use tempfile::TempDir;
use ulid::Ulid;

// ── Embedder de test (id stable, dim fixe — non appelé par distill) ────────────

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    fn embedder_id(&self) -> &str {
        "test-embedder"
    }
    fn dim(&self) -> u16 {
        3
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![0.0; 3])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(vec![vec![0.0; 3]; texts.len()])
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── QueueStore mocks ──────────────────────────────────────────────────────────

/// No-op QueueStore — silently accepts all enqueues (used in dry-run and error tests where
/// the queue path is never reached or the enqueued content is irrelevant).
struct NoopQueueStore;

#[async_trait]
impl QueueStore for NoopQueueStore {
    async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
        Ok(Ulid::new())
    }
    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn get(&self, _id: Ulid) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn complete(
        &self,
        _id: Ulid,
        _result: gradatum_core::JobResult,
    ) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        Ok(())
    }
    async fn cancel(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail_dlq(&self, _id: Ulid, _err: &str) -> Result<(), QueueError> {
        Ok(())
    }
    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
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
        Ok(())
    }
    async fn list(&self, _filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

/// Capturing QueueStore — records every enqueued `JobRecord` for assertion in real-mode tests.
struct CapturingQueueStore {
    jobs: std::sync::Mutex<Vec<JobRecord>>,
}

impl CapturingQueueStore {
    fn new() -> Self {
        Self {
            jobs: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Returns a snapshot of all enqueued jobs.
    fn enqueued(&self) -> Vec<JobRecord> {
        self.jobs.lock().expect("CapturingQueueStore lock").clone()
    }
}

#[async_trait]
impl QueueStore for CapturingQueueStore {
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError> {
        let id = job.id;
        self.jobs
            .lock()
            .expect("CapturingQueueStore lock")
            .push(job);
        Ok(id)
    }
    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn get(&self, _id: Ulid) -> Result<Option<JobRecord>, QueueError> {
        Ok(None)
    }
    async fn complete(
        &self,
        _id: Ulid,
        _result: gradatum_core::JobResult,
    ) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail(&self, _id: Ulid, _err: &str, _attempt: u32) -> Result<(), QueueError> {
        Ok(())
    }
    async fn cancel(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
    }
    async fn fail_dlq(&self, _id: Ulid, _err: &str) -> Result<(), QueueError> {
        Ok(())
    }
    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        Ok(())
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
        Ok(())
    }
    async fn list(&self, _filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        Ok(vec![])
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        drop(tx);
        rx
    }
}

/// Synthétiseur toujours en échec — simule un gateway LLM down.
struct FailingSynthesizer;

#[async_trait]
impl DistillSynthesizer for FailingSynthesizer {
    async fn synthesize(
        &self,
        _cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError> {
        Err(SynthesisError::Unavailable(
            "gateway down (test)".to_string(),
        ))
    }
}

// ── Fixture ────────────────────────────────────────────────────────────────────

struct DistillFixture {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
    embedder: Arc<dyn Embedder + Send + Sync>,
    _tmp: TempDir,
}

async fn make_fixture() -> DistillFixture {
    let tmp = TempDir::new().expect("TempDir — distill_handler");
    let vault = Arc::new(
        Vault::create(tmp.path().join("vault").as_path(), VaultId::new("main"))
            .await
            .expect("Vault::create — distill_handler"),
    );
    let index: Arc<SqliteIndex> = vault.index().clone();
    DistillFixture {
        vault,
        index,
        embedder: Arc::new(StubEmbedder),
        _tmp: tmp,
    }
}

/// Écrit une note Live (sans locus → chemin `main/<id>.md`, lisible) + insère son
/// embedding. Retourne l'ULID. Les tests ciblent via `JobScope::Notes(...)`.
///
/// Note : `read_note` ne tente que `<tenant>/<id>.md` puis `<tenant>/<section>/<id>.md` —
/// un locus arbitraire ≠ section casserait la relecture. Locus omis ici par robustesse.
async fn write_note_with_embedding(
    fx: &DistillFixture,
    title: &str,
    body: &str,
    embedding: Vec<f32>,
) -> NoteId {
    let id = NoteId::new();
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: smallvec::SmallVec::new(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: Some("agent-log".to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let full_body = format!("# {title}\n\n{body}");
    let written = fx
        .vault
        .write_note_with_id(fm, full_body, id)
        .await
        .expect("write_note_with_id");
    fx.index
        .insert_note_embedding(&written.id, "test-embedder", 3, &embedding)
        .await
        .expect("insert_note_embedding");
    written.id
}

fn make_distill_job(scope: JobScope, mode: JobMode) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::System;
    let spec = DistillSource {
        scope: scope.clone(),
        ..DistillSource::default()
    };
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Distill(spec),
                class,
                mode,
                scope,
                priority: JobPriority::Low,
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

// ── Tests ────────────────────────────────────────────────────────────────────

/// Dry-run : liste les clusters candidats SANS aucune mutation (pas de note créée,
/// pas de source marquée processed).
#[tokio::test]
async fn dry_run_lists_clusters_without_mutation() {
    let fx = make_fixture().await;
    // Deux notes quasi-identiques (cosine ≈ 1) → un cluster.
    let a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    let b = write_note_with_embedding(&fx, "B", "contenu b", vec![0.99, 0.01, 0.0]).await;

    let job = make_distill_job(JobScope::Notes(vec![a.0, b.0]), JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("dry-run ne doit pas échouer");

    assert!(
        out.notes_created.is_empty(),
        "dry-run ne crée aucune note : {:?}",
        out.notes_created
    );
    assert!(
        out.result_note_md.contains("DRY-RUN"),
        "{}",
        out.result_note_md
    );

    // La source A ne doit PAS être marquée processed.
    let note_a = fx.vault.read_note(a).await.expect("read A");
    assert!(
        note_a.frontmatter.extra.get("processed").is_none(),
        "dry-run ne doit pas marquer les sources"
    );
}

/// Mode réel : enqueues one `Job::Validate` per cluster (no direct persist in handle_distill).
/// Persistence (note write + source marking) is delegated to handle_validate.
#[tokio::test]
async fn real_mode_creates_pending_review_synthesis() {
    let fx = make_fixture().await;
    let a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    let b = write_note_with_embedding(&fx, "B", "contenu b", vec![0.99, 0.01, 0.0]).await;

    let queue = Arc::new(CapturingQueueStore::new());
    let job = make_distill_job(JobScope::Notes(vec![a.0, b.0]), JobMode::Batch);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(Arc::clone(&queue) as Arc<dyn QueueStore + Send + Sync>),
    )
    .await
    .expect("mode réel ne doit pas échouer");

    // One cluster → one Job::Validate enqueued (synth_id pre-allocated).
    assert_eq!(
        out.notes_created.len(),
        1,
        "un cluster → une synthèse enqueued"
    );

    // Persistence is delegated: no direct write to vault from handle_distill.
    // The synthesis note does NOT exist in the vault yet.
    let synth_id = NoteId(out.notes_created[0]);
    assert!(
        fx.vault.read_note(synth_id).await.is_err(),
        "handle_distill ne doit PAS écrire directement dans le vault (persistence déléguée à handle_validate)"
    );

    // Exactly one Job::Validate was enqueued with the correct source ULIDs.
    let enqueued = queue.enqueued();
    assert_eq!(
        enqueued.len(),
        1,
        "exactly one Job::Validate enqueued per cluster"
    );
    let kind = job_kind_str(&enqueued[0].spec.kind);
    assert_eq!(
        kind, "Validate",
        "enqueued job kind must be Validate, got {kind}"
    );

    // The ValidateSpec carries both source IDs.
    let Job::Validate(ref spec) = enqueued[0].spec.kind else {
        panic!("expected Job::Validate, got {:?}", kind);
    };
    let src_set: std::collections::HashSet<Ulid> = spec.source_ids.iter().copied().collect();
    assert!(
        src_set.contains(&a.0),
        "source A must be in ValidateSpec.source_ids"
    );
    assert!(
        src_set.contains(&b.0),
        "source B must be in ValidateSpec.source_ids"
    );
    assert_eq!(
        spec.note_id, synth_id.0,
        "ValidateSpec.note_id must match out.notes_created[0]"
    );

    // Sources must NOT be marked processed by handle_distill (marking moved to handle_validate).
    for src in [a, b] {
        let note = fx.vault.read_note(src).await.expect("read source");
        assert!(
            note.frontmatter.extra.get("processed").is_none(),
            "source {src} must NOT be marked processed by handle_distill"
        );
    }

    // result_note_md reports enqueuing, not direct persistence.
    assert!(
        out.result_note_md.contains("enqueued for validation"),
        "result_note_md must mention enqueue: {}",
        out.result_note_md
    );
}

/// Idempotence : une note déjà processed n'est jamais re-clusterisée.
/// Un second run sur le même scope ne produit aucun nouvel enqueue.
/// Since handle_distill no longer marks sources itself (moved to handle_validate),
/// we simulate the validate worker by marking sources processed manually between runs.
#[tokio::test]
async fn processed_notes_never_reclustered() {
    let fx = make_fixture().await;
    let a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    let b = write_note_with_embedding(&fx, "B", "contenu b", vec![0.99, 0.01, 0.0]).await;
    let scope = JobScope::Notes(vec![a.0, b.0]);

    let synth = Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>;
    let queue1 = Arc::new(CapturingQueueStore::new());

    // Run 1: enqueues one Job::Validate.
    let out1 = handle_distill(
        make_distill_job(scope.clone(), JobMode::Batch),
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::clone(&synth)),
        Data::new(Arc::clone(&queue1) as Arc<dyn QueueStore + Send + Sync>),
    )
    .await
    .expect("run 1");
    assert_eq!(out1.notes_created.len(), 1, "run 1: one synthesis enqueued");
    assert_eq!(
        queue1.enqueued().len(),
        1,
        "run 1: one Job::Validate enqueued"
    );

    // Simulate handle_validate marking sources as processed=true.
    for src in [a, b] {
        let note = fx.vault.read_note(src).await.expect("read source");
        let mut fm = note.frontmatter.clone();
        fm.extra
            .insert("processed".to_string(), toml::Value::Boolean(true));
        fx.vault
            .write_note_with_id(fm, note.body.markdown.clone(), src)
            .await
            .expect("mark processed");
    }

    let queue2 = Arc::new(CapturingQueueStore::new());

    // Run 2: sources now processed → excluded → zero enqueues.
    let out2 = handle_distill(
        make_distill_job(scope.clone(), JobMode::Batch),
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::clone(&synth)),
        Data::new(Arc::clone(&queue2) as Arc<dyn QueueStore + Send + Sync>),
    )
    .await
    .expect("run 2");
    assert!(
        out2.notes_created.is_empty(),
        "run 2 idempotent : aucune nouvelle synthèse, obtenu {:?}",
        out2.notes_created
    );
    assert!(
        queue2.enqueued().is_empty(),
        "run 2: no Job::Validate enqueued"
    );
}

/// Synthèse en échec (gateway down) → HandlerError (job Failed propre).
/// Aucune note de synthèse ne doit être créée.
#[tokio::test]
async fn synthesizer_failure_propagates_as_business_error() {
    let fx = make_fixture().await;
    let a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    let b = write_note_with_embedding(&fx, "B", "contenu b", vec![0.99, 0.01, 0.0]).await;

    let job = make_distill_job(JobScope::Notes(vec![a.0, b.0]), JobMode::Batch);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let res = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(FailingSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await;

    assert!(res.is_err(), "synthèse en échec doit propager une erreur");

    // La source ne doit PAS être marquée processed (job échoué avant marquage).
    let note_a = fx.vault.read_note(a).await.expect("read A");
    assert!(
        note_a.frontmatter.extra.get("processed").is_none(),
        "aucune source marquée si synthèse échoue"
    );
}

/// JobScope::VaultWide refusé hors dry-run (mitigation R3).
#[tokio::test]
async fn vaultwide_refused_in_real_mode() {
    let fx = make_fixture().await;
    let job = make_distill_job(JobScope::VaultWide, JobMode::Batch);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let res = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await;
    assert!(res.is_err(), "VaultWide en mode réel doit être refusé");
}

/// VaultWide autorisé en dry-run (exploration).
#[tokio::test]
async fn vaultwide_allowed_in_dry_run() {
    let fx = make_fixture().await;
    let _a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    let job = make_distill_job(JobScope::VaultWide, JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("VaultWide dry-run autorisé");
    assert!(out.result_note_md.contains("DRY-RUN"));
}

/// Note sans embedding : ignorée silencieusement (pas de panique, pas d'erreur).
#[tokio::test]
async fn notes_without_embedding_skipped() {
    let fx = make_fixture().await;
    // Note AVEC embedding.
    let a = write_note_with_embedding(&fx, "A", "contenu a", vec![1.0, 0.0, 0.0]).await;
    // Note SANS embedding : écrite directement sans insert_note_embedding.
    let id_no_emb = NoteId::new();
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: smallvec::SmallVec::new(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: Some("agent-log".to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    fx.vault
        .write_note_with_id(fm, "# NoEmb\n\nsans embedding".to_string(), id_no_emb)
        .await
        .expect("write note sans embedding");

    // Dry-run : seule la note A (avec embedding) est candidate → 1 cluster singleton.
    let job = make_distill_job(JobScope::Notes(vec![a.0, id_no_emb.0]), JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("note sans embedding ignorée sans erreur");
    assert!(
        out.result_note_md.contains("1 note(s) candidate(s)"),
        "seule la note avec embedding est candidate : {}",
        out.result_note_md
    );
}

// ── Helpers durcissement post-audit ─────────────────────────────────────────

/// Construit un job distill avec un `DistillSource` complet (batch_limit, seuil, scope).
fn make_distill_job_with_spec(spec: DistillSource, mode: JobMode) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::System;
    let scope = spec.scope.clone();
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Distill(spec),
                class,
                mode,
                scope,
                priority: JobPriority::Low,
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

/// Écrit une note Live + embedding, puis applique une mutation de frontmatter
/// (forgotten / Garbage) via une seconde écriture. Retourne l'ULID.
async fn write_note_then_mutate(
    fx: &DistillFixture,
    title: &str,
    embedding: Vec<f32>,
    forgotten: bool,
    garbage: bool,
) -> NoteId {
    let id = write_note_with_embedding(fx, title, "contenu", embedding).await;
    let mut note = fx.vault.read_note(id).await.expect("read");
    let mut fm = note.frontmatter.clone();
    if forgotten {
        fm.forgotten = Some(true);
        fm.forgotten_at = Some(Utc::now());
    }
    if garbage {
        fm.status = NoteStatus::Garbage;
    }
    note = fx
        .vault
        .write_note_with_id(fm, note.body.markdown.clone(), id)
        .await
        .expect("mutate");
    note.id
}

/// P2-3 : une note `forgotten=true` n'est jamais distillée, quel que soit le scope (Notes explicite).
#[tokio::test]
async fn forgotten_notes_skipped_in_distill() {
    let fx = make_fixture().await;
    let live = write_note_with_embedding(&fx, "Live", "contenu", vec![1.0, 0.0, 0.0]).await;
    let forgotten =
        write_note_then_mutate(&fx, "Oubliée", vec![0.99, 0.01, 0.0], true, false).await;

    // Scope Notes explicite incluant la note forgotten → elle doit être ignorée.
    let job = make_distill_job(JobScope::Notes(vec![live.0, forgotten.0]), JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("dry-run");
    assert!(
        out.result_note_md.contains("1 note(s) candidate(s)"),
        "forgotten exclue → 1 seule candidate (Live) : {}",
        out.result_note_md
    );
}

/// P2-3 : une note `status=Garbage` n'est jamais distillée, quel que soit le scope.
#[tokio::test]
async fn garbage_notes_skipped_in_distill() {
    let fx = make_fixture().await;
    let live = write_note_with_embedding(&fx, "Live", "contenu", vec![1.0, 0.0, 0.0]).await;
    let garbage =
        write_note_then_mutate(&fx, "Corbeille", vec![0.99, 0.01, 0.0], false, true).await;

    let job = make_distill_job(JobScope::Notes(vec![live.0, garbage.0]), JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("dry-run");
    assert!(
        out.result_note_md.contains("1 note(s) candidate(s)"),
        "Garbage exclue → 1 seule candidate (Live) : {}",
        out.result_note_md
    );
}

/// P2-1 : la troncature batch_limit s'applique APRÈS le filtre processed.
/// Des notes processed en tête de fenêtre ne doivent pas masquer les candidates suivantes.
#[tokio::test]
async fn batch_limit_applied_after_processed_filter() {
    let fx = make_fixture().await;
    // 2 notes déjà processed (en tête) + 2 notes fraîches.
    let p1 = write_note_with_embedding(&fx, "P1", "c", vec![1.0, 0.0, 0.0]).await;
    let p2 = write_note_with_embedding(&fx, "P2", "c", vec![0.99, 0.0, 0.0]).await;
    // Marquer p1/p2 processed via une mutation frontmatter.
    for pid in [p1, p2] {
        let note = fx.vault.read_note(pid).await.expect("read");
        let mut fm = note.frontmatter.clone();
        fm.extra
            .insert("processed".to_string(), toml::Value::Boolean(true));
        fx.vault
            .write_note_with_id(fm, note.body.markdown.clone(), pid)
            .await
            .expect("mark processed");
    }
    let f1 = write_note_with_embedding(&fx, "F1", "c", vec![0.0, 1.0, 0.0]).await;
    let f2 = write_note_with_embedding(&fx, "F2", "c", vec![0.0, 0.99, 0.0]).await;

    // batch_limit=2 : si la troncature était AVANT le filtre, p1/p2 (processed) rempliraient
    // la fenêtre et f1/f2 seraient inatteignables → 0 candidate. Avec le fix : 2 candidates (f1/f2).
    let spec = DistillSource {
        scope: JobScope::Notes(vec![p1.0, p2.0, f1.0, f2.0]),
        batch_limit: 2,
        ..DistillSource::default()
    };
    let job = make_distill_job_with_spec(spec, JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("dry-run");
    assert!(
        out.result_note_md.contains("2 note(s) candidate(s)"),
        "P2-1 : f1/f2 atteignables malgré p1/p2 processed en tête : {}",
        out.result_note_md
    );
}

/// P2-4 : `JobScope::Locus("")` refusé en mode réel (matcherait tout le vault).
#[tokio::test]
async fn empty_locus_rejected_in_real_mode() {
    let fx = make_fixture().await;
    let spec = DistillSource {
        scope: JobScope::Locus("   ".to_string()), // whitespace-only
        ..DistillSource::default()
    };
    let job = make_distill_job_with_spec(spec, JobMode::Batch);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let res = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await;
    assert!(
        res.is_err(),
        "Locus whitespace-only en mode réel doit être refusé"
    );
}

/// P2-4 : `confidence_threshold` hors borne est clampé (n'empêche pas le run).
#[tokio::test]
async fn out_of_range_threshold_clamped() {
    let fx = make_fixture().await;
    let _a = write_note_with_embedding(&fx, "A", "c", vec![1.0, 0.0, 0.0]).await;
    let a2 = write_note_with_embedding(&fx, "A2", "c", vec![1.0, 0.0, 0.0]).await;
    let a1 = write_note_with_embedding(&fx, "A1", "c", vec![1.0, 0.0, 0.0]).await;

    // Seuil aberrant 5.0 → clampé à 1.0 (clustering valide, pas de panique).
    let spec = DistillSource {
        scope: JobScope::Notes(vec![a1.0, a2.0]),
        confidence_threshold: 5.0,
        ..DistillSource::default()
    };
    let job = make_distill_job_with_spec(spec, JobMode::DryRun);
    let queue: Arc<dyn QueueStore + Send + Sync> = Arc::new(NoopQueueStore);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(queue),
    )
    .await
    .expect("seuil clampé → pas de panique");
    // Seuil clampé à 1.0 : les vecteurs identiques (cosine=1.0 ≥ 1.0) sont regroupés.
    assert!(
        out.result_note_md.contains("seuil cosine 1.00"),
        "{}",
        out.result_note_md
    );
}

/// P2-2 (adapted): the job summary reports enqueue count (persistence delegated to handle_validate).
/// Previously tested `mark_failures: 0`; with F-43, source marking moved to handle_validate.
#[tokio::test]
async fn job_output_reports_enqueue_count() {
    let fx = make_fixture().await;
    let a = write_note_with_embedding(&fx, "A", "c", vec![1.0, 0.0, 0.0]).await;
    let b = write_note_with_embedding(&fx, "B", "c", vec![0.99, 0.01, 0.0]).await;

    let queue = Arc::new(CapturingQueueStore::new());
    let job = make_distill_job(JobScope::Notes(vec![a.0, b.0]), JobMode::Batch);
    let out = handle_distill(
        job,
        Data::new(Arc::new(TestInternalClient::new(
            Arc::clone(&fx.vault),
            Arc::clone(&fx.index),
        )) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&fx.embedder)),
        Data::new(Arc::new(TemplateSynthesizer) as Arc<dyn DistillSynthesizer + Send + Sync>),
        Data::new(Arc::clone(&queue) as Arc<dyn QueueStore + Send + Sync>),
    )
    .await
    .expect("mode réel");
    // Nominal: one cluster → one Job::Validate enqueued.
    assert!(
        out.result_note_md.contains("enqueued for validation"),
        "result_note_md must report enqueue: {}",
        out.result_note_md
    );
    assert_eq!(queue.enqueued().len(), 1, "one Job::Validate enqueued");
}
