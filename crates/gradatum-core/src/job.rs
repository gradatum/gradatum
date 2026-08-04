//! Primitive types for the job system.
//!
//! Defines the canonical L0 types used across the entire job infrastructure
//! layer. Lives in `gradatum-core` so that higher-level crates
//! (`gradatum-queue`, `gradatum-worker`, `gradatum-db-sqlite`) can depend on
//! it without introducing circular dependencies.
//!
//! # Architectural layers
//!
//! ```text
//! gradatum-core (L0) — Job, JobRecord, QueueStore, QueueEvent, DryRunAware
//!     ↑
//! gradatum-db-sqlite (L2) — SqliteQueueStore impl QueueStore
//!     ↑
//! gradatum-queue (L3)     — GradatumQueue facade (Apalis backend)
//!     ↑
//! gradatum-worker (L4)    — Apalis handlers + orchestration
//! ```
//!
//! # Bincode order — IMMUTABLE
//!
//! [`Job`] variants are encoded by position by `bincode`.
//! **Never reorder existing variants.** New variants must be appended at the
//! end. Violation causes silent corruption of jobs stored in the database.

#![allow(dead_code)] // types consommés progressivement selon le pipeline

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

// ─────────────────────────────────────────────────────────────────────────────
// VaultScope — alias canonique
// ─────────────────────────────────────────────────────────────────────────────

/// Scope of a job or vault request.
///
/// `VaultScope` is a type alias for [`JobScope`]. Both names coexist to
/// preserve consistency between existing vault code (`VaultScope`) and the
/// job system (`JobScope`).
pub type VaultScope = JobScope;

// ─────────────────────────────────────────────────────────────────────────────
// Job enum — ordre bincode figé (v55)
// ─────────────────────────────────────────────────────────────────────────────

/// Job type submitted to the queue.
///
/// # Bincode order — IMMUTABLE
///
/// Variants are encoded by position (0-20). Never reorder.
/// New variants must be appended at the end.
///
/// Position comments `(N)` are informational — bincode encodes by Rust
/// declaration order, not by explicit numeric value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum Job {
    // System jobs (0-12) — automatic
    /// ReAct agent loop — bincode position 0.
    Agent,
    /// `[[pipelines]]` step — bincode position 1.
    Pipeline,
    /// Web crawler — bincode position 2.
    Collect,
    /// Semantic distillation — bincode position 3.
    ///
    /// The variant changed from unit to tuple; bincode position 3 is
    /// UNCHANGED (preserves bincode/serde stability of the `kind` column).
    /// Any `Distill` jobs in the queue must be absent before deployment.
    ///
    /// Only `DistillMode::Semantic` is implemented. `Learn`/`Peer`/`Rationale`
    /// modes require a complete event-log and are deferred (YAGNI).
    Distill(DistillSource),
    /// Vault backup — bincode position 4.
    Backup,
    /// Lifecycle purge (removes `Garbage` notes) — bincode position 5.
    ///
    /// The variant changed from unit to tuple; bincode position 5 is
    /// UNCHANGED. Any `Purge` jobs in the queue must be absent before deployment.
    Purge(PurgeSpec),
    /// Full-text and/or vector re-index — bincode position 6.
    ReIndex(ReIndexMode),
    /// Content summarisation — bincode position 7.
    Summarize,
    /// Memory validation and healing — bincode position 8.
    Validate(ValidateSpec),
    /// Vault scoring and deduplication — bincode position 9.
    Audit,
    /// Mental model consolidation — bincode position 10.
    Consolidate,
    /// Inbox classification and curation — bincode position 11.
    Curate(CurateSpec),
    /// Semantic forget of notes — bincode position 12.
    ///
    /// The variant changed from unit to tuple; bincode position 12 is
    /// UNCHANGED. Any `Forget` jobs in the queue must be absent before deployment.
    Forget(ForgetSpec),

    // Human jobs (13-16) — JobClass::Human required
    /// Validates or rejects a batch of `needs-review` notes — bincode position 13.
    Review,
    /// Manually classifies an unresolved `inbox/` note — bincode position 14.
    Classify,
    /// Merges two duplicate notes (after `Job::Audit`) — bincode position 15.
    Merge,
    /// Enriches metadata for a batch of notes — bincode position 16.
    Annotate,

    // Added at the end to preserve bincode order
    /// Predecessor vault import · `JobClass::Human` — bincode position 17.
    Migrate(MigrateSource),
    /// CSV/PDF/JSON export from notes · `JobClass::Agent|Human` — bincode position 18.
    Export(ExportSource),
    /// Cascade external notification · `JobClass::System` — bincode position 19.
    Notify(NotifySource),
    /// Document ingestion via queue · `JobClass::Agent|Human` — bincode position 20.
    Ingest(IngestSource),

    // Embed appended at the end to preserve bincode order of positions 0-20
    /// Vector embedding generation — bincode position 21.
    Embed(EmbedSpec),
}

// ─────────────────────────────────────────────────────────────────────────────
// ReIndexMode
// ─────────────────────────────────────────────────────────────────────────────

/// Re-index mode for [`Job::ReIndex`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReIndexMode {
    /// Rebuilds FTS5 index (default).
    FtsOnly,
    /// Recomputes all embeddings (e.g. after a model migration).
    VectorsOnly,
    /// `FtsOnly` + `VectorsOnly`.
    Full,
    /// Embeds only notes that have no vector yet (new notes).
    ///
    /// Faster than `VectorsOnly` on a large active vault.
    MissingOnly,
}

// ─────────────────────────────────────────────────────────────────────────────
// Source structs — variants actifs
// ─────────────────────────────────────────────────────────────────────────────

/// Specification for a curation job (`Job::Curate`).
///
/// Handles `inbox/` classification, scoring, and metadata updates.
///
/// ## Optional write fields
///
/// `title`, `body`, `author`, `tags`, and `section_hint` are carried directly
/// in `CurateSpec` for the `vault_write → job_store` path. They are optional
/// (`#[serde(default)]`) and backward-compatible with existing `JobRecord`s
/// (absent JSON field → `None`/`[]`).
///
/// For `Job::Curate` jobs triggered by `vault_write`:
/// - `title` + `body` are `Some` — carry the content to create in the vault.
///
/// For `Job::Curate` jobs triggered by reclassification:
/// - `title` + `body` are `None` — the note already exists in the vault.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurateSpec {
    /// ULID identifier of the note to curate.
    pub note_id: Ulid,
    /// Owning tenant identifier (default: `"main"`).
    #[serde(default = "default_tenant_main")]
    pub tenant_id: String,
    /// Note title (present for `vault_write`, absent for reclassification).
    #[serde(default)]
    pub title: Option<String>,
    /// Markdown body of the note (present for `vault_write`).
    #[serde(default)]
    pub body: Option<String>,
    /// Note author (optional).
    #[serde(default)]
    pub author: Option<String>,
    /// Initial tags (optional — the curator may add more).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Suggested section (optional — the curator may override).
    #[serde(default)]
    pub section_hint: Option<String>,
    /// Expected SHA-256 of the note being overwritten (optional — CREATE-vs-RMW discriminator).
    ///
    /// **Active compare-and-swap.** On the RMW branch (`Some`), the worker forwards this
    /// hash to the internal persist API, which performs a compare-and-swap against the note's
    /// stored content. A `Some` holding a *stale* hash makes the write FAIL: the job terminates
    /// in `JobStatus::Conflict`, the note is left INTACT (no overwrite), and the conflict payload
    /// carries the current hash so the caller can resolve it. Only a hash matching the current
    /// content lets the in-place write proceed. `None` selects the CREATE branch: a freshly
    /// pre-allocated ULID is written unconditionally (never conflicts).
    ///
    /// The value also drives temporal-anchor handling: `Some` means "in-place update", so an
    /// absent `occurred_at` leaves the existing anchor untouched instead of re-stamping it with
    /// the current time.
    ///
    /// Serialised in bincode as `Option<[u8; 32]>` (32 fixed bytes or `None`).
    /// Payload overhead: +33 bytes (1 bincode discriminant + 32-byte hash) — negligible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<[u8; 32]>,
    /// Temporal anchor — event date of the note (ISO 8601 UTC or YYYY-MM-DD string).
    ///
    /// When `Some`, the curate worker inserts this value into `ExtraFields["occurred_at"]`
    /// and calls `resolve_temporal_anchor` to compute `anchor_ms + anchor_src = OccurredAt`.
    ///
    /// `None` → `anchor_src = Created` (backward-compatible: all existing jobs without
    /// this field deserialize with `None` via `#[serde(default)]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
}

fn default_tenant_main() -> String {
    "main".to_string()
}

/// Specification for an embedding job (`Job::Embed`).
///
/// Generates or regenerates the vector via `gradatum-embed::FallbackEmbedder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedSpec {
    /// ULID identifier of the note to embed.
    pub note_id: Ulid,
    /// Owning tenant identifier (default: `"main"`).
    pub tenant_id: String,
    /// Force regeneration even if a vector already exists.
    pub force_regenerate: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Source structs — nouveaux variants v59
// ─────────────────────────────────────────────────────────────────────────────

/// Source for [`Job::Migrate`] — predecessor vault → Gradatum import.
///
/// `JobClass::Human` only — irreversible operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrateSource {
    /// Path to the source vault.
    pub from_path: String,
    /// Migration mode.
    pub mode: MigrateMode,
    /// Conflict resolution strategy.
    pub conflict: ConflictStrategy,
    /// Simulate without writing — required on the first pass.
    pub dry_run: bool,
    /// Destination vault.
    pub target: VaultScope,
}

/// Migration mode for [`MigrateSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MigrateMode {
    /// Maps predecessor sections to `CognitiveCategory` + `ContentSection`.
    PredecessorV1,
    /// Import from another Gradatum vault.
    GradatumVault,
    /// Raw Markdown import without section mapping.
    RawMarkdown,
}

/// Conflict resolution strategy for [`MigrateSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictStrategy {
    /// Overwrites existing notes.
    Overwrite,
    /// Keeps existing notes, skips incoming duplicates.
    Skip,
    /// Appends `-imported` suffix on conflicts.
    Rename,
}

/// Source for [`Job::Export`] — generates a file from vault notes.
///
/// `JobClass::Agent | Human`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSource {
    /// Scope of notes to export.
    pub scope: VaultScope,
    /// Optional FTS filter (e.g. `"sections:decisions"`).
    pub filter: Option<String>,
    /// Export format.
    pub format: ExportFormat,
    /// OpenDAL destination path (e.g. `"exports/decisions-2026-05.pdf"`).
    pub target: String,
    /// Markdown template for rendering.
    pub template: Option<String>,
}

/// Export format for [`ExportSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportFormat {
    /// One row per note — title, locus, trust, date, sections.
    Csv,
    /// Markdown rendered to PDF (via pandoc or similar).
    Pdf,
    /// Full serialisation of `Document` + frontmatter.
    Json,
    /// All notes concatenated into a single `.md` file.
    Markdown,
    /// Archive of raw `.md` files.
    Zip,
}

/// Source for [`Job::Notify`] — cascade external notification.
///
/// `JobClass::System` — triggered via `await_jobs` `OnDone`/`OnFailed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifySource {
    /// Notification channel.
    pub channel: NotifyChannel,
    /// Message template with variables (e.g. `"Job {{job_kind}} done: {{notes_created}} notes"`).
    pub template: String,
    /// Job whose completion is being notified.
    pub job_ref: Option<Ulid>,
}

/// Notification channel for [`NotifySource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotifyChannel {
    /// Telegram notification.
    Telegram {
        /// Telegram chat identifier.
        chat_id: String,
    },
    /// Slack notification via webhook.
    Slack {
        /// Slack webhook URL.
        webhook_url: String,
    },
    /// Generic HTTP webhook notification.
    Webhook {
        /// Webhook URL.
        url: String,
        /// HTTP method (`POST` recommended).
        method: String,
    },
    /// NATS publication.
    Nats {
        /// Target NATS subject.
        subject: String,
    },
    /// Email notification.
    Email {
        /// Recipient email address.
        to: String,
    },
}

/// Source for [`Job::Ingest`] — document ingestion via queue.
///
/// `JobClass::Agent | Human`. Replaces `gradatum-admin vault import --file`
/// for large corpora (with progress tracking and retry support).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestSource {
    /// Input source to ingest.
    pub source: IngestInputSource,
    /// Destination vault.
    pub vault: String,
    /// Destination locus (e.g. `"rag/"`).
    pub locus: String,
    /// Ingestion strategy.
    pub strategy: IngestStrategy,
    /// Simulate without writing — legitimate exception (potentially large
    /// operation; human validation recommended first).
    pub dry_run: bool,
}

/// Input source for [`IngestSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestInputSource {
    /// OpenDAL file path.
    File {
        /// File path.
        path: String,
    },
    /// URL to fetch.
    Url {
        /// URL to ingest.
        url: String,
    },
    /// Batch of URLs.
    Urls {
        /// List of URLs to ingest.
        urls: Vec<String>,
    },
    /// Directory of files already on disk.
    Locus {
        /// Directory path.
        path: String,
    },
}

/// Ingestion strategy for [`IngestSource`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IngestStrategy {
    /// Auto-detects between `StructureGuided` and `SlidingWindow`.
    Auto,
    /// Skeleton tree + structure-guided chunking.
    ForceStructured,
    /// Sliding window even when headings are present.
    ForceSlidingWindow,
}

// ─────────────────────────────────────────────────────────────────────────────
// PurgeSpec + PurgeMode — Job::Purge (F-32C)
// ─────────────────────────────────────────────────────────────────────────────

/// Purge mode for [`PurgeSpec`].
///
/// Only `Lifecycle` is implemented: removes `Garbage` notes whose age in the
/// `Garbage` state (via `status_changed`) exceeds `grace_days`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PurgeMode {
    /// Removes notes with `status=Garbage` that have exceeded the grace period.
    ///
    /// Age is measured via the `status_changed` column (updated by `update_status`).
    Lifecycle,
}

/// Specification for a purge job (`Job::Purge`).
///
/// ## Dry-run first
///
/// `dry_run = true` is the safe default. In dry-run mode the handler lists
/// eligible notes and returns [`JobOutput::dry_run`] **without deleting anything**.
///
/// The real purge (`dry_run = false`) must be triggered explicitly.
///
/// ## Grace period
///
/// `grace_days`: minimum time a note must have been in the `Garbage` state before
/// deletion. Measured via the `status_changed` column of the SQLite index.
/// Default: `Some(30)`. `None` = deletes all `Garbage` notes immediately
/// (dangerous — expert CLI use only).
///
/// ## Cron schedule
///
/// The nightly purge cron schedule is INTENTIONALLY disabled (see config-full.toml).
/// Enabling it is a separate operator decision (prerequisite: a nightly backup
/// strategy must be configured).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgeSpec {
    /// Purge mode — only `Lifecycle` is implemented.
    pub mode: PurgeMode,
    /// Simulate without writing — default `true` (safe).
    ///
    /// This is a legitimate exception to the single `JobMode::DryRun` rule:
    /// purge is an irreversible, high-impact operation (permanent deletion).
    /// The double mechanism (`spec.dry_run` + `JobMode::DryRun`) ensures no
    /// write occurs if either flag is active.
    #[serde(default = "PurgeSpec::default_dry_run")]
    pub dry_run: bool,
    /// Minimum time in the `Garbage` state before deletion (days).
    ///
    /// Default: `Some(30)`. `None` = no grace period (expert CLI use only).
    #[serde(default = "PurgeSpec::default_grace_days")]
    pub grace_days: Option<u32>,
}

impl PurgeSpec {
    fn default_dry_run() -> bool {
        true
    }

    fn default_grace_days() -> Option<u32> {
        Some(30)
    }
}

impl Default for PurgeSpec {
    fn default() -> Self {
        Self {
            mode: PurgeMode::Lifecycle,
            dry_run: true,
            grace_days: Some(30),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ForgetSpec + ForgetScope — Job::Forget (F-44)
// ─────────────────────────────────────────────────────────────────────────────

/// Target resolution scope for a forget job (`Job::Forget`).
///
/// Three targeting modes:
/// - `Topic`: FTS resolution (full-text search) within an optional vault.
/// - `Locus`: locus prefix (vault directory) — LIKE-escaped by the worker.
/// - `Agent`: all notes from a given agent in the specified vaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ForgetScope {
    /// FTS-based resolution — notes matching `query` are targeted.
    ///
    /// `vault`: optional tenant (`None` → default vault `"main"`).
    /// `limit`: safe cap on the number of targeted notes (default 50, max 200).
    Topic {
        /// FTS query (e.g. `"secrets api-key"`).
        query: String,
        /// Target tenant (optional — `None` = `"main"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        vault: Option<String>,
        /// Cap on the number of targeted results (default 50).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Locus prefix resolution.
    ///
    /// `locus` is LIKE-escaped by the worker. Examples: `"inbox/"`, `"rag/corpus-x/"`.
    Locus {
        /// Target vault.
        vault: String,
        /// Locus prefix (e.g. `"inbox/old/"`) — matches all notes whose locus
        /// starts with this value.
        locus: String,
    },
    /// Agent-based resolution — all notes from `agent_id` in the specified vaults.
    ///
    /// Empty `vaults` → `"main"` vault only.
    Agent {
        /// Agent identifier (`author_id` column).
        agent_id: String,
        /// Target vault list (`[]` → `["main"]`).
        #[serde(default)]
        vaults: Vec<String>,
    },
}

/// Specification for a forget job (`Job::Forget`).
///
/// ## Dry-run first
///
/// `dry_run = true` is the default. In dry-run mode the handler lists target
/// ULIDs + exclusions (protected sections) and returns [`JobOutput::dry_run`]
/// **without any mutation**.
///
/// The real mutation (`dry_run = false`) additionally requires `confirm_ulids`
/// matching exactly the ULIDs from the preview (double confirmation).
///
/// ## Protected sections
///
/// Notes in the `AgentIssues` and `Council` sections are **always excluded**
/// from the batch, regardless of scope. They are reported in the preview without
/// blocking the job.
///
/// ## Non-destructive
///
/// Forget updates the YAML frontmatter (`forgotten=true`, `forgotten_at`,
/// `forgotten_by`) via the normal write path (traced CoW). No physical deletion.
/// Physical purge is reserved for `Job::Purge`.
///
/// ## Idempotence
///
/// A double forget is idempotent and **preserves the first forget's audit trail**: a note
/// already carrying `forgotten: true` is skipped, so `forgotten_at` and `forgotten_by` keep
/// the values of the FIRST forget — they are never re-stamped with the second job's actor
/// or timestamp. That trail is the only record of when the destructive act happened and who
/// ordered it; overwriting it would be the actual data loss.
///
/// The skip still repairs the index: if the index reports the note as live while the
/// frontmatter reports it as forgotten, the handler re-asserts the index mark alone
/// (`DocumentStore::reassert_forgotten`), leaving both audit columns untouched. Both
/// properties are kept — never one traded for the other.
///
/// This is a JOB-level invariant. One layer down, `DocumentStore::mark_forgotten` does
/// overwrite `forgotten_at`/`forgotten_by` on every call — but the skip above means it is
/// no longer reached for an already-forgotten note, so that behaviour never surfaces here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetSpec {
    /// Resolution scope — determines the target notes.
    pub scope: ForgetScope,
    /// Simulate without mutation — default `true` (safe).
    ///
    /// Legitimate exception to the single `JobMode::DryRun` rule: forget has a
    /// persistent effect on scoring. The double mechanism (`spec.dry_run` +
    /// `JobMode::DryRun`) ensures no mutation if either flag is active.
    #[serde(default = "ForgetSpec::default_dry_run")]
    pub dry_run: bool,
    /// Actor who triggered the forget (for auditability — `forgotten_by`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgotten_by: Option<String>,
    /// ULIDs confirmed from a prior preview (double confirmation).
    ///
    /// In real mode (`dry_run = false`): must match exactly the ULIDs returned by
    /// the preview. Mismatch → job error, no mutation. `None` is allowed only in
    /// dry-run mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm_ulids: Vec<String>,
}

impl ForgetSpec {
    fn default_dry_run() -> bool {
        true
    }
}

impl Default for ForgetSpec {
    fn default() -> Self {
        Self {
            scope: ForgetScope::Topic {
                query: String::new(),
                vault: None,
                limit: Some(50),
            },
            dry_run: true,
            forgotten_by: None,
            confirm_ulids: vec![],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DistillSource + DistillMode — Job::Distill (F-22)
// ─────────────────────────────────────────────────────────────────────────────

/// Distillation mode for [`DistillSource`].
///
/// Only `Semantic` is implemented: cosine clustering of non-`processed` notes
/// in the scope, followed by per-cluster synthesis into a `PendingReview` note.
///
/// `Learn`/`Peer`/`Rationale` modes require a complete event-log (threshold
/// ≥ 100 events) and are intentionally absent here (YAGNI). The
/// `#[non_exhaustive]` marker allows their future addition without breaking
/// exhaustive matching in external consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DistillMode {
    /// Semantic distillation: cosine clustering → per-cluster synthesis.
    Semantic,
}

/// Specification for a distillation job (`Job::Distill`).
///
/// ## Dry-run first
///
/// Distillation writes LLM-generated content to the vault. Like `Purge`/`Forget`,
/// `JobMode::DryRun` (in `JobSpec`) lists candidate clusters **without any
/// mutation**. The real synthesis requires `JobMode::Batch`.
///
/// ## Scope required in real mode
///
/// Clustering is O(n²) bounded by `batch_limit`. In real mode the scope must
/// be `Locus` or `Notes` — `JobScope::VaultWide` is **rejected outside dry-run**
/// by the handler (combinatorial explosion mitigation).
///
/// ## Cron schedule
///
/// The distillation cron schedule is INTENTIONALLY disabled (see config-full.toml).
/// Enabling it is a separate operator decision (automated LLM writes to the vault).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistillSource {
    /// Distillation mode — only `Semantic` is implemented.
    #[serde(default = "DistillSource::default_mode")]
    pub mode: DistillMode,
    /// Scope of candidate notes to distill.
    ///
    /// In real mode: must target a `Locus` or a set of `Notes`
    /// (`VaultWide` is rejected outside dry-run).
    pub scope: JobScope,
    /// Optional time window — consider only notes created/modified within this
    /// duration. `None` = no time filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Duration>,
    /// Maximum number of notes considered per run — bounds the O(n²) clustering.
    ///
    /// Default: `500`. Above this limit the scope should be narrowed.
    #[serde(default = "DistillSource::default_batch_limit")]
    pub batch_limit: usize,
    /// Cosine similarity threshold for grouping two notes into a cluster.
    ///
    /// Default: `0.75`. A note pair with cosine ≥ this threshold is connected
    /// (connected components → clusters).
    #[serde(default = "DistillSource::default_confidence_threshold")]
    pub confidence_threshold: f32,
    /// Minimum QA events required before distillation (future `Learn`+ modes).
    ///
    /// `None` in `Semantic` mode (unused). Reserved for future use without
    /// breaking serialisation (`skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_qa_events: Option<u32>,
}

impl DistillSource {
    fn default_mode() -> DistillMode {
        DistillMode::Semantic
    }

    fn default_batch_limit() -> usize {
        500
    }

    fn default_confidence_threshold() -> f32 {
        0.75
    }
}

impl Default for DistillSource {
    fn default() -> Self {
        Self {
            mode: DistillMode::Semantic,
            scope: JobScope::VaultWide,
            window: None,
            batch_limit: 500,
            confidence_threshold: 0.75,
            min_qa_events: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ValidateSpec — Job::Validate payload (F-43)
// ─────────────────────────────────────────────────────────────────────────────

/// Payload for `Job::Validate` — deterministic validation of a distilled note
/// before persistence. Carries the synthesis + its sources (texts + trusts) so the worker
/// can score purely, without re-reading the vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateSpec {
    /// Pre-allocated ULID of the synthesis note to validate then persist.
    pub note_id: Ulid,
    /// Owning tenant identifier (default: `"main"`).
    pub tenant_id: String,
    /// Title of the synthesis note.
    pub title: String,
    /// Markdown body of the synthesis note.
    pub body: String,
    /// ULIDs of the source notes (`derived-from`).
    pub source_ids: Vec<Ulid>,
    /// Bodies of the source notes (parallel to `source_ids`) — for num/entity penalties + embedding.
    pub source_texts: Vec<String>,
    /// Trust of the sources (parallel to `source_ids`) — for the f47 factor.
    pub source_trusts: Vec<f32>,
    /// Base trust computed by distillation (`compute_distill_trust`).
    pub base_trust: f32,
    /// Gate threshold (default 0.75).
    #[serde(default = "ValidateSpec::default_threshold")]
    pub threshold: f32,
}

impl ValidateSpec {
    /// Default gate threshold for quality scoring (0.75).
    #[must_use]
    pub fn default_threshold() -> f32 {
        0.75
    }
}

impl Default for ValidateSpec {
    fn default() -> Self {
        Self {
            note_id: Ulid::nil(),
            tenant_id: String::new(),
            title: String::new(),
            body: String::new(),
            source_ids: vec![],
            source_texts: vec![],
            source_trusts: vec![],
            base_trust: 0.0,
            threshold: Self::default_threshold(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// job_kind_str — helper de routing
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the name of the [`Job`] variant as a static string.
///
/// Used to denormalise the `kind` column in `gradatum_jobs` at enqueue time
/// and to filter by `kind` in [`QueueStore::dequeue_by_kind`].
///
/// # Exhaustiveness
///
/// The match has no `_ =>` arm so that adding a new [`Job`] variant produces a
/// compile error rather than a silently incorrect routing.
///
/// # JSON correspondence
///
/// The returned value matches the `"type"` key of the payload serialised via
/// `#[serde(tag = "type", content = "data")]` (e.g. `{"spec":{"kind":{"type":"Curate",...}}}`).
#[must_use]
pub fn job_kind_str(job: &Job) -> &'static str {
    match job {
        Job::Agent => "Agent",
        Job::Pipeline => "Pipeline",
        Job::Collect => "Collect",
        Job::Distill(_) => "Distill",
        Job::Backup => "Backup",
        Job::Purge(_) => "Purge",
        Job::ReIndex(_) => "ReIndex",
        Job::Summarize => "Summarize",
        Job::Validate(_) => "Validate",
        Job::Audit => "Audit",
        Job::Consolidate => "Consolidate",
        Job::Curate(_) => "Curate",
        Job::Forget(_) => "Forget",
        Job::Review => "Review",
        Job::Classify => "Classify",
        Job::Merge => "Merge",
        Job::Annotate => "Annotate",
        Job::Migrate(_) => "Migrate",
        Job::Export(_) => "Export",
        Job::Notify(_) => "Notify",
        Job::Ingest(_) => "Ingest",
        Job::Embed(_) => "Embed",
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobSpec — ce que fait le job
// ─────────────────────────────────────────────────────────────────────────────

/// Functional specification of a job.
///
/// Contains the work type, trigger class, execution mode, scope, and priority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    /// Job type and payload.
    pub kind: Job,
    /// Who triggers the job.
    pub class: JobClass,
    /// How the job executes.
    pub mode: JobMode,
    /// What the job operates on.
    pub scope: JobScope,
    /// Priority in the queue.
    pub priority: JobPriority,
}

/// Returns the tenant **served** by a job, derived from its spec (source = the
/// spec, never the caller).
///
/// Used at enqueue time to stamp `gradatum_jobs.tenant_id`, and then as the tenant
/// filter that enforces isolation when `multi_tenant` is enabled.
///
/// The extraction matches on the `Job` kind **exhaustively** — no `_` wildcard — so
/// that every future variant MUST be handled explicitly. This is a security guard
/// rail: the compiler refuses to let a new kind be added without deciding its tenant,
/// which closes the leak class "new kind silently falls back to `main`".
///
/// Kinds that carry a tenant or vault:
/// - `Curate` → [`CurateSpec::tenant_id`]
/// - `Ingest` → [`IngestSource::vault`]
/// - `Embed` → [`EmbedSpec::tenant_id`] (enqueued after curate)
/// - `Validate` → [`ValidateSpec::tenant_id`] (enqueued after distill; carries title and body)
/// - `Distill` → [`DistillSource::scope`] · `Forget` → [`ForgetSpec::scope`]
/// - `Export` → [`ExportSource::scope`] · `Migrate` → [`MigrateSource::target`]
///   (both carry a [`JobScope`]; enqueued only from the admin CLI today, yet still
///   extracted explicitly rather than through a wildcard)
///
/// # Repli : le vault du job, pas `"main"`
///
/// A kind that carries **no** tenant of its own (`Purge`, `Backup`, `Audit`… — and
/// a `Distill`/`Export`/`Forget` whose inner scope describes *what* rather than
/// *where*) falls back to the vault carried by [`JobSpec::scope`], because A2-bis
/// posits **one job = exactly one vault**: `JobScope::Vault(v)` *is* the tenant the
/// job serves, and it is the very value the worker resolves before touching any
/// index (`resolve_job_vault`). Deriving the stamp from anything else would let the
/// queue and the worker disagree on which vault a job belongs to.
///
/// The literal `"main"` remains as the **last** resort only, when neither the kind
/// nor the job scope carries a vault — the mono-vault case (`multi_tenant` off, or a
/// system/cron job scoped `VaultWide`), where `"main"` is *the* answer rather than
/// an arbitrary pick. Same reasoning, and same wording, as `resolve_job_vault`.
#[must_use]
pub fn spec_tenant(spec: &JobSpec) -> &str {
    // Repli commun : le vault porté par le job lui-même (A2-bis), sinon mono-vault.
    let job_vault = job_scope_vault(&spec.scope).unwrap_or("main");
    match &spec.kind {
        Job::Curate(c) => c.tenant_id.as_str(),
        Job::Ingest(i) => i.vault.as_str(),
        Job::Embed(e) => e.tenant_id.as_str(),
        Job::Validate(v) => v.tenant_id.as_str(),
        Job::Distill(d) => job_scope_vault(&d.scope).unwrap_or(job_vault),
        Job::Forget(f) => forget_scope_vault(&f.scope).unwrap_or(job_vault),
        // Export/Migrate portent un VaultScope — enqueue admin/CLI-only aujourd'hui
        // (pas de site LIVE), mais traités explicitement pour l'exhaustivité.
        Job::Export(e) => job_scope_vault(&e.scope).unwrap_or(job_vault),
        Job::Migrate(m) => job_scope_vault(&m.target).unwrap_or(job_vault),
        // Kinds système/cron sans tenant porté → le vault du job.
        Job::Agent
        | Job::Pipeline
        | Job::Collect
        | Job::Backup
        | Job::Purge(_)
        | Job::ReIndex(_)
        | Job::Summarize
        | Job::Audit
        | Job::Consolidate
        | Job::Review
        | Job::Classify
        | Job::Merge
        | Job::Annotate
        | Job::Notify(_) => job_vault,
    }
}

/// Vault **carried** by a [`JobScope`] — `Vault(v)` → `Some(v)`, anything else → `None`.
///
/// `None` is not "main": it means the scope says *what* the work covers, never *where*
/// it lives. Electing a vault is the caller's decision, made explicit at the call site.
fn job_scope_vault(scope: &JobScope) -> Option<&str> {
    match scope {
        JobScope::Vault(v) => Some(v.as_str()),
        _ => None,
    }
}

/// Vault **carried** by a [`ForgetScope`] — `None` when the scope elects no single vault.
///
/// - `Topic { vault }` → the vault if set, else `None`.
/// - `Locus { vault }` → the vault.
/// - `Agent { vaults }` → the single vault if exactly one is targeted, else `None`
///   (multi-vault agent forget is not scopable to one tenant — decision (a)). The
///   worker refuses such a job anyway (`ensure_forget_scope_vault`), so falling back
///   to the job vault here narrows nothing: it merely stops mislabelling the job as
///   `"main"`'s.
fn forget_scope_vault(scope: &ForgetScope) -> Option<&str> {
    match scope {
        ForgetScope::Topic { vault, .. } => vault.as_deref(),
        ForgetScope::Locus { vault, .. } => Some(vault.as_str()),
        ForgetScope::Agent { vaults, .. } => match vaults.as_slice() {
            [only] => Some(only.as_str()),
            _ => None,
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobClass
// ─────────────────────────────────────────────────────────────────────────────

/// Trigger class of a job.
///
/// Determines the default priority and queue routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobClass {
    /// Autonomous cron — no human actor.
    System,
    /// Triggered/executed by an LLM agent.
    Agent,
    /// Explicit CLI/studio action.
    Human,
    /// External machine call (MCP, third-party).
    Api,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobMode
// ─────────────────────────────────────────────────────────────────────────────

/// Execution mode of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JobMode {
    /// Processes N items then stops (default).
    #[default]
    Batch,
    /// Processes continuously until the queue is empty.
    Streaming,
    /// Requires back-and-forth with an actor.
    Interactive,
    /// Simulates without writing.
    DryRun,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobScope
// ─────────────────────────────────────────────────────────────────────────────

/// Scope of a job — what the work operates on.
///
/// `#[non_exhaustive]`: new scope variants may be added within the `1.x` line, so
/// downstream matches must carry a `_` arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum JobScope {
    /// The entire vault.
    VaultWide,
    /// A specific locus (vault directory).
    Locus(String),
    /// A targeted set of notes.
    Notes(Vec<Ulid>),
    /// An agent session (isolated context).
    Session(Ulid),
    /// A single explicit vault.
    ///
    /// When `multi_tenant.enabled = true`, every periodic job is enqueued once **per
    /// active vault** with this scope, so no job ever spans two vaults. When the flag
    /// is off, a job carrying `Vault(v)` with `v != "main"` is rejected terminally by
    /// the worker (fail-closed).
    Vault(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// JobPriority
// ─────────────────────────────────────────────────────────────────────────────

/// Job priority in the queue.
///
/// Default mapping:
/// - `Agent`  → `High`    (active agent in conversation — response expected)
/// - `Human`  → `High`    (explicit human action — response expected)
/// - `Api`    → `Normal`  (machine call — acceptable latency)
/// - `System` → `Low`     (background cron task — must not block agents)
///
/// Wired in `GradatumQueue.dequeue(None)` via `ORDER BY priority DESC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JobPriority {
    /// Active agent and human jobs — response expected.
    High,
    /// API calls — acceptable latency (default).
    #[default]
    Normal,
    /// System cron — background task, must not block agents.
    Low,
    /// Scheduled in the future (e.g. quarterly `Consolidate`).
    Deferred,
}

impl JobPriority {
    /// SQL value for `ORDER BY priority DESC` (High=3 sorts before Low=0).
    #[must_use]
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::High => 3,
            Self::Normal => 2,
            Self::Low => 1,
            Self::Deferred => 0,
        }
    }

    /// Default priority for the given job class.
    #[must_use]
    pub fn default_for(class: &JobClass) -> Self {
        match class {
            JobClass::Agent => Self::High,
            JobClass::Human => Self::High,
            JobClass::Api => Self::Normal,
            JobClass::System => Self::Low,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobScheduling — quand s'exécute le job
// ─────────────────────────────────────────────────────────────────────────────

/// Scheduling constraints for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobScheduling {
    /// Trigger source.
    pub trigger: TriggerSource,
    /// Scheduled date/time (UTC).
    pub scheduled_at: DateTime<Utc>,
    /// Declarative chaining — `[]` = immediate · `[x]` = chain · `[x,y]` = DAG.
    ///
    /// Semantics: "trigger me when these jobs complete".
    /// More robust than a `not_before: DateTime` (which depends on wall-clock durations).
    pub await_jobs: Vec<JobTrigger>,
    /// Deadline for `Interactive` jobs — acts as a timeout.
    pub deadline: Option<DateTime<Utc>>,
    /// Cron expression (e.g. `"0 2 * * *"`).
    pub cron_expr: Option<String>,
}

/// Cascade trigger condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTrigger {
    /// Identifier of the awaited job.
    pub job_id: Ulid,
    /// Trigger condition.
    pub condition: TriggerCondition,
}

/// Condition on the terminal state of an awaited job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerCondition {
    /// Only if `Done` (success).
    OnDone,
    /// `Done | Failed | DLQ` — regardless of outcome.
    OnAnyTerminal,
    /// Only if `Failed` (alerting).
    OnFailed,
}

/// Trigger source of a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerSource {
    /// `[[worker.schedules]]` — tokio-cron-scheduler.
    Cron,
    /// `[[pipelines]]` step — pipeline_executor.
    Pipeline,
    /// `await_jobs` → `on_job_complete()` → `set_pending()`.
    Cascade,
    /// `WriteHook` or `QaEvent` interceptor.
    OnEvent,
    /// `POST /api/v1/jobs/trigger` · admin CLI · `invoke_agent()`.
    Demand,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobLifecycle — où en est le job
// ─────────────────────────────────────────────────────────────────────────────

/// Current lifecycle state of a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLifecycle {
    /// Current status.
    pub status: JobStatus,
    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,
    /// Start timestamp (UTC) — `None` if not yet started.
    pub started_at: Option<DateTime<Utc>>,
    /// Completion timestamp (UTC) — `None` if not yet finished.
    pub completed_at: Option<DateTime<Utc>>,
    /// SQLite lease expiry — prevents duplicate execution.
    pub lease_until: Option<DateTime<Utc>>,
    /// Job result — `None` if not yet finished.
    pub result: Option<JobResult>,
}

/// Status of a job in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobStatus {
    /// In the queue, ready to start.
    Pending,
    /// Lease active — currently executing.
    Running,
    /// Awaiting unsatisfied `await_jobs`.
    Waiting,
    /// Completed successfully.
    Done,
    /// Failed, retry possible.
    Failed,
    /// Dead-letter — `max_retries` reached.
    DLQ,
    /// Deadline exceeded or orphaned.
    Cancelled,
    /// Optimistic-lock conflict: write rejected because the provided
    /// `expected_sha256` does not match the note's current hash.
    /// Terminal state without retry — the note was NOT modified.
    /// Read `lifecycle.result.result_note_md` to retrieve the `current_sha256`
    /// encoded as JSON (`{ "current_sha256": "hex...", "attempted_sha256": "hex..." }`).
    ///
    /// Reached on the RMW branch of a curate job: when the `expected_sha256` carried by
    /// `CurateSpec` is stale, the compare-and-swap fails and the job terminates here with
    /// the note left intact — see `CurateSpec::expected_sha256`.
    Conflict,
}

impl JobStatus {
    /// Returns `true` if this status is a terminal state — no further transitions
    /// or retries are possible.
    ///
    /// Terminal states: `Done`, `DLQ`, `Cancelled`, `Conflict`.
    /// Non-terminal states: `Pending`, `Running`, `Waiting`, `Failed`.
    ///
    /// This is the single source of truth for terminal state membership,
    /// used by SQL guards (`status NOT IN (...)`) and Rust logic alike.
    ///
    /// # Examples
    ///
    /// ```
    /// use gradatum_core::JobStatus;
    /// assert!(JobStatus::Done.is_terminal());
    /// assert!(!JobStatus::Pending.is_terminal());
    /// ```
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::DLQ | Self::Cancelled | Self::Conflict
        )
    }

    /// SQL fragment `'Done','DLQ','Cancelled','Conflict'` for use in
    /// `WHERE status NOT IN (...)` guards.
    ///
    /// **Single source of truth** for the terminal set at the SQL level.
    /// Derived mechanically from `is_terminal` variants — any future change
    /// to `is_terminal` must be reflected here (test `job_status_sql_fragment_matches_is_terminal`
    /// guards against divergence).
    ///
    /// # Returns
    ///
    /// A static `&str` ready for interpolation into a SQL query. The fragment
    /// does not include the surrounding parentheses.
    pub const TERMINAL_SQL: &'static str = "'Done','DLQ','Cancelled','Conflict'";
}

/// Result of a completed job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    /// `true` if the job completed successfully.
    pub success: bool,
    /// Execution duration in milliseconds.
    pub duration_ms: u32,
    /// LLM cost in USD — `None` if no LLM was involved.
    pub cost_usd: Option<f32>,
    /// Result Gradatum note — single entry point for the agent.
    ///
    /// `vault_read(result_note)` → frontmatter + paths + wikilinks to produced notes.
    /// Present when `success=true`. Error note when `DLQ`.
    pub result_note: Option<Ulid>,
    /// Optimistic-lock conflict JSON payload (present when `JobStatus::Conflict`).
    ///
    /// Contains `current_sha256` (current hash) and `attempted_sha256` (attempted hash).
    /// Allows a polling client to retrieve the current hash to resolve the conflict.
    ///
    /// Example payload:
    /// ```json
    /// { "current_sha256": "a3f1...", "attempted_sha256": "b2e0...", "timestamp_ms": 1234567890 }
    /// ```
    ///
    /// `None` for all other statuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_payload: Option<serde_json::Value>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobWorkspace — workspace physique OpenDAL
// ─────────────────────────────────────────────────────────────────────────────

/// Physical job workspace — OpenDAL-backed structure.
///
/// All I/O goes through OpenDAL — same API regardless of the backend (fs/s3/gcs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobWorkspace {
    /// Input path — e.g. `"worker/2026-05-20/01J-XYZ/input/"`.
    pub input: String,
    /// Output path — e.g. `"worker/2026-05-20/01J-XYZ/output/"`.
    pub output: String,
    /// Metadata path — e.g. `"worker/2026-05-20/01J-XYZ/meta/"`.
    pub meta: String,
}

impl JobWorkspace {
    /// Builds the workspace from a [`JobRecord`].
    ///
    /// Format: `worker/{YYYY-MM-DD}/{job_id}/{input|output|meta}/`
    #[must_use]
    pub fn from_job(job: &JobRecord) -> Self {
        let date = job.lifecycle.created_at.format("%Y-%m-%d").to_string();
        let base = format!("worker/{}/{}", date, job.id);
        Self {
            input: format!("{}/input/", base),
            output: format!("{}/output/", base),
            meta: format!("{}/meta/", base),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobProgress — progress d'un job en cours
// ─────────────────────────────────────────────────────────────────────────────

/// Progress of a running job.
///
/// Persisted to SQLite periodically.
/// `GET /api/v1/jobs/:id/status` → `{ status: "running", progress: { current: 47, total: 200 } }`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    /// Items processed so far.
    pub current: u32,
    /// Total items (if known).
    pub total: u32,
    /// Description of the current step.
    pub step: String,
    /// Estimated time remaining in seconds.
    pub eta_secs: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobOutputFile + JobOutput
// ─────────────────────────────────────────────────────────────────────────────

/// File produced by a job — stored via OpenDAL in `output/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutputFile {
    /// File name (e.g. `"export.csv"` | `"report.pdf"` | `"chart.png"`).
    pub name: String,
    /// MIME type (e.g. `"text/csv"` | `"application/pdf"` | `"image/png"`).
    pub mime_type: String,
    /// Size in bytes.
    pub size: u64,
    /// TTL in days — `None` = locus default (`worker/`=30d, `exports/`=90d).
    pub ttl_days: Option<u32>,
}

/// Complete outputs produced by a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutput {
    /// Markdown notes created in the vault.
    pub notes_created: Vec<Ulid>,
    /// Notes modified (Validate/Heal).
    pub notes_modified: Vec<Ulid>,
    /// Binaries/CSV/images in `output/`.
    pub files: Vec<JobOutputFile>,
    /// Markdown content of the result note.
    ///
    /// Written to `output/result.md` and copied to `vault work/jobs/` for `vault_read()`.
    pub result_note_md: String,
}

impl JobOutput {
    /// Returns a dry-run output for `JobMode::DryRun` — no writes performed.
    #[must_use]
    pub fn dry_run(would_affect: usize, description: &str) -> Self {
        Self {
            notes_created: vec![],
            notes_modified: vec![],
            files: vec![],
            result_note_md: format!(
                "## DRY-RUN — {description}\n\n\
                 **Simulation only — no writes performed.**\n\n\
                 Notes that would have been affected: {would_affect}\n",
            ),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobRetry — comment le job récupère
// ─────────────────────────────────────────────────────────────────────────────

/// Retry policy for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRetry {
    /// Attempts made so far.
    pub count: u32,
    /// Maximum number of attempts — `0` = no retry.
    pub max: u32,
    /// Backoff strategy.
    pub backoff: RetryBackoff,
    /// Last recorded error.
    pub last_error: Option<String>,
    /// Full error history.
    pub errors: Vec<JobError>,
}

impl Default for JobRetry {
    fn default() -> Self {
        Self {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        }
    }
}

/// Individual error recorded during an attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobError {
    /// Error timestamp (UTC).
    pub at: DateTime<Utc>,
    /// Error message.
    pub message: String,
    /// Attempt number.
    pub attempt: u32,
}

/// Backoff strategy between retry attempts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RetryBackoff {
    /// Fixed N seconds between each attempt.
    Fixed(u64),
    /// Exponential backoff: `base → 2×base → ... → max` seconds.
    Exponential {
        /// Base delay in seconds.
        base: u64,
        /// Maximum delay in seconds.
        max: u64,
    },
}

impl RetryBackoff {
    /// Computes the wait duration for attempt `attempt` (0-indexed).
    ///
    /// For `Fixed(n)`: always `n` seconds.
    /// For `Exponential { base, max }`: `min(base * 2^attempt, max)` seconds.
    #[must_use]
    pub fn duration_for(&self, attempt: u32) -> Duration {
        match self {
            Self::Fixed(secs) => Duration::from_secs(*secs),
            Self::Exponential { base, max } => {
                let secs = base.saturating_mul(1_u64 << attempt.min(62));
                Duration::from_secs(secs.min(*max))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobLineage — d'où vient le job
// ─────────────────────────────────────────────────────────────────────────────

/// Traceability of the job's emitter and trigger context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLineage {
    /// `agent_id` | `user_id` | `cron_id` — `None` if not traced.
    pub triggered_by: Option<String>,
    /// Parent job if this is a child job (cascade, agent spawn).
    pub parent_job: Option<Ulid>,
    /// Pipeline identifier if this is a `[[pipelines]]` step.
    pub pipeline_id: Option<Ulid>,
    /// Step name within the pipeline.
    pub pipeline_step: Option<String>,
    /// Jobs created by this job (outgoing cascade).
    pub children: Vec<Ulid>,
    /// Cumulative LLM cost in USD.
    pub cost_usd: Option<f32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobRecord — enveloppe complète 5 blocs
// ─────────────────────────────────────────────────────────────────────────────

/// Complete job envelope structured as 5 orthogonal blocks.
///
/// `JobRecord` is the canonical L0 type circulating across the entire job layer.
/// Serialised as JSON by the `QueueStore` for persistence in SQLite.
///
/// # The 5 blocks
///
/// 1. [`JobSpec`]      — WHAT the job does
/// 2. [`JobScheduling`] — WHEN it executes
/// 3. [`JobLifecycle`] — WHERE it stands
/// 4. [`JobRetry`]     — HOW it recovers
/// 5. [`JobLineage`]   — WHERE it comes from / links
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    /// Unique job identifier (monotonic ULID, implicit FIFO order).
    pub id: Ulid,
    /// Block 1 — WHAT the job does.
    pub spec: JobSpec,
    /// Block 2 — WHEN it executes.
    pub scheduling: JobScheduling,
    /// Block 3 — WHERE it stands.
    pub lifecycle: JobLifecycle,
    /// Block 4 — HOW it recovers.
    pub retry: JobRetry,
    /// Block 5 — WHERE it comes from / links.
    pub lineage: JobLineage,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobFilter — introspection F-16
// ─────────────────────────────────────────────────────────────────────────────

/// Sort order for [`QueueStore::list`].
///
/// Sorting is performed on the ULID `id`, which is monotonic and therefore
/// equivalent to creation order.
///
/// `CreatedAsc` is the **default**: it strictly preserves the historical
/// `list()` behaviour (`id ASC`). The studio jobs page passes `CreatedDesc`
/// explicitly to display the most recent jobs first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JobOrder {
    /// `id ASC` — oldest first (default, historical behaviour).
    #[default]
    CreatedAsc,
    /// `id DESC` — most recent first.
    CreatedDesc,
}

/// Filter for [`QueueStore::list`].
///
/// The `cursor` field enables cursor-based pagination. `cursor` is the last
/// returned ULID `id`; the comparison direction depends on [`JobFilter::order`]:
/// - [`JobOrder::CreatedAsc`] (default): `id > cursor ORDER BY id ASC` (historical behaviour).
/// - [`JobOrder::CreatedDesc`]: `id < cursor ORDER BY id DESC` (most recent page first).
///
/// Since ULID is monotonic, comparing `id` is equivalent to temporal order.
///
/// # Serialisation stability
///
/// `order` and `created_before` are additive fields with `#[serde(default)]`:
/// a pre-existing `JobFilter` JSON (without these fields) deserialises with
/// `order = CreatedAsc` and `created_before = None` — identical to historical behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobFilter {
    /// Filter by job class.
    pub class: Option<JobClass>,
    /// Filter by status.
    pub status: Option<JobStatus>,
    /// Filter by kind (name of the `Job` variant).
    pub kind: Option<String>,
    /// Filter jobs created strictly after this date (exclusive lower bound).
    pub created_after: Option<DateTime<Utc>>,
    /// Filter jobs created strictly before this date (exclusive upper bound).
    ///
    /// Combined with `created_after`, isolates a time range (e.g. a single day).
    #[serde(default)]
    pub created_before: Option<DateTime<Utc>>,
    /// Sort order (default: [`JobOrder::CreatedAsc`] — historical behaviour).
    #[serde(default)]
    pub order: JobOrder,
    /// Maximum number of results (default: 50, max: 500).
    pub limit: usize,
    /// Pagination cursor — last returned ULID `id` (exclusive).
    ///
    /// `None` = start of list. `Some(ulid)` = jobs after (ASC) or before (DESC) this ULID.
    /// Use `next_cursor` from the previous API response.
    pub cursor: Option<Ulid>,
    /// Tenant filter (L1+L2 isolation).
    ///
    /// `None` = no tenant clause (byte-identical / OFF). `Some(t)` =
    /// `AND tenant_id = t` (list only the tenant's jobs). Additive field:
    /// a pre-existing `JobFilter` JSON deserialises with `tenant = None`.
    #[serde(default)]
    pub tenant: Option<String>,
}

impl Default for JobFilter {
    fn default() -> Self {
        Self {
            class: None,
            status: None,
            kind: None,
            created_after: None,
            created_before: None,
            order: JobOrder::CreatedAsc,
            limit: 50,
            cursor: None,
            tenant: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueEvent — événements publiés par le backend
// ─────────────────────────────────────────────────────────────────────────────

/// Events published by the `QueueStore` via broadcast.
///
/// Consumed by:
/// - SSE endpoint `GET /api/v1/jobs/events`
/// - Cascade engine (`find_awaiting` + `set_pending`)
/// - Real-time monitoring dashboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueueEvent {
    /// New job inserted — `Pending` or `Waiting`.
    JobInserted(Ulid),
    /// Job completed — `Done` or `DLQ`.
    JobCompleted(Ulid, JobStatus, JobResult),
    /// Job failed — `Failed` + attempt number.
    JobFailed(Ulid, u32),
    /// Job transitioned `Waiting → Pending` (cascade satisfied).
    JobReady(Ulid),
    /// Job cancelled — deadline or orphaned.
    JobCancelled(Ulid),
}

// ─────────────────────────────────────────────────────────────────────────────
// GradatumJob — payload Apalis
// ─────────────────────────────────────────────────────────────────────────────

/// Apalis payload wrapping a [`JobRecord`].
///
/// Serialised as JSON in the `job` column of the Apalis table. The `priority`
/// field duplicates `spec.priority.as_u8()` to allow `ORDER BY priority DESC`
/// without deserialising the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradatumJob {
    /// Complete job envelope.
    pub record: JobRecord,
    /// Denormalised priority value for SQL sorting (0-3).
    pub priority: u8,
}

// ─────────────────────────────────────────────────────────────────────────────
// DryRunAware trait
// ─────────────────────────────────────────────────────────────────────────────

/// Trait for jobs and handlers that support `DryRun` mode.
///
/// The single-mechanism rule: only [`JobMode::DryRun`] in [`JobSpec`] controls
/// dry-run. `Source` structs do NOT carry a `dry_run` field, except for
/// legitimate exceptions (`MigrateSource.dry_run`, `IngestSource.dry_run`) for
/// irreversible operations requiring human validation first.
///
/// In ALL handlers, the check is the first instruction:
///
/// ```rust,ignore
/// if job.spec.mode == JobMode::DryRun {
///     let count = ctx.vault.count(&src.scopes).await?;
///     return Ok(JobOutput::dry_run(count, "description"));
/// }
/// ```
pub trait DryRunAware {
    /// Returns `true` if this job is in dry-run mode.
    fn is_dry_run(&self) -> bool;

    /// Number of notes that would be affected (estimate).
    ///
    /// Returns `0` by default — overridden by implementations that can compute
    /// the value without side effects.
    fn notes_would_affect(&self) -> usize {
        0
    }
}

impl DryRunAware for JobRecord {
    fn is_dry_run(&self) -> bool {
        self.spec.mode == JobMode::DryRun
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JobSource trait — factorisation v63
// ─────────────────────────────────────────────────────────────────────────────

/// Common trait for `Job` variant source structs.
///
/// Factorises fields common to multiple `Source` structs. Rust does not support
/// struct inheritance — a trait is preferred over embedding.
pub trait JobSource {
    /// Vault scopes the job operates on.
    fn scopes(&self) -> &[VaultScope];

    /// `true` if the job is in simulation mode (no writes).
    fn dry_run(&self) -> bool;

    /// Optional time window (notes created/modified within this duration).
    fn window(&self) -> Option<Duration> {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueStore trait — L0
// ─────────────────────────────────────────────────────────────────────────────

/// Job queue storage contract — L0 `gradatum-core`.
///
/// Implementations:
/// - `SqliteQueueStore` in `gradatum-db-sqlite` (default, embedded).
///
/// There is no `LibsqlQueueStore`: `gradatum-db-sqlite` states so in its own module
/// documentation. No remote/opt-in backend exists.
///
/// # Errors
///
/// All methods return `Result<_, QueueError>`.
/// Implementations must not panic — propagate errors via `?`.
#[async_trait::async_trait]
pub trait QueueStore: Send + Sync {
    // ── Opérations de base ────────────────────────────────────────────────

    /// Inserts a new job into the queue — returns its `Ulid`.
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError>;

    /// Dequeues the next ready job (atomic lease).
    ///
    /// `tenant_filter`: `None` = no tenant clause (backward-compatible, single-tenant) ;
    /// `Some(t)` = `AND tenant_id = t` (multi-tenant isolation ON — a worker of tenant A
    /// never dequeues a job of tenant B).
    ///
    /// Returns `None` if the queue is empty or no job is ready.
    async fn dequeue(&self, tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError>;

    /// Dequeues the next ready job filtered by `kind` (atomic lease).
    ///
    /// Guarantees that a `curate` worker never receives an `Embed` or `ReIndex`
    /// job, eliminating the routing race condition (DLQ `UnexpectedVariant` bug).
    ///
    /// `tenant_filter`: `None` = no tenant clause (backward-compatible, single-tenant) ;
    /// `Some(t)` = `AND tenant_id = t` (multi-tenant isolation ON — a worker of tenant A
    /// never dequeues a job of tenant B).
    ///
    /// # Default implementation
    ///
    /// Unfiltered fallback. Implementations with native SQL filtering (e.g.
    /// `SqliteQueueStore`) should override with `WHERE kind = ?` to exploit the
    /// `idx_jobs_status_kind` index.
    ///
    /// # Parameter
    ///
    /// `kind`: name of the `Job` variant as returned by [`job_kind_str`] —
    /// e.g. `"Curate"`, `"Embed"`, `"ReIndex"`.
    async fn dequeue_by_kind(
        &self,
        _kind: &str,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        self.dequeue(tenant_filter).await
    }

    /// Retrieves a job by identifier — `None` if not found.
    ///
    /// `tenant_filter`: `None` = no tenant clause (byte-identical / OFF) ;
    /// `Some(t)` = `AND tenant_id = t` (isolation ON — a job of another tenant
    /// reads as `None`, enabling a 404 anti-disclosure at the handler).
    async fn get(
        &self,
        id: Ulid,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError>;

    /// Marks a job as `Done` with its result.
    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError>;

    /// Marks a job as `Failed` (retry possible per policy).
    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError>;

    /// Cancels a job (`Cancelled`).
    ///
    /// `tenant_filter`: `None` = no tenant clause (byte-identical / OFF) ;
    /// `Some(t)` = `AND tenant_id = t` (isolation ON — cancelling another
    /// tenant's job is a no-op: 0 row affected, the job stays active).
    async fn cancel(&self, id: Ulid, tenant_filter: Option<&str>) -> Result<(), QueueError>;

    /// Sends a job to the dead-letter queue (`DLQ`) — max retries reached.
    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError>;

    // ── Cascade — await_jobs chaining ────────────────────────────────────

    /// Finds jobs in `Waiting` whose `await_jobs` list contains `job_id`.
    async fn find_awaiting(&self, job_id: Ulid) -> Result<Vec<JobRecord>, QueueError>;

    /// Transitions a job from `Waiting` to `Pending`.
    async fn set_pending(&self, id: Ulid) -> Result<(), QueueError>;

    // ── Periodic sweep ────────────────────────────────────────────────────

    /// Recovers jobs with expired leases → resets them to `Pending`.
    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError>;

    /// Cancels jobs whose deadline has passed.
    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError>;

    /// Promotes retry-scheduled jobs with `scheduled_at <= now` → `Pending`.
    ///
    /// Guard: if `retry.count >= retry.max` → `fail_dlq` instead of re-`Pending`.
    /// Prevents infinite loops (`Failed → schedule → Failed → ...`).
    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError>;

    /// Schedules a job for retry at `at` (transition `Failed → Waiting`).
    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError>;

    // ── Introspection ────────────────────────────────────────────────────

    /// Lists jobs matching a filter.
    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError>;

    /// Counts jobs grouped by status (`GROUP BY status`).
    ///
    /// Includes all present statuses (including DLQ). Statuses with zero count
    /// may be absent from the map (callers treat absence as `0`).
    ///
    /// # Default implementation
    ///
    /// Returns an empty map. Database-backed stores (e.g. `SqliteQueueStore`)
    /// override with a native `GROUP BY status` (single query).
    ///
    /// `tenant_filter`: `None` = no tenant clause (byte-identical / OFF) ;
    /// `Some(t)` = `WHERE tenant_id = t` (counts only the tenant's jobs).
    async fn count_jobs_by_status(
        &self,
        tenant_filter: Option<&str>,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        let _ = tenant_filter;
        Ok(std::collections::HashMap::new())
    }

    /// Permanently deletes jobs in the Dead Letter Queue.
    ///
    /// Destructive operation: deleted DLQ entries are NOT recoverable (unlike
    /// `--replay`, which moves them back to `Pending`). Reserved for
    /// `gradatum-admin jobs dlq --prune`.
    ///
    /// # Parameters
    ///
    /// - `older_than`: if `Some(cutoff)`, only deletes DLQ jobs created before
    ///   `cutoff`. If `None`, deletes all DLQ jobs.
    ///
    /// # Returns
    ///
    /// The number of jobs actually deleted.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)` (no-op). Database-backed stores (e.g. `SqliteQueueStore`)
    /// override with a native `DELETE FROM ... WHERE status = 'DLQ'`. Mocks and
    /// in-memory stores inherit the no-op.
    #[must_use = "the number of deleted DLQ jobs must be consumed (prune report)"]
    async fn delete_dlq_jobs(&self, _older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        Ok(0)
    }

    /// Counts DLQ jobs that would be deleted by `delete_dlq_jobs` with the
    /// same `older_than` — faithful dry-run of the prune operation.
    ///
    /// # Parameters
    ///
    /// - `older_than`: must be **identical** to the value passed to
    ///   `delete_dlq_jobs` so that the count matches the DELETE exactly
    ///   (same WHERE clause).
    ///
    /// # Returns
    ///
    /// The exact number of targeted DLQ jobs (dedicated `COUNT(*)`, no `LIMIT`).
    ///
    /// # Motivation
    ///
    /// The original dry-run counted via `list(limit: 200)` — producing a false
    /// count when DLQ > 200, and an erroneous early-return "nothing to prune" when
    /// the matching jobs fell outside the first 200. A `COUNT(*)` with the **same**
    /// WHERE clause as the DELETE eliminates both bugs.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)`. Database-backed stores override.
    #[must_use = "the targeted DLQ count must be consumed (dry-run prune)"]
    async fn count_dlq_jobs(&self, _older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        Ok(0)
    }

    /// Dependency-graph catch-up: promotes the `Waiting` jobs whose dependencies are
    /// all `Done` but whose post-commit cascade never ran.
    ///
    /// Called by the periodic sweep (`run_sweep_once`) as a safety net. This is **not**
    /// the nominal path — nominally, `cascade_check_and_promote` runs immediately after
    /// `complete()`.
    ///
    /// # Returns
    ///
    /// The number of `Waiting` jobs promoted to `Pending`.
    ///
    /// # Errors
    ///
    /// Returns `QueueError::Storage` if the database cannot be accessed.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)` (no-op). `SqliteQueueStore` overrides with a real scan
    /// of `gradatum_jobs WHERE status = 'Waiting' AND await_jobs IS NOT NULL AND await_jobs != '[]'`.
    /// Mocks and in-memory stores inherit the no-op — the sweep becomes inert for them,
    /// which is the correct behavior in test environments.
    async fn promote_stranded_waiting_jobs(&self) -> Result<u32, QueueError> {
        Ok(0)
    }

    /// Returns the **most recent** job (last inserted), or `None` if the queue
    /// is empty.
    ///
    /// The dashboard shows a "last job": the expected semantics is the most
    /// recently inserted job. `list()` orders by `id ASC` (cursor pagination)
    /// and would return the *oldest* — hence this dedicated method with
    /// `ORDER BY id DESC`. Since ULID `id` is monotonic, lexicographic order
    /// on `id` is equivalent to creation order.
    ///
    /// `tenant_filter`: `None` = no tenant clause (byte-identical / OFF, closes
    /// L2 as-is) ; `Some(t)` = `WHERE tenant_id = t` (latest job of that tenant
    /// only — dashboard isolation).
    ///
    /// # Default implementation
    ///
    /// Returns `None` (queue considered empty). Database-backed stores override
    /// with a native `ORDER BY id DESC LIMIT 1`.
    #[must_use = "the last returned job must be consumed or explicitly ignored"]
    async fn latest_job(
        &self,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        let _ = tenant_filter;
        Ok(None)
    }

    // ── Événements ────────────────────────────────────────────────────────

    /// Subscribes to the [`QueueEvent`] broadcast.
    ///
    /// Each call returns a new independent `Receiver`. Events are emitted
    /// without delivery guarantees if the consumer is too slow (fixed-capacity
    /// broadcast channel).
    fn subscribe(&self) -> Receiver<QueueEvent>;

    /// Marks a job in the terminal `Conflict` state (optimistic-lock).
    ///
    /// The note was NOT written. `result_note_md` contains the conflict JSON
    /// (`WriteConflictDto`) so the client can retrieve the `current_sha256`.
    ///
    /// Distinct from `fail()` (which may trigger retries) and `complete()`
    /// (which indicates success). `Conflict` is terminal without retry.
    ///
    /// # Default implementation
    ///
    /// Delegates to `complete()` with `success: false`, then patches the status
    /// in the JSON payload (workaround: `complete()` writes `Done` to the SQL
    /// status column, but `lifecycle.status` in the JSON payload is what the
    /// polling client reads).
    ///
    /// Implementations with direct DB access can override to write
    /// `status = 'Conflict'` directly to the SQL column.
    async fn mark_conflict(
        &self,
        id: Ulid,
        result_note_md: String,
        duration_ms: u32,
    ) -> Result<(), QueueError> {
        // Implémentation par défaut : utilise complete() avec success=false,
        // puis corrige le lifecycle.status dans le payload via un get+patch+re-save.
        // Les implémentations concrètes (SqliteQueueStore) surchargent cette méthode
        // pour écrire directement le bon statut SQL.
        let result = JobResult {
            success: false,
            duration_ms,
            cost_usd: None,
            result_note: None,
            conflict_payload: serde_json::from_str(&result_note_md).ok(),
        };
        // Appel complet() avec le résultat — le lifecycle.status payload sera Done,
        // mais l'implémentation concrète le corrige dans sa surcharge.
        // L'implémentation par défaut ne peut pas corriger le status SQL sans accès à la DB.
        self.complete(id, result).await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QueueError — erreurs L0 (sans dépendances externes)
// ─────────────────────────────────────────────────────────────────────────────

/// Errors from the `QueueStore` — no dependency on `sqlx` or any other driver.
///
/// Implementations (`SqliteQueueStore`, etc.) map their internal errors to these
/// variants via `map_err()`.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Storage error (SQLite driver, libsql, etc.).
    #[error("storage error: {0}")]
    Storage(String),

    /// Job not found by identifier.
    #[error("job not found: {0}")]
    NotFound(Ulid),

    /// JSON payload serialisation/deserialisation error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Invalid state transition (e.g. `Done → Running`).
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    /// Operation cancelled (timeout, shutdown).
    #[error("operation cancelled: {0}")]
    Cancelled(String),

    /// Job is not in `Running` status or its lease has expired — the caller
    /// attempted to `complete()` or `fail()` a job it does not hold a valid
    /// lease on. This is a correctness guard against stale-lease processing:
    /// prevents a worker from completing or failing a job after its lease has
    /// been taken over by another worker (or expired).
    #[error("job {0} is not leased or lease expired — cannot complete/fail")]
    NotLeased(Ulid),

    /// Operation not implemented in this version.
    ///
    /// Used instead of `todo!()` to cleanly signal a deferred trait method
    /// implementation. Must never panic in production.
    #[error("operation not implemented: {method}")]
    NotImplemented {
        /// Name of the unimplemented method.
        method: &'static str,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job_record(job: Job, class: JobClass) -> JobRecord {
        let now = Utc::now();
        JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: job,
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::default_for(&class),
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
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        }
    }

    #[test]
    fn job_priority_as_u8_ordering() {
        assert!(JobPriority::High.as_u8() > JobPriority::Normal.as_u8());
        assert!(JobPriority::Normal.as_u8() > JobPriority::Low.as_u8());
        assert!(JobPriority::Low.as_u8() > JobPriority::Deferred.as_u8());
    }

    #[test]
    fn job_priority_default_for_class() {
        assert_eq!(
            JobPriority::default_for(&JobClass::Agent),
            JobPriority::High
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::Human),
            JobPriority::High
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::Api),
            JobPriority::Normal
        );
        assert_eq!(
            JobPriority::default_for(&JobClass::System),
            JobPriority::Low
        );
    }

    // ── E2: JobStatus::is_terminal() — exhaustive coverage all variants ──────

    /// Terminal variants must return `true` — anti-regression guard for the
    /// `status NOT IN (...)` SQL guards in `queue_store_sqlite.rs`.
    #[test]
    fn job_status_is_terminal_returns_true_for_terminal_variants() {
        assert!(JobStatus::Done.is_terminal(), "Done must be terminal");
        assert!(JobStatus::DLQ.is_terminal(), "DLQ must be terminal");
        assert!(
            JobStatus::Cancelled.is_terminal(),
            "Cancelled must be terminal"
        );
        assert!(
            JobStatus::Conflict.is_terminal(),
            "Conflict must be terminal"
        );
    }

    /// Non-terminal variants must return `false` — prevents silent inclusion
    /// of active job states in the terminal set.
    #[test]
    fn job_status_is_terminal_returns_false_for_non_terminal_variants() {
        assert!(
            !JobStatus::Pending.is_terminal(),
            "Pending must NOT be terminal"
        );
        assert!(
            !JobStatus::Running.is_terminal(),
            "Running must NOT be terminal"
        );
        assert!(
            !JobStatus::Waiting.is_terminal(),
            "Waiting must NOT be terminal"
        );
        assert!(
            !JobStatus::Failed.is_terminal(),
            "Failed must NOT be terminal"
        );
    }

    /// `TERMINAL_SQL` must exactly match the string used in SQL guards.
    /// This is the anti-divergence lock: if `is_terminal` is updated, this
    /// test will fail until `TERMINAL_SQL` is updated to match.
    #[test]
    fn job_status_sql_fragment_matches_is_terminal() {
        let expected = "'Done','DLQ','Cancelled','Conflict'";
        assert_eq!(
            JobStatus::TERMINAL_SQL,
            expected,
            "TERMINAL_SQL diverged from is_terminal() terminal set — update both together"
        );
    }

    #[test]
    fn job_mode_default_is_batch() {
        assert_eq!(JobMode::default(), JobMode::Batch);
    }

    #[test]
    fn job_retry_default_values() {
        let r = JobRetry::default();
        assert_eq!(r.count, 0);
        assert_eq!(r.max, 3);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn retry_backoff_fixed_is_constant() {
        let b = RetryBackoff::Fixed(10);
        assert_eq!(b.duration_for(0), Duration::from_secs(10));
        assert_eq!(b.duration_for(5), Duration::from_secs(10));
    }

    #[test]
    fn retry_backoff_exponential_caps_at_max() {
        let b = RetryBackoff::Exponential { base: 5, max: 120 };
        assert_eq!(b.duration_for(0), Duration::from_secs(5));
        assert_eq!(b.duration_for(1), Duration::from_secs(10));
        assert_eq!(b.duration_for(10), Duration::from_secs(120)); // plafonné
    }

    #[test]
    fn job_record_serialize_roundtrip() {
        let record = make_job_record(
            Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobClass::Agent,
        );

        let json =
            serde_json::to_string(&record).expect("JobRecord doit être sérialisable en JSON");
        let back: JobRecord =
            serde_json::from_str(&json).expect("JobRecord doit être désérialisable depuis JSON");
        assert_eq!(record.id, back.id);
        assert_eq!(record.spec.priority.as_u8(), back.spec.priority.as_u8());
    }

    #[test]
    fn job_workspace_paths_format() {
        let record = make_job_record(Job::Consolidate, JobClass::System);
        let ws = JobWorkspace::from_job(&record);
        assert!(ws.input.ends_with("/input/"));
        assert!(ws.output.ends_with("/output/"));
        assert!(ws.meta.ends_with("/meta/"));
    }

    #[test]
    fn dry_run_job_record() {
        let record = {
            let now = Utc::now();
            JobRecord {
                id: Ulid::new(),
                spec: JobSpec {
                    kind: Job::Curate(CurateSpec {
                        note_id: Ulid::new(),
                        tenant_id: "main".to_string(),
                        ..Default::default()
                    }),
                    class: JobClass::Agent,
                    mode: JobMode::DryRun,
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
                    parent_job: None,
                    pipeline_id: None,
                    pipeline_step: None,
                    children: vec![],
                    cost_usd: None,
                },
            }
        };
        assert!(record.is_dry_run());
    }

    #[test]
    fn job_output_dry_run_format() {
        let out = JobOutput::dry_run(42, "test curate");
        assert!(out.notes_created.is_empty());
        assert!(out.result_note_md.contains("DRY-RUN"));
        assert!(out.result_note_md.contains("42"));
    }

    #[test]
    fn job_filter_default_limit() {
        let f = JobFilter::default();
        assert_eq!(f.limit, 50);
        assert!(f.class.is_none());
        assert!(f.status.is_none());
    }

    #[test]
    fn gradatum_job_priority_matches_spec() {
        let record = make_job_record(Job::Agent, JobClass::Human);
        let expected_priority = record.spec.priority.as_u8();
        let job = GradatumJob {
            priority: expected_priority,
            record,
        };
        assert_eq!(job.priority, 3); // Human → High → 3
    }

    #[test]
    fn vault_scope_is_alias_of_job_scope() {
        // VaultScope = JobScope — vérification que le type alias compile
        let vs: VaultScope = JobScope::VaultWide;
        let js: JobScope = vs;
        assert!(matches!(js, JobScope::VaultWide));
    }

    #[test]
    fn queue_event_variants_serialize() {
        let id = Ulid::new();
        let ev = QueueEvent::JobInserted(id);
        let json = serde_json::to_string(&ev).expect("QueueEvent doit être sérialisable");
        assert!(json.contains("JobInserted"));
    }

    // ── Tests PurgeSpec + stabilité serde position 5 ─────────────────────────

    /// PurgeSpec par défaut : dry_run=true, grace_days=Some(30), mode=Lifecycle.
    ///
    /// Vérifie les valeurs prudentes par défaut.
    #[test]
    fn purge_spec_default_values() {
        let spec = PurgeSpec::default();
        assert!(spec.dry_run, "dry_run doit être true par défaut");
        assert_eq!(spec.grace_days, Some(30));
        assert_eq!(spec.mode, PurgeMode::Lifecycle);
    }

    /// Job::Purge(PurgeSpec) est sérialisable en JSON et le type discriminant est "Purge".
    ///
    /// Stabilité serde : `#[serde(tag = "type", content = "data")]` encode le variant
    /// par son nom ("Purge") — invariant pour la colonne `kind` de la queue.
    #[test]
    fn purge_job_serializes_with_correct_type_tag() {
        let job = Job::Purge(PurgeSpec::default());
        let json = serde_json::to_string(&job).expect("Job::Purge doit être sérialisable");
        assert!(
            json.contains("\"type\":\"Purge\""),
            "le tag serde doit être 'Purge', obtenu : {json}"
        );
    }

    /// Roundtrip JSON de Job::Purge — désérialisation depuis JSON correcte.
    #[test]
    fn purge_job_json_roundtrip() {
        let original = Job::Purge(PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: false,
            grace_days: Some(7),
        });
        let json = serde_json::to_string(&original).expect("sérialisation Job::Purge");
        let back: Job = serde_json::from_str(&json).expect("désérialisation Job::Purge");
        assert!(
            matches!(back, Job::Purge(ref s) if !s.dry_run && s.grace_days == Some(7)),
            "roundtrip JSON incorrect : {json}"
        );
    }

    /// job_kind_str retourne "Purge" pour Job::Purge(_).
    #[test]
    fn job_kind_str_purge() {
        let job = Job::Purge(PurgeSpec::default());
        assert_eq!(job_kind_str(&job), "Purge");
    }

    /// PurgeSpec grace_days=None : pas de délai de grâce, dry_run=true par défaut.
    #[test]
    fn purge_spec_no_grace_serializes() {
        let spec = PurgeSpec {
            mode: PurgeMode::Lifecycle,
            dry_run: true,
            grace_days: None,
        };
        let json = serde_json::to_string(&spec).expect("sérialisation PurgeSpec sans grace");
        let back: PurgeSpec = serde_json::from_str(&json).expect("désérialisation PurgeSpec");
        assert!(back.grace_days.is_none());
        assert!(back.dry_run);
    }

    // ── Tests ForgetSpec + stabilité serde position 12 ───────────────────────

    /// ForgetSpec par défaut : dry_run=true, confirm_ulids vide.
    #[test]
    fn forget_spec_default_values() {
        let spec = ForgetSpec::default();
        assert!(spec.dry_run, "dry_run doit être true par défaut");
        assert!(spec.confirm_ulids.is_empty());
        assert!(spec.forgotten_by.is_none());
    }

    /// Job::Forget(ForgetSpec) est sérialisable en JSON et le type discriminant est "Forget".
    ///
    /// Stabilité serde : `#[serde(tag = "type", content = "data")]` encode le variant
    /// par son nom ("Forget") — invariant pour la colonne `kind` de la queue.
    #[test]
    fn forget_job_serializes_with_correct_type_tag() {
        let job = Job::Forget(ForgetSpec::default());
        let json = serde_json::to_string(&job).expect("Job::Forget doit être sérialisable");
        assert!(
            json.contains("\"type\":\"Forget\""),
            "le tag serde doit être 'Forget', obtenu : {json}"
        );
    }

    /// Roundtrip JSON de Job::Forget — désérialisation correcte depuis JSON.
    #[test]
    fn forget_job_json_roundtrip() {
        let original = Job::Forget(ForgetSpec {
            scope: ForgetScope::Topic {
                query: "secret api-key".to_string(),
                vault: None,
                limit: Some(10),
            },
            dry_run: false,
            forgotten_by: Some("operator-1".to_string()),
            confirm_ulids: vec!["01HTEST00000000000000000AB".to_string()],
        });
        let json = serde_json::to_string(&original).expect("sérialisation Job::Forget");
        let back: Job = serde_json::from_str(&json).expect("désérialisation Job::Forget");
        assert!(
            matches!(back, Job::Forget(ref s) if !s.dry_run && s.forgotten_by.as_deref() == Some("operator-1")),
            "roundtrip JSON incorrect : {json}"
        );
    }

    /// job_kind_str retourne "Forget" pour Job::Forget(_).
    #[test]
    fn job_kind_str_forget() {
        let job = Job::Forget(ForgetSpec::default());
        assert_eq!(job_kind_str(&job), "Forget");
    }

    /// ForgetScope::Locus sérialisable roundtrip.
    #[test]
    fn forget_scope_locus_roundtrip() {
        let spec = ForgetSpec {
            scope: ForgetScope::Locus {
                vault: "main".to_string(),
                locus: "inbox/old/".to_string(),
            },
            dry_run: true,
            forgotten_by: None,
            confirm_ulids: vec![],
        };
        let json = serde_json::to_string(&spec).expect("sérialisation ForgetScope::Locus");
        let back: ForgetSpec =
            serde_json::from_str(&json).expect("désérialisation ForgetScope::Locus");
        assert!(
            matches!(back.scope, ForgetScope::Locus { ref locus, .. } if locus == "inbox/old/")
        );
    }

    /// ForgetScope::Agent sérialisable roundtrip avec vaults vide → défaut ["main"] côté handler.
    #[test]
    fn forget_scope_agent_empty_vaults() {
        let spec = ForgetSpec {
            scope: ForgetScope::Agent {
                agent_id: "claude-agent".to_string(),
                vaults: vec![],
            },
            dry_run: true,
            forgotten_by: None,
            confirm_ulids: vec![],
        };
        let json = serde_json::to_string(&spec).expect("sérialisation ForgetScope::Agent");
        let back: ForgetSpec =
            serde_json::from_str(&json).expect("désérialisation ForgetScope::Agent");
        assert!(matches!(back.scope, ForgetScope::Agent { ref vaults, .. } if vaults.is_empty()));
    }

    // ── Tests DistillSource + stabilité serde position 3 (F-22 T1) ────────────

    /// `DistillSource::default` : mode Semantic, batch_limit 500, seuil 0.75.
    #[test]
    fn distill_source_default_values() {
        let src = DistillSource::default();
        assert_eq!(src.mode, DistillMode::Semantic);
        assert_eq!(src.batch_limit, 500);
        assert!((src.confidence_threshold - 0.75).abs() < f32::EPSILON);
        assert!(src.window.is_none());
        assert!(src.min_qa_events.is_none());
        assert!(matches!(src.scope, JobScope::VaultWide));
    }

    /// Job::Distill(DistillSource) sérialise avec le tag discriminant "Distill".
    ///
    /// Stabilité serde : `#[serde(tag = "type", content = "data")]` encode le variant
    /// par son nom ("Distill") — invariant pour la colonne `kind` de la queue, inchangé
    /// par la conversion unit→tuple (position 3 préservée).
    #[test]
    fn distill_job_serializes_with_correct_type_tag() {
        let job = Job::Distill(DistillSource::default());
        let json = serde_json::to_string(&job).expect("Job::Distill doit être sérialisable");
        assert!(
            json.contains("\"type\":\"Distill\""),
            "le tag serde doit être 'Distill', obtenu : {json}"
        );
    }

    /// Roundtrip JSON de Job::Distill — désérialisation correcte depuis JSON.
    #[test]
    fn distill_job_json_roundtrip() {
        let original = Job::Distill(DistillSource {
            mode: DistillMode::Semantic,
            scope: JobScope::Locus("rag/corpus-x/".to_string()),
            window: Some(Duration::from_secs(86_400)),
            batch_limit: 200,
            confidence_threshold: 0.80,
            min_qa_events: None,
        });
        let json = serde_json::to_string(&original).expect("sérialisation Job::Distill");
        let back: Job = serde_json::from_str(&json).expect("désérialisation Job::Distill");
        assert!(
            matches!(
                back,
                Job::Distill(ref s)
                    if s.batch_limit == 200
                        && (s.confidence_threshold - 0.80).abs() < f32::EPSILON
                        && matches!(s.scope, JobScope::Locus(ref l) if l == "rag/corpus-x/")
            ),
            "roundtrip JSON incorrect : {json}"
        );
    }

    /// job_kind_str retourne "Distill" pour Job::Distill(_).
    #[test]
    fn job_kind_str_distill() {
        let job = Job::Distill(DistillSource::default());
        assert_eq!(job_kind_str(&job), "Distill");
    }

    /// Rétrocompatibilité serde : un payload `{"window": null}` omis désérialise en None.
    ///
    /// Les champs avec valeurs par défaut (`mode`, `batch_limit`, `confidence_threshold`)
    /// sont restaurés depuis les défauts si absents du JSON — robustesse forward-compat.
    #[test]
    fn distill_source_deserializes_with_defaults_when_minimal() {
        // Payload minimal : seul `scope` est requis (pas de #[serde(default)]).
        let json = r#"{"scope":"VaultWide"}"#;
        let src: DistillSource =
            serde_json::from_str(json).expect("DistillSource minimal doit désérialiser");
        assert_eq!(src.mode, DistillMode::Semantic);
        assert_eq!(src.batch_limit, 500);
        assert!((src.confidence_threshold - 0.75).abs() < f32::EPSILON);
    }

    // ── Tests F-43 ValidateSpec ───────────────────────────────────────────────

    /// ValidateSpec round-trips through serde-json and job_kind_str returns "Validate".
    /// Also verifies that the threshold serde default (0.75) is preserved.
    #[test]
    fn validate_spec_roundtrip_and_kind() {
        let spec = ValidateSpec {
            note_id: Ulid::new(),
            tenant_id: "main".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            source_ids: vec![Ulid::new()],
            source_texts: vec!["s".to_string()],
            source_trusts: vec![0.6],
            base_trust: 0.6,
            threshold: ValidateSpec::default_threshold(),
        };
        let job = Job::Validate(spec);
        assert_eq!(job_kind_str(&job), "Validate");
        let json = serde_json::to_string(&job).unwrap();
        let back: Job = serde_json::from_str(&json).unwrap();
        assert_eq!(job_kind_str(&back), "Validate");
    }

    // ── Tests F-74 occurred_at CurateSpec ────────────────────────────────────

    /// occurred_at présent → round-trip serde conserve la valeur.
    #[test]
    fn curate_spec_occurred_at_serde_roundtrip_with_value() {
        let spec = CurateSpec {
            occurred_at: Some("2026-01-15T00:00:00Z".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&spec).expect("serialization CurateSpec");
        let decoded: CurateSpec = serde_json::from_str(&json).expect("deserialization CurateSpec");
        assert_eq!(
            decoded.occurred_at,
            Some("2026-01-15T00:00:00Z".to_string())
        );
    }

    /// occurred_at absent dans le JSON → None (backward-compat jobs existants).
    #[test]
    fn curate_spec_occurred_at_absent_deserializes_as_none() {
        // Payload minimal sans occurred_at — simule un job en queue pré-feature.
        let json = r#"{"note_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","tenant_id":"main"}"#;
        let spec: CurateSpec = serde_json::from_str(json).expect("deserialization CurateSpec");
        assert_eq!(spec.occurred_at, None);
    }

    /// `spec_tenant` extrait le tenant SERVI par le job depuis son spec (pas
    /// l'appelant), avec fallback `"main"`.
    #[test]
    fn spec_tenant_extracts_per_kind() {
        let mk = |kind: Job| JobSpec {
            kind,
            class: JobClass::System,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        };

        // Curate → CurateSpec.tenant_id
        let curate = mk(Job::Curate(CurateSpec {
            tenant_id: "alice".into(),
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&curate), "alice");

        // Ingest → IngestSource.vault
        let ingest = mk(Job::Ingest(IngestSource {
            source: IngestInputSource::Url {
                url: "http://x".into(),
            },
            vault: "bob".into(),
            locus: "rag/".into(),
            strategy: IngestStrategy::Auto,
            dry_run: false,
        }));
        assert_eq!(spec_tenant(&ingest), "bob");

        // Distill → DistillSource.scope : Vault(v) sinon "main"
        let distill_wide = mk(Job::Distill(DistillSource {
            scope: JobScope::VaultWide,
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&distill_wide), "main");
        let distill_v = mk(Job::Distill(DistillSource {
            scope: JobScope::Vault("carol".into()),
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&distill_v), "carol");

        // Forget → ForgetScope
        let forget_topic = mk(Job::Forget(ForgetSpec {
            scope: ForgetScope::Topic {
                query: "q".into(),
                vault: Some("dave".into()),
                limit: None,
            },
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&forget_topic), "dave");
        let forget_topic_none = mk(Job::Forget(ForgetSpec {
            scope: ForgetScope::Topic {
                query: "q".into(),
                vault: None,
                limit: None,
            },
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&forget_topic_none), "main");
        let forget_locus = mk(Job::Forget(ForgetSpec {
            scope: ForgetScope::Locus {
                vault: "erin".into(),
                locus: "inbox/".into(),
            },
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&forget_locus), "erin");
        // Agent : un seul vault → ce vault ; multi-vault → "main" (isolation-safe).
        let forget_agent_1 = mk(Job::Forget(ForgetSpec {
            scope: ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec!["frank".into()],
            },
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&forget_agent_1), "frank");
        let forget_agent_multi = mk(Job::Forget(ForgetSpec {
            scope: ForgetScope::Agent {
                agent_id: "a".into(),
                vaults: vec!["x".into(), "y".into()],
            },
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&forget_agent_multi), "main");

        // Embed → EmbedSpec.tenant_id (enqueué LIVE après curate — apalis_handlers:1945).
        let embed = mk(Job::Embed(EmbedSpec {
            note_id: Ulid::new(),
            tenant_id: "eve".into(),
            force_regenerate: false,
        }));
        assert_eq!(spec_tenant(&embed), "eve");

        // Validate → ValidateSpec.tenant_id (enqueué LIVE après distill — porte title+body).
        let validate = mk(Job::Validate(ValidateSpec {
            tenant_id: "frank".into(),
            ..Default::default()
        }));
        assert_eq!(spec_tenant(&validate), "frank");

        // Export → ExportSource.scope (VaultScope) : Vault(v) sinon "main" (admin/CLI-only).
        let export = mk(Job::Export(ExportSource {
            scope: JobScope::Vault("grace".into()),
            filter: None,
            format: ExportFormat::Json,
            target: "exports/x.json".into(),
            template: None,
        }));
        assert_eq!(spec_tenant(&export), "grace");
        let export_wide = mk(Job::Export(ExportSource {
            scope: JobScope::VaultWide,
            filter: None,
            format: ExportFormat::Json,
            target: "exports/x.json".into(),
            template: None,
        }));
        assert_eq!(spec_tenant(&export_wide), "main");

        // Migrate → MigrateSource.target (VaultScope), admin/CLI-only.
        let migrate = mk(Job::Migrate(MigrateSource {
            from_path: "/p".into(),
            mode: MigrateMode::RawMarkdown,
            conflict: ConflictStrategy::Skip,
            dry_run: true,
            target: JobScope::Vault("heidi".into()),
        }));
        assert_eq!(spec_tenant(&migrate), "heidi");

        // Fallback global — kind sans tenant porté, job sans vault porté.
        assert_eq!(spec_tenant(&mk(Job::Backup)), "main");
    }

    /// A6' — quand le kind ne porte aucun tenant, le repli est le vault du JOB
    /// (`JobSpec.scope`), pas le littéral `"main"`.
    ///
    /// RED avant A6' : les quatre cas ci-dessous rendaient `"main"`, ce qui rendait
    /// `Purge`/`Distill` inatteignables pour tout tenant ≠ main (l'anti-forge de
    /// `POST /api/v1/jobs` comparait `"main"` au Bearer → 403 systématique) et
    /// estampillait `gradatum_jobs.tenant_id = "main"` un job servant un autre vault.
    ///
    /// A2-bis pose « un job = exactement un vault » : `JobScope::Vault(v)` EST le
    /// tenant servi — c'est déjà la valeur que le worker résout (`resolve_job_vault`)
    /// avant tout accès index.
    #[test]
    fn spec_tenant_falls_back_to_job_vault_not_main() {
        let scoped = |kind: Job, scope: JobScope| JobSpec {
            kind,
            class: JobClass::Api,
            mode: JobMode::Batch,
            scope,
            priority: JobPriority::Normal,
        };

        // Purge : PurgeSpec ne porte aucun tenant → vault du job.
        assert_eq!(
            spec_tenant(&scoped(
                Job::Purge(PurgeSpec::default()),
                JobScope::Vault("alice".into())
            )),
            "alice",
            "Purge doit servir le vault du job, pas 'main'"
        );

        // Distill dont le scope interne décrit le QUOI (Locus) → vault du job.
        assert_eq!(
            spec_tenant(&scoped(
                Job::Distill(DistillSource {
                    mode: DistillMode::Semantic,
                    scope: JobScope::Locus("inbox/".into()),
                    window: None,
                    batch_limit: 500,
                    confidence_threshold: 0.75,
                    min_qa_events: None,
                }),
                JobScope::Vault("bob".into())
            )),
            "bob",
            "Distill à scope Locus doit servir le vault du job, pas 'main'"
        );

        // Forget::Topic{vault:None} → vault du job (le scope n'élit rien).
        assert_eq!(
            spec_tenant(&scoped(
                Job::Forget(ForgetSpec {
                    scope: ForgetScope::Topic {
                        query: "q".into(),
                        vault: None,
                        limit: None,
                    },
                    ..Default::default()
                }),
                JobScope::Vault("carol".into())
            )),
            "carol",
            "Forget::Topic sans vault doit servir le vault du job, pas 'main'"
        );

        // Un vault PORTÉ par le spec prime toujours sur celui du job : c'est ce qui
        // garde l'anti-forge discriminante (spec 'bob' vs job 'alice' → divergence).
        assert_eq!(
            spec_tenant(&scoped(
                Job::Forget(ForgetSpec {
                    scope: ForgetScope::Locus {
                        vault: "bob".into(),
                        locus: "inbox/".into(),
                    },
                    ..Default::default()
                }),
                JobScope::Vault("alice".into())
            )),
            "bob",
            "le vault porté par le spec prime — sinon l'anti-forge devient aveugle"
        );

        // Repli ultime inchangé : ni le kind ni le job ne portent de vault → "main".
        assert_eq!(
            spec_tenant(&scoped(
                Job::Purge(PurgeSpec::default()),
                JobScope::VaultWide
            )),
            "main",
            "mono-vault : 'main' reste LA réponse quand aucun vault n'est porté"
        );
    }
}
