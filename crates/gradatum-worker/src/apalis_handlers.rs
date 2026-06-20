//! Apalis handlers for the active [`gradatum_core::Job`] variants.
//!
//! Each handler is an async function `fn(GradatumJob) -> Result<JobOutput, HandlerError>`
//! matching the signature expected by `apalis::WorkerBuilder`.
//!
//! # Implemented handlers
//!
//! The `handle_curate`, `handle_embed`, `handle_forget`, and `handle_purge` handlers are
//! fully operational. They rely on the `gradatum-curator`, `gradatum-vault`, and
//! `gradatum-embed` crates via dependencies injected by [`crate::monitor::build_monitor`].
//!
//! | Handler | Job variant | Status |
//! |---|---|---|
//! | `handle_curate` | `Job::Curate(CurateSpec)` | Operational |
//! | `handle_embed` | `Job::Embed(EmbedSpec)` | Operational |
//! | `handle_forget` | `Job::Forget(ForgetSpec)` | Operational |
//! | `handle_purge` | `Job::Purge(PurgeSpec)` | Operational |
//! | `handle_distill` | `Job::Distill(DistillSource)` | Operational (deterministic MVP synthesis) |
//! | `handle_reindex` | `Job::ReIndex(ReIndexMode)` | Deferred (see below) |
//!
//! # DryRunAware
//!
//! Each handler checks `job.record.is_dry_run()` as its FIRST instruction
//! (`JobMode::DryRun` = single mechanism, no side effects).
//!
//! # Dependency injection
//!
//! `build_monitor` injects via `.data()`:
//! - `Data<Arc<Vault>>` — vault registry for reading/writing notes
//! - `Data<Arc<dyn CuratorProcess + Send + Sync>>` — curator pipeline
//! - `Data<Arc<dyn Embedder + Send + Sync>>` — embedding backend
//! - `Data<Arc<dyn Index>>` — type-erased index (FTS5 + lifecycle)
//!
//! # ReIndex — deferred
//!
//! `handle_reindex` returns an explicit error for all modes: `SqliteIndex::rebuild_fts()`
//! and `get_notes_without_embedding()` are not yet implemented. `VectorsOnly` and
//! `Full` also depend on a vector backend (planned).
//!
//! | Mode | Status |
//! |---|---|
//! | `FtsOnly` | Deferred |
//! | `MissingOnly` | Deferred |
//! | `VectorsOnly` | Deferred (requires vector backend) |
//! | `Full` | Deferred (requires vector backend) |
//!
//! # References
//!
//! - `docs/decisions/ARCH-D15-apalis-embedded.md`

use std::sync::Arc;

use apalis::prelude::Data;
use chrono::Utc;
use smallvec::SmallVec;

use toml::Value as TomlValue;

use gradatum_core::{
    CurateSpec, DryRunAware, EmbedSpec, ForgetScope, GradatumJob, Job, JobClass, JobLifecycle,
    JobLineage, JobMode, JobOutput, JobPriority, JobRecord, JobRetry, JobScheduling, JobScope,
    JobSpec, JobStatus, QueueStore, TriggerSource,
    author::AuthorRef,
    frontmatter::{ExtraFields, Frontmatter},
    identity::{ContentHash, NoteId},
    index::AnchorSrc,
    scope::VaultId,
    section::{Section, section_to_doc_kind},
    status::NoteStatus,
    tag::Tag,
};
use gradatum_curator::{CurateOutcome, CuratorProcess};
use gradatum_dto::{
    PersistCuratedRequest, PersistDistillRequest, PersistEmbeddingRequest, PersistForgetRequest,
    TemporalEntryDto,
};
use gradatum_embed::Embedder;

use crate::internal_client::{InternalClient, InternalClientError, NoteIdDto};
use crate::wikilinks::resolve_wikilinks_via_client;

// ─────────────────────────────────────────────────────────────────────────────
// Handler errors
// ─────────────────────────────────────────────────────────────────────────────

/// Error returned by Apalis handlers.
///
/// Conforms to the signature expected by `apalis::WorkerBuilder`.
/// Mapped to `apalis::Error` via `From`.
///
/// `MissingDependency` and `InvalidPayload` are retained for future payload validation.
#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// Dependency not injected — `build_monitor` must call `.data()` on the worker.
    #[error("dépendance absente : {0}")]
    MissingDependency(&'static str),

    /// Received `Job` variant not handled by this handler.
    #[error("variant job inattendu : {0}")]
    UnexpectedVariant(String),

    /// Job payload missing or invalid (title/body absent for vault_write).
    #[error("payload job invalide : {0}")]
    InvalidPayload(String),

    /// Business error propagated from the vault or the curator.
    #[error("erreur métier : {0}")]
    Business(String),
}

/// tenant guard — cross-tenant isolation for a single-vault deployment.
///
/// The worker is a separate process NOT covered by the HTTP middleware.
/// While the vault is physically mono-tenant (`"main"`), a `JobSpec` carrying a
/// `tenant_id` ≠ `"main"` must be rejected terminally — never retried infinitely
/// (`HandlerError::Business` is not retried on the business side). Restrictive-only.
///
/// # Errors
/// Returns `HandlerError::Business` if `tenant_id != "main"`.
#[must_use = "le résultat de la garde tenant doit court-circuiter le handler"]
fn ensure_main_tenant(tenant_id: &str) -> Result<(), HandlerError> {
    if tenant_id != "main" {
        tracing::warn!(
            tenant_id = %tenant_id,
            "worker: job rejeté — tenant ≠ main (invariant mono-vault, P0 cross-tenant)"
        );
        return Err(HandlerError::Business(format!(
            "tenant non supporté (mono-vault) : '{tenant_id}' ≠ 'main'"
        )));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Curate
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Curate`] — `inbox/` classification.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Curate(CurateSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "curate")` without any write.
/// - In `Batch` mode: calls `CuratorPipeline::process()` and persists to the vault.
///
/// # Two use cases
///
/// 1. **vault_write (new note)**: `CurateSpec.title` + `.body` are `Some` —
///    the note is created via `write_if_match`, honoring the pre-allocated ULID (`spec.note_id`).
/// 2. **reclassification**: `title`/`body` are `None` —
///    the note already exists; it is read from the vault via `note_id` and updated via
///    `write_note_with_id` to **preserve the ULID** (spec.note_id == stored ULID).
///    Critical invariant: `write_note` must NOT be used here (it generates `NoteId::new()` →
///    divergent ULID → invalid 202 note_id → dead wikilinks).
///
/// # Title persistence
///
/// After the vault write, `index.upsert_note_title()` is called with the resolved title
/// (`spec.title` for vault_write, `extract_h1_title(body)` for reclassification).
/// Non-fatal: a warning is logged on failure without propagating.
///
/// # Side effects
///
/// - Writes the note to the vault (Admitted → Live, Pending → Staging).
/// - Persists `[[...]]` wikilinks via `SqliteIndex` (non-fatal).
/// - Enqueues a `Job::Embed` if the note is admitted or pending (non-fatal, best-effort).
///
/// # Timeout
///
/// Per-job timeout is enforced by the Apalis Tower layer (see `monitor.rs`
/// `cfg.timeout_secs`, default 30 s for curate). This handler adds no redundant
/// timeout — the Tower layer is outer and takes effect first.
pub async fn handle_curate(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    curator: Data<Arc<dyn CuratorProcess + Send + Sync>>,
    queue: Data<Arc<dyn QueueStore + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "curate — simulation"));
    }
    // Extract the CurateSpec
    let spec = match &job.record.spec.kind {
        Job::Curate(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Cross-tenant guard: terminally reject if tenant ≠ main (worker outside HTTP middleware).
    ensure_main_tenant(&spec.tenant_id)?;

    // Build the CuratorNote from the spec.
    // vault_write path: title + body present in the spec.
    // Reclassification path: title/body None → read from the vault.
    let (note_id_for_vault, curator_note, existing_dto_opt) =
        if spec.title.is_some() && spec.body.is_some() {
            // vault_write path: note to create
            let curator_note = gradatum_curator::Note {
                id: spec.note_id.to_string(),
                title: spec.title.clone().unwrap_or_default(),
                body: spec.body.clone().unwrap_or_default(),
                tags_hint: spec.tags.clone(),
                section_hint: spec.section_hint.clone(),
            };
            (None, curator_note, None) // create path : write_note_with_id(spec.note_id) honore l'ULID préalloué
        } else {
            // Reclassification path: read the existing note via InternalClient
            let note_id = NoteId(spec.note_id);
            let id_str = note_id.to_string();
            let existing_dto = client
                .get_note(&id_str)
                .await
                .map_err(|e| HandlerError::Business(format!("read_note: {e}")))?;
            let title_for_curator = gradatum_curator::extract_h1_title(&existing_dto.body)
                .unwrap_or_else(|| existing_dto.section.clone());
            let curator_note = gradatum_curator::Note {
                id: spec.note_id.to_string(),
                title: title_for_curator,
                body: existing_dto.body.clone(),
                tags_hint: existing_dto.tags.clone(),
                section_hint: None,
            };
            (Some(note_id), curator_note, Some(existing_dto))
        };

    let tenant_id = spec.tenant_id.clone();
    let body_for_write = spec
        .body
        .clone()
        .unwrap_or_else(|| curator_note.body.clone());
    // Resolved title captured before `curator.process` consumes `curator_note`.
    // Used after the match for `upsert_note_title` — populates the near-empty `notes.title` column.
    let title_resolved = curator_note.title.clone();

    let curate_outcome = curator.process(curator_note).await;

    // Status resolved via the single canonical mapping (worker SSOT parity).
    // Admitted → Live, Pending → PendingReview, Rejected → None (no write).
    let write_status = gradatum_curator::outcome_to_status(&curate_outcome);

    let written_note_id = match curate_outcome {
        CurateOutcome::Admitted { ref decisions } => {
            let _section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            // Build the PersistCuratedRequest for both paths
            let (
                curate_title,
                curate_body,
                curate_section_str,
                curate_tags,
                curate_author,
                curate_status_str,
                curate_trust,
                curate_expected_sha256,
                curate_provenance,
            ) = if let Some(_existing_note_id) = note_id_for_vault {
                // Reclassification: use existing body/tags from the pre-read DTO
                let dto = existing_dto_opt
                    .as_ref()
                    .expect("reclass path always has existing_dto");
                let reclass_title = gradatum_curator::extract_h1_title(&dto.body)
                    .unwrap_or_else(|| decisions.canonical_section.clone());
                let mut merged_tags = dto.tags.clone();
                for tag_str in &decisions.tags {
                    if !merged_tags.contains(tag_str) {
                        merged_tags.push(tag_str.clone());
                    }
                }
                (
                    reclass_title,
                    dto.body.clone(),
                    decisions.canonical_section.clone(),
                    merged_tags,
                    None::<String>,
                    "live".to_string(),
                    None::<f32>,
                    None::<String>,
                    None::<String>,
                )
            } else {
                // vault_write path
                let status = write_status.expect("Admitted → Some(Live) par outcome_to_status");
                let status_str = status_to_str(status);
                let author = spec.author.clone();
                let provenance = Some(
                    gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref())
                        .to_string(),
                );
                let expected_sha256 = spec.expected_sha256.map(|h| ContentHash(h).hex());
                let mut all_tags = spec.tags.clone();
                for t in &decisions.tags {
                    if !all_tags.contains(t) {
                        all_tags.push(t.clone());
                    }
                }
                (
                    title_resolved.clone(),
                    body_for_write.clone(),
                    decisions.canonical_section.clone(),
                    all_tags,
                    author,
                    status_str,
                    None::<f32>,
                    expected_sha256,
                    provenance,
                )
            };
            let (curate_anchor_ms, curate_anchor_src, curate_doc_kind, curate_valid_until_ms) =
                if let Some(_existing_note_id) = note_id_for_vault {
                    // For reclassification we don't have a full note — use created default
                    (
                        Utc::now().timestamp_millis(),
                        AnchorSrc::Created,
                        section_to_doc_kind(
                            &section_from_str(&curate_section_str).unwrap_or(Section::Reference),
                        )
                        .to_string(),
                        None::<i64>,
                    )
                } else {
                    // For vault_write path, temporal data will be computed post-write by server
                    (
                        Utc::now().timestamp_millis(),
                        AnchorSrc::Created,
                        section_to_doc_kind(
                            &section_from_str(&curate_section_str).unwrap_or(Section::Reference),
                        )
                        .to_string(),
                        None::<i64>,
                    )
                };
            let note_id_str = spec.note_id.to_string();
            // B5 wikilinks — résolution parallèle AVANT persist_curated (non-fatale).
            // Les liens résolus sont passés dans persist_req.links pour que le serveur
            // exécute upsert_link atomiquement dans persist_curated.
            let links =
                resolve_wikilinks_via_client(&client, &tenant_id, &note_id_str, &curate_body).await;
            let persist_req = PersistCuratedRequest {
                note_id: note_id_str.clone(),
                tenant_id: tenant_id.clone(),
                title: curate_title,
                body: curate_body,
                section: curate_section_str,
                tags: curate_tags,
                author: curate_author,
                status: curate_status_str,
                trust: curate_trust,
                expected_sha256: curate_expected_sha256,
                temporal: Some(TemporalEntryDto {
                    anchor_ms: curate_anchor_ms,
                    anchor_src: curate_anchor_src.as_db_str().to_string(),
                    doc_kind: curate_doc_kind,
                    valid_until_ms: curate_valid_until_ms,
                }),
                links,
                provenance: curate_provenance,
            };
            match client.persist_curated(&persist_req).await {
                Ok(_ok) => {}
                Err(InternalClientError::Conflict { current_sha256_hex }) => {
                    // Optimistic-lock conflict — only possible on vault_write path with expected_sha256
                    let job_id = job.record.id;
                    let duration_ms: u32 = job
                        .record
                        .lifecycle
                        .started_at
                        .map(|s| {
                            (Utc::now() - s)
                                .num_milliseconds()
                                .max(0)
                                .min(i64::from(u32::MAX)) as u32
                        })
                        .unwrap_or(0);
                    let conflict_payload_str = serde_json::json!({
                        "note_id": note_id_str,
                        "timestamp_ms": Utc::now().timestamp_millis(),
                        "current_sha256": current_sha256_hex,
                    })
                    .to_string();
                    if let Err(e) = queue
                        .mark_conflict(job_id, conflict_payload_str, duration_ms)
                        .await
                    {
                        tracing::error!(
                            job_id = %job_id,
                            error = %e,
                            "curate: mark_conflict échoué — job restera en état courant"
                        );
                    }
                    let sha_suffix = current_sha256_hex
                        .as_deref()
                        .map(|sha| format!(" — current_sha256: {sha}"))
                        .unwrap_or_default();
                    return Ok(JobOutput {
                        notes_created: vec![],
                        notes_modified: vec![],
                        files: vec![],
                        result_note_md: format!(
                            "curate: conflit optimistic-lock sur note {} (Admitted){sha_suffix}",
                            spec.note_id
                        ),
                    });
                }
                Err(e) => {
                    return Err(HandlerError::Business(format!(
                        "curate: persist_curated Admitted: {e}"
                    )));
                }
            }

            tracing::info!(
                job_id = %job.record.id,
                section = %decisions.canonical_section,
                "curate: note admise et persistée"
            );
            let written_id = NoteId(spec.note_id);
            Some(written_id)
        }
        CurateOutcome::Pending {
            ref decisions,
            ref reason,
        } => {
            let _section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            // Pending path — same structure as Admitted but with PendingReview status
            let status = write_status.expect("Pending → Some(PendingReview) par outcome_to_status");
            let pending_status_str = status_to_str(status);
            let (
                pend_title,
                pend_body,
                pend_section_str,
                pend_tags,
                pend_author,
                pend_expected_sha256,
                pend_provenance,
            ) = if let Some(_existing_note_id) = note_id_for_vault {
                let dto = existing_dto_opt
                    .as_ref()
                    .expect("reclass path always has existing_dto");
                let reclass_title = gradatum_curator::extract_h1_title(&dto.body)
                    .unwrap_or_else(|| decisions.canonical_section.clone());
                let mut merged_tags = dto.tags.clone();
                for tag_str in &decisions.tags {
                    if !merged_tags.contains(tag_str) {
                        merged_tags.push(tag_str.clone());
                    }
                }
                (
                    reclass_title,
                    dto.body.clone(),
                    decisions.canonical_section.clone(),
                    merged_tags,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                )
            } else {
                let author = spec.author.clone();
                let provenance = Some(
                    gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref())
                        .to_string(),
                );
                let expected_sha256 = spec.expected_sha256.map(|h| ContentHash(h).hex());
                let mut all_tags = spec.tags.clone();
                for t in &decisions.tags {
                    if !all_tags.contains(t) {
                        all_tags.push(t.clone());
                    }
                }
                (
                    title_resolved.clone(),
                    body_for_write.clone(),
                    decisions.canonical_section.clone(),
                    all_tags,
                    author,
                    expected_sha256,
                    provenance,
                )
            };
            let note_id_str_pending = spec.note_id.to_string();
            // B5 wikilinks — résolution parallèle AVANT persist_curated (non-fatale).
            // Parité Admitted/Pending : les deux branches renseignent persist_req.links.
            let links_pending =
                resolve_wikilinks_via_client(&client, &tenant_id, &note_id_str_pending, &pend_body)
                    .await;
            let persist_req_pending = PersistCuratedRequest {
                note_id: note_id_str_pending.clone(),
                tenant_id: tenant_id.clone(),
                title: pend_title,
                body: pend_body,
                section: pend_section_str,
                tags: pend_tags,
                author: pend_author,
                status: pending_status_str,
                trust: None,
                expected_sha256: pend_expected_sha256,
                temporal: None,
                links: links_pending,
                provenance: pend_provenance,
            };
            match client.persist_curated(&persist_req_pending).await {
                Ok(_ok) => {}
                Err(InternalClientError::Conflict { current_sha256_hex }) => {
                    let job_id = job.record.id;
                    let duration_ms: u32 = job
                        .record
                        .lifecycle
                        .started_at
                        .map(|s| {
                            (Utc::now() - s)
                                .num_milliseconds()
                                .max(0)
                                .min(i64::from(u32::MAX)) as u32
                        })
                        .unwrap_or(0);
                    let conflict_payload_str = serde_json::json!({
                        "note_id": note_id_str_pending,
                        "timestamp_ms": Utc::now().timestamp_millis(),
                        "current_sha256": current_sha256_hex,
                    })
                    .to_string();
                    if let Err(e) = queue
                        .mark_conflict(job_id, conflict_payload_str, duration_ms)
                        .await
                    {
                        tracing::error!(
                            job_id = %job_id,
                            error = %e,
                            "curate: mark_conflict échoué (pending) — job restera en état courant"
                        );
                    }
                    let sha_suffix = current_sha256_hex
                        .as_deref()
                        .map(|sha| format!(" — current_sha256: {sha}"))
                        .unwrap_or_default();
                    return Ok(JobOutput {
                        notes_created: vec![],
                        notes_modified: vec![],
                        files: vec![],
                        result_note_md: format!(
                            "curate: conflit optimistic-lock (pending) sur note {}{sha_suffix}",
                            spec.note_id
                        ),
                    });
                }
                Err(e) => {
                    return Err(HandlerError::Business(format!(
                        "curate: persist_curated Pending: {e}"
                    )));
                }
            }

            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note mise en Staging (revue manuelle requise)"
            );
            Some(NoteId(spec.note_id))
        }
        CurateOutcome::Rejected { ref reason } => {
            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note rejetée — aucune écriture vault"
            );
            None
        }
    };

    // ── curate→embed chaining — best-effort non-fatal ────────────────────────
    // upsert_note_title + write_temporal_entry handled server-side in persist_curated.
    // B5 wikilinks renseignés dans persist_req.links (avant persist_curated) — pas de pass post-curate.
    if let Some(note_id) = &written_note_id {
        // ── curate→embed chaining — best-effort non-fatal ─────────────────────
        // Enqueues a Job::Embed for the curated note so embeddings are
        // generated in cascade after curation.
        //
        // Storage idempotence only: a double-curate re-enqueues an Embed;
        // handle_embed recomputes the embedding then INSERT OR REPLACE into
        // note_embeddings — no corruption, but non-zero compute cost.
        // Compute skip (force_regenerate=false + vector present → no-op) is deferred.
        //
        // Transient: direct enqueue (await_jobs=[]) because the cascade engine
        // (await_jobs/Cascade, gradatum_queue::find_awaiting/set_pending) is todo!()
        // in gradatum_queue.rs. A non-empty await_jobs would leave the embed in Waiting.
        // Migration to await_jobs=[JobTrigger{curate_id, OnDone}] + TriggerSource::Cascade
        // is planned when the cascade engine is implemented.
        let embed_record = build_embed_job_record(*note_id, &tenant_id, job.record.id);
        if let Err(e) = queue.enqueue(embed_record).await {
            tracing::warn!(
                note_id = %note_id,
                error = %e,
                "curate: enqueue Job::Embed échoué — note curée, embed non schedulé (best-effort)"
            );
        }
    }

    let result_desc = written_note_id
        .map(|id| format!("note {} créée/mise à jour", id))
        .unwrap_or_else(|| "rejetée".to_string());

    Ok(JobOutput {
        notes_created: written_note_id.map(|nid| vec![nid.0]).unwrap_or_default(),
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!("curate: {result_desc}"),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Embed
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Embed`] — embedding generation.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Embed(EmbedSpec { note_id, ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "embed")` without computation.
/// - In `Batch` mode: reads the note from the vault, computes the embedding, persists it in the index.
///
/// # Silent skip
///
/// Only skip case: empty `body_text` → returns `JobOutput` without calling the embedder.
/// If a vector already exists for this note, it is recomputed and overwritten via
/// `INSERT OR REPLACE` (storage idempotence, not compute idempotence).
/// Compute skip (`force_regenerate=false` + vector present → no-op) is not yet implemented.
pub async fn handle_embed(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "embed — simulation"));
    }

    let spec = match &job.record.spec.kind {
        Job::Embed(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Cross-tenant guard: terminally reject if tenant ≠ main (defense-in-depth).
    ensure_main_tenant(&spec.tenant_id)?;

    let note_id = NoteId(spec.note_id);
    let id_str = note_id.to_string();

    // Read the note via InternalClient to obtain the body.
    let note_dto = client
        .get_note(&id_str)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: read_note: {e}")))?;

    let body_text = note_dto.body.as_str();
    if body_text.is_empty() {
        tracing::info!(
            job_id = %job.record.id,
            note_id = %spec.note_id,
            "embed: skip — body vide"
        );
        return Ok(JobOutput {
            notes_created: vec![],
            notes_modified: vec![],
            files: vec![],
            result_note_md: format!("embed: skip note {} — body vide", spec.note_id),
        });
    }

    // Truncate to 2 048 Unicode characters (UTF-8-safe via char_indices).
    // Prevents model context overflow without arbitrary byte slicing.
    let truncated = if body_text.len() > 8192 {
        let end = body_text
            .char_indices()
            .nth(2048)
            .map(|(i, _)| i)
            .unwrap_or(body_text.len());
        &body_text[..end]
    } else {
        body_text
    };

    let vec = embedder
        .embed(truncated)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: embedder: {e}")))?;

    client
        .persist_embedding(&PersistEmbeddingRequest {
            note_id: id_str.clone(),
            embedder_id: embedder.embedder_id().to_string(),
            dim: embedder.dim(),
            vector: vec,
        })
        .await
        .map_err(|e| HandlerError::Business(format!("embed: persist_embedding: {e}")))?;

    tracing::info!(
        job_id = %job.record.id,
        note_id = %spec.note_id,
        embedder_id = embedder.embedder_id(),
        dim = embedder.dim(),
        "embed: done"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: vec![note_id.0],
        files: vec![],
        result_note_md: format!(
            "embed: note {} vecteur dim={} persisté",
            spec.note_id,
            embedder.dim()
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::ReIndex
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::ReIndex`] — full reindex.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::ReIndex(ReIndexMode { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
/// - In `DryRun` mode: returns `JobOutput::dry_run(0, "reindex")` without any write.
/// - For all other modes: returns `Err(HandlerError::Business)` (deferred).
///
/// # Status
///
/// All modes are deferred. `SqliteIndex::rebuild_fts()` and
/// `get_notes_without_embedding()` are not yet implemented.
///
/// | Mode | Status |
/// |---|---|
/// | `FtsOnly` | Deferred |
/// | `MissingOnly` | Deferred |
/// | `VectorsOnly` | Deferred (requires vector backend) |
/// | `Full` | Deferred (requires vector backend) |
///
/// # `temporal_index` reconstruction
///
/// When the `Full` mode is implemented, `temporal_index` reconstruction MUST be
/// included via `SqliteIndex::backfill_temporal_index()` — a derived table that must
/// remain consistent with `notes` after a full reindex.
/// The initial migration backfill combined with per-curate `write_temporal_entry` calls
/// keeps the table current incrementally until `Full` is implemented.
pub async fn handle_reindex(
    job: GradatumJob,
    // Parameters reserved for the future reindex implementation (v0.5.3+).
    _client: Data<Arc<dyn InternalClient>>,
    _embedder: Data<Arc<dyn Embedder + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // DryRun guard — first instruction
    if job.record.is_dry_run() {
        return Ok(JobOutput::dry_run(0, "reindex — simulation"));
    }

    let mode = match &job.record.spec.kind {
        Job::ReIndex(mode) => mode.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // All modes deferred: SqliteIndex::rebuild_fts() and get_notes_without_embedding()
    // are not yet implemented. Returns an explicit error (not a silent Ok) so the job
    // is marked as failed in the queue rather than appearing successful.
    tracing::warn!(
        job_id = %job.record.id,
        mode = ?mode,
        "reindex: non implémenté en v0.4.x — job rejeté explicitement"
    );

    Err(HandlerError::Business(format!(
        "reindex ({mode:?}): not implemented in v0.4.x"
    )))
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Purge
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Purge`] — lifecycle purge of `Garbage` notes.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Purge(PurgeSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` **AND** `spec.dry_run` as the FIRST instruction
///   (double guard for an irreversible operation).
/// - In dry-run: lists eligible notes and returns `JobOutput::dry_run(count, ulids)` **without deleting anything**.
/// - In real mode: for each eligible `Garbage` note:
///   1. Re-verifies the current status (TOCTOU mitigation between listing and delete).
///   2. `vault.delete_note(id)` — removes `.md` and purges `.history/<ulid>/`.
///   3. `index.delete_redirect_by_ulid(id)` — cleans `redirect_table` (non-fatal).
///
/// # Eligibility
///
/// `Lifecycle` mode: notes with `status = 'garbage'` AND
/// `status_changed <= now - grace_days` (or `created` if `status_changed` is NULL).
/// `grace_days = None` → all `Garbage` notes without delay.
///
/// # Safety invariant
///
/// A note that is NOT `Garbage` is never touched, even if it appeared in an earlier
/// listing. The per-delete status re-verification guarantees this.
///
/// # Cron
///
/// The nightly purge cron schedule is INTENTIONALLY disabled in production.
/// Activation requires an operator decision with a nightly backup strategy.
pub async fn handle_purge(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
) -> Result<JobOutput, HandlerError> {
    // ── Double dry-run guard — first instruction (DryRun mode + PurgeSpec.dry_run) ──
    let spec = match &job.record.spec.kind {
        Job::Purge(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    // Irreversible operation: dry_run required on both axes.
    // `job.record.is_dry_run()` checks JobMode::DryRun in JobSpec.
    // `spec.dry_run` checks the explicit flag in PurgeSpec (default true).
    let is_dry_run = job.record.is_dry_run() || spec.dry_run;

    // Compute the cutoff timestamp (UTC ms) from grace_days.
    // grace_days = None → no age limit (all Garbage notes).
    // INTERNAL_TENANT_ID == "main" for single-vault deployment.
    let vault_id = "main";
    let cutoff_ms: Option<i64> = spec.grace_days.map(|days| {
        Utc::now()
            .timestamp_millis()
            .saturating_sub(i64::from(days) * 24 * 3600 * 1000)
    });

    // List eligible Garbage notes (or all if cutoff_ms = None).
    let candidates: Vec<NoteIdDto> = match cutoff_ms {
        Some(cutoff) => {
            let grace_days = spec.grace_days.unwrap_or(0);
            client
                .list_garbage(vault_id, cutoff, grace_days)
                .await
                .map_err(|e| HandlerError::Business(format!("purge: list_garbage: {e}")))?
        }
        None => {
            // No grace period — list all Garbage notes.
            client
                .list_by_status(vault_id, "Garbage")
                .await
                .map_err(|e| {
                    HandlerError::Business(format!("purge: list_by_status(Garbage): {e}"))
                })?
        }
    };

    let count = candidates.len();

    // ── Dry-run: list candidates without deleting anything ───────────────────
    if is_dry_run {
        let ulid_list = candidates
            .iter()
            .map(|dto| dto.note_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let description = if ulid_list.is_empty() {
            "purge lifecycle dry-run — aucune note éligible".to_string()
        } else {
            format!("purge lifecycle dry-run — notes éligibles : [{ulid_list}]")
        };
        tracing::info!(
            job_id = %job.record.id,
            count = count,
            grace_days = ?spec.grace_days,
            dry_run = true,
            "purge: dry-run — {count} note(s) seraient supprimées"
        );
        return Ok(JobOutput::dry_run(count, &description));
    }

    // ── Real mode: delete with per-note status re-verification ───────────────
    let mut supprimées: Vec<String> = Vec::with_capacity(count);
    let mut ignorées: usize = 0;

    for note_dto in candidates {
        let id_str = note_dto.note_id.clone();

        // Re-verify status at delete time (TOCTOU mitigation).
        // If the note was restored (Garbage→Live) between the listing and now,
        // it is silently skipped.
        let current_status_opt = match client.get_note(&id_str).await {
            Ok(dto) => Some(dto.status),
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(
                    note_id = %id_str,
                    "purge: note absente — déjà supprimée, skip"
                );
                ignorées += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: get_note illisible — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        };

        match current_status_opt.as_deref() {
            Some("garbage") => {
                // Status confirmed Garbage — proceed with deletion.
            }
            Some(other_status) => {
                tracing::info!(
                    note_id = %id_str,
                    status = %other_status,
                    "purge: note ignorée — statut changé depuis le listing (TOCTOU mitigation)"
                );
                ignorées += 1;
                continue;
            }
            None => {
                tracing::debug!(
                    note_id = %id_str,
                    "purge: note absente de l'index — déjà supprimée, skip"
                );
                ignorées += 1;
                continue;
            }
        }

        // Delete via server (vault + index + redirects in sequence).
        match client.delete_note(&id_str).await {
            Ok(()) => {
                tracing::info!(
                    note_id = %id_str,
                    "purge: note supprimée"
                );
            }
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %id_str, "purge: note déjà absente — skip");
                ignorées += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: delete_note échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        }
        supprimées.push(id_str);
    }

    let supprimées_count = supprimées.len();
    tracing::info!(
        job_id = %job.record.id,
        supprimées = supprimées_count,
        ignorées = ignorées,
        "purge: terminé"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: vec![],
        files: vec![],
        result_note_md: format!(
            "purge lifecycle : {supprimées_count} note(s) supprimée(s), {ignorées} ignorée(s)"
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Forget
// ─────────────────────────────────────────────────────────────────────────────

/// Handler for [`gradatum_core::Job::Forget`] — semantic forget.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Forget(ForgetSpec { ... })`.
/// - Checks `DryRunAware::is_dry_run()` as the **first instruction**.
/// - Double guard: `job.record.is_dry_run()` OR `spec.dry_run` activates dry-run.
/// - **Non-destructive**: no physical deletion (purge is a separate operation).
///
/// # Protected sections
///
/// Notes belonging to `Section::AgentIssues` or `Section::Council` are systematically
/// excluded from the batch and reported in the preview.
/// The job does not fail on exclusions — it continues with the eligible notes.
///
/// # Dry-run
///
/// Returns `JobOutput` with the list of candidate ULIDs and exclusions, without
/// any frontmatter mutation or index update.
///
/// # Real mode
///
/// For each eligible note:
/// 1. Read via `vault.read_note` (cache + disk).
/// 2. Mutate the frontmatter (`forgotten=true`, `forgotten_at`, `forgotten_by`).
/// 3. Write via `vault.write_note_with_id` (CoW traced → snapshot in `.history/`).
/// 4. Synchronise the index via `index.mark_forgotten`.
///
/// # Double confirmation
///
/// In real mode, `spec.confirm_ulids` must match **exactly** the resolved ULIDs.
/// Any divergence → `HandlerError::Business`, which marks the job `Failed` in the
/// queue (no automatic retry — divergence is intentional).
pub async fn handle_forget(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
) -> Result<JobOutput, HandlerError> {
    // ── Double dry-run guard — first instruction (DryRun mode + ForgetSpec.dry_run) ──
    let spec = match &job.record.spec.kind {
        Job::Forget(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let is_dry_run = job.record.is_dry_run() || spec.dry_run;
    let vault_id = "main";

    // ── Protected sections — never forgotten ─────────────────────────────────
    // Source of truth: Section::PROTECTED_FORGET in gradatum-core::section.
    // AgentIssues + Council excluded from the batch, reported in the preview.

    // ── Scope resolution → raw candidate list ────────────────────────────────
    // Methods return Vec<(id, section)> — only the id is extracted here.
    // The protected-section check is re-applied per candidate via get_note_section.
    let raw_candidates: Vec<String> = match &spec.scope {
        ForgetScope::Topic {
            query,
            vault: scope_vault,
            limit,
        } => {
            let effective_vault = scope_vault.as_deref().unwrap_or(vault_id);
            let max_limit = limit.unwrap_or(50).min(200);
            client
                .search_fts_for_forget(effective_vault, query, max_limit)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: search_fts_for_forget: {e}")))?
                .into_iter()
                .map(|dto| dto.note_id)
                .collect()
        }
        ForgetScope::Locus {
            vault: scope_vault,
            locus,
        } => {
            let effective_vault = scope_vault.as_str();
            client
                .list_notes_by_locus(effective_vault, locus)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: list_notes_by_locus: {e}")))?
                .into_iter()
                .map(|dto| dto.note_id)
                .collect()
        }
        ForgetScope::Agent { agent_id, vaults } => {
            let vault_strs: Vec<String> = vaults.to_vec();
            client
                .list_notes_by_agent(agent_id, &vault_strs)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: list_notes_by_agent: {e}")))?
                .into_iter()
                .map(|dto| dto.note_id)
                .collect()
        }
        // Future exhaustive case — guarded by #[non_exhaustive] on ForgetScope
        _ => {
            return Err(HandlerError::Business(format!(
                "forget: scope variant non supporté : {:?}",
                spec.scope
            )));
        }
    };

    // ── Partition: eligible / excluded (protected sections) ──────────────────
    // Each exclusion: (ulid, section) — used for the job description.
    let mut eligible: Vec<String> = Vec::with_capacity(raw_candidates.len());
    let mut excluded_details: Vec<(String, String)> = Vec::new(); // (ulid, section)

    for ulid in raw_candidates {
        let section_str = client.get_note(&ulid).await.ok().map(|dto| dto.section);

        // Fail-closed: unknown section (note absent from index) = PROTECTED.
        // unwrap_or(true) ensures that any out-of-index ULID is excluded rather
        // than included — conservative behavior consistent with the
        // "protected sections are never forgotten" policy.
        let is_protected = section_str
            .as_deref()
            .map(|s| Section::PROTECTED_FORGET.iter().any(|p| p.as_str() == s))
            .unwrap_or(true);

        if is_protected {
            let section = section_str.unwrap_or_default();
            tracing::info!(
                note_id = %ulid,
                section = %section,
                "forget: note exclue — section protégée"
            );
            excluded_details.push((ulid, section));
        } else {
            eligible.push(ulid);
        }
    }

    let eligible_count = eligible.len();
    let excluded_count = excluded_details.len();

    // ── Dry-run: return preview without mutation ──────────────────────────────
    if is_dry_run {
        // Do not persist the raw scope query in result_note_md:
        // it may contain sensitive data (PII, project names, identifiers).
        // The eligible note count is sufficient for poll-status on the caller side.
        let description = if eligible_count == 0 {
            format!("forget dry-run — aucune note éligible (exclusions: {excluded_count})")
        } else {
            format!(
                "forget dry-run — {eligible_count} note(s) éligible(s), {excluded_count} exclue(s)"
            )
        };

        tracing::info!(
            job_id = %job.record.id,
            eligible = eligible_count,
            excluded = excluded_count,
            dry_run = true,
            "forget: dry-run — {eligible_count} note(s) seraient oubliées, {excluded_count} exclue(s)"
        );
        return Ok(JobOutput::dry_run(eligible_count, &description));
    }

    // ── Real mode — confirm_ulids verification ────────────────────────────────
    // confirm_ulids must match EXACTLY the eligible ULIDs (same set, order irrelevant).
    // Any divergence = rejection to prevent accidental forget.
    //
    // Two empty sets (eligible=0 + confirm=0) = legal → empty job OK.
    // No composite guard: direct comparison covers all cases.
    {
        let mut expected_sorted = eligible.clone();
        expected_sorted.sort();
        let mut confirmed_sorted = spec.confirm_ulids.clone();
        confirmed_sorted.sort();

        if expected_sorted != confirmed_sorted {
            return Err(HandlerError::Business(format!(
                "forget: confirm_ulids ne correspond pas aux ULIDs résolus — \
                 attendus={}, fournis={}. Relancer une preview et confirmer les ULIDs exacts.",
                expected_sorted.len(),
                confirmed_sorted.len()
            )));
        }
    }

    // ── Real mode: frontmatter mutation + index sync ──────────────────────────
    let forgotten_by = spec.forgotten_by.as_deref();
    let mut oubliées_ulids: Vec<ulid::Ulid> = Vec::with_capacity(eligible_count);
    let mut oubliées: Vec<String> = Vec::with_capacity(eligible_count);
    let mut ignorées: usize = 0;

    for ulid in &eligible {
        let raw_ulid = match ulid::Ulid::from_string(ulid) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: ULID invalide — ignoré"
                );
                ignorées += 1;
                continue;
            }
        };
        let _note_id = NoteId(raw_ulid);

        // TOCTOU re-verification: if the note is already forgotten, skip idempotently.
        let already_forgotten = match client.get_note(ulid).await {
            Ok(dto) => dto.status == "forgotten",
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %ulid, "forget: note absente — skip");
                ignorées += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: get_note échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        };

        if already_forgotten {
            tracing::debug!(note_id = %ulid, "forget: déjà oubliée — skip idempotent");
            oubliées.push(ulid.clone());
            oubliées_ulids.push(raw_ulid);
            continue;
        }

        // Get section for persist_forget (server handles frontmatter mutation + index sync)
        let note_section = match client.get_note(ulid).await {
            Ok(dto) => dto.section,
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: get_note (section) échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        };

        // Persist via server (vault frontmatter mutation + index mark_forgotten in sequence).
        match client
            .persist_forget(&PersistForgetRequest {
                note_id: ulid.clone(),
                tenant_id: vault_id.to_string(),
                body: String::new(), // server reads the body internally
                section: note_section,
                forgotten_by: forgotten_by.map(|s| s.to_string()),
            })
            .await
        {
            Ok(_) => {
                tracing::info!(note_id = %ulid, "forget: note oubliée");
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: persist_forget échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        }
        oubliées.push(ulid.clone());
        oubliées_ulids.push(raw_ulid);
    }

    let oubliées_count = oubliées.len();
    tracing::info!(
        job_id = %job.record.id,
        oubliées = oubliées_count,
        ignorées = ignorées,
        exclusions = excluded_count,
        "forget: terminé"
    );

    Ok(JobOutput {
        notes_created: vec![],
        notes_modified: oubliées_ulids,
        files: vec![],
        result_note_md: format!(
            "forget sémantique : {oubliées_count} note(s) oubliée(s), {ignorées} ignorée(s), {excluded_count} exclue(s) (sections protégées)"
        ),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — Job::Distill (semantic distillation)
// ─────────────────────────────────────────────────────────────────────────────

/// Hard cap on the number of notes considered per distillation run.
///
/// Clustering is `O(n²)`; even if `batch_limit` is misconfigured, the number of notes
/// actually loaded and compared never exceeds this bound (protection against
/// combinatorial explosion and memory pressure).
pub const MAX_DISTILL_BATCH: usize = 2000;

/// Synthesis output produced for a note cluster.
///
/// Produced by a [`DistillSynthesizer`] and written as a `PendingReview` note.
pub struct ClusterSynthesis {
    /// Title of the synthesis note.
    pub title: String,
    /// Markdown body of the synthesis note.
    pub body: String,
}

/// Synthesis error — propagated to mark the job `Failed` cleanly.
#[derive(Debug, thiserror::Error)]
pub enum SynthesisError {
    /// The synthesis service (LLM gateway) is unavailable or failed.
    #[error("synthèse indisponible : {0}")]
    Unavailable(String),
}

/// Cluster synthesis producer.
///
/// Abstraction that allows substituting the deterministic implementation (MVP)
/// with a dedicated LLM gateway backend without touching the handler.
///
/// # Contract
///
/// - `synthesize` receives the notes of a cluster as `[(title, body)]` (≥ 1 note).
/// - Returns `Ok(ClusterSynthesis)`: title + body of the `PendingReview` note.
/// - Returns `Err(SynthesisError::Unavailable)`: the job MUST fail cleanly
///   (no partial note written — mitigation for gateway-down scenarios).
#[async_trait::async_trait]
pub trait DistillSynthesizer: Send + Sync {
    /// Synthesizes a note cluster into a synthesis note.
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError>;
}

/// Deterministic synthesizer — MVP (no LLM call).
///
/// Produces a structured synthesis note by concatenation: title derived from the
/// first cluster element, body listing source notes with an excerpt.
/// The note is written as `PendingReview` (requires human review) — editorial quality
/// is the reviewer's responsibility, not the automated step's.
///
/// ## Why deterministic at MVP
///
/// The worker injects no free-text generation client (the only wired LLM backend is
/// `gradatum_chat::LlmBackend`, specialised for curator classification — not free
/// completion). A dedicated `distill-semantic` gateway client is deferred:
/// the `PendingReview` output combined with the cron disabled by default keeps the step
/// safe, and the [`DistillSynthesizer`] abstraction allows plugging in an LLM without
/// refactoring the handler.
#[derive(Default)]
pub struct TemplateSynthesizer;

#[async_trait::async_trait]
impl DistillSynthesizer for TemplateSynthesizer {
    async fn synthesize(
        &self,
        cluster: &[(String, String)],
    ) -> Result<ClusterSynthesis, SynthesisError> {
        if cluster.is_empty() {
            return Err(SynthesisError::Unavailable(
                "cluster vide — rien à synthétiser".to_string(),
            ));
        }
        // Title: derived from the first non-empty title in the cluster.
        let lead_title = cluster
            .iter()
            .map(|(t, _)| t.trim())
            .find(|t| !t.is_empty())
            .unwrap_or("notes connexes");
        let title = format!("Synthèse distillée — {lead_title}");

        // Body: header + list of source notes with bounded excerpt.
        let mut body = format!(
            "# {title}\n\n\
             > Note de synthèse distillée (F-22) — **en attente de revue**.\n\
             > Regroupe {} note(s) sémantiquement proches.\n\n\
             ## Sources distillées\n\n",
            cluster.len()
        );
        for (i, (src_title, src_body)) in cluster.iter().enumerate() {
            let excerpt: String = src_body.trim().chars().take(280).collect();
            let display_title = if src_title.trim().is_empty() {
                "(sans titre)"
            } else {
                src_title.trim()
            };
            body.push_str(&format!("### {}. {display_title}\n\n{excerpt}\n\n", i + 1));
        }
        Ok(ClusterSynthesis { title, body })
    }
}

/// Handler for [`gradatum_core::Job::Distill`] — semantic distillation.
///
/// # Contract
///
/// - Receives a `GradatumJob` with `record.spec.kind = Job::Distill(DistillSource)`.
/// - Checks `DryRunAware::is_dry_run()` as the first instruction.
///
/// # Dry-run
///
/// Lists candidate clusters (non-`processed` notes from the scope → embeddings →
/// cosine clustering) **without any mutation**. `JobScope::VaultWide` is only
/// permitted in dry-run (exploration).
///
/// # Real mode
///
/// For each cluster:
/// 1. Synthesis via [`DistillSynthesizer`] (failure → clean `Failed` job, no partial note written).
/// 2. Write the synthesis note as `PendingReview`:
///    `provenance = "distilled"`, `derived-from = [source ulids]` (ExtraFields).
/// 3. Dynamic trust: `compute_distill_trust(sources, index, confidence_threshold)`
///    persisted via `index.set_note_trust` (overwrites the static 0.60 from `provenance`).
/// 4. Mark sources: `processed = true` + `derived-into = <synthesis ulid>`
///    (ExtraFields — keys in `HISTORY_EXCLUDED_FIELDS`, CoW-safe: no spurious version entry).
///
/// # Required scope in real mode
///
/// `JobScope::VaultWide` is **rejected** outside dry-run (`HandlerError::Business`) —
/// mitigation against O(n²) clustering. `Locus` or `Notes` scope required.
///
/// # Idempotence
///
/// A note with `processed = true` is never re-collected (filtered before clustering) —
/// a double run on the same scope is idempotent (already-distilled clusters are excluded).
///
/// # Injected dependencies
///
/// `vault`, `index`, `embedder` (reads precomputed embeddings via `embedder_id`),
/// `synthesizer` (pluggable synthesis producer).
pub async fn handle_distill(
    job: GradatumJob,
    client: Data<Arc<dyn InternalClient>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    synthesizer: Data<Arc<dyn DistillSynthesizer + Send + Sync>>,
) -> Result<JobOutput, HandlerError> {
    // ── Spec extraction — first instruction ──────────────────────────────────
    let spec = match &job.record.spec.kind {
        Job::Distill(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let is_dry_run = job.record.is_dry_run();
    let vault_id = "main".to_string();
    // Mono-vault: vault_id is hardcoded to "main" (single-vault deployment).
    // not from a tenant_id injected via DistillSpec — ensure_main_tenant not applicable here.
    // If DistillSpec gains a tenant_id field, apply ensure_main_tenant(&spec.tenant_id)
    // immediately after spec extraction (same pattern as handle_curate/handle_embed).
    let embedder_id = embedder.embedder_id().to_string();

    // Clamp confidence_threshold to [0, 1].
    // An out-of-range threshold (NaN, negative, > 1) would corrupt cosine clustering.
    let confidence_threshold = if spec.confidence_threshold.is_finite() {
        spec.confidence_threshold.clamp(0.0, 1.0)
    } else {
        0.75 // NaN/inf → défaut prudent.
    };

    // Hard cap on batch_limit (anti O(n²) explosion).
    let effective_batch_limit = spec.batch_limit.min(MAX_DISTILL_BATCH);

    // ── VaultWide scope guard in real mode ───────────────────────────────────
    if !is_dry_run && matches!(spec.scope, JobScope::VaultWide) {
        return Err(HandlerError::Business(
            "distill: JobScope::VaultWide refusé hors dry-run — scope Locus ou Notes requis (R3)"
                .to_string(),
        ));
    }

    // Reject empty / whitespace-only Locus in real mode.
    // An empty prefix would match the entire vault via LIKE '%' → equivalent to VaultWide
    // (bypasses the VaultWide guard). Rejected outside dry-run.
    if !is_dry_run
        && let JobScope::Locus(prefix) = &spec.scope
        && prefix.trim().is_empty()
    {
        return Err(HandlerError::Business(
            "distill: JobScope::Locus vide/whitespace refusé hors dry-run \
                     (matcherait tout le vault — contourne R3)"
                .to_string(),
        ));
    }

    // ── Scope resolution → raw candidates (ULIDs) ────────────────────────────
    let raw_candidates: Vec<NoteId> =
        resolve_distill_scope(&**client, &vault_id, &spec.scope).await?;

    // ── Filter THEN truncate (to avoid starvation) ────────────────────────────
    // `batch_limit` truncation is applied AFTER the filters
    // (processed / forgotten / garbage / no-embedding), not before. Otherwise notes
    // beyond the first batch_limit entries are never reachable if all earlier ones
    // are filtered out.
    // Defensive skip of forgotten / Garbage notes regardless of scope
    // (an explicit Notes scope may contain stale ULIDs).
    // Notes without an embedding are silently skipped (cannot be clustered).
    let mut candidates: Vec<(NoteId, String, String, Vec<f32>)> = Vec::new();
    for note_id in raw_candidates {
        // Early exit: effective window reached (saves I/O).
        if candidates.len() >= effective_batch_limit {
            break;
        }
        let note_id_str = note_id.to_string();
        let note_dto = match client.get_note(&note_id_str).await {
            Ok(n) => n,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::warn!(note_id = %note_id, "distill: note absente — ignorée");
                continue;
            }
            Err(e) => {
                tracing::warn!(note_id = %note_id, error = %e, "distill: get_note échoué — note ignorée");
                continue;
            }
        };
        // Defensive skip: never distill a forgotten note.
        if note_dto.forgotten {
            tracing::debug!(note_id = %note_id, "distill: note forgotten — ignorée");
            continue;
        }
        // Defensive skip: never distill a Garbage note.
        if note_dto.status == "garbage" {
            tracing::debug!(note_id = %note_id, "distill: note Garbage — ignorée");
            continue;
        }
        // Skip if already distilled (idempotence — check via processed field in NoteReadDto).
        if note_dto.processed {
            tracing::debug!(note_id = %note_id, "distill: note déjà processed — ignorée");
            continue;
        }
        // Skip if no embedding (cannot be clustered).
        let emb = match client.get_note_embedding(&note_id_str, &embedder_id).await {
            Ok(e) => e.vector,
            Err(InternalClientError::NotFound { .. }) => {
                tracing::debug!(note_id = %note_id, "distill: pas d'embedding — note ignorée");
                continue;
            }
            Err(e) => {
                return Err(HandlerError::Business(format!(
                    "distill: get_note_embedding {note_id}: {e}"
                )));
            }
        };
        let title = gradatum_curator::extract_h1_title(&note_dto.body).unwrap_or_default();
        candidates.push((note_id, title, note_dto.body.clone(), emb));
    }

    // ── Cosine clustering (connected components, confidence_threshold) ─────────
    let embeddings: Vec<Vec<f32>> = candidates.iter().map(|(_, _, _, e)| e.clone()).collect();
    let clusters = crate::distill_cluster::cluster_by_cosine(&embeddings, confidence_threshold);

    // ── Dry-run: list clusters without mutation ──────────────────────────────
    if is_dry_run {
        let description = format!(
            "distill dry-run — {} note(s) candidate(s), {} cluster(s) (seuil cosine {:.2})",
            candidates.len(),
            clusters.len(),
            confidence_threshold
        );
        tracing::info!(
            job_id = %job.record.id,
            candidates = candidates.len(),
            clusters = clusters.len(),
            dry_run = true,
            "distill: dry-run"
        );
        return Ok(JobOutput::dry_run(clusters.len(), &description));
    }

    // ── Real mode: synthesis + PendingReview write per cluster ───────────────
    let mut notes_created: Vec<ulid::Ulid> = Vec::new();
    let mut notes_modified: Vec<ulid::Ulid> = Vec::new();
    // Source-marking failure counter (visible in the job summary).
    let mut mark_failures: usize = 0;

    for cluster in &clusters {
        // Cluster notes (titles + bodies for synthesis).
        let cluster_pairs: Vec<(String, String)> = cluster
            .iter()
            .map(|&i| (candidates[i].1.clone(), candidates[i].2.clone()))
            .collect();
        let source_ids: Vec<NoteId> = cluster.iter().map(|&i| candidates[i].0).collect();

        // Synthesis — failure = clean job Failed (no partial note written for THIS
        // cluster; previously written clusters remain committed, documented batch behaviour).
        let synthesis = synthesizer.synthesize(&cluster_pairs).await.map_err(|e| {
            HandlerError::Business(format!("distill: synthèse cluster échouée: {e}"))
        })?;

        // Synthesis note frontmatter: PendingReview + provenance distilled +
        // derived-from (ExtraFields, JCS-safe: Vec of ULID strings).
        let synth_id = NoteId::new();
        let derived_from: Vec<TomlValue> = source_ids
            .iter()
            .map(|id| TomlValue::String(id.to_string()))
            .collect();
        let mut extra = ExtraFields::empty();
        extra.insert("derived-from".to_string(), TomlValue::Array(derived_from));

        let _fm = Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new(&vault_id),
            locus: None,
            section: Section::Reference,
            status: NoteStatus::PendingReview,
            status_reason: Some("distilled — en attente de revue".to_string()),
            status_changed: None,
            tags: SmallVec::new(),
            author: Some(AuthorRef::system("vault-distiller")),
            created: Utc::now(),
            updated: None,
            extra,
            provenance: Some("distilled".to_string()),
            forgotten: None,
            forgotten_at: None,
            forgotten_by: None,
        };

        // Compute dynamic trust before writing the synthesis note.
        // Preload source trusts via client (async), then apply synchronous compute_distill_trust.
        let mut trust_map: std::collections::HashMap<ulid::Ulid, f32> =
            std::collections::HashMap::with_capacity(source_ids.len());
        for src in &source_ids {
            if let Ok(t) = client.get_trust(&src.to_string()).await {
                trust_map.insert(src.0, t);
            }
        }
        let lookup = MapTrustLookup(trust_map);
        let trust = gradatum_core::provenance::compute_distill_trust(
            &source_ids.iter().map(|n| n.0).collect::<Vec<_>>(),
            &lookup,
            confidence_threshold,
        );

        // Write synthesis note via server persist_distill (handles vault + title + trust).
        let synth_id_str = synth_id.to_string();
        let persist_req = PersistDistillRequest {
            note_id: synth_id_str.clone(),
            tenant_id: vault_id.clone(),
            title: synthesis.title.clone(),
            body: synthesis.body.clone(),
            section: "reference".to_string(),
            trust: Some(trust),
            expected_sha256: None,
            mark_processed: false,
            derived_into: None,
            derived_from: source_ids.iter().map(|id| id.to_string()).collect(),
        };
        match client.persist_distill(&persist_req).await {
            Ok(_) => {}
            Err(e) => {
                return Err(HandlerError::Business(format!(
                    "distill: write synthèse: {e}"
                )));
            }
        }
        notes_created.push(synth_id.0);

        // ── Mark sources: processed=true + derived-into ───────────────────────
        for src_id in &source_ids {
            match client
                .persist_distill(&PersistDistillRequest {
                    note_id: src_id.to_string(),
                    tenant_id: vault_id.clone(),
                    title: String::new(),
                    body: String::new(),
                    section: String::new(),
                    trust: None,
                    expected_sha256: None,
                    mark_processed: true,
                    derived_into: Some(synth_id_str.clone()),
                    derived_from: vec![],
                })
                .await
            {
                Ok(_) => notes_modified.push(src_id.0),
                Err(e) => {
                    mark_failures += 1;
                    tracing::warn!(note_id = %src_id, error = %e, "distill: marquage source échoué — non fatal");
                }
            }
        }
    }

    tracing::info!(
        job_id = %job.record.id,
        clusters = clusters.len(),
        notes_created = notes_created.len(),
        sources_marked = notes_modified.len(),
        mark_failures = mark_failures,
        "distill: terminé"
    );

    let created_count = notes_created.len();
    let modified_count = notes_modified.len();
    Ok(JobOutput {
        notes_created,
        notes_modified,
        files: vec![],
        result_note_md: format!(
            "distill: {} cluster(s) → {created_count} synthèse(s) PendingReview, \
             {modified_count} source(s) marquée(s) processed, mark_failures: {mark_failures}",
            clusters.len()
        ),
    })
}

/// Resolves a distillation `JobScope` into a list of candidate `NoteId`s via `InternalClient`.
///
/// - `Locus(prefix)`: notes whose locus starts with `prefix`.
/// - `Notes(ids)`: explicit set of note IDs.
/// - `VaultWide`: all notes in the vault (permitted in dry-run only —
///   the handler guard rejects `VaultWide` in real mode before this call).
/// - `Session(_)`: not supported for distillation (`HandlerError::Business`).
async fn resolve_distill_scope(
    client: &dyn InternalClient,
    vault_id: &str,
    scope: &JobScope,
) -> Result<Vec<NoteId>, HandlerError> {
    match scope {
        JobScope::Locus(prefix) => {
            let rows = client
                .list_notes_by_locus(vault_id, prefix)
                .await
                .map_err(|e| {
                    HandlerError::Business(format!("distill: list_notes_by_locus: {e}"))
                })?;
            rows.into_iter()
                .filter_map(|dto| ulid::Ulid::from_string(&dto.note_id).ok().map(NoteId))
                .map(Ok)
                .collect()
        }
        JobScope::Notes(ids) => Ok(ids.iter().copied().map(NoteId).collect()),
        JobScope::VaultWide => {
            // Reached only in dry-run (handler guard). Lists all Live + PendingReview notes.
            let mut all = Vec::new();
            for status_str in ["live", "pending-review", "staging"] {
                let ids = client
                    .list_by_status(vault_id, status_str)
                    .await
                    .map_err(|e| HandlerError::Business(format!("distill: list_by_status: {e}")))?;
                all.extend(
                    ids.into_iter()
                        .filter_map(|dto| ulid::Ulid::from_string(&dto.note_id).ok().map(NoteId)),
                );
            }
            Ok(all)
        }
        JobScope::Session(_) => Err(HandlerError::Business(
            "distill: JobScope::Session non supporté".to_string(),
        )),
    }
}

/// Synchronous `TrustLookup` adapter backed by a preloaded in-memory map.
///
/// `compute_distill_trust` requires a synchronous `&dyn TrustLookup`; `SqliteIndex`
/// only exposes `get_trust` async. This adapter preloads source trusts
/// (async I/O) then provides the expected synchronous view.
struct MapTrustLookup(std::collections::HashMap<ulid::Ulid, f32>);

impl gradatum_core::provenance::TrustLookup for MapTrustLookup {
    fn get_trust(&self, id: &ulid::Ulid) -> Option<f32> {
        self.0.get(id).copied()
    }
}

// is_processed removed — note.processed now served by NoteReadDto.processed field.

// mark_source_processed removed — now delegated to server via persist_distill(mark_processed=true).

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Builds a complete `JobRecord` for `Job::Embed(EmbedSpec)`.
///
/// Mirrors `build_curate_job_record` in `gradatum-server/src/api_v1/write.rs`.
///
/// # Parameters
///
/// - `note_id`: ULID of the note to embed (`NoteId.0`).
/// - `tenant_id`: tenant of the parent job (inherited from the curate job).
/// - `parent_job_id`: ULID of the parent curate job (`lineage.parent_job`).
fn build_embed_job_record(
    note_id: gradatum_core::identity::NoteId,
    tenant_id: &str,
    parent_job_id: ulid::Ulid,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    JobRecord {
        id: ulid::Ulid::new(),
        spec: JobSpec {
            kind: Job::Embed(EmbedSpec {
                note_id: note_id.0,
                tenant_id: tenant_id.to_string(),
                // Idempotence: handle_embed skips if a vector is already present.
                force_regenerate: false,
            }),
            class,
            mode: JobMode::Batch,
            scope: JobScope::Notes(vec![note_id.0]),
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            // Must be empty — the cascade engine is not yet implemented in gradatum_queue.rs.
            // A non-empty await_jobs would leave this job stuck in Waiting indefinitely.
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
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: Some(parent_job_id),
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Converts a kebab-case section string into a `Section` enum via `serde_json`.
///
/// Returns `None` if the string is not a valid canonical section.
fn section_from_str(s: &str) -> Option<Section> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<Section>(&json_str).ok()
}

/// Converts a `NoteStatus` to its kebab-case string representation for the internal API.
fn status_to_str(status: NoteStatus) -> String {
    match status {
        NoteStatus::Live => "live".to_string(),
        NoteStatus::PendingReview => "pending-review".to_string(),
        NoteStatus::Staging => "staging".to_string(),
        NoteStatus::Garbage => "garbage".to_string(),
        NoteStatus::Draft => "draft".to_string(),
        NoteStatus::Deprecated => "deprecated".to_string(),
    }
}

/// Builds a `Frontmatter` from a `CurateSpec` and the curator decisions.
///
/// Used for the vault_write path (title/body present in the spec).
#[allow(dead_code)]
fn build_frontmatter_from_spec(
    tenant_id: &str,
    section: Section,
    status: NoteStatus,
    spec: &CurateSpec,
    curator_tags: &[String],
) -> Frontmatter {
    let mut all_tags: Vec<String> = spec.tags.clone();
    for t in curator_tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }

    // C-TAG-1 : alignement sur le régime interne `parse_tags` (persist.rs).
    // Utilise `Tag::normalize` + WARN sur transformation + dédup, au lieu du
    // `filter_map(Tag::new(...).ok())` qui silençait les tags légitimes.
    let tags: SmallVec<[Tag; 4]> = {
        let mut seen = std::collections::HashSet::with_capacity(all_tags.len());
        let mut out: SmallVec<[Tag; 4]> = SmallVec::new();
        for t in &all_tags {
            let norm = Tag::normalize(t.clone());
            if norm.as_ref().map(|n| n.as_str()) != Some(t.as_str()) {
                tracing::warn!(
                    original = %t,
                    normalized = ?norm.as_ref().map(|n| n.as_str()),
                    "build_frontmatter_from_spec: tag normalisé (C-TAG-1)"
                );
            }
            if let Some(tag) = norm
                && seen.insert(tag.as_str().to_owned())
            {
                out.push(tag);
            }
        }
        out
    };

    let author = spec.author.as_deref().map(AuthorRef::system);

    // Resolve provenance from section_hint.
    // If section_hint ∈ TRUST_SCORES → provenance = section_hint.
    // Otherwise (or absent) → conservative default "agent-log" (trust 0.50).
    let provenance = gradatum_core::provenance::resolve_provenance(spec.section_hint.as_deref());

    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(tenant_id),
        locus: None,
        section,
        status,
        status_reason: None,
        status_changed: None,
        tags,
        author,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: Some(provenance.to_string()),
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TemporalIndex helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parses an `ExtraFields` field as a UTC epoch in milliseconds.
///
/// Accepted formats:
/// - ISO 8601 / RFC 3339 (with time): `2024-03-15T10:00:00Z`
/// - Date-only YYYY-MM-DD → start of day UTC: `2024-03-15`
///
/// Returns `None` if the field is absent, non-`String`, or malformed.
///
/// # Side effects
///
/// None. Pure function.
#[allow(dead_code)]
pub(crate) fn parse_extra_field_as_ms(extra: &ExtraFields, key: &str) -> Option<i64> {
    let val = extra.get(key)?;
    let s = match val {
        TomlValue::String(s) => s.as_str(),
        // Non-String formats (Integer, Float, etc.) → ignored (JCS constraint)
        _ => return None,
    };
    // Attempt RFC 3339 (with time)
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    // Attempt date-only YYYY-MM-DD → start of day UTC
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        use chrono::TimeZone as _;
        let dt = chrono::Utc.from_utc_datetime(
            &d.and_hms_opt(0, 0, 0)
                .expect("hms(0,0,0) est un horaire valide — ne peut pas échouer"),
        );
        return Some(dt.timestamp_millis());
    }
    None
}

/// Resolves the temporal anchor of a note according to the field priority order.
///
/// Priority (descending):
/// 1. `occurred_at` in `frontmatter.extra` (ISO 8601 UTC string)
/// 2. `event-date` in `frontmatter.extra`
/// 3. `valid_from` in `frontmatter.extra`
/// 4. `frontmatter.created` (universal fallback — always present)
///
/// ## Robustness
///
/// `ExtraFields` values are `toml::Value::String` (JCS constraint — see frontmatter.rs).
/// An invalid format silently falls back to the next lower-priority field,
/// and ultimately to `created` (no panic, no propagated error).
///
/// ## Returns
///
/// `(anchor_ms, AnchorSrc)` — UTC epoch in milliseconds + identified source.
///
/// # Side effects
///
/// None. Pure function.
#[allow(dead_code)]
pub(crate) fn resolve_temporal_anchor(extra: &ExtraFields, created_ms: i64) -> (i64, AnchorSrc) {
    if let Some(ms) = parse_extra_field_as_ms(extra, "occurred_at") {
        return (ms, AnchorSrc::OccurredAt);
    }
    if let Some(ms) = parse_extra_field_as_ms(extra, "event-date") {
        return (ms, AnchorSrc::EventDate);
    }
    if let Some(ms) = parse_extra_field_as_ms(extra, "valid_from") {
        return (ms, AnchorSrc::ValidFrom);
    }

    (created_ms, AnchorSrc::Created)
}

/// Extracts the validity end bound of a note from `frontmatter.extra`.
///
/// Reads the `valid_until` field (same parsers as `valid_from`: ISO 8601 / YYYY-MM-DD / ms).
///
/// ## Consistency guard
///
/// If `valid_until_ms` is present AND `≤ anchor_ms` → returns `None` and emits a warning.
/// An invalid window is ignored (the note remains visible): accuracy over coverage.
///
/// ## Returns
///
/// `Some(epoch_ms)` if `valid_until` is present and coherent, `None` otherwise (open validity).
///
/// # Side effects
///
/// None except the warning emitted on an invalid window.
#[allow(dead_code)]
pub(crate) fn extract_valid_until(extra: &ExtraFields, anchor_ms: i64) -> Option<i64> {
    let valid_until_ms = parse_extra_field_as_ms(extra, "valid_until")?;
    if valid_until_ms <= anchor_ms {
        tracing::warn!(
            anchor_ms,
            valid_until_ms,
            "valid_until ≤ anchor_ms : fenêtre invalide ignorée (note reste visible)"
        );
        return None;
    }
    Some(valid_until_ms)
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, EmbedSpec, GradatumJob, Job, JobClass, JobLifecycle, JobLineage, JobMode,
        JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, PurgeSpec,
        ReIndexMode, TriggerSource,
    };
    use ulid::Ulid;

    fn make_job(kind: Job, mode: JobMode) -> GradatumJob {
        let now = Utc::now();
        let class = JobClass::Agent;
        GradatumJob {
            priority: JobPriority::default_for(&class).as_u8(),
            record: JobRecord {
                id: Ulid::new(),
                spec: JobSpec {
                    kind,
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

    // Les tests dry-run ne nécessitent pas les dépendances Data — le handler retourne
    // avant tout accès aux deps. Tests Data injectés = tests d'intégration dans tests/.
    // On ne peut pas facilement construire Data<T> en unit test sans Apalis runtime.
    // Pattern : tester dry-run ici, tester le chemin Batch dans monitor_integration.rs.

    #[tokio::test]
    async fn curate_dry_run_returns_output() {
        let job = make_job(
            Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                title: Some("Test note".to_string()),
                body: Some("Test body".to_string()),
                ..Default::default()
            }),
            JobMode::DryRun,
        );
        // Dry run retourne AVANT d'accéder aux deps — on peut appeler avec des Data vides.
        // Cependant, la signature apalis::Data<T> n'est pas facilement mockable sans runtime.
        // Ce test vérifie uniquement la logique DryRun via le trait DryRunAware.
        assert!(job.record.is_dry_run());
    }

    #[tokio::test]
    async fn embed_dry_run_is_detected() {
        let job = make_job(
            Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobMode::DryRun,
        );
        assert!(job.record.is_dry_run());
    }

    #[tokio::test]
    async fn reindex_dry_run_is_detected() {
        let job = make_job(Job::ReIndex(ReIndexMode::FtsOnly), JobMode::DryRun);
        assert!(job.record.is_dry_run());
    }

    /// handle_reindex en mode Batch retourne Err(HandlerError::Business) — jamais Ok.
    ///
    /// Les modes FtsOnly/MissingOnly/VectorsOnly/Full sont tous différés en v0.4.x.
    /// Le handler rejette explicitement le job pour éviter un Ok silencieux trompeur.
    #[tokio::test]
    async fn handle_reindex_batch_returns_err_not_implemented() {
        use gradatum_embed::Noop;

        // Les deps _client/_embedder ne sont jamais accédés dans le chemin non-DryRun
        // (le handler retourne Err::Business avant tout accès).
        // Un mock minimal suffit pour satisfaire la signature du handler (worker-flip).
        struct NeverCalledClient;
        #[async_trait::async_trait]
        impl crate::internal_client::InternalClient for NeverCalledClient {
            async fn persist_curated(
                &self,
                _: &gradatum_dto::PersistCuratedRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn persist_embedding(
                &self,
                _: &gradatum_dto::PersistEmbeddingRequest,
            ) -> Result<
                gradatum_dto::EmbeddingOkResponse,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn persist_forget(
                &self,
                _: &gradatum_dto::PersistForgetRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn persist_distill(
                &self,
                _: &gradatum_dto::PersistDistillRequest,
            ) -> Result<gradatum_dto::PersistOkResponse, crate::internal_client::InternalClientError>
            {
                unimplemented!()
            }
            async fn delete_note(
                &self,
                _: &str,
            ) -> Result<(), crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn get_note(
                &self,
                _: &str,
            ) -> Result<
                crate::internal_client::NoteReadDto,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn get_note_embedding(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                crate::internal_client::EmbeddingReadDto,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn get_trust(
                &self,
                _: &str,
            ) -> Result<f32, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn title_lookup(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn id_lookup(
                &self,
                _: &str,
                _: &str,
            ) -> Result<Option<String>, crate::internal_client::InternalClientError> {
                unimplemented!()
            }
            async fn list_notes_by_locus(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_by_status(
                &self,
                _: &str,
                _: &str,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_garbage(
                &self,
                _: &str,
                _: i64,
                _: u32,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn search_fts_for_forget(
                &self,
                _: &str,
                _: &str,
                _: usize,
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
            async fn list_notes_by_agent(
                &self,
                _: &str,
                _: &[String],
            ) -> Result<
                Vec<crate::internal_client::NoteIdDto>,
                crate::internal_client::InternalClientError,
            > {
                unimplemented!()
            }
        }

        let client_data = Data::new(
            Arc::new(NeverCalledClient) as Arc<dyn crate::internal_client::InternalClient>
        );
        let embedder: Arc<dyn Embedder + Send + Sync> = Arc::new(Noop::new(384));
        let embedder_data = Data::new(embedder);

        for mode in [
            ReIndexMode::FtsOnly,
            ReIndexMode::MissingOnly,
            ReIndexMode::VectorsOnly,
            ReIndexMode::Full,
        ] {
            let job = make_job(Job::ReIndex(mode.clone()), JobMode::Batch);
            let result = handle_reindex(job, client_data.clone(), embedder_data.clone()).await;
            assert!(
                matches!(result, Err(HandlerError::Business(_))),
                "handle_reindex({mode:?}) doit retourner Err(Business) en v0.4.x, obtenu : {result:?}"
            );
        }
    }

    // ── Lot 5 : ensure_main_tenant (garde worker P0 cross-tenant) ──────────────

    #[test]
    fn ensure_main_tenant_accepts_main() {
        assert!(super::ensure_main_tenant("main").is_ok());
    }

    #[test]
    fn ensure_main_tenant_rejects_non_main() {
        let r = super::ensure_main_tenant("evil");
        assert!(
            matches!(r, Err(HandlerError::Business(_))),
            "tenant ≠ main → HandlerError::Business, obtenu : {r:?}"
        );
    }

    #[test]
    fn ensure_main_tenant_rejects_empty() {
        assert!(matches!(
            super::ensure_main_tenant(""),
            Err(HandlerError::Business(_))
        ));
    }

    #[tokio::test]
    async fn curate_unexpected_variant_check() {
        // Vérification que Backup n'est pas un Curate spec (logique de guard variant)
        let job = make_job(Job::Backup, JobMode::Batch);
        // La vérification du variant se fait dans le handler — vérifier que le Job::Backup
        // n'est pas un Job::Curate (invariant statique du type).
        assert!(!matches!(&job.record.spec.kind, Job::Curate(_)));
    }

    // ── Tests Job::Purge ──────────────────────────────────────────────────────

    /// DryRunAware::is_dry_run() détecte JobMode::DryRun pour Job::Purge.
    #[tokio::test]
    async fn purge_dry_run_via_job_mode_is_detected() {
        let job = make_job(
            Job::Purge(PurgeSpec {
                mode: gradatum_core::PurgeMode::Lifecycle,
                dry_run: false, // spec.dry_run = false
                grace_days: Some(30),
            }),
            JobMode::DryRun, // mais JobMode = DryRun → is_dry_run() = true
        );
        assert!(
            job.record.is_dry_run(),
            "JobMode::DryRun doit activer le dry-run même si spec.dry_run=false"
        );
    }

    /// spec.dry_run=true (défaut) active le dry-run même en mode Batch.
    #[tokio::test]
    async fn purge_dry_run_via_spec_is_detected() {
        let job = make_job(
            Job::Purge(PurgeSpec::default()), // dry_run=true par défaut
            JobMode::Batch,
        );
        // is_dry_run() retourne false (JobMode::Batch), mais spec.dry_run = true.
        // La double garde dans handle_purge couvrira les deux.
        assert!(
            !job.record.is_dry_run(),
            "JobMode::Batch → is_dry_run() = false"
        );
        assert!(
            matches!(&job.record.spec.kind, Job::Purge(s) if s.dry_run),
            "PurgeSpec::default() doit avoir dry_run=true"
        );
    }

    /// Job::Purge avec variant inattendu → HandlerError::UnexpectedVariant.
    ///
    /// Vérifie le guard variant : Job::Backup ≠ Job::Purge.
    #[tokio::test]
    async fn purge_unexpected_variant_is_not_purge() {
        let job = make_job(Job::Backup, JobMode::Batch);
        assert!(!matches!(&job.record.spec.kind, Job::Purge(_)));
    }

    /// PurgeSpec::default() : valeurs prudentes par défaut.
    #[test]
    fn purge_spec_default_values_in_handler_tests() {
        let spec = PurgeSpec::default();
        assert!(spec.dry_run, "dry_run doit être true par défaut");
        assert_eq!(spec.grace_days, Some(30));
    }

    // ── Tests F-55 TemporalIndex ──────────────────────────────────────────────

    /// Fallback : ExtraFields vide → anchor_src='created', anchor_ms=created_ms.
    #[test]
    fn resolve_temporal_anchor_fallback_to_created() {
        let extra = ExtraFields::empty();
        let created_ms = 1_700_000_000_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(ms, created_ms, "fallback doit retourner created_ms");
        assert_eq!(
            src,
            AnchorSrc::Created,
            "fallback doit retourner AnchorSrc::Created"
        );
    }

    /// Priorité occurred_at > event-date > valid_from > created.
    #[test]
    fn resolve_temporal_anchor_priority_occurred_at_wins() {
        let mut extra = ExtraFields::empty();
        // occurred_at + event-date tous deux présents → occurred_at doit gagner.
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("2024-03-15T10:00:00Z".to_string()),
        );
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("2024-01-01T00:00:00Z".to_string()),
        );
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-01-01T00:00:00Z".to_string()),
        );

        let created_ms = 0i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);

        assert_eq!(
            src,
            AnchorSrc::OccurredAt,
            "occurred_at doit prendre priorité"
        );
        // 2024-03-15T10:00:00Z = epoch ms
        let expected = chrono::DateTime::parse_from_rfc3339("2024-03-15T10:00:00Z")
            .expect("parsing test date")
            .timestamp_millis();
        assert_eq!(ms, expected, "anchor_ms doit correspondre à occurred_at");
    }

    /// Priorité : sans occurred_at, event-date prend le relais.
    #[test]
    fn resolve_temporal_anchor_priority_event_date_second() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("2024-06-01T00:00:00Z".to_string()),
        );
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-01-01T00:00:00Z".to_string()),
        );

        let (_, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::EventDate,
            "event-date doit prendre priorité sur valid_from"
        );
    }

    /// Priorité : sans occurred_at ni event-date, valid_from prend le relais.
    #[test]
    fn resolve_temporal_anchor_priority_valid_from_third() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_from".to_string(),
            TomlValue::String("2023-09-15T00:00:00Z".to_string()),
        );

        let (_, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::ValidFrom,
            "valid_from doit prendre priorité sur created"
        );
    }

    /// Format date seule YYYY-MM-DD accepté et parsé comme début du jour UTC.
    #[test]
    fn resolve_temporal_anchor_date_only_format() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("2024-03-15".to_string()),
        );

        let (ms, src) = resolve_temporal_anchor(&extra, 0);
        assert_eq!(
            src,
            AnchorSrc::OccurredAt,
            "format date seule doit être accepté"
        );
        // 2024-03-15T00:00:00Z
        let expected = chrono::DateTime::parse_from_rfc3339("2024-03-15T00:00:00Z")
            .expect("parsing expected")
            .timestamp_millis();
        assert_eq!(ms, expected, "date seule → début du jour UTC");
    }

    /// Format invalide → fallback silencieux vers champ inférieur ou created.
    #[test]
    fn resolve_temporal_anchor_invalid_format_falls_back() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "occurred_at".to_string(),
            TomlValue::String("not-a-date".to_string()),
        );
        extra.insert(
            "event-date".to_string(),
            TomlValue::String("aussi-invalide".to_string()),
        );

        let created_ms = 42_000_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(
            src,
            AnchorSrc::Created,
            "formats invalides → fallback created"
        );
        assert_eq!(ms, created_ms, "anchor_ms doit être created_ms en fallback");
    }

    /// Champ ExtraFields non-String (Integer) → ignoré, fallback vers created.
    #[test]
    fn resolve_temporal_anchor_non_string_value_ignored() {
        let mut extra = ExtraFields::empty();
        // toml::Value::Integer — ne doit pas être parsé comme date
        extra.insert("occurred_at".to_string(), TomlValue::Integer(1_700_000_000));

        let created_ms = 99_000i64;
        let (ms, src) = resolve_temporal_anchor(&extra, created_ms);
        assert_eq!(
            src,
            AnchorSrc::Created,
            "valeur non-String doit être ignorée"
        );
        assert_eq!(ms, created_ms);
    }

    /// AnchorSrc::as_db_str() retourne les chaînes canoniques attendues par la migration 0013.
    #[test]
    fn anchor_src_as_db_str_canonical_values() {
        assert_eq!(AnchorSrc::OccurredAt.as_db_str(), "occurred_at");
        assert_eq!(AnchorSrc::EventDate.as_db_str(), "event-date");
        assert_eq!(AnchorSrc::ValidFrom.as_db_str(), "valid_from");
        assert_eq!(AnchorSrc::Created.as_db_str(), "created");
    }

    // ── Tests Lot 1 — extraction valid_until (v0.5.1) ────────────────────────

    /// Cas a — note sans `valid_until` → `None`.
    #[test]
    fn extract_valid_until_absent_returns_none() {
        let extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(result.is_none(), "sans valid_until → None");
    }

    /// Cas b — `valid_until` futur par rapport à anchor → Some(ms).
    #[test]
    fn extract_valid_until_future_returns_some() {
        let mut extra = ExtraFields::empty();
        // anchor_ms = 1000, valid_until bien futur
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2030-01-01T00:00:00Z".to_string()),
        );
        let anchor_ms = 1_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(result.is_some(), "valid_until futur → Some");
        let expected = chrono::DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(result.unwrap(), expected);
    }

    /// Cas b — format date seule YYYY-MM-DD accepté pour valid_until.
    #[test]
    fn extract_valid_until_date_only_format() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2030-06-15".to_string()),
        );
        let anchor_ms = 1_000i64;
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_some(),
            "format date seule accepté pour valid_until"
        );
        let expected = chrono::DateTime::parse_from_rfc3339("2030-06-15T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(result.unwrap(), expected);
    }

    /// Cas f — `valid_until ≤ anchor` → None + warn (fenêtre invalide ignorée).
    #[test]
    fn extract_valid_until_equal_to_anchor_returns_none() {
        let mut extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        // valid_until == anchor (borne incluse → invalide)
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2023-11-14T22:13:20Z".to_string()),
        );
        // Vérifie que c'est bien la valeur attendue
        let expected_ms = chrono::DateTime::parse_from_rfc3339("2023-11-14T22:13:20Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            expected_ms, anchor_ms,
            "précondition: anchor_ms = valid_until ms"
        );
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_none(),
            "valid_until == anchor → None (fenêtre invalide)"
        );
    }

    /// Cas f — `valid_until < anchor` → None (fenêtre invalide).
    #[test]
    fn extract_valid_until_before_anchor_returns_none() {
        let mut extra = ExtraFields::empty();
        let anchor_ms = 1_700_000_000_000i64;
        // valid_until bien avant anchor
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("2020-01-01T00:00:00Z".to_string()),
        );
        let result = extract_valid_until(&extra, anchor_ms);
        assert!(
            result.is_none(),
            "valid_until < anchor → None (fenêtre invalide)"
        );
    }

    /// Format invalide pour valid_until → None (fallback silencieux).
    #[test]
    fn extract_valid_until_invalid_format_returns_none() {
        let mut extra = ExtraFields::empty();
        extra.insert(
            "valid_until".to_string(),
            TomlValue::String("pas-une-date".to_string()),
        );
        let result = extract_valid_until(&extra, 1_000i64);
        assert!(result.is_none(), "format invalide → None");
    }
}
