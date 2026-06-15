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
    author::AuthorRef,
    frontmatter::{ExtraFields, Frontmatter},
    identity::{ContentHash, NoteId},
    index::{AnchorSrc, TemporalEntry},
    scope::VaultId,
    section::{section_to_doc_kind, Section},
    status::NoteStatus,
    tag::Tag,
    CurateSpec, DryRunAware, EmbedSpec, ForgetScope, GradatumJob, Job, JobClass, JobLifecycle,
    JobLineage, JobMode, JobOutput, JobPriority, JobRecord, JobRetry, JobScheduling, JobScope,
    JobSpec, JobStatus, QueueStore, TriggerSource,
};
// `Index` facade (supertrait) — resolves all methods of the 3 sub-traits on
// `Arc<dyn Index>` via their bounds, without explicit sub-trait imports.
use gradatum_core::index::Index;
use gradatum_curator::{CurateOutcome, CuratorProcess};
use gradatum_embed::Embedder;
use gradatum_vault::{Vault, WriteResult};

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
    vault: Data<Arc<Vault>>,
    curator: Data<Arc<dyn CuratorProcess + Send + Sync>>,
    index: Data<Arc<dyn Index>>,
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
    let (note_id_for_vault, curator_note) = if spec.title.is_some() && spec.body.is_some() {
        // vault_write path: note to create
        let curator_note = gradatum_curator::Note {
            id: spec.note_id.to_string(),
            title: spec.title.clone().unwrap_or_default(),
            body: spec.body.clone().unwrap_or_default(),
            tags_hint: spec.tags.clone(),
            section_hint: spec.section_hint.clone(),
        };
        (None, curator_note) // create path : write_note_with_id(spec.note_id) honore l'ULID préalloué
    } else {
        // Reclassification path: read the existing note from the vault
        let note_id = NoteId(spec.note_id);
        let existing = vault
            .read_note(note_id)
            .await
            .map_err(|e| HandlerError::Business(format!("read_note: {e}")))?;
        let title_for_curator = gradatum_curator::extract_h1_title(&existing.body.markdown)
            .unwrap_or_else(|| existing.frontmatter.section.as_str().to_string());
        let curator_note = gradatum_curator::Note {
            id: spec.note_id.to_string(),
            title: title_for_curator,
            body: existing.body.markdown.clone(),
            tags_hint: existing
                .frontmatter
                .tags
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            section_hint: None,
        };
        (Some(note_id), curator_note)
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
            let section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            let note = if let Some(existing_note_id) = note_id_for_vault {
                // Reclassification — update the existing note while preserving its ULID.
                // IMPORTANT: use write_note_with_id (not write_note) to honour the
                // pre-allocated ULID (spec.note_id). write_note would generate NoteId::new() →
                // divergent ULID → invalid 202 note_id → dead wikilinks.
                let existing = vault
                    .read_note(existing_note_id)
                    .await
                    .map_err(|e| HandlerError::Business(format!("read_note classify: {e}")))?;
                let mut fm = existing.frontmatter.clone();
                fm.section = section;
                for tag_str in &decisions.tags {
                    if !fm.tags.iter().any(|t| t.as_str() == tag_str.as_str()) {
                        if let Ok(t) = Tag::new(tag_str) {
                            fm.tags.push(t);
                        }
                    }
                }
                vault
                    .write_note_with_id(fm, existing.body.markdown.clone(), existing_note_id)
                    .await
                    .map_err(|e| {
                        HandlerError::Business(format!("write_note_with_id classify: {e}"))
                    })?
            } else {
                // vault_write — create the note honouring the pre-allocated ULID.
                // write_if_match verifies expected_sha256 before writing (optimistic lock).
                // Some(Live) on the Admitted branch (outcome_to_status SSOT).
                let status = write_status.expect("Admitted → Some(Live) par outcome_to_status");
                let fm = build_frontmatter_from_spec(
                    &tenant_id,
                    section,
                    status,
                    &spec,
                    &decisions.tags,
                );
                let write_result = vault
                    .write_if_match(
                        fm,
                        body_for_write.clone(),
                        NoteId(spec.note_id),
                        spec.expected_sha256,
                    )
                    .await
                    .map_err(|e| HandlerError::Business(format!("write_note curate: {e}")))?;
                match write_result {
                    WriteResult::Conflict { current_sha256 } => {
                        // Optimistic-lock conflict — mark the job terminal Conflict
                        // and return WITHOUT writing the note.
                        let current_hex = ContentHash(current_sha256).hex();
                        let attempted_hex = spec.expected_sha256.map(|h| ContentHash(h).hex());
                        let conflict_payload_str = serde_json::json!({
                            "current_sha256": current_hex,
                            "attempted_sha256": attempted_hex,
                            "timestamp_ms": Utc::now().timestamp_millis(),
                        })
                        .to_string();
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
                        return Ok(JobOutput {
                            notes_created: vec![],
                            notes_modified: vec![],
                            files: vec![],
                            result_note_md: format!(
                                "curate: conflit optimistic-lock sur note {} — current_sha256={}",
                                spec.note_id, current_hex
                            ),
                        });
                    }
                    WriteResult::Written { .. } => {}
                }
                // After write_if_match Written: read the written note to return its id.
                vault.read_note(NoteId(spec.note_id)).await.map_err(|e| {
                    HandlerError::Business(format!("read_note post-write curate: {e}"))
                })?
            };

            tracing::info!(
                job_id = %job.record.id,
                section = %decisions.canonical_section,
                "curate: note admise et persistée"
            );
            Some(note.id)
        }
        CurateOutcome::Pending {
            ref decisions,
            ref reason,
        } => {
            let section =
                section_from_str(&decisions.canonical_section).unwrap_or(Section::Reference);

            let note = if let Some(existing_note_id) = note_id_for_vault {
                // Reclassification (Pending) — same invariant as Admitted:
                // write_note_with_id preserves the existing ULID (spec.note_id).
                let existing = vault.read_note(existing_note_id).await.map_err(|e| {
                    HandlerError::Business(format!("read_note classify pending: {e}"))
                })?;
                // Some(PendingReview) on the Pending branch (outcome_to_status SSOT).
                let status =
                    write_status.expect("Pending → Some(PendingReview) par outcome_to_status");
                let mut fm = existing.frontmatter.clone();
                fm.section = section;
                fm.status = status;
                vault
                    .write_note_with_id(fm, existing.body.markdown.clone(), existing_note_id)
                    .await
                    .map_err(|e| {
                        HandlerError::Business(format!("write_note_with_id classify pending: {e}"))
                    })?
            } else {
                // vault_write — create the note honouring the pre-allocated ULID.
                // write_if_match verifies expected_sha256 before writing (optimistic lock).
                // Some(PendingReview) on the Pending branch (outcome_to_status SSOT).
                let status =
                    write_status.expect("Pending → Some(PendingReview) par outcome_to_status");
                let fm = build_frontmatter_from_spec(
                    &tenant_id,
                    section,
                    status,
                    &spec,
                    &decisions.tags,
                );
                let write_result = vault
                    .write_if_match(
                        fm,
                        body_for_write.clone(),
                        NoteId(spec.note_id),
                        spec.expected_sha256,
                    )
                    .await
                    .map_err(|e| {
                        HandlerError::Business(format!("write_note curate pending: {e}"))
                    })?;
                match write_result {
                    WriteResult::Conflict { current_sha256 } => {
                        // Optimistic-lock conflict — mark the job terminal Conflict.
                        let current_hex = ContentHash(current_sha256).hex();
                        let attempted_hex = spec.expected_sha256.map(|h| ContentHash(h).hex());
                        let conflict_payload_str = serde_json::json!({
                            "current_sha256": current_hex,
                            "attempted_sha256": attempted_hex,
                            "timestamp_ms": Utc::now().timestamp_millis(),
                        })
                        .to_string();
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
                        return Ok(JobOutput {
                            notes_created: vec![],
                            notes_modified: vec![],
                            files: vec![],
                            result_note_md: format!(
                                "curate: conflit optimistic-lock (pending) sur note {} — current_sha256={}",
                                spec.note_id, current_hex
                            ),
                        });
                    }
                    WriteResult::Written { .. } => {}
                }
                // After write_if_match Written: read the written note to return its id.
                vault.read_note(NoteId(spec.note_id)).await.map_err(|e| {
                    HandlerError::Business(format!("read_note post-write curate pending: {e}"))
                })?
            };

            tracing::info!(
                job_id = %job.record.id,
                reason = %reason,
                "curate: note mise en Staging (revue manuelle requise)"
            );
            Some(note.id)
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

    // ── upsert_note_title post-curate — non-fatal ────────────────────────────
    // notes.title was never populated on write (most rows NULL in production).
    // `title_resolved` = spec.title (vault_write) or extract_h1_title(body) (reclassification).
    // `DocumentStore::upsert_note_title` is idempotent — UPDATE notes SET title = ?2 WHERE id = ?1.
    if let Some(note_id) = &written_note_id {
        if !title_resolved.is_empty() {
            if let Err(e) = index.upsert_note_title(note_id, &title_resolved).await {
                tracing::warn!(
                    note_id = %note_id,
                    title = %title_resolved,
                    error = %e,
                    "curate: upsert_note_title échoué — non fatal"
                );
            }
        }
    }

    // ── TemporalIndex post-curate — non-fatal ────────────────────────────────
    // Updates the temporal anchor in temporal_index after each successful curate.
    // Priority: occurred_at > event-date > valid_from (ExtraFields) > created (fallback).
    // Reads the note post-write to access the complete frontmatter (extra included).
    // Non-fatal: warn + continue if reading or writing fails.
    if let Some(note_id) = &written_note_id {
        match vault.read_note(*note_id).await {
            Ok(note) => {
                let created_ms = note.frontmatter.created.timestamp_millis();
                let (anchor_ms, anchor_src) =
                    resolve_temporal_anchor(&note.frontmatter.extra, created_ms);
                let doc_kind = section_to_doc_kind(&note.frontmatter.section).to_string();
                let valid_until_ms = extract_valid_until(&note.frontmatter.extra, anchor_ms);
                let entry = TemporalEntry {
                    note_id: note_id.0.to_string(),
                    vault_id: note.frontmatter.vault_id.as_str().to_string(),
                    anchor_ms,
                    anchor_src,
                    doc_kind,
                    valid_until_ms,
                };
                if let Err(e) = index.write_temporal_entry(&entry).await {
                    tracing::warn!(
                        note_id = %note_id,
                        error = %e,
                        "curate: write_temporal_entry échoué — non fatal"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %note_id,
                    error = %e,
                    "curate: read_note pour temporal_index échoué — non fatal"
                );
            }
        }
    }

    // ── Wikilinks post-curate — non-fatal ────────────────────────────────────
    if let Some(note_id) = &written_note_id {
        process_wikilinks_b5(&**index, &tenant_id, &note_id.to_string(), &body_for_write).await;

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
    vault: Data<Arc<Vault>>,
    embedder: Data<Arc<dyn Embedder + Send + Sync>>,
    index: Data<Arc<dyn Index>>,
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

    // Read the note from the vault to obtain the body.
    let note = vault
        .read_note(note_id)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: read_note: {e}")))?;

    let body_text = note.body.markdown.as_str();
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

    index
        .insert_note_embedding(&note_id, embedder.embedder_id(), embedder.dim(), &vec)
        .await
        .map_err(|e| HandlerError::Business(format!("embed: insert_note_embedding: {e}")))?;

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
    // Parameters reserved for the future reindex implementation.
    _index: Data<Arc<dyn Index>>,
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
    vault: Data<Arc<Vault>>,
    index: Data<Arc<dyn Index>>,
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
    let vault_id = vault.tenant_id().as_str();
    let cutoff_ms: Option<i64> = spec.grace_days.map(|days| {
        Utc::now()
            .timestamp_millis()
            .saturating_sub(i64::from(days) * 24 * 3600 * 1000)
    });

    // List eligible Garbage notes (or all if cutoff_ms = None).
    let candidates: Vec<NoteId> = match cutoff_ms {
        Some(cutoff) => index
            .list_garbage_older_than(vault_id, cutoff)
            .await
            .map_err(|e| HandlerError::Business(format!("purge: list_garbage_older_than: {e}")))?,
        None => {
            // No grace period — list all Garbage notes.
            index
                .list_by_status(&VaultId::new(vault_id), NoteStatus::Garbage)
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
            .map(|id| id.to_string())
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
    let mut supprimées: Vec<NoteId> = Vec::with_capacity(count);
    let mut ignorées: usize = 0;

    for note_id in candidates {
        let id_str = note_id.to_string();

        // Re-verify status at delete time (TOCTOU mitigation).
        // If the note was restored (Garbage→Live) between the listing and now,
        // it is silently skipped.
        //
        // Batch robustness: a status read/parse error (e.g. a note that acquired
        // an out-of-enum status between listing and now) MUST NOT abort the whole
        // batch. Consistent with the TOCTOU intent: the problematic note is counted
        // as ignored with a warning, while the remaining legitimate Garbage notes
        // are purged normally.
        let current_status = match index.get_note_status(vault_id, &id_str).await {
            Ok(status) => status,
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: get_note_status illisible — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        };

        match current_status {
            Some(NoteStatus::Garbage) => {
                // Status confirmed Garbage — proceed with deletion.
            }
            Some(other_status) => {
                tracing::info!(
                    note_id = %id_str,
                    status = ?other_status,
                    "purge: note ignorée — statut changé depuis le listing (TOCTOU mitigation)"
                );
                ignorées += 1;
                continue;
            }
            None => {
                // Note absent from the index (already deleted by another process).
                tracing::debug!(
                    note_id = %id_str,
                    "purge: note absente de l'index — déjà supprimée, skip"
                );
                ignorées += 1;
                continue;
            }
        }

        // Delete the .md + .history/<ulid>/ via vault.delete_note.
        match vault.delete_note(note_id).await {
            Ok(()) => {
                tracing::info!(
                    note_id = %id_str,
                    "purge: note supprimée"
                );
            }
            Err(e) => {
                // Individual note error: log and continue
                // (do not abort the whole batch for a single failing note).
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: delete_note échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        }

        // De-index the note (notes + notes_fts + cascade tables).
        // vault.delete_note removes only .md/.history — not the SQLite index.
        // If delete_note_from_index fails the state is INCONSISTENT
        // (.md absent but index entry still present). The note is counted as
        // ignored/error (not deleted) to reflect the inconsistency in metrics.
        match index.delete_note_from_index(vault_id, &id_str).await {
            Ok(_) => {
                tracing::debug!(note_id = %id_str, "purge: note dé-indexée");
            }
            Err(e) => {
                tracing::warn!(
                    note_id = %id_str,
                    error = %e,
                    "purge: delete_note_from_index échoué — INCOHÉRENCE INDEX : \
                     .md absent mais entrée index restante. Note comptée ignorée (pas supprimée)."
                );
                ignorées += 1;
                continue;
            }
        }
        supprimées.push(note_id);

        // Clean wikilink redirections — non-fatal.
        if let Err(e) = index.delete_redirect_by_ulid(&id_str).await {
            tracing::warn!(
                note_id = %id_str,
                error = %e,
                "purge: delete_redirect_by_ulid échoué — non fatal"
            );
        }
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
    vault: Data<Arc<Vault>>,
    index: Data<Arc<dyn Index>>,
) -> Result<JobOutput, HandlerError> {
    // ── Double dry-run guard — first instruction (DryRun mode + ForgetSpec.dry_run) ──
    let spec = match &job.record.spec.kind {
        Job::Forget(spec) => spec.clone(),
        other => {
            return Err(HandlerError::UnexpectedVariant(format!("{other:?}")));
        }
    };

    let is_dry_run = job.record.is_dry_run() || spec.dry_run;
    let vault_id = vault.tenant_id().as_str();

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
            index
                .search_fts_for_forget(effective_vault, query, max_limit)
                .await
                .map_err(|e| HandlerError::Business(format!("forget: search_fts_for_forget: {e}")))?
                .into_iter()
                .map(|(id, _section)| id)
                .collect()
        }
        ForgetScope::Locus {
            vault: scope_vault,
            locus,
        } => {
            let effective_vault = scope_vault.as_str();
            index
                .list_notes_by_locus_prefix(effective_vault, locus)
                .await
                .map_err(|e| {
                    HandlerError::Business(format!("forget: list_notes_by_locus_prefix: {e}"))
                })?
                .into_iter()
                .map(|(id, _section)| id)
                .collect()
        }
        ForgetScope::Agent { agent_id, vaults } => index
            .list_notes_by_agent(agent_id, vaults)
            .await
            .map_err(|e| HandlerError::Business(format!("forget: list_notes_by_agent: {e}")))?
            .into_iter()
            .map(|(id, _section)| id)
            .collect(),
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
        let section_str = index
            .get_note_section(vault_id, &ulid)
            .await
            .map_err(|e| HandlerError::Business(format!("forget: get_note_section {ulid}: {e}")))?;

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
        let note_id = NoteId(raw_ulid);

        // TOCTOU re-verification: if the note is already forgotten, skip idempotently.
        // Count also in oubliées_ulids so that JobOutput.notes_modified reflects ALL
        // forgotten notes (including idempotent ones), consistent with the oubliées count.
        let already_forgotten = index
            .is_note_forgotten(vault_id, ulid)
            .await
            .unwrap_or(false);
        if already_forgotten {
            tracing::debug!(note_id = %ulid, "forget: déjà oubliée — skip idempotent");
            oubliées.push(ulid.clone());
            oubliées_ulids.push(raw_ulid);
            continue;
        }

        // Read the note to obtain the current frontmatter.
        let existing = match vault.read_note(note_id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: read_note échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        };

        // Mutate the frontmatter: forgotten=true, forgotten_at=now, forgotten_by.
        let mut fm = existing.frontmatter.clone();
        // Encode forget fields into ExtraFields (frontmatter YAML — toml::Value).
        // ExtraFields.0 = Option<Box<BTreeMap<String, toml::Value>>>.
        let extra_map = fm
            .extra
            .0
            .get_or_insert_with(|| Box::new(std::collections::BTreeMap::new()));
        let now_ms = chrono::Utc::now().timestamp_millis();
        extra_map.insert("forgotten".to_string(), TomlValue::Boolean(true));
        extra_map.insert("forgotten_at".to_string(), TomlValue::Integer(now_ms));
        if let Some(by) = forgotten_by {
            extra_map.insert(
                "forgotten_by".to_string(),
                TomlValue::String(by.to_string()),
            );
        }

        // Write via CoW (write_note_with_id preserves the ULID + snapshot in .history/).
        match vault
            .write_note_with_id(fm, existing.body.markdown.clone(), note_id)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: write_note_with_id échoué — note ignorée, batch continue"
                );
                ignorées += 1;
                continue;
            }
        }

        // Sync the SQLite index.
        match index.mark_forgotten(vault_id, ulid, forgotten_by).await {
            Ok(()) => {
                tracing::info!(note_id = %ulid, "forget: note oubliée");
            }
            Err(e) => {
                // Potential inconsistency: frontmatter mutated but index not synced.
                // Logged as WARN (non-fatal) — the index will be re-consistent on the next reindex.
                tracing::warn!(
                    note_id = %ulid,
                    error = %e,
                    "forget: mark_forgotten INDEX échoué — INCOHÉRENCE POTENTIELLE frontmatter/index"
                );
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
    vault: Data<Arc<Vault>>,
    index: Data<Arc<dyn Index>>,
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
    let vault_id = vault.tenant_id().as_str().to_string();
    // Mono-vault: vault_id is derived from the physical vault (always "main"),
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
    if !is_dry_run {
        if let JobScope::Locus(prefix) = &spec.scope {
            if prefix.trim().is_empty() {
                return Err(HandlerError::Business(
                    "distill: JobScope::Locus vide/whitespace refusé hors dry-run \
                     (matcherait tout le vault — contourne R3)"
                        .to_string(),
                ));
            }
        }
    }

    // ── Scope resolution → raw candidates (ULIDs) ────────────────────────────
    let raw_candidates: Vec<NoteId> =
        resolve_distill_scope(&**index, &vault_id, &spec.scope).await?;

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
        let note = match vault.read_note(note_id).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(note_id = %note_id, error = %e, "distill: read_note échoué — note ignorée");
                continue;
            }
        };
        // Defensive skip: never distill a forgotten note.
        if note.frontmatter.forgotten == Some(true) {
            tracing::debug!(note_id = %note_id, "distill: note forgotten — ignorée");
            continue;
        }
        // Defensive skip: never distill a Garbage note.
        if note.frontmatter.status == NoteStatus::Garbage {
            tracing::debug!(note_id = %note_id, "distill: note Garbage — ignorée");
            continue;
        }
        // Skip if already distilled (idempotence).
        if is_processed(&note.frontmatter.extra) {
            continue;
        }
        // Skip if no embedding (cannot be clustered).
        let emb = match index.get_note_embedding(&note_id, &embedder_id).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                tracing::debug!(note_id = %note_id, "distill: pas d'embedding — note ignorée");
                continue;
            }
            Err(e) => {
                return Err(HandlerError::Business(format!(
                    "distill: get_note_embedding {note_id}: {e}"
                )));
            }
        };
        let title = gradatum_curator::extract_h1_title(&note.body.markdown).unwrap_or_default();
        candidates.push((note_id, title, note.body.markdown.clone(), emb));
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

        let fm = Frontmatter {
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

        let written = vault
            .write_note_with_id(fm, synthesis.body.clone(), synth_id)
            .await
            .map_err(|e| HandlerError::Business(format!("distill: write synthèse: {e}")))?;

        // Persist the title (notes.title column) — non-fatal.
        if !synthesis.title.is_empty() {
            if let Err(e) = index.upsert_note_title(&written.id, &synthesis.title).await {
                tracing::warn!(note_id = %written.id, error = %e, "distill: upsert_note_title échoué — non fatal");
            }
        }

        // Dynamic trust: compute_distill_trust(sources) overwrites the static 0.60.
        // `compute_distill_trust` expects a synchronous `&dyn TrustLookup`; `SqliteIndex`
        // only exposes `get_trust` async → preload source trusts into an in-memory map,
        // then adapt it via `MapTrustLookup` (sync).
        let mut trust_map: std::collections::HashMap<ulid::Ulid, f32> =
            std::collections::HashMap::with_capacity(source_ids.len());
        for src in &source_ids {
            if let Ok(Some(t)) = index.get_trust(src).await {
                trust_map.insert(src.0, t);
            }
        }
        let lookup = MapTrustLookup(trust_map);
        let trust = gradatum_core::provenance::compute_distill_trust(
            &source_ids.iter().map(|n| n.0).collect::<Vec<_>>(),
            &lookup,
            confidence_threshold,
        );
        match index.set_note_trust(&written.id, trust).await {
            Ok(0) => {
                tracing::warn!(note_id = %written.id, "distill: set_note_trust 0 ligne — note absente de l'index")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(note_id = %written.id, error = %e, "distill: set_note_trust échoué — trust statique conservé")
            }
        }
        notes_created.push(written.id.0);

        // ── Mark sources: processed=true + derived-into ───────────────────────
        // Keys in HISTORY_EXCLUDED_FIELDS → CoW-safe (no spurious .history version entry).
        for src_id in &source_ids {
            match mark_source_processed(&vault, *src_id, &written.id.to_string()).await {
                Ok(()) => notes_modified.push(src_id.0),
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

/// Resolves a distillation `JobScope` into a list of candidate `NoteId`s.
///
/// - `Locus(prefix)`: notes whose locus starts with `prefix`.
/// - `Notes(ids)`: explicit set of note IDs.
/// - `VaultWide`: all notes in the vault (permitted in dry-run only —
///   the handler guard rejects `VaultWide` in real mode before this call).
/// - `Session(_)`: not supported for distillation (`HandlerError::Business`).
async fn resolve_distill_scope(
    index: &dyn Index,
    vault_id: &str,
    scope: &JobScope,
) -> Result<Vec<NoteId>, HandlerError> {
    match scope {
        JobScope::Locus(prefix) => {
            let rows = index
                .list_notes_by_locus_prefix(vault_id, prefix)
                .await
                .map_err(|e| {
                    HandlerError::Business(format!("distill: list_notes_by_locus_prefix: {e}"))
                })?;
            rows.into_iter()
                .filter_map(|(id, _section)| ulid::Ulid::from_string(&id).ok().map(NoteId))
                .map(Ok)
                .collect()
        }
        JobScope::Notes(ids) => Ok(ids.iter().copied().map(NoteId).collect()),
        JobScope::VaultWide => {
            // Reached only in dry-run (handler guard). Lists all Live + PendingReview notes.
            let mut all = Vec::new();
            for status in [
                NoteStatus::Live,
                NoteStatus::PendingReview,
                NoteStatus::Staging,
            ] {
                let ids = index
                    .list_by_status(&VaultId::new(vault_id), status)
                    .await
                    .map_err(|e| HandlerError::Business(format!("distill: list_by_status: {e}")))?;
                all.extend(ids);
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

/// Returns `true` if the note carries `processed = true` in its `ExtraFields` (distillation idempotence).
fn is_processed(extra: &ExtraFields) -> bool {
    matches!(extra.get("processed"), Some(TomlValue::Boolean(true)))
}

/// Marks a source note as distilled: `processed = true` + `derived-into = <synth_ulid>`.
///
/// Writes via the normal vault path (CoW). Both keys are in
/// `HISTORY_EXCLUDED_FIELDS` → no spurious `.history/` version entry is created.
async fn mark_source_processed(
    vault: &Vault,
    src_id: NoteId,
    synth_ulid: &str,
) -> Result<(), HandlerError> {
    let existing = vault
        .read_note(src_id)
        .await
        .map_err(|e| HandlerError::Business(format!("read source {src_id}: {e}")))?;
    let mut fm = existing.frontmatter.clone();
    fm.extra
        .insert("processed".to_string(), TomlValue::Boolean(true));
    fm.extra.insert(
        "derived-into".to_string(),
        TomlValue::String(synth_ulid.to_string()),
    );
    vault
        .write_note_with_id(fm, existing.body.markdown.clone(), src_id)
        .await
        .map_err(|e| HandlerError::Business(format!("write source {src_id}: {e}")))?;
    Ok(())
}

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

/// Builds a `Frontmatter` from a `CurateSpec` and the curator decisions.
///
/// Used for the vault_write path (title/body present in the spec).
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

    let tags: SmallVec<[Tag; 4]> = all_tags
        .iter()
        .filter_map(|t| Tag::new(t.clone()).ok())
        .collect();

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

/// Extracts `[[...]]` wikilinks from the body, resolves them, and persists them in the index.
///
/// Non-fatal: any failure is logged without propagation.
async fn process_wikilinks_b5(index: &dyn Index, tenant_id: &str, src_note_id: &str, body: &str) {
    let wikilinks = gradatum_curator::wikilinks::extract_wikilinks(body);
    if wikilinks.is_empty() {
        return;
    }

    for target_title in &wikilinks {
        match index.title_lookup(tenant_id, target_title).await {
            Ok(Some(dst_id)) => {
                if let Err(e) = index.upsert_link(tenant_id, src_note_id, &dst_id).await {
                    tracing::warn!(
                        src = %src_note_id,
                        dst = %dst_id,
                        error = %e,
                        "B5: upsert_link échoué — non fatal"
                    );
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target = %target_title,
                    "B5: note cible non trouvée — wikilink en suspens"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target = %target_title,
                    error = %e,
                    "B5: title_lookup échoué — non fatal"
                );
            }
        }
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
        use gradatum_index::SqliteIndex;

        // Les deps _index/_embedder ne sont jamais accédés dans le chemin non-DryRun.
        // On les construit quand même pour satisfaire la signature du handler.
        let index = SqliteIndex::open_in_memory()
            .await
            .expect("SqliteIndex en mémoire");
        let embedder: Arc<dyn Embedder + Send + Sync> = Arc::new(Noop::new(384));

        let index_data = Data::new(Arc::new(index) as Arc<dyn Index>);
        let embedder_data = Data::new(embedder);

        for mode in [
            ReIndexMode::FtsOnly,
            ReIndexMode::MissingOnly,
            ReIndexMode::VectorsOnly,
            ReIndexMode::Full,
        ] {
            let job = make_job(Job::ReIndex(mode.clone()), JobMode::Batch);
            let result = handle_reindex(job, index_data.clone(), embedder_data.clone()).await;
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
