//! Integration tests for [`gradatum_worker::apalis_handlers::handle_validate`] (F-43).
//!
//! # Cases
//!
//! - `well_grounded_synthesis_passes_with_base_trust`: aligned embedder (cosine=1.0)
//!   → quality_score >= threshold → trust == base_trust, tags empty.
//! - `poorly_grounded_synthesis_degrades_trust_and_adds_tag`: orthogonal embedder
//!   (cosine=0.0) → quality_score < threshold → degraded trust, tags=["quality-low"].

use std::sync::Arc;

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::{
    GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
    JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource, ValidateSpec,
};
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_embed::{EmbedBackend, EmbedError, Embedder};
use gradatum_worker::apalis_handlers::handle_validate;
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tokio::sync::Mutex;
use ulid::Ulid;

// ── CapturingClient mock ─────────────────────────────────────────────────────

/// Records `(trust, tags)` from the first `persist_distill` call with
/// `mark_processed=false` (the synthesis note creation call).
///
/// Uses `tokio::sync::Mutex` to avoid holding a lock across `.await` points.
struct CapturingClient {
    synthesis: Mutex<Option<(Option<f32>, Vec<String>)>>,
}

impl CapturingClient {
    fn new() -> Self {
        Self {
            synthesis: Mutex::new(None),
        }
    }

    /// Returns `(trust, tags)` from the first synthesis persist call, or `None`.
    async fn get_synthesis(&self) -> Option<(Option<f32>, Vec<String>)> {
        self.synthesis.lock().await.clone()
    }
}

#[async_trait]
impl InternalClient for CapturingClient {
    async fn persist_curated(
        &self,
        _req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        Err(InternalClientError::ServerError {
            status: 501,
            body: "not used in validate tests".to_string(),
        })
    }

    async fn persist_embedding(
        &self,
        _req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        Err(InternalClientError::ServerError {
            status: 501,
            body: "not used in validate tests".to_string(),
        })
    }

    async fn persist_forget(
        &self,
        _req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        Err(InternalClientError::ServerError {
            status: 501,
            body: "not used in validate tests".to_string(),
        })
    }

    async fn persist_distill(
        &self,
        req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        // Capture only the synthesis call (the first one with mark_processed=false).
        if !req.mark_processed {
            let mut guard = self.synthesis.lock().await;
            if guard.is_none() {
                *guard = Some((req.trust, req.tags.clone()));
            }
        }
        Ok(PersistOkResponse {
            note_id: req.note_id.clone(),
            status: "ok".to_string(),
        })
    }

    async fn delete_note(&self, _vault_id: &str, ulid: &str) -> Result<(), InternalClientError> {
        Err(InternalClientError::NotFound {
            ulid: ulid.to_string(),
        })
    }

    async fn get_note(
        &self,
        _vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        Err(InternalClientError::NotFound {
            ulid: ulid.to_string(),
        })
    }

    async fn get_note_status(
        &self,
        _vault_id: &str,
        _ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None)
    }

    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        Err(InternalClientError::NotFound {
            ulid: ulid.to_string(),
        })
    }

    async fn get_trust(&self, _vault_id: &str, ulid: &str) -> Result<f32, InternalClientError> {
        Err(InternalClientError::NotFound {
            ulid: ulid.to_string(),
        })
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None)
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        Ok(None)
    }

    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        Ok(vec![])
    }
}

// ── Embedder mocks ───────────────────────────────────────────────────────────

/// Returns [1,0,0] for both `embed()` and `embed_batch()`.
/// cosine(synthesis_embedding, source_centroid) = 1.0 → well-grounded.
struct AlignedEmbedder;

#[async_trait]
impl Embedder for AlignedEmbedder {
    fn embedder_id(&self) -> &str {
        "aligned-test"
    }
    fn dim(&self) -> u16 {
        3
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(vec![1.0, 0.0, 0.0])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(vec![vec![1.0, 0.0, 0.0]; texts.len()])
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

/// `embed()` returns [0,1,0] (synthesis); `embed_batch()` returns [[1,0,0]] (sources).
/// cosine([0,1,0], centroid([[1,0,0]])) = 0.0 → poorly-grounded.
struct OrthogonalEmbedder;

#[async_trait]
impl Embedder for OrthogonalEmbedder {
    fn embedder_id(&self) -> &str {
        "orthogonal-test"
    }
    fn dim(&self) -> u16 {
        3
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        // Synthesis vector [0,1,0] — orthogonal to source centroid [1,0,0].
        Ok(vec![0.0, 1.0, 0.0])
    }
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Source vectors — [1,0,0]; cosine with [0,1,0] = 0.0.
        Ok(vec![vec![1.0, 0.0, 0.0]; texts.len()])
    }
    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── Job factory ──────────────────────────────────────────────────────────────

fn make_validate_job(spec: ValidateSpec) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::Agent;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Validate(spec),
                class,
                mode: JobMode::Batch,
                // scope is not used by handle_validate (spec carries source_ids directly).
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

/// Shared ValidateSpec for both test cases.
/// No uppercase words and no numbers → num_penalty=1.0, entity_penalty=1.0.
fn base_spec(source_id: Ulid) -> ValidateSpec {
    ValidateSpec {
        note_id: Ulid::generate(),
        tenant_id: "main".to_string(),
        title: "test synthesis".to_string(),
        body: "synthesis body without numbers or proper nouns".to_string(),
        source_ids: vec![source_id],
        source_texts: vec!["source body without numbers or proper nouns".to_string()],
        source_trusts: vec![1.0],
        base_trust: 0.8,
        threshold: ValidateSpec::default_threshold(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Case A — aligned embedder: cosine(synthesis, source_centroid) = 1.0, f17≈1.0,
/// f47=1.0, num_penalty=1.0, entity_penalty=1.0 → score=1.0 >= 0.75 (threshold).
///
/// Expected: persist_distill called with trust == base_trust (0.8), tags empty.
#[tokio::test]
async fn well_grounded_synthesis_passes_with_base_trust() {
    let source_id = Ulid::generate();
    let spec = base_spec(source_id);
    let base_trust = spec.base_trust;

    let client = Arc::new(CapturingClient::new());
    let embedder = Arc::new(AlignedEmbedder);

    let out = handle_validate(
        make_validate_job(spec),
        Data::new(Arc::clone(&client) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&embedder) as Arc<dyn Embedder + Send + Sync>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("well-grounded synthesis must succeed");

    assert!(
        !out.notes_created.is_empty(),
        "synthesis note must appear in notes_created"
    );

    let (trust, tags) = client
        .get_synthesis()
        .await
        .expect("persist_distill (mark_processed=false) must have been called");

    assert!(
        (trust.unwrap_or(0.0) - base_trust).abs() < 1e-5,
        "trust must equal base_trust ({base_trust}) for well-grounded synthesis, got {trust:?}"
    );
    assert!(
        tags.is_empty(),
        "tags must be empty for well-grounded synthesis, got {tags:?}"
    );
}

/// Case B — orthogonal embedder: cosine(synthesis, source_centroid) = 0.0
/// → score = 0.0 < 0.75 (threshold).
///
/// Expected: persist_distill called with trust < base_trust (degraded), tags=["quality-low"].
#[tokio::test]
async fn poorly_grounded_synthesis_degrades_trust_and_adds_tag() {
    let source_id = Ulid::generate();
    let spec = base_spec(source_id);
    let base_trust = spec.base_trust;

    let client = Arc::new(CapturingClient::new());
    let embedder = Arc::new(OrthogonalEmbedder);

    let out = handle_validate(
        make_validate_job(spec),
        Data::new(Arc::clone(&client) as Arc<dyn InternalClient>),
        Data::new(Arc::clone(&embedder) as Arc<dyn Embedder + Send + Sync>),
        Data::new(gradatum_worker::apalis_handlers::MultiTenantCfg::default()),
    )
    .await
    .expect("poorly-grounded synthesis must not fail (non-blocking gate)");

    assert!(
        !out.notes_created.is_empty(),
        "synthesis note must appear in notes_created even when degraded"
    );

    let (trust, tags) = client
        .get_synthesis()
        .await
        .expect("persist_distill (mark_processed=false) must have been called");

    assert!(
        trust.unwrap_or(1.0) < base_trust,
        "trust must be degraded (< {base_trust}) for poorly-grounded synthesis, got {trust:?}"
    );
    assert_eq!(
        tags,
        vec!["quality-low".to_string()],
        "tags must be [\"quality-low\"] for poorly-grounded synthesis"
    );
}
