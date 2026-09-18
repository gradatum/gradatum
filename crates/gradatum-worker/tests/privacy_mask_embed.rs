//! F-160 — what the async embedding path is handed once the body has been masked.
//!
//! ## Why this file exists
//!
//! A masked body is only masked if every consumer downstream of the write reads the body
//! that was written. The embedding path is the one consumer that **turns a body into a
//! durable artefact of its own**: a vector computed over the clear text would outlive the
//! masking, carry the destroyed passage into every similarity search that follows, and be
//! invisible in the note — the masked note would look masked while its vector still
//! described what was removed.
//!
//! ## What is measured, and how
//!
//! The assertion is on the **text the `Embedder` receives**, read back from a recording
//! embedder rather than from a mock that returns a constant. A mute mock would let the
//! handler pass any string at all; a recorder shows which one. The client in front of it is
//! a real `Vault` behind the `InternalClient` seam — the handler's own read path, not a
//! fixture constant — so what the embedder sees is what the vault actually persists under
//! the mounted policy.
//!
//! The pair of tests is the measurement: the same body, the same detector, the same write,
//! and two policies — the difference between them is the mode, and nothing else in the
//! fixture is allowed to explain it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use apalis::prelude::Data;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::VectorStore as _;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::pii::{
    DetectInput, Detection, MASK_FILLER, PiiCategory, PiiDetectError, PiiDetector, PiiSpan,
    PrivacyBudgets, PrivacyPolicy,
};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::{
    EmbedSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
};
use gradatum_dto::{
    EmbeddingOkResponse, PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest,
    PersistForgetRequest, PersistOkResponse,
};
use gradatum_embed::{EmbedBackend, EmbedError, Embedder};
use gradatum_index::SqliteIndex;
use gradatum_vault::{Registry, Vault};
use gradatum_worker::apalis_handlers::{MultiTenantCfg, handle_embed};
use gradatum_worker::internal_client::{
    EmbeddingReadDto, InternalClient, InternalClientError, NoteIdDto, NoteReadDto,
};
use tempfile::TempDir;
use ulid::Ulid;

/// The literal the detector reports and the write destroys.
const NEEDLE: &str = "alice@example.com";

// ── The detector ─────────────────────────────────────────────────────────────

/// Reports one `email` span on the needle, at the top of the scale.
struct DetectsNeedle;

impl PiiDetector for DetectsNeedle {
    fn detect(&self, input: &DetectInput<'_>) -> Result<Detection, PiiDetectError> {
        Ok(match input.text.find(NEEDLE) {
            Some(start) => Detection::new(
                vec![PiiSpan::new(start, NEEDLE.len(), PiiCategory::Email, 1.0)],
                0,
            ),
            None => Detection::new(Vec::new(), 0),
        })
    }
}

// ── Policies ─────────────────────────────────────────────────────────────────

/// The activation a masking deployment runs: one calibrated category.
fn masking_policy() -> PrivacyPolicy {
    let mut thresholds = BTreeMap::new();
    thresholds.insert(PiiCategory::Email, 900);
    PrivacyPolicy::new(true, thresholds, BTreeMap::new())
}

/// The same activation in the observing mode — the control.
fn reporting_policy() -> PrivacyPolicy {
    let mut thresholds = BTreeMap::new();
    thresholds.insert(PiiCategory::Email, 900);
    PrivacyPolicy::new(false, thresholds, BTreeMap::new())
}

// ── The client in front of a real vault ──────────────────────────────────────

/// An `InternalClient` whose `get_note` reads the **vault** the write path filled, rather
/// than a body the fixture hands it.
///
/// This is the seam the handler reads through, and putting a real vault behind it is what
/// makes the test a measurement of the product: a body supplied by the fixture would make
/// the assertion a statement about the fixture.
struct VaultReadClient {
    vault: Arc<Vault>,
    index: Arc<SqliteIndex>,
}

#[async_trait]
impl InternalClient for VaultReadClient {
    async fn get_note(
        &self,
        _vault_id: &str,
        ulid: &str,
    ) -> Result<NoteReadDto, InternalClientError> {
        let note = self.vault.read_note_by_id(ulid).await.map_err(|e| {
            InternalClientError::ServerError {
                status: 404,
                body: e.to_string(),
            }
        })?;
        Ok(NoteReadDto {
            note_id: note.id.0.to_string(),
            sha256_hex: String::new(),
            body: note.body.markdown,
            section: note.frontmatter.section.as_str().to_owned(),
            title: None,
            status: "live".to_owned(),
            tags: Vec::new(),
            forgotten: false,
            processed: false,
        })
    }

    async fn persist_embedding(
        &self,
        req: &PersistEmbeddingRequest,
    ) -> Result<EmbeddingOkResponse, InternalClientError> {
        let note_ulid =
            Ulid::from_string(&req.note_id).map_err(|e| InternalClientError::ServerError {
                status: 400,
                body: format!("invalid ULID: {e}"),
            })?;
        self.index
            .insert_note_embedding(
                "main",
                &NoteId(note_ulid),
                &req.embedder_id,
                req.dim,
                &req.vector,
            )
            .await
            .map_err(|e| InternalClientError::ServerError {
                status: 500,
                body: e.to_string(),
            })?;
        Ok(EmbeddingOkResponse {
            note_id: req.note_id.clone(),
            embedder_id: req.embedder_id.clone(),
            dim: req.dim as usize,
        })
    }

    async fn persist_curated(
        &self,
        _req: &PersistCuratedRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("the embedding path does not curate")
    }

    async fn persist_forget(
        &self,
        _req: &PersistForgetRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("the embedding path does not forget")
    }

    async fn persist_distill(
        &self,
        _req: &PersistDistillRequest,
    ) -> Result<PersistOkResponse, InternalClientError> {
        unimplemented!("the embedding path does not distil")
    }

    async fn delete_note(&self, _vault_id: &str, _ulid: &str) -> Result<(), InternalClientError> {
        unimplemented!("the embedding path does not delete")
    }

    async fn get_note_status(
        &self,
        _vault_id: &str,
        _ulid: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("the embedding path reads the body alone")
    }

    async fn get_note_embedding(
        &self,
        _vault_id: &str,
        _ulid: &str,
        _embedder_id: &str,
    ) -> Result<EmbeddingReadDto, InternalClientError> {
        unimplemented!("the embedding path writes vectors, it does not read them")
    }

    async fn get_trust(&self, _vault_id: &str, _ulid: &str) -> Result<f32, InternalClientError> {
        unimplemented!("the embedding path does not score trust")
    }

    async fn title_lookup(
        &self,
        _tenant: &str,
        _title: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("the embedding path does not resolve titles")
    }

    async fn id_lookup(
        &self,
        _tenant: &str,
        _note_id: &str,
    ) -> Result<Option<String>, InternalClientError> {
        unimplemented!("the embedding path does not resolve identifiers")
    }

    async fn list_notes_by_locus(
        &self,
        _vault: &str,
        _prefix: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("the embedding path lists nothing")
    }

    async fn list_by_status(
        &self,
        _vault: &str,
        _status: &str,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("the embedding path lists nothing")
    }

    async fn list_garbage(
        &self,
        _vault: &str,
        _before_ms: i64,
        _grace_days: u32,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("the embedding path sweeps nothing")
    }

    async fn search_fts_for_forget(
        &self,
        _vault: &str,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("the embedding path searches nothing")
    }

    async fn list_notes_by_agent(
        &self,
        _agent: &str,
        _vaults: &[String],
    ) -> Result<Vec<NoteIdDto>, InternalClientError> {
        unimplemented!("the embedding path lists nothing")
    }
}

// ── The recording embedder ───────────────────────────────────────────────────

/// An embedder that keeps the text it was handed.
///
/// The product's contract is that the vector describes *this* text; a mock returning a
/// constant would satisfy the handler while proving nothing about what was embedded.
struct RecordingEmbedder {
    seen: Mutex<Vec<String>>,
}

impl RecordingEmbedder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
        })
    }

    /// The single text this embedder was handed.
    fn only_text(&self) -> String {
        let seen = self.seen.lock().expect("unpoisoned");
        assert_eq!(seen.len(), 1, "one note is embedded once, got {seen:?}");
        seen[0].clone()
    }
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    fn embedder_id(&self) -> &str {
        "recording"
    }

    fn dim(&self) -> u16 {
        4
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.seen.lock().expect("unpoisoned").push(text.to_owned());
        Ok(vec![0.1_f32; 4])
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text).await?);
        }
        Ok(out)
    }

    fn backend_kind(&self) -> EmbedBackend {
        EmbedBackend::Noop
    }
}

// ── Fixture ──────────────────────────────────────────────────────────────────

/// Writes the body through a vault mounted under `policy` and embeds it.
///
/// Returns the text the embedder received. The vault is a real one and the client reads
/// through it, so the only thing the policy can change is the body the write persisted.
async fn embed_under(policy: PrivacyPolicy) -> String {
    let dir = TempDir::new().expect("TempDir vault");
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("Vault::create")
        .with_detector(Arc::new(DetectsNeedle), PrivacyBudgets::default())
        .with_privacy_policy(policy);
    let vault = Arc::new(vault);

    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Reference,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let written = vault
        .write_note(frontmatter, format!("contact {NEEDLE} for the audit"))
        .await
        .expect("write the note");

    // The embedding index is the one the handler persists into; it needs a row for the
    // note so the foreign key holds. Its body is never read — the client reads the vault.
    let index = Arc::new(SqliteIndex::open_in_memory().await.expect("index"));
    let persisted = Note {
        id: written.id,
        frontmatter: written.frontmatter.clone(),
        body: NoteBody {
            markdown: written.body.markdown.clone(),
        },
        version: NoteVersion::initial(),
        content_hash: ContentHash::compute(&written.frontmatter, &written.body.markdown),
        integrity_signature: None,
    };
    index.upsert_note(&persisted).await.expect("upsert_note");

    let client: Arc<dyn InternalClient> = Arc::new(VaultReadClient {
        vault: Arc::clone(&vault),
        index: Arc::clone(&index),
    });
    let embedder = RecordingEmbedder::new();
    let as_trait: Arc<dyn Embedder + Send + Sync> =
        Arc::clone(&embedder) as Arc<dyn Embedder + Send + Sync>;

    let result = handle_embed(
        embed_job(written.id.0),
        Data::new(client),
        Data::new(as_trait),
        Data::new(MultiTenantCfg::default()),
    )
    .await;
    assert!(
        result.is_ok(),
        "the embedding path must succeed over a masked body — err={result:?}"
    );
    drop(vault);
    drop(dir);

    embedder.only_text()
}

/// Builds the embed job for `note_id`, as the cascade enqueues it.
fn embed_job(note_id: Ulid) -> GradatumJob {
    let now = Utc::now();
    let class = JobClass::System;
    GradatumJob {
        priority: JobPriority::default_for(&class).as_u8(),
        record: JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: Job::Embed(EmbedSpec {
                    note_id,
                    tenant_id: "main".to_owned(),
                    force_regenerate: false,
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::Low,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Cascade,
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

// ── The measurement ──────────────────────────────────────────────────────────

/// Under the masking mode, the vector is computed over the text that replaced the
/// passage — what the embedder receives is the persisted body, filler and all.
#[tokio::test]
async fn the_embedder_receives_the_masked_body() {
    let text = embed_under(masking_policy()).await;

    assert!(
        !text.contains(NEEDLE),
        "the embedding must not describe the passage the mode destroyed, got: {text}"
    );
    assert!(
        text.contains(MASK_FILLER),
        "the embedder is handed the body as persisted, not a body with the passage cut \
         out of it, got: {text}"
    );
}

/// The control: the same fixture in the observing mode hands the embedder the clear text.
///
/// Without it, the test above would pass on a fixture that never had the passage in the
/// first place — the difference between the two is the mode, and nothing else.
#[tokio::test]
async fn the_embedder_receives_the_clear_body_when_nothing_rewrites_it() {
    let text = embed_under(reporting_policy()).await;

    assert!(
        text.contains(NEEDLE),
        "a deployment that rewrites nothing embeds what it wrote, got: {text}"
    );
    assert!(
        !text.contains(MASK_FILLER),
        "the observing mode writes no filler, got: {text}"
    );
}
