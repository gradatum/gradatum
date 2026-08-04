//! Job dispatcher: poll queue → curator cascade → server persist → audit.
//!
//! This module is retained for compatibility with existing integration tests.
//! The active binary uses the Apalis Monitor (`monitor.rs`) — Dispatcher not active.
//!
//! ## Architecture (post worker-flip)
//!
//! All vault/index mutations go through [`InternalClient`] instead of direct
//! `Vault`/`SqliteIndex` access.  The `Dispatcher` now holds an `Arc<dyn InternalClient>`.
//!
//! `process_job` handles 3 job kinds:
//! - `curate`    : decode VaultWriteRequest → CuratorPipeline.process → InternalClient.persist_curated
//! - `classify`  : decode VaultClassifyRequest → client.get_note → CuratorProcess.process → client.persist_curated
//! - `downgrade` : decode VaultDowngradeRequest → client.get_note → state machine → client.persist_curated
//!
//! ## Guarantees
//!
//! - `run_once` is idempotent: returns `Ok(false)` if the queue is empty.
//! - Processing errors are logged and passed to `Queue::fail` — no silent crash.
//! - The job is `complete`-d only if `process_job` returns `Ok(())`.
//! - `AuditSink` is optional: if absent, events are logged but not persisted.
//!
//! ## Invariant
//!
//! No direct calls to `Vault::write_note_with_id`, `Index::insert_note_embedding`,
//! `Index::upsert_link`, or `Vault::open` in this module — all writes go via
//! `InternalClient`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use chrono::Utc;
use gradatum_core::audit::http::{AuditSink, HttpAuditActor, HttpAuditEvent};
use gradatum_core::status::NoteStatus;
use gradatum_curator::{CurateOutcome, CuratorProcess, Note as CuratorNote};
use gradatum_dto::{PersistCuratedRequest, PersistEmbeddingRequest};
use gradatum_embed::Embedder;
use gradatum_queue::{LeasedJob, NewJob, Queue, SqliteQueue};
use serde::{Deserialize, Serialize};
use tracing::instrument;
use ulid::Ulid;

use crate::internal_client::InternalClient;
use crate::wikilinks::resolve_wikilinks_via_client;

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
/// 1. `title`           `String`
/// 2. `body`            `String`
/// 3. `author`          `Option<String>`
/// 4. `tags`            `Vec<String>`
/// 5. `section_hint`    `Option<String>`
/// 6. `tenant_id`       `Option<TenantId>`  (A1 — omitted client-side, `Some(_)` at enqueue)
/// 7. `expected_sha256` `Option<String>`
/// 8. `note_id`         `Option<String>`   ← pre-allocated ULID
use gradatum_dto::VaultWriteRequest;

/// `vault_classify` request decoded from the queue bincode payload.
///
/// Lot A1: `tenant_id` is an `Option<String>` (positional mirror of
/// `gradatum_dto::VaultClassifyRequest.tenant_id: Option<TenantId>`, transparent) —
/// the encoder always sets `Some(_)` (the effective tenant is resolved server-side before
/// enqueue), so `skip_serializing_if` never fires on the wire (bincode decoding stays aligned).
#[derive(Debug, Serialize, Deserialize)]
struct VaultClassifyRequest {
    note_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
}

/// `vault_downgrade` request decoded from the queue bincode payload.
#[derive(Debug, Serialize, Deserialize)]
struct VaultDowngradeRequest {
    note_id: String,
    reason: String,
    #[serde(default)]
    replaced_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
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
/// All vault/index mutations go through the injected [`InternalClient`].
///
/// Built via the builder pattern:
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use gradatum_queue::SqliteQueue;
/// # use gradatum_worker::dispatch::{Dispatcher, NoopAuditSink};
/// # use gradatum_worker::internal_client::InternalClient;
/// # async fn ex(queue: Arc<SqliteQueue>, client: Arc<dyn InternalClient>, curator: Arc<gradatum_curator::CuratorPipeline>) {
/// let dispatcher = Dispatcher::new(queue)
///     .with_client(client)
///     .with_curator(curator)
///     .with_audit(Arc::new(NoopAuditSink));
/// # }
/// ```
pub struct Dispatcher {
    queue: Arc<SqliteQueue>,
    /// HTTP client to the server internal API — routes all vault/index mutations.
    client: Option<Arc<dyn InternalClient>>,
    /// Injectable curation pipeline (trait object).
    ///
    /// Accepts `CuratorPipeline` (production) or a mock for tests.
    curator: Option<Arc<dyn CuratorProcess>>,
    audit: Option<Arc<dyn AuditSink>>,
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
            client: None,
            curator: None,
            audit: None,
            embedder: None,
        }
    }

    /// Injects the internal client for all vault/index persistence.
    ///
    /// Routes `curate`, `classify`, `downgrade`, and `embed_note` writes
    /// through the server's `/internal/v1/*` endpoints.
    #[must_use]
    pub fn with_client(mut self, client: Arc<dyn InternalClient>) -> Self {
        self.client = Some(client);
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
                    "job failed — recording for retry or dead-letter"
                );
                self.queue.fail(job.id, &e.to_string()).await?;
            }
        }

        Ok(true)
    }

    /// Processes a leased job — full curator + client persist + audit cascade.
    ///
    /// ## Supported kinds
    ///
    /// - `curate`    : heuristic admission + note persistence via InternalClient.
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

        // embed_note is handled before accessing client/curator (optional for this kind).
        if job.kind.as_str() == "embed_note" {
            self.process_embed_note(job).await?;
            let duration_ms = start.elapsed().as_millis() as i64;
            self.emit_audit(job, "ok", duration_ms).await;
            tracing::info!(
                job_id = job.id,
                kind = %job.kind,
                outcome = "ok",
                duration_ms = duration_ms,
                "job processed"
            );
            return Ok(());
        }

        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("client not configured — call with_client"))?;
        let curator = self
            .curator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("curator not configured — call with_curator"))?;

        match job.kind.as_str() {
            // ── curate : novelty + routing + tags → client.persist_curated ───────
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
                // falls back to a fresh ULID if absent or invalid.
                let prealloc_note_id: String = req
                    .note_id
                    .as_deref()
                    .and_then(|s| ulid::Ulid::from_string(s).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| Ulid::new().to_string());

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
                        let status =
                            write_status.expect("Admitted → Some(Live) by outcome_to_status");

                        // ── B5: resolve wikilinks BEFORE persist_curated ──────────
                        //
                        // Wikilinks are resolved via client.title_lookup and packed into
                        // PersistCuratedRequest.links so the server handles upsert_link
                        // atomically with the note write.
                        let resolved_links = resolve_wikilinks_via_client(
                            client,
                            &job.tenant_id,
                            &prealloc_note_id,
                            &req.body,
                        )
                        .await;

                        let persist_req = PersistCuratedRequest {
                            note_id: prealloc_note_id.clone(),
                            tenant_id: job.tenant_id.clone().into(),
                            title: req.title.clone(),
                            body: req.body.clone(),
                            section: decisions.canonical_section.clone(),
                            tags: build_merged_tags(&req.tags, &decisions.tags),
                            author: req.author.clone(),
                            status: status_to_kebab(status),
                            trust: None,
                            expected_sha256: req.expected_sha256.clone(),
                            temporal: None,
                            links: resolved_links,
                            provenance: None,
                            // Legacy dispatch path — not instrumented (F-66).
                            curator_decision: None,
                            target_vault: None,
                        };

                        client
                            .persist_curated(&persist_req)
                            .await
                            .context("client.persist_curated curate admitted")?;

                        outcome = "admitted";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note admitted and persisted via InternalClient"
                        );

                        // Automatic chaining: enqueue embed_note after a successful curate write.
                        // Best-effort: an enqueue failure does not invalidate the curate.
                        let embed_payload = serde_json::json!({
                            "note_id": prealloc_note_id,
                            "body_text": req.body,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %prealloc_note_id,
                                error = %e,
                                "embed_note enqueue chaining failed — backfill may retry"
                            );
                        }
                    }
                    CurateOutcome::Rejected { reason } => {
                        // Note rejected — no write to the vault
                        outcome = "rejected";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note rejected by the curator — no write"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        let status = write_status
                            .expect("Pending → Some(PendingReview) by outcome_to_status");

                        // ── B5: resolve wikilinks BEFORE persist_curated (Pending parity) ──
                        let resolved_links = resolve_wikilinks_via_client(
                            client,
                            &job.tenant_id,
                            &prealloc_note_id,
                            &req.body,
                        )
                        .await;

                        let persist_req = PersistCuratedRequest {
                            note_id: prealloc_note_id.clone(),
                            tenant_id: job.tenant_id.clone().into(),
                            title: req.title.clone(),
                            body: req.body.clone(),
                            section: decisions.canonical_section.clone(),
                            tags: build_merged_tags(&req.tags, &decisions.tags),
                            author: req.author.clone(),
                            status: status_to_kebab(status),
                            trust: None,
                            expected_sha256: req.expected_sha256.clone(),
                            temporal: None,
                            links: resolved_links,
                            provenance: None,
                            // Legacy dispatch path — not instrumented (F-66).
                            curator_decision: None,
                            target_vault: None,
                        };

                        client
                            .persist_curated(&persist_req)
                            .await
                            .context("client.persist_curated curate pending")?;

                        outcome = "pending";
                        tracing::info!(
                            job_id = job.id,
                            reason = %reason,
                            "note moved to PendingReview via InternalClient (manual review required)"
                        );

                        // Automatic chaining: enqueue embed_note after a successful curate write.
                        let embed_payload = serde_json::json!({
                            "note_id": prealloc_note_id,
                            "body_text": req.body,
                        });
                        let new_embed_job = NewJob {
                            tenant_id: job.tenant_id.clone(),
                            kind: "embed_note".to_string(),
                            payload: serde_json::to_vec(&embed_payload).unwrap_or_default(),
                            max_attempts: 3,
                        };
                        if let Err(e) = self.queue.enqueue(new_embed_job).await {
                            tracing::warn!(
                                note_id = %prealloc_note_id,
                                error = %e,
                                "embed_note enqueue chaining failed — backfill may retry"
                            );
                        }
                    }
                }
            }

            // ── classify: re-route a note's section via the full curator cascade ──
            "classify" => {
                let req: VaultClassifyRequest =
                    bincode::serde::decode_from_slice(&job.payload, bincode::config::standard())
                        .context("decode VaultClassifyRequest bincode")?
                        .0;

                tracing::info!(
                    job_id = job.id,
                    note_id = %req.note_id,
                    "job classify — full curator cascade"
                );

                // Read the existing note via the internal client.
                let existing = client
                    .get_note(&job.tenant_id, &req.note_id)
                    .await
                    .context("get_note for classify")?;

                // Build the CuratorNote from the existing note.
                let title_for_curator = gradatum_curator::extract_h1_title(&existing.body)
                    .unwrap_or_else(|| existing.section.clone());

                let curator_note = CuratorNote {
                    id: req.note_id.clone(),
                    title: title_for_curator,
                    body: existing.body.clone(),
                    tags_hint: existing.tags.clone(),
                    section_hint: None,
                };

                let curate_outcome = curator.process(curator_note).await;
                let write_status = gradatum_curator::outcome_to_status(&curate_outcome);

                match curate_outcome {
                    CurateOutcome::Admitted { decisions } => {
                        let status =
                            write_status.expect("Admitted → Some(Live) by outcome_to_status");

                        // Merge curator tags with existing tags
                        let merged_tags = build_merged_tags(&existing.tags, &decisions.tags);

                        let persist_req = PersistCuratedRequest {
                            note_id: req.note_id.clone(),
                            tenant_id: job.tenant_id.clone().into(),
                            title: gradatum_curator::extract_h1_title(&existing.body)
                                .unwrap_or_else(|| existing.section.clone()),
                            body: existing.body.clone(),
                            section: decisions.canonical_section.clone(),
                            tags: merged_tags,
                            author: None,
                            status: status_to_kebab(status),
                            trust: None,
                            expected_sha256: None,
                            temporal: None,
                            links: vec![],
                            provenance: None,
                            // Legacy dispatch path — not instrumented (F-66).
                            curator_decision: None,
                            target_vault: None,
                        };

                        client
                            .persist_curated(&persist_req)
                            .await
                            .context("client.persist_curated classify admitted")?;

                        outcome = "reclassified";
                        tracing::info!(
                            job_id = job.id,
                            section = %decisions.canonical_section,
                            "note reclassified via InternalClient (Admitted)"
                        );
                    }
                    CurateOutcome::Pending { decisions, reason } => {
                        let status = write_status
                            .expect("Pending → Some(PendingReview) by outcome_to_status");

                        let merged_tags = build_merged_tags(&existing.tags, &decisions.tags);

                        let persist_req = PersistCuratedRequest {
                            note_id: req.note_id.clone(),
                            tenant_id: job.tenant_id.clone().into(),
                            title: gradatum_curator::extract_h1_title(&existing.body)
                                .unwrap_or_else(|| existing.section.clone()),
                            body: existing.body.clone(),
                            section: decisions.canonical_section.clone(),
                            tags: merged_tags,
                            author: None,
                            status: status_to_kebab(status),
                            trust: None,
                            expected_sha256: None,
                            temporal: None,
                            links: vec![],
                            provenance: None,
                            // Legacy dispatch path — not instrumented (F-66).
                            curator_decision: None,
                            target_vault: None,
                        };

                        client
                            .persist_curated(&persist_req)
                            .await
                            .context("client.persist_curated classify pending")?;

                        outcome = "classify_pending";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "note mise en PendingReview via InternalClient (classify)"
                        );
                    }
                    CurateOutcome::Rejected { reason } => {
                        outcome = "classify_rejected";
                        tracing::warn!(
                            job_id = job.id,
                            reason = %reason,
                            "classify rejected — note unchanged"
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
                    "job downgrade — note downgrade"
                );

                // Read the existing note via the internal client.
                let existing = client
                    .get_note(&job.tenant_id, &req.note_id)
                    .await
                    .context("get_note for downgrade")?;

                // Parse existing status and validate state machine.
                let existing_status = parse_status_kebab(&existing.status).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown status '{}' for note {} (downgrade)",
                        existing.status,
                        req.note_id
                    )
                })?;

                if !existing_status.can_transition_to(NoteStatus::Deprecated) {
                    anyhow::bail!(
                        "invalid transition {:?} → Deprecated for note {} — only Live is allowed",
                        existing_status,
                        req.note_id
                    );
                }

                let persist_req = PersistCuratedRequest {
                    note_id: req.note_id.clone(),
                    tenant_id: job.tenant_id.clone().into(),
                    title: gradatum_curator::extract_h1_title(&existing.body)
                        .unwrap_or_else(|| existing.section.clone()),
                    body: existing.body.clone(),
                    section: existing.section.clone(),
                    tags: existing.tags.clone(),
                    author: None,
                    status: "deprecated".to_string(),
                    trust: None,
                    expected_sha256: None,
                    temporal: None,
                    links: vec![],
                    provenance: None,
                    // Legacy dispatch path — not instrumented (F-66).
                    curator_decision: None,
                    target_vault: None,
                };

                client
                    .persist_curated(&persist_req)
                    .await
                    .context("client.persist_curated downgrade")?;

                outcome = "deprecated";
                tracing::info!(
                    job_id = job.id,
                    "note downgraded to Deprecated via InternalClient"
                );
            }

            other => {
                anyhow::bail!("unknown job kind: {other:?}");
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
            "job processed"
        );

        Ok(())
    }

    /// Processes an `embed_note` job: computes the embedding for the note body
    /// and persists it into `note_embeddings` via InternalClient.
    ///
    /// ## Silent-skip behaviour
    ///
    /// - Embedder absent (`with_embedder` not called) → `Ok(())` without insert.
    /// - Client absent (`with_client` not called) → `Ok(())` without insert.
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
        let client = match &self.client {
            Some(c) => c,
            None => {
                tracing::info!(job_id = job.id, "embed_note skipped — client absent");
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
                "embed_note skipped — empty body"
            );
            return Ok(());
        }

        // Truncate to 2 048 Unicode characters (UTF-8-safe via char_indices).
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

        // Validate ULID format before calling persist_embedding.
        Ulid::from_string(note_id_str)
            .map_err(|e| anyhow::anyhow!("embed_note: invalid ULID '{note_id_str}': {e}"))?;

        let persist_req = PersistEmbeddingRequest {
            note_id: note_id_str.to_string(),
            embedder_id: embedder.embedder_id().to_string(),
            dim: embedder.dim(),
            vector: vec,
            // C4-1e Slice B3 (MIGRATE) : le worker émet le vault réel du job.
            // OFF → job.tenant_id == "main" (single-owner DB) = byte-identical.
            vault_id: Some(job.tenant_id.clone().into()),
        };

        client
            .persist_embedding(&persist_req)
            .await
            .map_err(|e| anyhow::anyhow!("embed_note persist_embedding: {e}"))?;

        tracing::info!(
            job_id = job.id,
            note_id = %note_id_str,
            embedder_id = embedder.embedder_id(),
            dim = embedder.dim(),
            "embed_note done via InternalClient"
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
                    jti: None,
                },
                tenant_id: job.tenant_id.clone(),
                locus: format!("{}/{}", job.tenant_id, job.kind),
                note_id: None,
                content_hash: None,
                outcome: outcome.into(),
                curator: Some(serde_json::json!({ "duration_ms": duration_ms })),
                request_id: format!("job-{}", job.id),
            };
            if let Err(e) = audit.record(event).await {
                tracing::warn!(
                    job_id = job.id,
                    error = %e,
                    "audit write failed — the job is still marked complete"
                );
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Builds a merged tag list from request tags and curator tags (deduplicated).
///
/// Request tags appear first, curator tags are appended if not already present.
fn build_merged_tags(base_tags: &[String], curator_tags: &[String]) -> Vec<String> {
    let mut all_tags: Vec<String> = base_tags.to_vec();
    for t in curator_tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    all_tags
}

/// Converts a `NoteStatus` to its serde kebab-case string representation.
fn status_to_kebab(status: NoteStatus) -> String {
    // NoteStatus implements Display via serde_kebab_repr()
    status.to_string()
}

/// Parses a kebab-case status string back to `NoteStatus`.
///
/// Returns `None` for unknown/unrecognised strings.
fn parse_status_kebab(s: &str) -> Option<NoteStatus> {
    let json_str = format!("\"{}\"", s);
    serde_json::from_str::<NoteStatus>(&json_str).ok()
}

// ── Wikilink resolution — délégué à crate::wikilinks::resolve_wikilinks_via_client ──
