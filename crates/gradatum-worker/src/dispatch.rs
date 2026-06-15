//! Job dispatcher: poll queue → curator cascade → vault write → audit.
//!
//! This module is retained for compatibility with existing integration tests.
//! The active binary uses the Apalis Monitor (`monitor.rs`) — Dispatcher not active.
//!
//! ## Full implementation
//!
//! `process_job` handles 3 job kinds:
//! - `curate`    : decode VaultWriteRequest → CuratorPipeline.process → Vault.write_note
//! - `classify`  : decode VaultClassifyRequest → read_note → CuratorProcess.process (full cascade) → Vault.write_note
//! - `downgrade` : decode VaultDowngradeRequest → read_note → state machine → Vault.write_note
//!
//! ## Guarantees
//!
//! - `run_once` is idempotent: returns `Ok(false)` if the queue is empty.
//! - Processing errors are logged and passed to `Queue::fail` — no silent crash.
//! - The job is `complete`-d only if `process_job` returns `Ok(())`.
//! - `AuditSink` is optional: if absent, events are logged but not persisted.
//!

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::audit::http::{AuditSink, HttpAuditActor, HttpAuditEvent};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_curator::{CurateOutcome, CuratorProcess, Note as CuratorNote};
use gradatum_embed::Embedder;
// `Index` facade (supertrait) — resolves `insert_note_embedding`/`title_lookup`/`upsert_link`
// via sub-trait bounds, without explicit sub-trait imports.
use gradatum_core::index::Index;
use gradatum_queue::{LeasedJob, NewJob, Queue, SqliteQueue};
use gradatum_vault::Vault;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tracing::instrument;
use ulid::Ulid;

/// Default lease duration for a dispatched job.
///
/// 5 minutes: sufficient for the curator cascade (novelty + routing + tags
/// + wikilinks + dedup) on heuristic and lightweight LLM models.
const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(300);

/// System actor used for audit events emitted by the worker.
///
/// Distinct from the HTTP server's JWT actors — the worker operates in
/// non-interactive batch mode on behalf of the curator pipeline.
const WORKER_SYSTEM_KID: &str = "gradatum-worker";

// ── Shared DTO (gradatum-dto as single source of truth) ──────────────────────

/// `VaultWriteRequest` — bincode wire contract for the `curate` queue.
///
/// Imported from `gradatum-dto` (single source of truth) to prevent field-order
/// drift between encoder (server/test) and decoder (worker).
///
/// ## Field order (bincode positional invariant)
///
/// Bincode v2 (`config::standard()`) encodes fields in declaration order —
/// without field names. Any encoder/decoder desynchronisation produces silent
/// misalignment errors or `UnexpectedVariant`.
///
/// Canonical order in `gradatum_dto::VaultWriteRequest`:
/// 1. `title`           String
/// 2. `body`            String
/// 3. `author`          Option<String>
/// 4. `tags`            Vec<String>
/// 5. `section_hint`    Option<String>
/// 6. `tenant_id`       String   (default "main")
/// 7. `expected_sha256` Option<String>
/// 8. `note_id`         Option<String>   ← pre-allocated ULID
use gradatum_dto::VaultWriteRequest;

/// `vault_classify` request decoded from the queue bincode payload.
#[derive(Debug, Serialize, Deserialize)]
struct VaultClassifyRequest {
    note_id: String,
    #[serde(default = "default_main")]
    tenant_id: String,
}

/// `vault_downgrade` request decoded from the queue bincode payload.
#[derive(Debug, Serialize, Deserialize)]
struct VaultDowngradeRequest {
    note_id: String,
    reason: String,
    #[serde(default)]
    replaced_by: Option<String>,
    #[serde(default = "default_main")]
    tenant_id: String,
}

fn default_main() -> String {
    "main".into()
}

// ── NoopAuditSink ─────────────────────────────────────────────────────────────

/// No-op audit sink for tests and modes without audit persistence.
pub struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    /// No-op — events are silently discarded.
    async fn record(&self, _event: HttpAuditEvent) -> Result<(), std::io::Error> {
        Ok(())
    }
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

/// Job dispatcher for the worker.
///
/// Built via the builder pattern:
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use gradatum_queue::SqliteQueue;
/// # use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
/// # async fn ex(queue: Arc<SqliteQueue>, vault: Arc<gradatum_vault::Vault>, curator: Arc<gradatum_curator::CuratorPipeline>) {
/// let dispatcher = Dispatcher::new(queue)
///     .with_vault(vault)
///     .with_curator(curator)
///     .with_audit(Arc::new(NoopAuditSink));
/// # }
/// ```
pub struct Dispatcher {
    queue: Arc<SqliteQueue>,
    vault: Option<Arc<Vault>>,
    /// Injectable curation pipeline (trait object).
    ///
    /// Accepts `CuratorPipeline` (production) or a mock for tests.
    curator: Option<Arc<dyn CuratorProcess>>,
    audit: Option<Arc<dyn AuditSink>>,
    /// Index for persisting embeddings and wikilinks (type-erased).
    ///
    /// Optional — if absent, `embed_note` is silently skipped.
    index: Option<Arc<dyn Index>>,
    /// Embedding backend.
    ///
    /// Optional — if absent, `embed_note` is silently skipped.
    embedder: Option<Arc<dyn Embedder>>,
}

impl Dispatcher {
    /// Creates a new dispatcher with the given queue.
    pub fn new(queue: Arc<SqliteQueue>) -> Self {
        Self {
            queue,
            vault: None,
            curator: None,
            audit: None,
            index: None,
            embedder: None,
        }
    }

    /// Injects the vault for note persistence.
    #[must_use]
    pub fn with_vault(mut self, vault: Arc<Vault>) -> Self {
        self.vault = Some(vault);
        self
    }

    /// Injects the curator pipeline for heuristic and LLM processing.
    ///
    /// Accepts any type implementing [`CuratorProcess`] (including `CuratorPipeline`
    /// for production and mocks for tests).
    #[must_use]
    pub fn with_curator(mut self, curator: Arc<dyn CuratorProcess>) -> Self {
        self.curator = Some(curator);
        self
    }

    /// Injects the audit sink for operation traceability.
    ///
    /// Optional — if absent, events are logged without persistence.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Injects the index for embedding persistence (type-erased).
    ///
    /// Required to process `embed_note` jobs. Without it, jobs are
    /// silently skipped (noop).
    #[must_use]
    pub fn with_index(mut self, index: Arc<dyn Index>) -> Self {
        self.index = Some(index);
        self
    }

    /// Injects the embedding backend.
    ///
    /// Required to process `embed_note` jobs. Without it, jobs are
    /// silently skipped (noop).
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Attempts to process one available job.
    ///
    /// Returns `Ok(true)` if a job was processed (success or logged failure),
    /// `Ok(false)` if the queue was empty (backoff to the caller).
    pub async fn run_once(&self) -> anyhow::Result<bool> {
        let leased = self
            .queue
            .lease(
                &["curate", "classify", "downgrade", "embed_note"],
                DEFAULT_LEASE_DURATION,
            )
            .await?;

        let Some(job) = leased else {
            return Ok(false);
        };

        match self.process_job(&job).await {
            Ok(()) => {
                self.queue.complete(job.id).await?;
            }
            Err(e) => {
                tracing::error!(
                    job_id = job.id,
                    kind = %job.kind,
                    error = %e,
                    "job échoué — enregistrement pour retry ou dead-letter"
                );
                self.queue.fail(job.id, &e.to_string()).await?;
            }
        }

        Ok(true)
    }

    /// Processes a leased job — full curator + vault + audit cascade.
    ///
    /// ## Supported kinds
    ///
    /// - `curate`    : heuristic admission + note persistence.
    /// - `classify`  : re-routing the section of an existing note.
    /// - `downgrade` : Live → Deprecated transition validated by the state machine.
    ///
    /// ## Errors
    ///
    /// Any error is propagated to `run_once` which passes it to `Queue::fail`.
    /// No panic — the job is retryable.
    #[instrument(skip(self), fields(job_id = job.id, kind = %job.kind))]
    async fn process_job(&self, job: &LeasedJob) -> anyhow::Result<()> {
        let start = std::time::Instant::now();
        let outcome: &str;

        // embed_note is handled before accessing vault/curator (optional for this kind).
        if job.kind.as_str() == "embed_note" {
            self.process_embed_note(job).await?;
            let duration_ms = start.elapsed().as_millis() as i64;
            self.emit_audit(job, "ok", duration_ms).await;
            tracing::info!(
                job_id = job.id,
                kind = %job.kind,
                outcome = "ok",
                duration_ms = duration_ms,
                "job traité"
            );
            return Ok(());
        }

        let vault = self
            .vault
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault non configuré — appeler with_vault"))?;
        let curator = self
            .curator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("curator non configuré — appeler with_curator"))?;

        match job.kind.as_str() {
            // ── curate : novelty + routing + tags → vault.write_note ──────────
            "curate" => {
                let req: VaultWriteRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultWriteRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    title = %req.title,
                    "job curate — lancement cascade curator"
                );

                // Resolve the pre-allocated ULID: honoured if present and parseable,
                // falls back to NoteId::new() if absent or invalid.
                // The same NoteId is used in both create branches (Admitted + Pending)
                // to guarantee write-time id == stored id.
                let prealloc_note_id: NoteId = req
                    .note_id
                    .as_deref()
                    .and_then(|s| ulid::Ulid::from_string(s).ok())
                    .map(NoteId)
                    .unwrap_or_default();

                // Build the curator Note from the request
                let curator_note = CuratorNote {
                    id: ulid::Ulid::new().to_string(),
                    title: req.title.clone(),
                    body: req.body.clone(),
                    tags_hint: req.tags.clone(),
                    section_hint: req.section_hint.clone(),
                };

                let curate_outcome = curator.process(curator_note).await;

                // Status resolved via the single canonical mapping (worker SSOT parity).
                // Admitted → Live, Pending → PendingReview, Rejected → None (no write).
                let write_status = gradatum_curator::outcome_to_status(&curate_outcome);

                match curate_outcome {
                    CurateOutcome::Admitted { decisions } => {
                        // Convert section string → Section enum via serde
                        let section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        // Some(Live) on the Admitted branch (invariant guaranteed by outcome_to_status).
                        let status =
                            write_status.expect("Admitted → Some(Live) par outcome_to_status");
                        let frontmatter = build_frontmatter(
                            &job.tenant_id,
                            section,
                            status,
                            &req,
                            &decisions.tags,
                        );

                        // Honour the pre-allocated ULID:
                        // write_note_with_id preserves stored id == enqueued note_id.
                        let note = vault
                            .write_note_with_id(frontmatter, req.body.clone(), prealloc_note_id)
                            .await
                            .context("vault.write_note_with_id curate")?;

                        outcome = "admitted";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note admise et persistée"
                        );

                        // ── B5: wikilinks post-curate ─────────────────────────
                        //
                        // Extracts `[[Title]]` from the request body, resolves via
                        // `idx.title_lookup` (filter `status='live'`), persists via
                        // `idx.upsert_link` (idempotent INSERT OR IGNORE).
                        //
                        // **Non-fatal**: a failure in extraction, title_lookup or upsert
                        // never invalidates the already-persisted note — only logged.
                        //
                        // The `for target in &wikilinks` loop is serial (N×N for N notes × N wikilinks).
                        // Planned improvement: batch `WHERE title IN (?, ?, ?)` or tokio::join_all.
                        process_wikilinks_b5(self, &job.tenant_id, &note.id.to_string(), &req.body)
                            .await;

                        // Automatic chaining: enqueue embed_note after a successful curate write.
                        // Best-effort: an enqueue failure does not invalidate the curate (note already persisted).
                        // The backfill tool can retry if necessary.
                        let embed_payload = serde_json::json!({
                            "note_id": note.id.to_string(),
                            "body_text": note.body.markdown,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %note.id,
                                error = %e,
                                "chaînage embed_note enqueue échoué — backfill pourra re-tenter"
                            );
                        }
                    }
                    CurateOutcome::Rejected { reason } => {
                        // Note rejected — no write to the vault
                        outcome = "rejected";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note rejetée par le curator — aucune écriture"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        // Note awaiting manual review — written with PendingReview status.
                        let section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        // Some(PendingReview) on the Pending branch (outcome_to_status SSOT).
                        let status = write_status
                            .expect("Pending → Some(PendingReview) par outcome_to_status");
                        let frontmatter = build_frontmatter(
                            &job.tenant_id,
                            section,
                            status,
                            &req,
                            &decisions.tags,
                        );

                        // Same pre-allocated ULID as the Admitted branch.
                        let note = vault
                            .write_note_with_id(frontmatter, req.body.clone(), prealloc_note_id)
                            .await
                            .context("vault.write_note_with_id curate pending")?;

                        outcome = "pending";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note mise en PendingReview (revue manuelle requise)"
                        );

                        // ── B5: wikilinks post-curate (Pending branch parity) ─────────────
                        //
                        // Same logic as the Admitted branch. A draft with wikilinks must have
                        // its links persisted just like an admitted note.
                        process_wikilinks_b5(self, &job.tenant_id, &note.id.to_string(), &req.body)
                            .await;

                        // Automatic chaining: enqueue embed_note after a successful curate write.
                        // Best-effort: an enqueue failure does not invalidate the curate.
                        // The backfill tool can retry if necessary.
                        let embed_payload = serde_json::json!({
                            "note_id": note.id.to_string(),
                            "body_text": note.body.markdown,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %note.id,
                                error = %e,
                                "chaînage embed_note enqueue échoué — backfill pourra re-tenter"
                            );
                        }
                    }
                }
            }

            // ── classify: re-route a note's section via the full curator cascade ──
            // Uses the complete pipeline (heuristic + LLM if configured) for
            // curate/classify consistency.
            "classify" => {
                let req: VaultClassifyRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultClassifyRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    "job classify — cascade curator complète (B3 alpha.15)"
                );

                // Read the existing note from the vault.
                let note_ulid = Ulid::from_string(&req.note_id)
                    .map_err(|e| anyhow::anyhow!("ULID invalide {}: {e}", req.note_id))?;
                let note_id = NoteId(note_ulid);
                let existing = vault
                    .read_note(note_id)
                    .await
                    .context("read_note pour classify")?;

                // Build the CuratorNote from the existing note.
                // section_hint = None: let the curator decide the canonical section.
                // Title is reconstructed from the body H1 if present, otherwise
                // from the current section (semantic proxy).
                let title_for_curator = gradatum_curator::extract_h1_title(&existing.body.markdown)
                    .unwrap_or_else(|| existing.frontmatter.section.as_str().to_string());

                let curator_note = CuratorNote {
                    id: req.note_id.clone(),
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

                tracing::debug!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    "classify curator processing — cascade complète"
                );

                let curate_outcome = curator.process(curator_note).await;

                // Status resolved via the single canonical mapping (worker SSOT parity).
                let write_status = gradatum_curator::outcome_to_status(&curate_outcome);

                match curate_outcome {
                    CurateOutcome::Admitted { decisions } => {
                        let new_section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let mut updated_fm = existing.frontmatter.clone();
                        updated_fm.section = new_section;

                        // Union curator tags with existing tags
                        // (Tag::new may return Err for malformed tags — silently skipped)
                        for tag_str in &decisions.tags {
                            if !updated_fm
                                .tags
                                .iter()
                                .any(|t| t.as_str() == tag_str.as_str())
                            {
                                if let Ok(t) = gradatum_core::tag::Tag::new(tag_str) {
                                    updated_fm.tags.push(t);
                                }
                            }
                        }

                        // NOTE: if this classify path is extended (write_note below),
                        // wire upsert_note_title (cf apalis_handlers.rs) otherwise notes.title
                        // will not be populated for reclassified notes.
                        vault
                            .write_note(updated_fm, existing.body.markdown.clone())
                            .await
                            .context("vault.write_note classify admitted")?;

                        outcome = "reclassified";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note reclassifiée par cascade curator (Admitted)"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        let new_section = section_from_str(&decisions.canonical_section)
                            .unwrap_or(Section::Reference);

                        let mut updated_fm = existing.frontmatter.clone();
                        updated_fm.section = new_section;
                        // Some(PendingReview) on the Pending branch (outcome_to_status SSOT).
                        updated_fm.status = write_status
                            .expect("Pending → Some(PendingReview) par outcome_to_status");

                        // NOTE: if this classify path is extended (write_note below),
                        // wire upsert_note_title (cf apalis_handlers.rs) otherwise notes.title
                        // will not be populated for notes moved to PendingReview.
                        vault
                            .write_note(updated_fm, existing.body.markdown.clone())
                            .await
                            .context("vault.write_note classify pending")?;

                        outcome = "classify_pending";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "note mise en PendingReview par classify (LLM incertain)"
                        );
                    }
                    CurateOutcome::Rejected { reason } => {
                        // Rejected = log warn + skip write — note unchanged in the vault.
                        // Non-fatal: the job is considered processed.
                        outcome = "classify_rejected";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "classify rejeté par le curator — note inchangée dans le vault"
                        );
                    }
                }
            }

            // ── downgrade: Live → Deprecated transition ────────────────────────
            "downgrade" => {
                let req: VaultDowngradeRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultDowngradeRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    reason = %req.reason,
                    "job downgrade — rétrogradation de la note"
                );

                // Lire la note existante
                let note_ulid = Ulid::from_string(&req.note_id)
                    .map_err(|e| anyhow::anyhow!("ULID invalide {}: {e}", req.note_id))?;
                let note_id = NoteId(note_ulid);
                let existing = vault
                    .read_note(note_id)
                    .await
                    .context("read_note pour downgrade")?;

                // Validate state machine: only Live can transition to Deprecated
                if !existing
                    .frontmatter
                    .status
                    .can_transition_to(NoteStatus::Deprecated)
                {
                    anyhow::bail!(
                        "transition invalide {:?} → Deprecated pour la note {} — seul Live est autorisé",
                        existing.frontmatter.status,
                        req.note_id
                    );
                }

                // Rewrite with Deprecated status + reason
                let mut downgraded_fm = existing.frontmatter.clone();
                downgraded_fm.status = NoteStatus::Deprecated;
                downgraded_fm.status_reason = Some(req.reason.clone());
                downgraded_fm.status_changed = Some(Utc::now());

                vault
                    .write_note(downgraded_fm, existing.body.markdown.clone())
                    .await
                    .context("vault.write_note downgrade")?;

                outcome = "deprecated";
                tracing::info!(job_id = job.id, "note rétrogradée vers Deprecated");
            }

            other => {
                anyhow::bail!("kind de job inconnu : {other:?}");
            }
        }

        // ── Audit emission ────────────────────────────────────────────────────
        let duration_ms = start.elapsed().as_millis() as i64;
        self.emit_audit(job, outcome, duration_ms).await;

        tracing::info!(
            job_id = job.id,
            kind = %job.kind,
            outcome = outcome,
            duration_ms = duration_ms,
            "job traité"
        );

        Ok(())
    }

    /// Processes an `embed_note` job: computes the embedding for the note body
    /// and persists it into `note_embeddings` via the SQLite index.
    ///
    /// ## Silent-skip behaviour
    ///
    /// - Embedder absent (`with_embedder` not called) → `Ok(())` without insert.
    /// - Index absent (`with_index` not called) → `Ok(())` without insert.
    /// - Empty `body_text` → `Ok(())` without computation.
    ///
    /// ## JSON payload
    ///
    /// ```json
    /// { "note_id": "<ULID>", "body_text": "<markdown>" }
    /// ```
    ///
    /// ## Truncation
    ///
    /// The body is truncated to 2 048 Unicode characters (≈ 8 KB UTF-8 worst-case)
    /// before calling the embedder to avoid model context overflows.
    async fn process_embed_note(&self, job: &LeasedJob) -> anyhow::Result<()> {
        let embedder = match &self.embedder {
            Some(e) => e,
            None => {
                tracing::info!(job_id = job.id, "embed_note skipped — embedder absent");
                return Ok(());
            }
        };
        let index = match &self.index {
            Some(i) => i,
            None => {
                tracing::info!(job_id = job.id, "embed_note skipped — index absent");
                return Ok(());
            }
        };

        let payload: serde_json::Value =
            serde_json::from_slice(&job.payload).context("embed_note: parse payload JSON")?;

        let note_id_str = payload
            .get("note_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("embed_note: payload manque 'note_id'"))?;

        let body_text = payload
            .get("body_text")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if body_text.is_empty() {
            tracing::info!(
                job_id = job.id,
                note_id = %note_id_str,
                "embed_note skipped — body vide"
            );
            return Ok(());
        }

        // Truncate to 2 048 Unicode characters (UTF-8-safe via char_indices).
        // Avoids model context overflows without arbitrary byte slicing.
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
            .map_err(|e| anyhow::anyhow!("embed_note embed: {e}"))?;

        let note_ulid = Ulid::from_string(note_id_str)
            .map_err(|e| anyhow::anyhow!("embed_note: ULID invalide '{note_id_str}': {e}"))?;
        let note_id = NoteId(note_ulid);

        index
            .insert_note_embedding(&note_id, embedder.embedder_id(), embedder.dim(), &vec)
            .await
            .map_err(|e| anyhow::anyhow!("embed_note insert_note_embedding: {e}"))?;

        tracing::info!(
            job_id = job.id,
            note_id = %note_id_str,
            embedder_id = embedder.embedder_id(),
            dim = embedder.dim(),
            "embed_note done"
        );

        Ok(())
    }

    /// Emits an audit event for a processed job.
    ///
    /// Audit errors are logged without propagating — the job is already processed.
    async fn emit_audit(&self, job: &LeasedJob, outcome: &str, duration_ms: i64) {
        if let Some(audit) = &self.audit {
            let event = HttpAuditEvent {
                ts: Utc::now(),
                event: format!("worker_{}", job.kind),
                actor: HttpAuditActor {
                    kid: WORKER_SYSTEM_KID.into(),
                    sub: "gradatum-worker".into(),
                    aud: "gradatum".into(),
                },
                tenant_id: job.tenant_id.clone(),
                locus: format!("{}/{}", job.tenant_id, job.kind),
                note_id: None,
                content_hash: None,
                outcome: outcome.into(),
                curator: Some(serde_json::json!({ "duration_ms": duration_ms })),
                request_id: format!("job-{}", job.id),
            };
            // Audit errors are logged without propagating — the job is already processed.
            if let Err(e) = audit.record(event).await {
                tracing::warn!(
                    job_id = job.id,
                    error = %e,
                    "échec écriture audit — le job est quand même marqué complet"
                );
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Converts a kebab-case section string into a `Section` enum via `serde_json`.
///
/// Returns `None` if the string is not a valid canonical section.
/// The caller must provide a fallback (typically `Section::Reference`).
fn section_from_str(s: &str) -> Option<Section> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<Section>(&json_str).ok()
}

/// Builds a `Frontmatter` from a `VaultWriteRequest` and curator decisions.
///
/// ## Invariants
///
/// - `vault_id` = current tenant (mono-tenant).
/// - `created` = `Utc::now()`.
/// - `tags` = union of hint tags and curator tags (deduplicated).
/// - `author` = `request.author` if provided.
fn build_frontmatter(
    tenant_id: &str,
    section: Section,
    status: NoteStatus,
    req: &VaultWriteRequest,
    curator_tags: &[String],
) -> Frontmatter {
    // Union of request tags and curator tags (order: request first, curator second)
    let mut all_tags: Vec<String> = req.tags.clone();
    for t in curator_tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }

    // Validated tags — malformed tags are silently dropped (defence in depth)
    let tags: SmallVec<[gradatum_core::tag::Tag; 4]> = all_tags
        .iter()
        .filter_map(|t| gradatum_core::tag::Tag::new(t.clone()).ok())
        .collect();

    let author = req
        .author
        .as_deref()
        .map(gradatum_core::author::AuthorRef::system);

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
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

// ── Wikilink post-curate resolution ──────────────────────────────────────────

/// Extracts `[[...]]` wikilinks from the body, resolves them via `idx.title_lookup`,
/// and persists them into `note_links` via `idx.upsert_link`.
///
/// **Strictly non-fatal**: any failure in extraction, `title_lookup`, or `upsert_link`
/// never propagates — only logged (`warn!`/`debug!`). The note is already persisted
/// at this point; a wikilink failure must NEVER cause the job to retry.
///
/// **Idempotence**: `upsert_link` uses `INSERT OR IGNORE` on the SQLite side — a
/// duplicate (same src/dst pair on the same vault) is silently ignored.
///
/// **Behaviour when `index` is absent**: silent skip (worker started without
/// `with_index` — e.g. tests started without an index). Wikilinks are not
/// extracted in that case.
///
/// **Behaviour when a target note does not exist** (`title_lookup` returns `None`):
/// logs `debug` and skips — the wikilink remains unresolved. Retroactive resolution
/// on subsequent creation of the target note is not performed.
async fn process_wikilinks_b5(
    dispatcher: &Dispatcher,
    tenant_id: &str,
    src_note_id: &str,
    body: &str,
) {
    let Some(idx) = dispatcher.index.as_ref() else {
        tracing::debug!(
            note_id = %src_note_id,
            "B5 skip: dispatcher sans index injecté (test historique ?)"
        );
        return;
    };

    let wikilinks = gradatum_curator::wikilinks::extract_wikilinks(body);
    if wikilinks.is_empty() {
        return;
    }

    // Parallel title_lookup via tokio::task::JoinSet.
    //
    // All N lookups are spawned concurrently — only internal SQLite mutex contention
    // within SqliteIndex serialises them, without inter-task delays.
    // `upsert_link` calls remain sequential: the SQLite write lock does not allow
    // parallelising inserts without notable contention (N ≤ 10 in practice).
    let mut join_set = tokio::task::JoinSet::new();

    for target_title in &wikilinks {
        // Clone required: JoinSet::spawn requires 'static — Arc<dyn Index> and
        // tenant_id String are the only two additional allocations (N ≤ 10).
        let idx_arc = Arc::clone(idx);
        let tenant = tenant_id.to_string();
        let title = target_title.clone();
        join_set.spawn(async move {
            let result = idx_arc.title_lookup(&tenant, &title).await;
            (title, result)
        });
    }

    // Collect lookup results in completion order — no ordering guarantee with JoinSet.
    // upsert_link calls remain sequential.
    let mut lookup_results = Vec::with_capacity(wikilinks.len());
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(pair) => lookup_results.push(pair),
            Err(e) => {
                // Panic in a lookup task — non-fatal, log warn and skip.
                tracing::warn!(err = %e, "B5 title_lookup task panicked — wikilink ignoré");
            }
        }
    }

    for (target_title, lookup_result) in lookup_results {
        match lookup_result {
            Ok(Some(dst_id)) => {
                if let Err(e) = idx.upsert_link(tenant_id, src_note_id, &dst_id).await {
                    tracing::warn!(
                        err = %e,
                        src = %src_note_id,
                        dst = %dst_id,
                        "B5 upsert_link failed — non-fatal"
                    );
                } else {
                    tracing::debug!(
                        src = %src_note_id,
                        dst = %dst_id,
                        target = %target_title,
                        "B5 wikilink persisté"
                    );
                }
            }
            Ok(None) => {
                tracing::debug!(
                    target = %target_title,
                    "B5 wikilink non résolu — note cible absente (caveat C3 Phase 3)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    target = %target_title,
                    "B5 title_lookup failed — wikilink ignoré (non-fatal)"
                );
            }
        }
    }
}
