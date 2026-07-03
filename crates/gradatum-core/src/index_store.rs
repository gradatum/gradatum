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
use crate::scope::{OverrideScope, VaultId};
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

/// Full-text search, override, and checksum storage contract — async, thread-safe.
///
/// Implemented by `gradatum-index::SqliteIndex`.
///
/// ## Stability
///
/// `#[stability::unstable]` — the API may change before v1.0.0.
/// `GradatumError` will converge to a dedicated `StoreError` (see `QueueStore`) in a future release.
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
        reason = "filtres de recherche orthogonaux (F-37+F-65) — struct d'options sans gain"
    )]
    async fn search_fts_with_snippet(
        &self,
        vault_id: &VaultId,
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

    /// Vérifie qu'une note existe et est `live` par son identifiant (ULID string).
    ///
    /// Utilisé pour la résolution ULID-first des wikilinks `[[section:ULID]]` :
    /// quand le wikilink contient directement un ULID, cette méthode confirme
    /// l'existence sans passer par la correspondance H1 (titre Markdown).
    ///
    /// Retourne `Some(id)` si la note existe et est `live`, `None` sinon.
    /// Exclut les sentinelles et les notes non-live (archived, garbage, etc.).
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

    /// Liste les notes par STATUT (métadonnées, inclut `downgraded`).
    ///
    /// Contrairement à [`Self::list_notes`], n'exclut PAS les notes `downgraded` —
    /// c'est l'objet de cette méthode (browse archived/downgraded, fix drill-down studio).
    ///
    /// - `statuses` : ensemble de statuts à inclure. Vide → `Ok((vec![], 0))`.
    /// - `section` : filtre optionnel sur la section.
    /// - `cursor` : dernier ULID reçu (exclusif). `None` ou `""` = début.
    /// - `limit` : cappé à `[1, 200]` par l'implémentation.
    ///
    /// Implémentation par défaut : retourne `Ok((vec![], 0))` — uniquement l'index SQLite
    /// réel fournit des résultats. Les mocks de test héritent du défaut sans modification.
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
        vault_id: &str,
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
        _vault_id: &str,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, String>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Batch-reads `anchor_ms` from `temporal_index` for a list of note IDs (F-65).
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
    /// hits** whenever bounds are active — making the sémantique branch fail-closed.
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
        _vault_id: &str,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, i64>, GradatumError> {
        Ok(std::collections::HashMap::new())
    }

    /// Reads the trust score for a note from the `notes.trust` column.
    ///
    /// Returns `Some(trust)` if the note exists and the column is non-NULL,
    /// `None` if the note is absent or trust has not been set.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn get_trust(&self, id: &NoteId) -> Result<Option<f32>, GradatumError>;

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
    /// `renamed_at_ms`: rename timestamp in Unix epoch milliseconds.
    ///
    /// Idempotent: `INSERT OR REPLACE` — if a redirect exists for this slug,
    /// it is replaced by the new ULID (last rename wins).
    ///
    /// Called by `gradatum-admin vault rename` after each rename.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the INSERT fails.
    async fn upsert_redirect(
        &self,
        slug: &str,
        ulid: &Ulid,
        renamed_at_ms: i64,
    ) -> Result<(), GradatumError>;

    /// Resolves an old-title slug to its current ULID via `redirect_table`.
    ///
    /// Returns `Some(ulid)` if the slug exists in the redirect table,
    /// `None` if no redirect is registered for this slug.
    ///
    /// The slug is obtained via `title_to_slug(old_title)` (lowercase + spaces→hyphens).
    ///
    /// Used by the read layer (handler `vault_read`) as a fallback when `title_lookup`
    /// fails — transparent resolution after a rename.
    ///
    /// # Errors
    ///
    /// Returns `GradatumError::Storage` if the SQLite query fails.
    async fn resolve_redirect(&self, slug: &str) -> Result<Option<Ulid>, GradatumError>;

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
        vault_id: &str,
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
    /// Idempotent: `UPDATE notes SET trust = ?2 WHERE id = ?1`.
    ///
    /// Returns the number of affected rows (`0` if the note is absent — non-fatal
    /// for the caller: the static `provenance` value remains in place).
    ///
    /// # Default implementation
    ///
    /// Returns `Ok(0)` — backend without computed trust support (neutral no-op).
    /// `SqliteIndex` overrides with the real `UPDATE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` if the `UPDATE` fails.
    #[must_use = "le nombre de lignes affectées indique si la note existait"]
    async fn set_note_trust(&self, _id: &NoteId, _trust: f32) -> Result<usize, GradatumError> {
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
        vault_id: &VaultId,
        filter: &crate::temporal_query::TimelineFilter,
    ) -> Result<Vec<crate::temporal_query::TimelineRow>, GradatumError>;

    /// Deletes a redirect by target ULID from `redirect_table`.
    ///
    /// Used during note purge/forget to clean up redirects pointing to it.
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
    async fn delete_redirect_by_ulid(&self, _ulid_str: &str) -> Result<usize, GradatumError> {
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
        _vault_id: &VaultId,
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

    /// Mutations index atomiques pour persist/curated.
    ///
    /// ## Contrat d'atomicité
    ///
    /// - Si une mutation échoue → TOUTES les mutations index sont rollback.
    /// - Ne couvre PAS le vault write (markdown sur disque) — celui-ci est
    ///   écrit avant, est idempotent (CoW + .history), et reste cohérent
    ///   même si la tx index échoue : l'état est ré-exécutable (le worker
    ///   re-tentera le job, retrouvera le markdown présent et re-tentera index).
    ///
    /// ## Implémentation default
    ///
    /// No-op `Ok(())` — suffisant pour les implémentations de test/mock.
    /// L'implémentation concrète `SqliteIndex` override avec une vraie transaction
    /// (`unchecked_transaction`) pour une atomicité garantie.
    ///
    /// ## Paramètres
    ///
    /// - `note_id`: identifiant de la note cible.
    /// - `title`: titre H1 extrait du corps (upsert dans `notes.title`).
    /// - `temporal`: entrée temporelle optionnelle (`temporal_index`).
    /// - `links`: paires `(src_note_id, dst_note_id)` à insérer dans `note_links`.
    /// - `trust`: confiance optionnelle (`notes.trust`).
    /// - `vault_id`: tenant — utilisé pour `note_links.vault_id`
    ///   et `temporal_index.vault_id`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si l'une des mutations échoue.
    /// L'implémentation concrète (`SqliteIndex`) garantit le rollback de
    /// toutes les mutations du lot en cas d'échec.
    async fn persist_curated_index_atomic(
        &self,
        _note_id: &NoteId,
        _title: &str,
        _temporal: Option<&TemporalEntry>,
        _links: &[(String, String)],
        _trust: Option<f32>,
        _vault_id: &str,
    ) -> Result<(), GradatumError> {
        // Default : no-op — utilisé par les mocks de test.
        // SqliteIndex override avec une vraie transaction atomique.
        Ok(())
    }

    /// Remplit la table ANN `note_embeddings_ann` (vec0) à partir de `note_embeddings`.
    ///
    /// Opération idempotente — les vecteurs déjà présents sont ignorés (upsert).
    /// Appelée une seule fois au boot du serveur, après l'enregistrement de l'extension
    /// sqlite-vec, pour garantir qu'un corpus existant est immédiatement interrogeable
    /// en mode ANN sans nécessiter un re-index manuel.
    ///
    /// ## Implémentation default
    ///
    /// No-op `Ok(0)` — pour les backends brute-force et les mocks de test.
    /// `SqliteIndex` override en déléguant à `sqlite_vec::backfill_ann_from_conn`.
    ///
    /// ## Retour
    ///
    /// Nombre de vecteurs effectivement écrits dans la table ANN (0 = déjà à jour).
    ///
    /// ## Erreurs
    ///
    /// `GradatumError::Storage` si la lecture ou l'écriture SQLite échoue.
    /// En production, l'erreur est non-fatale (le serveur continue en dégradé brute-force).
    async fn backfill_ann_index(&self) -> Result<u64, GradatumError> {
        // Default : no-op — BruteForce path et mocks de test.
        Ok(0)
    }

    // ── Santé des tâches récurrentes (v0.7.5 F-85) ──────────────────────────

    /// Enregistre un tick d'une tâche récurrente dans `scheduled_task_health`.
    ///
    /// Sémantique :
    /// - Upsert `scheduled_task_health` (PK `task_name`) : `run_count + 1`,
    ///   `last_run_ms`, `last_outcome`, `last_duration_ms`, `last_error`, `updated_at`.
    /// - Si `outcome == Error` : append 1 ligne dans `scheduled_task_error`
    ///   + purge paresseuse (`DELETE WHERE occurred_ms < now - 7j`).
    ///
    /// ## Infaillibilité
    ///
    /// L'appelant ne doit PAS propager une erreur de cette méthode dans une tâche tokio —
    /// logguer en `warn` et continuer. La tâche ne doit jamais paniquer à cause de
    /// l'instrumentation.
    ///
    /// ## Implémentation default
    ///
    /// No-op `Ok(())` — backends sans table `scheduled_task_health` (mocks, tests).
    /// `SqliteIndex` override avec le vrai upsert.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si le write SQLite échoue.
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

    /// Initialise la ligne d'une tâche dans `scheduled_task_health` (seed boot).
    ///
    /// `INSERT OR IGNORE` — n'écrase pas un enregistrement existant.
    /// Appelé au boot du serveur pour garantir que toutes les tâches connues
    /// apparaissent dans l'endpoint avec `last_run_ms = null` dès le démarrage.
    ///
    /// ## Implémentation default
    ///
    /// No-op `Ok(())` — mocks et backends sans la table.
    /// `SqliteIndex` override avec le vrai `INSERT OR IGNORE`.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si le write SQLite échoue.
    async fn seed_scheduled_task(&self, _task_name: &str) -> Result<(), GradatumError> {
        Ok(())
    }

    /// Liste la santé de toutes les tâches récurrentes seedées.
    ///
    /// `errors_24h` est calculé comme `COUNT(occurred_ms > now_ms - 86_400_000)`
    /// depuis `scheduled_task_error`.
    ///
    /// ## Implémentation default
    ///
    /// Retourne `Ok(vec![])` — backends sans la table (mocks, tests).
    /// `SqliteIndex` override avec la vraie requête.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si la lecture SQLite échoue.
    async fn list_scheduled_health(
        &self,
        _now_ms: i64,
    ) -> Result<Vec<ScheduledTaskHealth>, GradatumError> {
        Ok(vec![])
    }

    /// Insère un lot de samples métriques (timeseries). Default no-op `Ok(0)` (mocks).
    /// `SqliteIndex` override avec le vrai INSERT OR IGNORE.
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si l'écriture SQLite échoue.
    async fn insert_metric_samples(
        &self,
        _ts_ms: i64,
        _samples: &[(String, f64)],
    ) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Requête timeseries downsamplée (moyenne par bucket). Default `Ok(vec![])` (mocks).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si la lecture SQLite échoue.
    async fn query_metric_timeseries(
        &self,
        _series: &[String],
        _from_ms: i64,
        _to_ms: i64,
        _bucket_ms: i64,
    ) -> Result<Vec<MetricSamplePoint>, GradatumError> {
        Ok(vec![])
    }

    /// Purge les samples antérieurs à `cutoff_ms`. Default no-op `Ok(0)` (mocks).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si la suppression SQLite échoue.
    async fn purge_metric_samples(&self, _cutoff_ms: i64) -> Result<usize, GradatumError> {
        Ok(0)
    }

    /// Liste les séries distinctes présentes (catalog). Default `Ok(vec![])` (mocks).
    ///
    /// # Errors
    ///
    /// `GradatumError::Storage` si la lecture SQLite échoue.
    async fn list_distinct_metric_series(&self) -> Result<Vec<String>, GradatumError> {
        Ok(vec![])
    }
}
