//! DTOs for the internal server-to-worker API.
//!
//! These types are NEVER exposed through the MCP stub or the public OpenAPI surface.
//! They are consumed only by the internal listener, which binds to loopback
//! (`127.0.0.1:19092` by default).
//!
//! ## Isolation
//!
//! The `/internal/v1/*` routes are mounted on a separate, loopback-only listener and are
//! NEVER merged into the public router that serves `/api/v1/*`.

use gradatum_core::scope::{TenantId, VaultId};
use serde::{Deserialize, Serialize};

/// Request body for `POST /internal/v1/persist/curated` — a two-phase persist pipeline.
///
/// ## Phases
///
/// 1. **Vault write** — writes the Markdown note to disk. Blocking: on failure the request
///    returns **500** (storage), and phase 2 is not attempted. When `expected_sha256` is
///    supplied (RMW), the write is guarded by a compare-and-swap and a stale hash returns
///    **409** without touching the note; otherwise (CREATE) the write is unconditional. See
///    [`PersistCuratedRequest::expected_sha256`].
/// 2. **Index mutations** — note title, temporal entry, links and trust are applied inside
///    a **single SQLite transaction**. Blocking as well: if any of them fails, all are
///    rolled back and the request returns **500**.
///
/// ## Atomicity boundary
///
/// The two phases are not part of one transaction — they target two distinct storage
/// systems. An intermediate state (vault written, index rolled back) is therefore possible
/// and is left deliberately recoverable: the vault write is idempotent, so the caller
/// re-runs the job and converges.
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PersistCuratedRequest {
    /// Note ULID (26 uppercase characters).
    pub note_id: String,
    /// Tenant identifier (for example `"main"`).
    pub tenant_id: TenantId,
    /// Note title, used to upsert the indexed title.
    pub title: String,
    /// Full Markdown body.
    pub body: String,
    /// Canonical section (for example `"decisions"`, `"lessons-learned"`).
    pub section: String,
    /// Note tags.
    pub tags: Vec<String>,
    /// Note author (for example `"main-agent"`).
    pub author: Option<String>,
    /// Note status (for example `"live"`, `"draft"`).
    pub status: String,
    /// Trust score in `[0.0, 1.0]` — optional, omitted when undefined.
    pub trust: Option<f32>,
    /// Expected SHA-256 of the note being replaced, 64 hex characters.
    ///
    /// **Honoured.** This field both carries the compare-and-swap hash AND
    /// discriminates the two write modes of the `/internal/v1/persist/curated` handler:
    ///
    /// - `None` → **CREATE**: a fresh pre-allocated ULID is written **unconditionally**.
    /// - `Some(hash)` → **RMW** in-place under an **optimistic lock**. The handler compares
    ///   `hash` against the note's current content:
    ///   - match → the note is rewritten;
    ///   - mismatch → **409**, the write is aborted and the note is left **intact** (the
    ///     worker then moves the job to terminal `JobStatus::Conflict`);
    ///   - malformed hex → **400**, before any write.
    ///
    /// A populated `expected_sha256` is therefore genuine protection against a lost update:
    /// a *stale* hash no longer silently overwrites the concurrent winner. The primitive is
    /// `gradatum_vault::Vault::write_if_match`, reached in production through
    /// `gradatum_vault::Registry::write_if_match_internal`.
    pub expected_sha256: Option<String>,
    /// Inline temporal entry (optional).
    pub temporal: Option<TemporalEntryDto>,
    /// Links to upsert (source → destination, within the same vault).
    pub links: Vec<LinkDto>,
    /// Whether `links` is the AUTHORITATIVE, complete set of outgoing edges
    /// for this note, recomputed from the current body.
    ///
    /// - `false` (default) → **non-authoritative**: `links` are merely upserted
    ///   (`INSERT OR IGNORE`); **no existing edge is ever removed**. This preserves
    ///   the historical behaviour and is the SAFE default — a caller that did not
    ///   recompute links (a title/section/status-only rewrite such as `classify` or
    ///   `downgrade`) cannot silently wipe valid edges.
    /// - `true` → **authoritative**: before inserting, the server DELETEs every
    ///   existing outgoing edge of this note (scoped `src_note_id` + `vault_id`)
    ///   inside the same transaction, so the graph reflects the current body and
    ///   stale edges left by a previous body are removed. It MUST be set **only** by
    ///   paths that resolved the *complete* link set from the body
    ///   (`resolve_wikilinks_via_client` reporting `complete == true`).
    ///
    /// Additive `#[serde(default)]`: an older worker that omits the field, or a
    /// non-recomputing path, deserializes to `false` — never destructive by default.
    #[serde(default)]
    pub links_authoritative: bool,
    /// Note provenance (for example `"distilled"`, `"human-decision"`).
    pub provenance: Option<String>,
    /// Curator decision that produced this note, used for metrics instrumentation.
    ///
    /// Backward-compatible additive field (`#[serde(default)]`): `None` for callers that
    /// are not instrumented. When present, the server increments the
    /// `gradatum_curator_decisions{path, outcome}` counter at persist time.
    #[serde(default)]
    pub curator_decision: Option<CuratorDecisionDto>,
    /// TARGET vault of the write, decoupled from `tenant_id` (the principal).
    ///
    /// Absent (default) → the namespace is `tenant_id`, byte-identical to the historical
    /// behaviour. A writable `target_vault` differing from `tenant_id` remains FORBIDDEN:
    /// the persist guard rejects it. Cross-vault writes are not an open capability.
    ///
    /// `#[serde(default, skip_serializing_if = "Option::is_none")]` is required:
    /// `default` lets already-persisted jobs that predate the field deserialize, and
    /// `skip_serializing_if` omits the key when `None`, keeping the wire JSON
    /// byte-identical to the earlier schema (same treatment as
    /// [`PersistEmbeddingRequest::vault_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_vault: Option<VaultId>,
}

impl PersistCuratedRequest {
    /// Constructs a curated-persist request with the mandatory identity/content
    /// fields. Collections (`tags`, `links`), all `Option` fields and the additive
    /// flags default to empty/`None`/`false`; set them on the returned value as needed.
    #[must_use]
    pub fn new(
        note_id: String,
        tenant_id: TenantId,
        title: String,
        body: String,
        section: String,
        status: String,
    ) -> Self {
        Self {
            note_id,
            tenant_id,
            title,
            body,
            section,
            tags: Vec::new(),
            author: None,
            status,
            trust: None,
            expected_sha256: None,
            temporal: None,
            links: Vec::new(),
            links_authoritative: false,
            provenance: None,
            curator_decision: None,
            target_vault: None,
        }
    }
}

/// A curator decision — its path and its outcome — carried for metrics instrumentation.
///
/// Ferries the two label values of the `gradatum_curator_decisions` metric from the worker,
/// where the decision is made, to the server, which owns the Prometheus registry. Both
/// fields are always populated together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorDecisionDto {
    /// Decision path: `hint_bypass` | `fast_admit` | `pending_band` | `llm_review`.
    pub path: String,
    /// Outcome: `admitted` | `pending` | `rejected`.
    pub outcome: String,
}

/// Request body for `POST /internal/v1/persist/embedding` — stores one vector.
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PersistEmbeddingRequest {
    /// ULID of the target note.
    pub note_id: String,
    /// Embedder model identifier (for example `"bge-m3"`).
    pub embedder_id: String,
    /// Vector dimension.
    pub dim: u16,
    /// Embedding vector.
    pub vector: Vec<f32>,
    /// Vault partition of the ANN index. Optional: an older worker does not emit this
    /// field, in which case the handler defaults to `"main"`. Omitted from the payload
    /// when `None` (`skip_serializing_if`), keeping the wire byte-identical to the
    /// earlier schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_id: Option<VaultId>,
}

impl PersistEmbeddingRequest {
    /// Constructs an embedding-persist request; `vault_id` defaults to `None`
    /// (the handler falls back to `"main"`).
    #[must_use]
    pub fn new(note_id: String, embedder_id: String, dim: u16, vector: Vec<f32>) -> Self {
        Self {
            note_id,
            embedder_id,
            dim,
            vector,
            vault_id: None,
        }
    }
}

/// Request body for `POST /internal/v1/persist/forget` — marks a note as semantically forgotten.
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PersistForgetRequest {
    /// ULID of the note to forget.
    pub note_id: String,
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// Markdown body carrying the `forget = true` frontmatter.
    pub body: String,
    /// Section of the note.
    pub section: String,
    /// Actor that triggered the forget, recorded in the `forgotten_by` frontmatter field.
    pub forgotten_by: Option<String>,
}

impl PersistForgetRequest {
    /// Constructs a forget-persist request; `forgotten_by` defaults to `None`.
    #[must_use]
    pub fn new(note_id: String, tenant_id: TenantId, body: String, section: String) -> Self {
        Self {
            note_id,
            tenant_id,
            body,
            section,
            forgotten_by: None,
        }
    }
}

/// Request body for `POST /internal/v1/persist/distill` — updates a distilled note.
///
/// Used by the distillation pipeline to rewrite the content of an existing note with a
/// re-evaluated trust score.
#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PersistDistillRequest {
    /// ULID of the note to update.
    pub note_id: String,
    /// Tenant identifier.
    pub tenant_id: TenantId,
    /// New title.
    pub title: String,
    /// New Markdown body.
    pub body: String,
    /// Section, either preserved or updated.
    pub section: String,
    /// New trust score.
    pub trust: Option<f32>,
    /// Expected SHA-256 of the note being replaced, 64 hex characters.
    ///
    /// **Inert on the distill path** — unlike [`PersistCuratedRequest::expected_sha256`],
    /// which is honoured. The `/internal/v1/persist/distill` handler never reads
    /// this field and its vault write is unconditional: it is not a compare-and-swap guard
    /// and offers no protection against a concurrent write. Wiring the optimistic lock into
    /// the distill path (as done for curated) is a separate, deliberately unscoped change.
    pub expected_sha256: Option<String>,
    /// When `true`, sets `processed = true` in the note's extra frontmatter fields,
    /// flagging the note as a source that has already been distilled.
    #[serde(default)]
    pub mark_processed: bool,
    /// When present, inserts `derived-into = <ulid>` in the extra frontmatter fields.
    ///
    /// Points at the synthesis note produced from this source.
    pub derived_into: Option<String>,
    /// Source ULIDs that produced this synthesis note, written as `derived-from`.
    ///
    /// Only supplied when the synthesis note is created (the first call, with
    /// `mark_processed = false`). Inserted into the note's extra frontmatter fields.
    #[serde(default)]
    pub derived_from: Vec<String>,
    /// Tags to apply to the note frontmatter on creation (e.g. `["quality-low"]`).
    ///
    /// Applied only when the note does not yet exist in the vault (first call).
    /// On an existing note, tags are preserved as-is (non-destructive update).
    /// Passed through `parse_tags` (normalize + deduplicate).
    #[serde(default)]
    pub tags: Vec<String>,
}

impl PersistDistillRequest {
    /// Constructs a distill-persist request with the mandatory identity/content
    /// fields; `trust`, `expected_sha256`, `derived_into`, the flags and collections
    /// default to their unset/empty values.
    #[must_use]
    pub fn new(
        note_id: String,
        tenant_id: TenantId,
        title: String,
        body: String,
        section: String,
    ) -> Self {
        Self {
            note_id,
            tenant_id,
            title,
            body,
            section,
            trust: None,
            expected_sha256: None,
            mark_processed: false,
            derived_into: None,
            derived_from: Vec::new(),
            tags: Vec::new(),
        }
    }
}

/// Inline temporal entry — avoids importing the core `TemporalEntry` type into the DTO layer.
///
/// Serialized in snake_case, consistently with the other public DTOs.
#[derive(Debug, Serialize, Deserialize)]
pub struct TemporalEntryDto {
    /// Anchor timestamp, Unix epoch milliseconds.
    pub anchor_ms: i64,
    /// Anchor source: `"occurred_at"` | `"event-date"` | `"valid_from"` | `"created"`.
    pub anchor_src: String,
    /// Document kind (for example `"Event"`, `"Static"`).
    pub doc_kind: String,
    /// End of the validity window, Unix epoch milliseconds. `None` = valid indefinitely.
    pub valid_until_ms: Option<i64>,
}

/// A link to upsert (wikilink from source to destination, within the same vault).
#[derive(Debug, Serialize, Deserialize)]
pub struct LinkDto {
    /// Source ULID of the link.
    pub src: String,
    /// Destination ULID of the link.
    pub dst: String,
}

/// Success response shared by the `persist/*` handlers.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistOkResponse {
    /// ULID of the created or updated note.
    pub note_id: String,
    /// Always `"ok"` on success.
    pub status: String,
}

/// Success response for `persist/embedding`.
#[derive(Debug, Serialize, Deserialize)]
pub struct EmbeddingOkResponse {
    /// ULID of the target note.
    pub note_id: String,
    /// Embedder model identifier.
    pub embedder_id: String,
    /// Dimension of the stored vector.
    pub dim: usize,
}
