//! Full-text search, override, and checksum storage contract.
//!
//! [`IndexStore`] exposes indexing operations (FTS5), generic override management,
//! and drift detection. It is a sub-trait of the legacy [`Index`](crate::index::Index),
//! designed to be consumed by search pipelines without depending on `gradatum-index`.
//!
//! ## Planned evolution
//!
//! `GradatumError` will converge to a dedicated `StoreError` (cf. `QueueStore`) in a future release.

use async_trait::async_trait;
use ulid::Ulid;

use crate::error::GradatumError;
use crate::identity::NoteId;
use crate::index::{FileChecksumEntry, NoteRecord, TemporalEntry};
use crate::metric_sample::MetricSamplePoint;
use crate::scheduled_health::{ScheduledTaskHealth, TaskOutcome};
use crate::scope::{AclCheckedVaultId, OverrideScope, TenantId, VaultId};
use crate::status::NoteStatus;

// ── Types publics migrés depuis gradatum-index ────────────────────────────────

/// Raw result from an FTS5 search with snippet.
///
/// Returned by [`IndexStore::search_fts_with_snippet`] — contains the native FTS5 snippet
/// localised around the matched term (as opposed to `build_snippet` which truncates the body head).
///
/// Moved from `gradatum-index::sqlite::SearchHitRaw` during an earlier refactoring
/// to allow exposure via the `IndexStore` trait (object-safe).
#[derive(Debug, Clone)]
pub struct SearchHitRaw {
    /// Note ULID.
    pub note_id: NoteId,
    /// Raw BM25 score (negative — better matches are closer to 0).
    pub bm25: f64,
    /// Note status (`"live"`, `"downgraded"`, etc.).
    pub status: String,
    /// Native FTS5 snippet localised around the match (`snippet(notes_fts, 0, '»', '«', '...', 32)`).
    pub snippet: String,
    /// Note section (e.g. `"decisions"`, `"reference"`).
    pub section: String,
    /// Markdown H1 title (extracted after curate via migration 0005, may be absent).
    pub title: Option<String>,
    /// Temporal anchor (`temporal_index.anchor_ms`), Unix epoch milliseconds.
    ///
    /// `Some(ms)` when the note has a `temporal_index` entry (populated via LEFT JOIN
    /// in `search_fts_with_snippet`). `None` when absent — notes without an entry are
    /// still returned unless a temporal bound (`from_ms`/`to_ms`) is active, in which
    /// case the SQL WHERE clause excludes them.
    pub anchor_ms: Option<i64>,
}

/// Projected note row for the retrospective audit scan ([`IndexStore::audit_scan`]).
///
/// Carries only what the audit detection needs (id, section, body, optional embedding).
/// Notes in [`crate::section::Section::PROTECTED_DELETE`] are excluded at the SQL level —
/// this row is never produced for a governance note.
#[derive(Debug, Clone)]
pub struct AuditScanRow {
    /// Note ULID.
    pub note_id: String,
    /// Note section (never a `PROTECTED_DELETE` section).
    pub section: String,
    /// Stored H1 title (migration 0005), if present. Falls back to body-derived at the caller.
    pub title: Option<String>,
    /// Logical author id (`author_id`), if present. Audit signal: `tester` in `debug` = test note.
    pub author_id: Option<String>,
    /// Creation timestamp (`notes.created`, epoch ms) — relevance axis: note age.
    pub created_ms: i64,
    /// Trust score (`notes.trust`, `REAL`), if set (NULL → `None`). Relevance axis.
    pub trust: Option<f64>,
    /// Lifecycle status (`notes.status`, kebab-case). The irrelevance detector filters on
    /// `"live"`, but the SQL scan only excludes `('downgraded','garbage')`, so `staging`
    /// and `pending-review` rows do reach here.
    pub status: String,
    /// Body Markdown (source for hash / MinHash / title derivation).
    pub body_text: String,
    /// Body embedding, if present (degraded ANN mode tolerated → `None`).
    pub embedding: Option<Vec<f32>>,
    /// Embedding model id (audit compares only same-model vectors).
    pub embedder_id: Option<String>,
}

/// Raw lesson result returned by [`IndexStore::recall_lessons`].
///
/// Contains the native FTS5 snippet, the H1 title, decoded tags (space-split from `notes.tags`),
/// and `anchor_ms` (creation timestamp from `notes.created`, Unix epoch ms).
/// Distinct from [`SearchHitRaw`], which carries neither tags nor timestamp —
/// both are required by the `LessonHit` wire contract.
#[derive(Debug, Clone)]
pub struct LessonHitRaw {
    /// Lesson note ULID.
    pub note_id: NoteId,
    /// H1 title (may be absent for notes predating migration 0009).
    pub title: Option<String>,
    /// Native FTS5 snippet localised around the match (`snippet(notes_fts, 0, '»', '«', '...', 32)`).
    pub snippet: String,
    /// Note tags, decoded from `notes.tags` (space-split, order preserved).
    /// The `codified` tag is guaranteed absent (filtered upstream by `recall_lessons`).
    pub tags: Vec<String>,
    /// Creation timestamp (`notes.created`, Unix epoch ms) — the temporal anchor.
    pub anchor_ms: i64,
}

/// Review queue row returned by `/api/v1/review`.
///
/// Note awaiting human judgement: `status ∈ {pending-review, staging}`.
/// `provenance` distinguishes the origin (`"distilled"` = semantic distillation, otherwise curator).
/// The curator `confidence` score is **not** persisted in the current version.
#[derive(Debug, Clone)]
pub struct ReviewQueueRow {
    /// Note ULID.
    pub note_id: NoteId,
    /// H1 title (may be absent).
    pub title: Option<String>,
    /// Canonical section (e.g. `"decisions"`).
    pub section: String,
    /// Physical locus (path), `None` when unassigned.
    pub locus: Option<String>,
    /// Current status: `"pending-review"` or `"staging"` (distinct badge in UI).
    pub status: String,
    /// Note provenance (`"distilled"` for semantic distillation, otherwise curator/agent origin).
    pub provenance: Option<String>,
    /// Creation timestamp (`notes.created`, Unix epoch ms).
    pub created_ms: i64,
}

/// Author entry returned by [`IndexStore::distinct_authors`].
///
/// Moved from `gradatum-index::queries::AuthorRow` during an earlier refactoring
/// to allow exposure via the `IndexStore` trait (object-safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorRow {
    /// Displayed author name (`author_display_name` when set, otherwise `author_id`).
    pub name: String,
    /// Number of notes attributed to this author.
    pub note_count: u64,
}

/// Note lineage: parents (backlinks) and children (forward links).
///
/// Returned by [`IndexStore::trace_lineage`].
///
/// Moved from `gradatum-index::queries::Lineage` during an earlier refactoring.
#[derive(Debug, Clone, Default)]
pub struct Lineage {
    /// ULIDs of notes that link to this note (backlinks).
    pub parents: Vec<String>,
    /// ULIDs of notes that this note links to (forward links).
    pub children: Vec<String>,
}

/// Query selector for `code_scope` (stable contract).
///
/// Explicit discriminant — only one criterion is active per query.
#[derive(Debug, Clone)]
pub enum CodeSelector {
    /// Full-text FTS5 search (BM25) on `body_text`/`tags`.
    Query(String),
    /// Prefix filter on `source_path` (all symbols in a file/directory).
    Path(String),
    /// Filter on `qualified_name` (LIKE substring match).
    Symbol(String),
}

/// Raw entry from a `code_scope` result.
///
/// Produced by [`IndexStore::code_scope_query`]. Contains the structured symbol fields
/// (read from `notes.extra_json["cs"]`) plus the raw BM25 score.
///
/// Drift detection and structure-aware ranking are the responsibility of the handler —
/// this struct is purely an index → handler transport.
#[derive(Debug, Clone)]
pub struct CodeScopeEntryRaw {
    /// Deterministic `NoteId` (stable key derived from path + kind + qualified name).
    pub note_id: NoteId,
    /// Source file path (relative to the repo root).
    pub source_path: String,
    /// Entity kind.
    pub kind: String,
    /// Qualified name.
    pub qualified_name: String,
    /// Signature (`None` when not extractable).
    pub signature: Option<String>,
    /// Outgoing intra-repo dependencies (qualified names, best-effort).
    pub deps: Vec<String>,
    /// Raw BM25 score (negative — closer to 0 = better match).
    /// `0.0` for `path`/`symbol` selectors (no lexical scoring).
    pub bm25: f64,
    /// Inclusive 1-based span `(start_line, end_line)` of the tree-sitter node.
    ///
    /// `None` for notes ingested before `include_body` support was added (v0.5.2).
    /// When `None`, the `code_scope include_body` handler returns `body=None` for the entry.
    pub span: Option<(u32, u32)>,
}

/// One ANN partition `(vault_id, embedder_id)` whose derived index is **incomplete**.
///
/// Produced by [`IndexStore::ann_health_gate`] at server boot: `indexed < eligible` means
/// the vec0 table holds fewer rows than the source table `note_embeddings` legitimately
/// requires for that partition, so part of the corpus is unreachable through the ANN path.
///
/// ## Deficit only, never surplus
///
/// A surplus (`indexed > eligible`) is deliberately **not** reported: downgrading a note
/// only updates `notes.status`, leaving both the embedding and its ANN row in place, so a
/// surplus is a legitimate steady state. Such a row can never surface in a result either —
/// the ANN query re-joins `notes` and filters `status != 'downgraded'` plus sentinels.
/// A deficit is the opposite: it is invisible at query time and silently shrinks recall.
///
/// ## Not `#[non_exhaustive]`
///
/// The four fields **are** the measurement, and the struct is built outside this crate (by
/// `gradatum-index`), which `#[non_exhaustive]` would forbid. Extending the record would
/// change what is measured, not add an optional detail — that is a new type, not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnPartitionDeficit {
    /// Vault owning the partition (vec0 `PARTITION KEY`).
    pub vault_id: String,
    /// Embedder owning the partition (vec0 `PARTITION KEY`).
    pub embedder_id: String,
    /// Pairs `(note, embedder)` eligible for the ANN index in `note_embeddings`.
    pub eligible: u64,
    /// Rows actually present in `note_embeddings_ann` for that partition.
    pub indexed: u64,
}

/// Curated outgoing links of a note, bundled with their authority.
///
/// The edge slice and the authority flag are indissociable **by design**: a caller cannot
/// pass the edges and *forget* the flag, because a single argument carries both. This makes
/// the destructive case impossible to express by accident: the risk must be inexpressible,
/// not merely absent from the callers we know today.
#[derive(Debug, Clone, Copy)]
pub struct CuratedLinks<'a> {
    /// `(src_note_id, dst_note_id)` pairs to (re)insert into `note_links`.
    pub edges: &'a [(String, String)],
    /// When `true`, `edges` is the **complete, authoritative** outgoing set for the note:
    /// the store DELETEs every pre-existing outgoing edge of the note (scoped
    /// `src_note_id` + `vault_id`) inside the same transaction before inserting `edges`, so
    /// the graph reflects the current body and stale edges are removed. When `false` (the
    /// safe default for callers that did not recompute links — a title/section/status-only
    /// rewrite), edges are only upserted and **nothing is deleted**.
    pub authoritative: bool,
}

/// Full-text search, override, and checksum storage contract — async, thread-safe.
///
/// Implemented by `gradatum-index::SqliteIndex`.
///
/// ## Stability
///
/// `#[stability::unstable]` — this trait sits outside the crate's SemVer guarantee and may
/// change in a future release. `GradatumError` will converge to a dedicated `StoreError`
/// (see `QueueStore`).
///
/// ## Contention
///
/// The initial implementation shares a single `Arc<Mutex<Connection>>` with `DocumentStore`
/// and `VectorStore`. Physical connection separation is planned for a future release.
// AM1 : instabilité documentée ici et dans le module doc.
// `#[stability::unstable]` différé v0.4.0 — nécessite `[features] unstable-storage-traits = []`
// dans gradatum-core/Cargo.toml + opt-in de tous les consommateurs workspace.
// La macro stability n'empêche rien (pas d'E0365) ; sans la feature déclarée elle émettrait
// un deprecated warning sur chaque consommateur.
#[async_trait]
pub trait IndexStore: Send + Sync {
    /// Full-text search within a vault.
    ///
    /// Returns matching `NoteId` values sorted by relevance (descending).
    /// `limit` is the maximum number of results.
    async fn search_fts(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteId>, GradatumError>;

    /// FTS5 search returning ids sorted by BM25 score with status.
    ///
    /// The score is the raw `bm25(notes_fts)` value (negative — best match
    /// is closest to 0). Order is consistent with `search_fts` (ASC by score).
    ///
    /// Returns triples `(NoteId, bm25_score, status)` sorted best-first.
    ///
    /// Parameter `include_downgraded`:
    /// - `false` (default): excludes notes with `status = 'downgraded'`.
    /// - `true`: includes downgraded notes with their BM25 score multiplied by 0.1
    ///   (relevance penalty — they appear last).
    async fn search_fts_scored(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
    ) -> Result<Vec<(NoteId, f64, String)>, GradatumError>;

    /// Inserts or updates an override in the generic `note_overrides` table.
    ///
    /// Key is `(note_id, scope, override_type)` — one active override per tuple.
    /// `payload_toml` is the payload serialised via `OverridePayload::to_toml()`.
    async fn upsert_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
        schema_version: u32,
        payload_toml: &str,
    ) -> Result<(), GradatumError>;

    /// Retrieves an override from the generic table.
    ///
    /// Returns `(schema_version, payload_toml)` or `None` if absent.
    async fn get_override_raw(
        &self,
        note_id: NoteId,
        scope: &OverrideScope,
        override_type: &str,
    ) -> Result<Option<(u32, String)>, GradatumError>;

    /// Inserts or updates a file checksum entry.
    ///
    /// Used by the drift detector to track the expected state of Markdown files.
    async fn upsert_file_checksum(&self, entry: &FileChecksumEntry) -> Result<(), GradatumError>;

    /// Lists all file checksum entries.
    ///
    /// Used by the drift detector during a full vault scan.
    async fn list_file_checksums(&self) -> Result<Vec<FileChecksumEntry>, GradatumError>;

    /// Returns `(created_ms, in_degree)` for a note.
    ///
    /// `created_ms`: creation timestamp in Unix epoch milliseconds.
    /// `in_degree`: number of incoming backlinks (wikilinks pointing to this note).
    ///
    /// A backend without a link table MAY return `(created_ms, 0)` for `in_degree`.
    ///
    /// # Errors
    ///
    /// - `GradatumError::NoteNotFound` if the note is absent.
    /// - `GradatumError::Storage` if the query fails or `note_id` is not a valid ULID.
    async fn get_note_created_and_indegree(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<(i64, u64), GradatumError>;

    // ── Methods promoted to dyn-wiring ────────────────────────────────────────

    /// FTS5 search with native FTS5 snippet and optional section/locus filters.
    ///
    /// Returns [`SearchHitRaw`] including snippet, section, title, and BM25 score.
    ///
    /// # Parameters
    ///
    /// - `vault_id`: vault identifier (e.g. `VaultId::new("main")`).
    /// - `query`: normalised FTS5 query (via `build_fts_query`).
    /// - `limit`: maximum number of results.
    /// - `include_downgraded`: if `false`, excludes notes with `status='downgraded'`.
    /// - `section`: optional section filter (`None` = all sections).
    /// - `locus`: optional locus prefix filter (`None` = all loci). The value
    ///   must already be escaped via `escape_like` before being passed.
    /// - `status`: optional raw SQL status filter (`None` = all statuses). Raw SQL
    ///   value (kebab-case, e.g. `"live"`, `"pending-review"`, `"downgraded"`).
    ///   Validated by the caller.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    // 10 args: orthogonal search filters (downgraded/section/locus/status/from_ms/to_ms)
    // on a single FTS path. An options struct would obscure the wire contract with no
    // readability gain (each filter is an independent `Option`). Cap accepted here.
    #[expect(
        clippy::too_many_arguments,
        reason = "orthogonal search filters (F-37+F-65) — options struct without benefit"
    )]
    async fn search_fts_with_snippet(
        &self,
        vault_id: &AclCheckedVaultId,
        query: &str,
        limit: usize,
        include_downgraded: bool,
        section: Option<&str>,
        locus: Option<&str>,
        status: Option<&str>,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
    ) -> Result<Vec<SearchHitRaw>, GradatumError>;

    /// Recalls lessons by class — BM25-only, section `lessons-learned`.
    ///
    /// FTS5 search (`MATCH class`) restricted to the `lessons-learned` section,
    /// excluding notes with `status='downgraded'` and those tagged `codified`
    /// (lessons already integrated into the system — anti-pollution). Returns
    /// [`LessonHitRaw`] enriched with `tags` and `anchor_ms`, sorted by BM25
    /// (best score first), capped at `limit`.
    ///
    /// **No LLM call**: this path is purely lexical (FTS5), designed for
    /// sub-50ms latency on a normalised lesson corpus.
    ///
    /// # Parameters
    ///
    /// - `vault_id`: vault identifier (e.g. `VaultId::new("main")`).
    /// - `class`: controlled-vocabulary class tag, already validated by the caller.
    ///   Passed as-is to FTS5 — the caller guarantees membership in the closed
    ///   vocabulary (injection-safe: no FTS5 metacharacters).
    /// - `limit`: maximum number of lessons returned.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn recall_lessons(
        &self,
        vault_id: &VaultId,
        class: &str,
        limit: usize,
    ) -> Result<Vec<LessonHitRaw>, GradatumError>;

    /// Hydrate raw lesson data for a list of ULIDs.
    ///
    /// Returns notes in `section = 'lessons-learned'` matching the provided ULID list,
    /// excluding downgraded, forgotten, and sentinel notes.
    /// Callers apply their own filters (codified tag, class match) on the returned data.
    ///
    /// ## Default implementation
    ///
    /// Returns an empty vec — safe for all mock implementations (fanout-safe).
    /// Override in `SqliteIndex` via the `index_store_impl` module (gradatum-index).
    ///
    /// ## Parameters
    ///
    /// - `vault_id`: vault identifier (e.g. `VaultId::new("main")`).
    /// - `ulids`: list of note ULIDs to hydrate; empty slice → returns `Ok(vec![])` immediately.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn hydrate_lessons_by_ulids(
        &self,
        vault_id: &VaultId,
        ulids: &[&str],
    ) -> Result<Vec<LessonHitRaw>, GradatumError> {
        let _ = (vault_id, ulids);
        Ok(vec![])
    }

    /// Review queue — notes with `status ∈ {pending-review, staging}`.
    ///
    /// Returns at most `limit` rows, paginated by lexicographic ULID cursor
    /// (`cursor > last_id`, `None`/`""` = start). Sorted `created DESC` then
    /// `id DESC` (most recent first — queue order). Excludes sentinels.
    ///
    /// # Parameters
    /// - `vault_id`: vault identifier.
    /// - `cursor`: last received ULID (pagination), `None` = first page.
    /// - `limit`: maximum number of rows (clamped by the caller).
    ///
    /// # Errors
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn list_review_queue(
        &self,
        vault_id: &VaultId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReviewQueueRow>, GradatumError>;

    /// Total count of notes in the review queue (`status ∈ {pending-review, staging}`).
    ///
    /// Excludes sentinels. Used for the `total` field of `/api/v1/review`.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn count_review_queue(&self, vault_id: &VaultId) -> Result<u64, GradatumError>;

    /// Notes promotable from review statuses — staging or pending-review older than cutoff.
    ///
    /// Returns notes where `status ∈ {staging, pending-review}` AND
    /// `COALESCE(status_changed, created) < cutoff_ms`, excluding sentinels.
    /// Sorted oldest-first. Capped to `limit`.
    ///
    /// Used by the review auto-promote background job.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn find_promotable(
        &self,
        cutoff_ms: i64,
        limit: usize,
    ) -> Result<Vec<(String, NoteStatus)>, GradatumError>;

    /// Per-vault variant of [`IndexStore::find_promotable`].
    ///
    /// Same predicate (aged `staging`/`pending-review` notes, sentinels excluded),
    /// restricted to one `vault_id`. Used by the review-promote tick when
    /// `multi_tenant.enabled = true`, where no job may ever span two vaults.
    ///
    /// # Default implementation
    ///
    /// Returns an empty list: a backend without support promotes nothing, which is the
    /// safe outcome. `SqliteIndex` provides the real query.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn find_promotable_in_vault(
        &self,
        _vault_id: &VaultId,
        _cutoff_ms: i64,
        _limit: usize,
    ) -> Result<Vec<(String, NoteStatus)>, GradatumError> {
        Ok(vec![])
    }

    /// Active vaults — `tenants.status = 'active'`, sorted by id.
    ///
    /// The iteration source of the periodic jobs when `multi_tenant.enabled = true`:
    /// each tick processes the vaults **one at a time**. `suspended` and `deleted`
    /// tenants are excluded, so their jobs are refused outright.
    ///
    /// # Default implementation
    ///
    /// Returns an empty list — fail-closed: a backend without support iterates over no
    /// vault at all, rather than falling back to an implicit cross-vault scan.
    /// `SqliteIndex` provides the real query.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn list_active_vaults(&self) -> Result<Vec<VaultId>, GradatumError> {
        Ok(vec![])
    }

    /// Provisions a vault: `INSERT OR IGNORE` of the tenant row (as `active`) plus its
    /// `write` self-grant — transactional and idempotent, so replaying it is a no-op.
    ///
    /// Returns `true` if at least one row was created, `false` if the vault was already
    /// provisioned.
    ///
    /// # Default implementation
    ///
    /// `Err(Storage)` — a backend without support must NEVER claim to have provisioned
    /// anything: fail loud rather than silently no-op. `SqliteIndex` provides the real
    /// transaction.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the write fails or the backend has no support.
    async fn provision_vault(&self, _vault_id: &str) -> Result<bool, GradatumError> {
        Err(GradatumError::Storage(
            "provision_vault not supported by this backend".to_owned(),
        ))
    }

    /// Changes the lifecycle status of a tenant (suspend or soft-delete).
    ///
    /// Returns `Ok(Some(changed))` when the tenant exists — `changed = false` if it was
    /// already in that status, so the call is idempotent — and `Ok(None)` when the
    /// tenant is unknown.
    ///
    /// # Default implementation
    ///
    /// `Err(Storage)` — same fail-loud contract as [`IndexStore::provision_vault`].
    ///
    /// # Errors
    /// `GradatumError::Storage` if the write fails or the backend has no support.
    async fn set_tenant_status(
        &self,
        _vault_id: &str,
        _status: crate::scope::TenantStatus,
    ) -> Result<Option<bool>, GradatumError> {
        Err(GradatumError::Storage(
            "set_tenant_status not supported by this backend".to_owned(),
        ))
    }

    /// Reads the lifecycle status of a tenant.
    ///
    /// `Ok(None)` means the tenant is unknown (never provisioned). A row holding an
    /// out-of-domain status value is a `Storage` error — fail-closed, so that a
    /// corrupted value is never read back as a valid status.
    ///
    /// # Default implementation
    ///
    /// `Err(Storage)` — same fail-loud contract as [`IndexStore::provision_vault`]:
    /// the callers (the read-target guard, the purge path) then refuse downstream.
    ///
    /// # Errors
    /// `GradatumError::Storage` if the read fails or the backend has no support.
    async fn get_tenant_status(
        &self,
        _vault_id: &str,
    ) -> Result<Option<crate::scope::TenantStatus>, GradatumError> {
        Err(GradatumError::Storage(
            "get_tenant_status not supported by this backend".to_owned(),
        ))
    }

    /// Lists the ULIDs of **every** note in a vault — any status, `downgraded`
    /// included, sentinels excluded — together with the absolute total.
    /// This is the eligibility listing used when purging a soft-deleted vault.
    /// `limit` is clamped to `[1, 500]`.
    ///
    /// # Default implementation
    ///
    /// `Err(Storage)` — fail loud: a backend without support must never let the caller
    /// believe a purge was complete, which `(vec![], 0)` would suggest ("nothing left
    /// to purge").
    ///
    /// # Errors
    /// `GradatumError::Storage` if the read fails or the backend has no support.
    async fn list_vault_note_ulids(
        &self,
        _vault_id: &str,
        _limit: usize,
    ) -> Result<(Vec<String>, u64), GradatumError> {
        Err(GradatumError::Storage(
            "list_vault_note_ulids not supported by this backend".to_owned(),
        ))
    }

    /// Looks up a note by its Markdown title (first `# {title}` line).
    ///
    /// Returns the ULID of the first matching note, or `None`.
    /// Excludes notes where `status != 'live'` (archived notes are not addressable by title).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn title_lookup(
        &self,
        vault_id: &str,
        title: &str,
    ) -> Result<Option<String>, GradatumError>;

    /// Checks that a note exists and is `live`, addressing it by ULID string.
    ///
    /// Used for the ULID-first resolution of `[[section:ULID]]` wikilinks: when the
    /// wikilink already carries a ULID, this method confirms existence without going
    /// through the Markdown H1 title match.
    ///
    /// Returns `Some(id)` when the note exists and is `live`, `None` otherwise.
    /// Sentinels and non-live notes (archived, garbage, …) are excluded.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn id_lookup(
        &self,
        vault_id: &str,
        note_id: &str,
    ) -> Result<Option<String>, GradatumError>;

    /// Counts notes with `status = 'live'` for a vault.
    ///
    /// Excludes sentinels (`id NOT LIKE '__sentinel__%'`).
    /// Used for `vault_status.note_count`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn live_note_count(&self, vault_id: &str) -> Result<u64, GradatumError>;

    /// Returns the allow-list grants of a tenant (multi-vault substrate).
    ///
    /// Only grants whose tenant row is `active` (table `tenants`, migration 0030)
    /// are returned. Consulted by the auth middleware and the scoped write paths
    /// when `multi_tenant.enabled = true`; the legacy single-vault path never calls it.
    ///
    /// Default implementation: **empty list, fail-closed** — a backend without
    /// grant storage grants nothing (enforcement points treat "no grant" as deny).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails (callers must treat an
    /// error as a denial, never as an implicit grant).
    ///
    /// `tenant_id` is typed [`TenantId`]: the principal is the same type here, on the
    /// api-key record and on the returned [`crate::scope::VaultGrant`], so no call site
    /// can accidentally pass a vault id where a tenant id is expected.
    async fn tenant_grants(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<crate::scope::VaultGrant>, GradatumError> {
        let _ = tenant_id;
        Ok(Vec::new())
    }

    /// Allow-list grants of an agent — mirror of [`Self::tenant_grants`] at the
    /// agent level (lot B6, plan v1.0.0).
    ///
    /// Returns the set of vaults an agent identity may access, with access level and
    /// optional section scope. Consulted by the auth middleware after the tenant-level
    /// grant check: effective access = `min(tenant_grant, agent_grant)`.
    ///
    /// Default implementation: **empty list, fail-closed** — a backend without
    /// grant storage grants nothing.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails (callers must treat an
    /// error as a denial, never as an implicit grant).
    async fn agent_grants(
        &self,
        agent_id: &crate::scope::AgentId,
    ) -> Result<Vec<crate::scope::AgentVaultGrant>, GradatumError> {
        let _ = agent_id;
        Ok(Vec::new())
    }

    /// Upserts a row into `agent_vault_grants` — **INSERT OR IGNORE**, idempotent by
    /// construction (lot B7, v1.0.0 plan).
    ///
    /// Called at boot by the `agent_id` ↔ `agent_vault_grants` reconciliation to
    /// ensure every active key has its grant row, and at `api-key create` time
    /// to provision the grant alongside the key.
    ///
    /// A pre-existing row (same `agent_id` + `vault_id`) is **never**
    /// overwritten — the upsert is additive, never destructive. A grant that was
    /// granted then manually revoked is not silently restored at boot.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the INSERT fails.
    async fn upsert_agent_grant(
        &self,
        agent_id: &crate::scope::AgentId,
        vault_id: &crate::scope::VaultId,
        access: crate::scope::GrantAccess,
        section: Option<&str>,
    ) -> Result<(), GradatumError> {
        // Default: no-op pour les stores sans agent_vault_grants.
        let _ = (agent_id, vault_id, access, section);
        Ok(())
    }

    /// Lists distinct authors in a vault with their note counts.
    ///
    /// Excludes sentinels and notes without an author.
    /// Returns `name` = `author_display_name` if set, otherwise `author_id`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn distinct_authors(&self, vault_id: &str) -> Result<Vec<AuthorRow>, GradatumError>;

    /// Lists distinct tags in a vault with their frequency.
    ///
    /// Returns `Vec<(tag, count)>` sorted by descending frequency.
    /// Tags are aggregated in Rust (space-split from `notes.tags`).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn distinct_tags(&self, vault_id: &str) -> Result<Vec<(String, u64)>, GradatumError>;

    /// Returns neighbours of a note up to `depth` levels (max 3).
    ///
    /// Uses a recursive BFS CTE on `note_links`. The source note is excluded from the result.
    /// `depth` is capped at 3 to prevent runaway traversal.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the CTE query fails.
    async fn neighbors(
        &self,
        vault_id: &str,
        note_id: &str,
        depth: u8,
    ) -> Result<Vec<String>, GradatumError>;

    /// Returns backlinks (notes that link to `note_id`) for a vault.
    ///
    /// Requires the `note_links` table (migration 0002).
    /// Returns a list of ULID identifiers (`src_note_id`) pointing to `note_id`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn backlinks(&self, vault_id: &str, note_id: &str) -> Result<Vec<String>, GradatumError>;

    /// Returns the lineage of a note: parents (backlinks) and children (forward links).
    ///
    /// Combines two queries on `note_links`:
    /// - `parents` = notes that link to `note_id`.
    /// - `children` = notes that `note_id` links to.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if either query fails.
    async fn trace_lineage(&self, vault_id: &str, note_id: &str) -> Result<Lineage, GradatumError>;

    /// Lists notes in a vault with ULID-cursor pagination.
    ///
    /// Returns `(records, total)` — `total` is the absolute count (for `X-Total-Count`).
    /// `cursor` = last received ULID (exclusive); `None` = start of list.
    /// `section` = optional section filter.
    /// Downgraded notes are excluded.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn list_notes(
        &self,
        vault_id: &str,
        section: Option<&str>,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError>;

    /// Atomically allocates the next **feature number** for `vault_id`.
    ///
    /// Returns a number that is guaranteed unique across concurrent callers: the backing
    /// store increments a persistent per-vault counter inside a single serialized
    /// transaction, so two simultaneous allocations always receive two distinct numbers.
    /// The counter never goes backwards.
    ///
    /// On **every** allocation the store recomputes the current maximum from the project-map
    /// card **bodies** (role `[[feature:F-XX]]`, via [`crate::project_map::max_feature_number`])
    /// — the reliable source of truth, never the note tags — and takes the floor as
    /// `max(persistent_counter, derived_max)`. This holds the sequence above any card created
    /// out-of-band (an explicit client-side number), so allocation does not depend on the
    /// (incomplete, inconsistent) `f-NNN` tags and cannot collide with an existing card.
    ///
    /// # Default
    ///
    /// The default implementation fails **loudly** with [`GradatumError::Storage`]: a store
    /// that does not persist a feature counter must never silently return a doubtful number.
    /// The production `SqliteIndex` overrides it.
    ///
    /// # Errors
    ///
    /// Returns [`GradatumError::Storage`] if the counter cannot be read, seeded or written —
    /// in which case **no number is handed out** (the transaction rolls back).
    async fn allocate_feature_number(&self, vault_id: &VaultId) -> Result<u32, GradatumError> {
        let _ = vault_id;
        Err(GradatumError::Storage(
            "allocate_feature_number is not supported by this index backend".to_string(),
        ))
    }

    /// Lists notes **by status** (metadata only, `downgraded` notes included).
    ///
    /// Unlike [`Self::list_notes`], this does **not** exclude `downgraded` notes — that
    /// is the whole point of the method: browsing archived or downgraded material, and
    /// drilling down from the Studio UI.
    ///
    /// - `statuses`: the set of statuses to include. Empty → `Ok((vec![], 0))`.
    /// - `section`: optional filter on the section.
    /// - `cursor`: the last ULID received (exclusive). `None` or `""` starts from the top.
    /// - `limit`: clamped to `[1, 200]` by the implementation.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok((vec![], 0))` — only the real SQLite index produces results; test
    /// mocks inherit the default unchanged.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn list_notes_by_status(
        &self,
        _vault_id: &str,
        _statuses: &[&str],
        _section: Option<&str>,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<(Vec<NoteRecord>, u64), GradatumError> {
        Ok((Vec::new(), 0))
    }

    /// Lists the `k` most recently active notes in a vault.
    ///
    /// **"Recently active"** is defined as `ORDER BY COALESCE(updated, created) DESC`:
    /// a note that was updated recently but has an older ULID creation timestamp will rank
    /// above a newer note that has never been edited. This aligns with the Active Recall
    /// goal of surfacing notes the user has recently engaged with, not merely the most
    /// recently *created* ones.
    ///
    /// Excludes sentinel notes (`id NOT LIKE '__sentinel__%'`) and downgraded notes
    /// (`status != 'downgraded'`). `k` is clamped to `[1, 200]` by the implementation.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn list_recent_notes(
        &self,
        vault_id: &str,
        k: usize,
    ) -> Result<Vec<NoteRecord>, GradatumError>;

    /// Sum of `LENGTH(body_text)` for non-sentinel notes in a vault.
    ///
    /// Returns 0 if no notes exist. Used for `vault_status.total_size_bytes`.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn total_body_size_bytes(&self, vault_id: &str) -> Result<u64, GradatumError>;

    /// Inserts or ignores a wikilink between two notes.
    ///
    /// Idempotent (`INSERT OR IGNORE`). Used by the curator to record
    /// `[[...]]` links detected in the note body.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the query fails.
    async fn upsert_link(
        &self,
        vault_id: &str,
        src_note_id: &str,
        dst_note_id: &str,
    ) -> Result<(), GradatumError>;

    /// Batch-fetches `title` and `section` for a list of ULID identifiers.
    ///
    /// Used by the `vault_search` handler to enrich semantic-only hits
    /// (present in the RRF-merged result but absent from the BM25 map) with
    /// their `title` and `section` metadata.
    ///
    /// ## Behaviour
    ///
    /// - Single `SELECT id, title, section FROM notes WHERE vault_id = ? AND id IN (…)`.
    /// - Identifiers absent from the `notes` table are not included in the result.
    /// - Sentinels (id LIKE `__sentinel__%`) are excluded.
    ///
    /// ## Return
    ///
    /// `HashMap<note_id, (title, section)>` — `title` is `None` if the column is
    /// NULL (note predates migration 0009 and has no H1).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn get_titles_sections(
        &self,
        vault_id: &AclCheckedVaultId,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError>;

    /// Reads the raw SQL status (kebab-case, e.g. `"live"`, `"downgraded"`) for a batch of notes.
    ///
    /// Returns `HashMap<note_id, status>` for present ids. Used to filter semantic hits by
    /// status (the semantic path does not receive the SQL filter, unlike the BM25 path).
    /// Returns the raw SQL value (not `NoteStatus`) to handle the legacy `downgraded` value.
    ///
    /// # Default implementation
    ///
    /// Returns an empty map (backend without support → no semantic status filtering,
    /// safe degradation to BM25-status-only). `SqliteIndex` overrides.
    async fn get_statuses(
        &self,
        _vault_id: &AclCheckedVaultId,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Batch-reads `anchor_ms` from `temporal_index` for a list of note IDs.
    ///
    /// Returns `HashMap<note_id, anchor_ms>` for notes present in `temporal_index`.
    /// Notes absent from the index are simply not in the returned map.
    ///
    /// Used by the semantic path to enrich `RrfHit.anchor_ms` and apply optional
    /// temporal bounds (`from_ms`/`to_ms`) without modifying `VectorStore::search_semantic`.
    ///
    /// # Default implementation
    ///
    /// Returns an empty map — safe for all mock implementations and backends
    /// that do not maintain a `temporal_index`. `SqliteIndex` overrides.
    ///
    /// # Warning — fail-closed on semantic temporal filtering
    ///
    /// When a backend does **not** override this method, the returned map is always empty.
    /// If a temporal bound (`from_ms`/`to_ms`) is active in `vault_search_impl`, the caller
    /// retains only hits whose `note_id` appears in the map (the `None => false` branch).
    /// A backend that returns an empty map here will therefore **silently drop ALL semantic
    /// hits** whenever bounds are active — making the semantic branch fail-closed.
    ///
    /// Any backend that populates `note_embeddings` and exposes semantic search MUST override
    /// this method to maintain parity between the FTS and semantic temporal filtering paths
    /// (FTS∪ANN temporal filter parity).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn get_anchor_ms_batch(
        &self,
        _vault_id: &AclCheckedVaultId,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, i64>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Reads the trust score for a note from the `notes.trust` column, scoped to `vault_id`.
    ///
    /// Returns `Some(trust)` if the note exists in `vault_id` and the column is non-NULL,
    /// `None` if the note is absent (in that vault) or trust has not been set.
    ///
    /// The lookup is `(vault_id, id)` — never id-only — to avoid resolving a homonymous
    /// note from another vault on ULID collision (composite PK since migration 0032).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn get_trust(&self, vault_id: &str, id: &NoteId) -> Result<Option<f32>, GradatumError>;

    /// Reads `(trust, provenance)` for a note to support decay-trust scoring.
    ///
    /// Returns `(Option<trust>, Option<provenance>)`: `trust` from `notes.trust`,
    /// `provenance` from `notes.provenance`. Allows the scorer to select the
    /// decay half-life by provenance in a single read.
    ///
    /// # Default implementation
    ///
    /// Returns `(None, None)` — backends without combined-read support effectively
    /// disable decay-trust (neutral behaviour). `SqliteIndex` overrides with a real read.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn get_trust_and_provenance(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<(Option<f32>, Option<String>), GradatumError> {
        Ok((None, None))
    }

    /// Inserts or updates a redirect `old_slug → ulid` in `redirect_table`.
    ///
    /// `vault_id`: namespace of the redirect (part of the composite PK
    /// `(vault_id, title_slug)`, migration 0035). Two vaults may carry the same
    /// slug without clobbering each other.
    /// `renamed_at_ms`: rename timestamp in Unix epoch milliseconds.
    ///
    /// Idempotent: `INSERT OR REPLACE` — if a redirect exists for this
    /// `(vault_id, slug)`, it is replaced by the new ULID (last rename wins).
    ///
    /// Called by `gradatum-admin vault rename` after each rename.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the INSERT fails.
    async fn upsert_redirect(
        &self,
        vault_id: &str,
        slug: &str,
        ulid: &Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError>;

    /// Resolves an old-title slug to its current ULID via `redirect_table`,
    /// scoped to `vault_id` (composite PK `(vault_id, title_slug)`, migration 0035).
    ///
    /// Returns `Some(ulid)` if the slug exists in the redirect table for this vault,
    /// `None` if no redirect is registered for this `(vault_id, slug)`.
    ///
    /// The slug is obtained via `title_to_slug(old_title)` (lowercase + spaces→hyphens).
    ///
    /// Used by the read layer (handler `vault_read`) as a fallback when `title_lookup`
    /// fails — transparent resolution after a rename.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn resolve_redirect(
        &self,
        vault_id: &str,
        slug: &str,
    ) -> Result<Option<Ulid>, GradatumError>;

    // ── Semantic Forget — scope resolution ────────────────────────────────────

    /// Resolves a Topic scope via FTS5 for semantic forget.
    ///
    /// Returns `Vec<(id, section)>` — notes matching the FTS query,
    /// with a safety limit of 200.
    ///
    /// ## Empty query guard
    ///
    /// If `query.trim().is_empty()`, returns `vec![]` without error.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the FTS5 query fails.
    async fn search_fts_for_forget(
        &self,
        vault_id: &VaultId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>, GradatumError>;

    /// Resolves a Locus scope via LIKE prefix for semantic forget.
    ///
    /// Returns `Vec<(id, section)>` — notes whose `locus` starts with `prefix`.
    /// LIKE escaping is applied by the implementation — do not pre-escape upstream.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn list_notes_by_locus_prefix(
        &self,
        vault_id: &str,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, GradatumError>;

    /// Resolves an Agent scope via `author_id` for semantic forget.
    ///
    /// Returns `Vec<(id, section)>` — notes created by `agent_id` in `vaults`.
    /// Empty `vaults` → all notes by the agent (no vault filter).
    /// Safety cap: 20 vaults maximum (DoS protection).
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn list_notes_by_agent(
        &self,
        agent_id: &str,
        vaults: &[String],
    ) -> Result<Vec<(String, String)>, GradatumError>;

    // ── Lifecycle / index mutations promoted to trait ──────────────────────────
    //
    // Ces 4 méthodes étaient des méthodes inhérentes concrètes sur `SqliteIndex`,
    // appelées par le worker via `Arc<SqliteIndex>`. Leur promotion en trait permet
    // au worker de basculer sur `Arc<dyn Index>` (type effacé) — symétrie avec le
    // server. Aucune ne fait fuiter de type rusqlite (signatures `&str`/`&NoteId`/
    // `&TemporalEntry`/scalaires). Chaque méthode a une impl par défaut neutre
    // (backend-additive) — un backend alternatif n'a pas à les
    // implémenter pour compiler ; il désactive de fait la fonctionnalité concernée.

    /// Writes the computed trust score for a note into the `notes.trust` column.
    ///
    /// Single write point for a computed trust value outside `TRUST_SCORES` (distillation).
    /// Idempotent, scoped on `(vault_id, id)` — mirrors
    /// [`get_trust_and_provenance`](Self::get_trust_and_provenance).
    ///
    /// Returns the number of affected rows (`0` if no note matches `(vault_id, id)` —
    /// non-fatal for the caller: the static `provenance` value remains in place).
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)` — backend without computed trust support (neutral no-op).
    /// `SqliteIndex` overrides with the real `UPDATE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the `UPDATE` fails.
    #[must_use = "the number of affected rows indicates whether the note existed"]
    async fn set_note_trust(
        &self,
        _vault_id: &str,
        _id: &NoteId,
        _trust: f32,
    ) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Writes (or replaces) the temporal anchor for a note in `temporal_index`.
    ///
    /// `INSERT OR REPLACE` — idempotent on the `note_id` primary key.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(())` without writing — backend without a temporal table (neutral no-op).
    /// `SqliteIndex` overrides with the real `INSERT OR REPLACE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the write fails.
    async fn write_temporal_entry(&self, _entry: &TemporalEntry) -> Result<(), GradatumError> {
        Ok(())
    }

    /// Paginated temporal read from `temporal_index`.
    ///
    /// Returns rows sorted `anchor_ms DESC, note_id DESC`, filtered by
    /// `vault_id`, `filter.doc_kind`, the window `filter.from_ms`/`to_ms` (inclusive
    /// bounds), and `filter.cursor` (keyset pagination). Excludes `status='garbage'`,
    /// sentinels, and **protected sections** (`agent-issues`, `council` —
    /// canonical source `Section::PROTECTED_FORGET`, sensitive titles not exposed).
    /// `filter.limit` caps the number of rows.
    ///
    /// **Explicit design choice**: notes with `forgotten=1` are **included** —
    /// the timeline is a factual temporal journal; decay search does not apply here.
    /// Only `status='garbage'` (async cleanup) is excluded.
    ///
    /// **No default implementation**: every backend must provide a real read —
    /// a default `Ok(vec![])` would silently hide a non-conformant backend.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn timeline(
        &self,
        vault_id: &AclCheckedVaultId,
        filter: &crate::temporal_query::TimelineFilter,
    ) -> Result<Vec<crate::temporal_query::TimelineRow>, GradatumError>;

    /// Deletes a redirect by target ULID from `redirect_table`, scoped to `vault_id`.
    ///
    /// Used during note purge/forget to clean up redirects pointing to it.
    /// `vault_id` scopes the `DELETE` to the note's namespace (composite PK
    /// `(vault_id, title_slug)`, migration 0035) so a purge in one vault never
    /// removes a homonymous redirect in another.
    /// Returns the number of deleted rows.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)` — backend without a redirect table (neutral no-op).
    /// `SqliteIndex` overrides with the real `DELETE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the `DELETE` fails.
    async fn delete_redirect_by_ulid(
        &self,
        _vault_id: &str,
        _ulid_str: &str,
    ) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Deletes a note from the index (`notes` table + FTS `notes_fts` + derived tables).
    ///
    /// Atomic operation on the locked connection. Returns `true` if a note was deleted,
    /// `false` if it was already absent (idempotent for the caller).
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(false)` — backend without physical indexed deletion (neutral no-op).
    /// `SqliteIndex` overrides with the real deletion.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the deletion fails.
    async fn delete_note_from_index(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<bool, GradatumError> {
        Ok(false)
    }

    /// Lists `Garbage` notes whose retention has expired (`status_changed < cutoff`).
    ///
    /// Used by the `purge` worker to select notes for physical deletion.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(vec![])` — backend without Garbage lifecycle (neutral no-op).
    /// `SqliteIndex` overrides with the real query.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn list_garbage_older_than(
        &self,
        _vault_id: &str,
        _cutoff_ms: i64,
    ) -> Result<Vec<NoteId>, GradatumError> {
        Ok(vec![])
    }

    /// Reads the current status of a note from `notes.status`.
    ///
    /// Returns `Some(status)` if the note exists, `None` otherwise.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(None)` — backend without an indexed status table (neutral no-op).
    /// `SqliteIndex` overrides with the real read.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn get_note_status(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Option<NoteStatus>, GradatumError> {
        Ok(None)
    }

    /// Reads the current section of a note from `notes.section`.
    ///
    /// Returns `Some(section)` if the note exists, `None` otherwise.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(None)` — backend without an indexed section table (neutral no-op).
    /// `SqliteIndex` overrides with the real read.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn get_note_section(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }

    /// Indicates whether a note is marked forgotten (`notes.forgotten`).
    ///
    /// Returns `true` if the note exists and is forgotten, `false` otherwise
    /// (absent or not forgotten).
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(false)` — backend without indexed semantic forget (neutral no-op).
    /// `SqliteIndex` overrides with the real read.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the query fails.
    async fn is_note_forgotten(
        &self,
        _vault_id: &str,
        _note_id: &str,
    ) -> Result<bool, GradatumError> {
        Ok(false)
    }

    // ── Code-map ───────────────────────────────────────────────────────────────

    /// Queries the code-ingest index for a `code-<project>` vault — **BM25-only**.
    ///
    /// Bypasses the mono-vault guard by design: the caller (handler `code_scope`)
    /// MUST validate the `code-` prefix BEFORE calling. No trust/decay/ANN scoring.
    /// Structured fields are read back from `extra_json["cs"]`.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(vec![])` — backend without code-ingest. `SqliteIndex` overrides.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error or vault_id without `code-` prefix.
    async fn code_scope_query(
        &self,
        _vault_id: &str,
        _selector: &CodeSelector,
        _limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        Ok(Vec::new())
    }

    /// Returns symbols that list `qualified_name` in their outgoing `deps`
    /// (reverse-dependency / callers lookup).
    ///
    /// ## Contract
    ///
    /// - Only scans the `code-*` vault specified by `vault_id`.
    /// - Results are sorted `qualified_name` ASC (deterministic, no lexical scoring).
    /// - `limit` caps the number of returned entries (protection against payload explosion
    ///   on widely-used symbols). Pass a site-specific cap (e.g. 32).
    /// - The caller MUST validate `vault_id` starts with `code-` before calling
    ///   (same invariant as [`IndexStore::code_scope_query`]).
    ///
    /// ## Default implementation
    ///
    /// Returns `Ok(vec![])` — backend without code-ingest. `SqliteIndex` overrides.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error or vault_id without `code-` prefix.
    async fn code_scope_reverse_deps(
        &self,
        _vault_id: &str,
        _qualified_name: &str,
        _limit: usize,
    ) -> Result<Vec<CodeScopeEntryRaw>, GradatumError> {
        Ok(Vec::new())
    }

    /// Batch reverse-dependency lookup for multiple symbols in one call.
    ///
    /// For each `qualified_name` in `names`, returns the list of symbols in `vault_id`
    /// whose outgoing `deps` JSON array contains that name. Results are grouped by
    /// `qualified_name` in a `HashMap`. Each list is capped at `limit` entries and
    /// sorted `qualified_name` ASC (deterministic).
    ///
    /// ## Default implementation
    ///
    /// Returns `Ok(HashMap::new())` — backends without code-ingest. `SqliteIndex` overrides.
    ///
    /// ## Contract
    ///
    /// The caller MUST validate `vault_id` starts with `code-` before calling.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error or vault_id without `code-` prefix.
    async fn code_scope_reverse_deps_batch(
        &self,
        _vault_id: &str,
        _names: &[&str],
        _limit: usize,
    ) -> Result<std::collections::HashMap<String, Vec<String>>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Returns the last recorded `ingested_sha` for a code vault.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(None)`. `SqliteIndex` overrides.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error.
    async fn get_last_ingested_sha(
        &self,
        _vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }

    /// Returns the absolute path of the git repository for a code vault (drift detection).
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(None)` → drift detection skipped (accuracy over coverage: no false Fresh).
    /// `SqliteIndex` overrides.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error.
    async fn get_code_vault_repo_path(
        &self,
        _vault_id: &str,
    ) -> Result<Option<String>, GradatumError> {
        Ok(None)
    }

    /// Counts notes matching an FTS5/BM25 query in the filtered scope — **lexical only**.
    ///
    /// Used by `vault_search` when `include_corpus_count=true` to distinguish
    /// "topic absent" (`0`) from "notes below K" (`> len(results)`).
    ///
    /// ## Contract
    ///
    /// - Same 5 predicates as `search_fts_with_snippet`: `vault_id`, `MATCH`, `section?`,
    ///   `locus?`, `status`, `downgraded?` (parity guaranteed by construction — both share
    ///   the same `build_fts_where_parts` function).
    /// - `LIMIT 10001`: if > 10000 results, returns `(10000, true)` (capped).
    /// - Executed ONLY if `include_corpus_count=true` — zero overhead by default.
    /// - Matches only BM25/FTS5 hits (not ANN) → `corpus_match_count < len(results)`
    ///   is a nominal case when the embedder is active (semantic-only hits).
    ///
    /// ## Return
    ///
    /// `(count, capped)` — `count ≤ 10000`, `capped=true` if the real corpus is ≥ 10000.
    ///
    /// # Default implementation
    ///
    /// Returns `Ok((0, false))` — backend without FTS (neutral no-op).
    /// `SqliteIndex` overrides with the real COUNT.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error.
    async fn count_fts_matches(
        &self,
        _vault_id: &AclCheckedVaultId,
        _query: &str,
        _include_downgraded: bool,
        _section: Option<&str>,
        _locus: Option<&str>,
        _status: Option<&str>,
    ) -> Result<(u64, bool), GradatumError> {
        Ok((0, false))
    }

    /// Returns the stored `content_hash_source` values for a set of `source_path` entries
    /// in a code vault (drift detection).
    ///
    /// The handler compares these hashes against the current on-disk file hashes to flag
    /// stale entries. Cost is bounded: only the distinct paths from the result are fetched.
    ///
    /// # Default implementation
    ///
    /// Returns an empty map → no comparison possible → entries not flagged
    /// (consistent with absent repo_path: clean drift-detection skip).
    /// `SqliteIndex` overrides.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on SQLite error.
    async fn code_freshness_hashes(
        &self,
        _vault_id: &str,
        _source_paths: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Applies the index mutations of a persist/curate step atomically.
    ///
    /// ## Atomicity contract
    ///
    /// - If any one mutation fails, **all** index mutations are rolled back.
    /// - It does **not** cover the vault write (the Markdown file on disk). That write
    ///   happens first, is idempotent (copy-on-write plus `.history`), and stays
    ///   consistent even when the index transaction fails: the state remains replayable,
    ///   because the worker retries the job, finds the Markdown already there and retries
    ///   the indexing step.
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(())` — enough for test and mock implementations.
    /// The concrete `SqliteIndex` overrides it with a real transaction
    /// (`unchecked_transaction`) that guarantees atomicity.
    ///
    /// ## Parameters
    ///
    /// - `note_id`: identifier of the target note.
    /// - `title`: H1 title extracted from the body (upserted into `notes.title`).
    /// - `temporal`: optional temporal entry (`temporal_index`).
    /// - `links`: the outgoing edges to (re)insert, bundled with their authority
    ///   (see [`CuratedLinks`]). When `links.authoritative` is `true` the concrete
    ///   implementation DELETEs every pre-existing outgoing edge of this note (scoped
    ///   `src_note_id` + `vault_id`) inside the same transaction before inserting, so the
    ///   graph reflects the current body; when `false` the edges are only upserted and
    ///   **nothing is deleted** (historical `INSERT OR IGNORE` behaviour).
    /// - `trust`: optional trust score (`notes.trust`).
    /// - `vault_id`: the tenant — used for `note_links.vault_id` and
    ///   `temporal_index.vault_id`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if any mutation fails. The concrete `SqliteIndex`
    /// implementation guarantees that the whole batch is rolled back in that case.
    async fn persist_curated_index_atomic(
        &self,
        _note_id: &NoteId,
        _title: &str,
        _temporal: Option<&TemporalEntry>,
        _links: CuratedLinks<'_>,
        _trust: Option<f32>,
        _vault_id: &str,
    ) -> Result<(), GradatumError> {
        // Default : no-op — utilisé par les mocks de test.
        // SqliteIndex override avec une vraie transaction atomique.
        Ok(())
    }

    /// Fills the ANN table `note_embeddings_ann` (vec0) from `note_embeddings`.
    ///
    /// Idempotent: vectors already present are skipped (upsert). Called once at server
    /// boot, right after the sqlite-vec extension is registered, so that an existing
    /// corpus is immediately queryable in ANN mode without a manual re-index.
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(0)` — for brute-force backends and test mocks.
    /// `SqliteIndex` overrides it by delegating to `sqlite_vec::backfill_ann_from_conn`.
    ///
    /// ## Returns
    ///
    /// The number of vectors actually written to the ANN table (`0` = already up to date).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite read or write fails. In production the
    /// error is non-fatal: the server carries on in degraded brute-force mode.
    async fn backfill_ann_index(&self) -> Result<u64, GradatumError> {
        // Default : no-op — BruteForce path et mocks de test.
        Ok(0)
    }

    /// Garbage-collects orphan ANN vectors (`note_embeddings_ann`) of the given `vault_id`
    /// partition, whose `note_id` no longer exists in that vault's `notes`. The sweep is
    /// **partition-scoped**: only the target vault's vectors are considered.
    ///
    /// Idempotent one-shot safety net for orphans created before the atomic ANN
    /// cascade landed. Called at server boot after [`Self::backfill_ann_index`], once per
    /// active vault when `multi_tenant` is enabled (and once for `"main"` when it is not).
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(0)` — for brute-force backends and test mocks. `SqliteIndex`
    /// overrides it with the real `DELETE ... WHERE vault_id = ?1 AND note_id NOT IN
    /// (SELECT id FROM notes WHERE vault_id = ?1)`.
    ///
    /// ## Returns
    ///
    /// The number of orphan vectors actually deleted in this partition (`0` = nothing to
    /// clean up, or ANN inactive in degraded mode).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` on a SQLite error other than a missing ANN table or
    /// module. In production the boot caller treats the error as non-fatal.
    async fn gc_orphan_ann(&self, _vault_id: &VaultId) -> Result<u64, GradatumError> {
        // Default : no-op — BruteForce path et mocks de test.
        Ok(0)
    }

    /// Boot health gate for the derived ANN index — **fail-closed on the ANN path**.
    ///
    /// Compares, per partition `(vault_id, embedder_id)`, the number of pairs eligible in
    /// `note_embeddings` (source of truth) with the number of rows present in
    /// `note_embeddings_ann` (derived index), and returns one [`AnnPartitionDeficit`] per
    /// partition where rows are **missing**. Called at boot right after
    /// [`Self::backfill_ann_index`] and the [`Self::gc_orphan_ann`] sweep.
    ///
    /// ## Why a gate rather than a warning
    ///
    /// The semantic search path falls back to brute force on `Err` only, never on
    /// `Ok(vec![])`: an ANN partition that is empty or partially filled returns *fewer*
    /// neighbours — or none — with no error, so recall silently collapses. Implementations
    /// therefore **disable the ANN path** (brute force takes over, results stay exact and
    /// complete) as soon as a deficit is found, and also when the measurement itself cannot
    /// conclude (`Err`): a gate that did not run is not a pass. Boot is never interrupted —
    /// a slow axis is recoverable, a mute one is not.
    ///
    /// ## Runs only when ANN is enabled
    ///
    /// Implementations MUST return `Ok(Vec::new())` without issuing any query when the ANN
    /// path is disabled, so a brute-force deployment pays nothing and logs nothing.
    ///
    /// ## Cost
    ///
    /// Two grouped `COUNT(*)` aggregates over metadata columns — no vector is decoded.
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(Vec::new())` — brute-force backends and test mocks have no
    /// derived index to keep in sync. `SqliteIndex` overrides it.
    ///
    /// ## Returns
    ///
    /// One entry per deficient partition, ordered by `(vault_id, embedder_id)`. An empty
    /// vector means the derived index covers the whole eligible corpus (or ANN is off).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the measurement fails for any reason other than a missing
    /// ANN table or extension (that case is the documented brute-force degradation, not a
    /// deficit). The ANN path is closed before the error is returned.
    async fn ann_health_gate(&self) -> Result<Vec<AnnPartitionDeficit>, GradatumError> {
        // Default : no-op — BruteForce path et mocks de test (aucun index dérivé à garder
        // en cohérence). Aucune requête émise.
        Ok(Vec::new())
    }

    // ── Santé des tâches récurrentes (v0.7.5 F-85) ──────────────────────────

    /// Records one tick of a recurring task into `scheduled_task_health`.
    ///
    /// Semantics:
    /// - Upsert into `scheduled_task_health` (primary key `task_name`): `run_count + 1`,
    ///   `last_run_ms`, `last_outcome`, `last_duration_ms`, `last_error`, `updated_at`.
    /// - When `outcome == Error`, one row is appended to `scheduled_task_error`, together
    ///   with a lazy purge (`DELETE WHERE occurred_ms < now - 7 days`).
    ///
    /// ## Must not break the caller
    ///
    /// Callers must **not** propagate an error from this method inside a Tokio task:
    /// log it at `warn` level and carry on. A task must never die because of its own
    /// instrumentation.
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(())` — for mocks and backends without a
    /// `scheduled_task_health` table. `SqliteIndex` overrides it with the real upsert.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite write fails.
    async fn record_task_run(
        &self,
        _task_name: &str,
        _outcome: TaskOutcome,
        _duration_ms: i64,
        _error: Option<&str>,
        _now_ms: i64,
    ) -> Result<(), GradatumError> {
        Ok(())
    }

    /// Seeds the row of a task in `scheduled_task_health` at boot.
    ///
    /// Uses `INSERT OR IGNORE`, so an existing record is never overwritten. Called at
    /// server boot so that every known task shows up in the health endpoint with
    /// `last_run_ms = null` from the very start.
    ///
    /// ## Default implementation
    ///
    /// A no-op returning `Ok(())` — for mocks and backends without the table.
    /// `SqliteIndex` overrides it with the real `INSERT OR IGNORE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite write fails.
    async fn seed_scheduled_task(&self, _task_name: &str) -> Result<(), GradatumError> {
        Ok(())
    }

    /// Lists the health of every seeded recurring task.
    ///
    /// `errors_24h` is computed as `COUNT(occurred_ms > now_ms - 86_400_000)` over
    /// `scheduled_task_error`.
    ///
    /// ## Default implementation
    ///
    /// Returns `Ok(vec![])` — for mocks and backends without the table.
    /// `SqliteIndex` overrides it with the real query.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite read fails.
    async fn list_scheduled_health(
        &self,
        _now_ms: i64,
    ) -> Result<Vec<ScheduledTaskHealth>, GradatumError> {
        Ok(vec![])
    }

    /// Inserts a batch of metric samples (time series). Default: no-op `Ok(0)` for mocks;
    /// `SqliteIndex` overrides it with the real `INSERT OR IGNORE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite write fails.
    async fn insert_metric_samples(
        &self,
        _ts_ms: i64,
        _samples: &[(String, f64)],
    ) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Downsampled time-series query (mean per bucket). Default: `Ok(vec![])` for mocks.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite read fails.
    async fn query_metric_timeseries(
        &self,
        _series: &[String],
        _from_ms: i64,
        _to_ms: i64,
        _bucket_ms: i64,
    ) -> Result<Vec<MetricSamplePoint>, GradatumError> {
        Ok(vec![])
    }

    /// Purges the samples older than `cutoff_ms`. Default: no-op `Ok(0)` for mocks.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite delete fails.
    async fn purge_metric_samples(&self, _cutoff_ms: i64) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Lists the distinct series present (catalogue). Default: `Ok(vec![])` for mocks.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite read fails.
    async fn list_distinct_metric_series(&self) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }

    /// Projected note scan for the retrospective audit and dedup pass.
    ///
    /// Returns up to `limit` notes of `vault_id`, **excluding** all
    /// [`crate::section::Section::PROTECTED_DELETE`] sections (defense in depth: the
    /// audit never even sees a governance note) and sentinel rows, each with its body
    /// embedding when available. Default no-op `Ok(vec![])` for mocks.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the SQLite query fails.
    async fn audit_scan(
        &self,
        _vault_id: &str,
        _limit: usize,
    ) -> Result<Vec<AuditScanRow>, GradatumError> {
        Ok(vec![])
    }
}
