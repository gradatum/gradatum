//! DTOs for API v1 — strict wire-compatibility with the legacy vault.
//!
//! `Vault*Request` structs live in `gradatum-dto` (single source of truth
//! for HTTP wire contracts shared with `gradatum-mcp-stub` and `gradatum-sdk-rs`).
//! `Vault*Response` structs are kept local — strictly server-internal.
//!
//! Covered endpoints:
//! - `POST /api/v1/vault_search`  → [`VaultSearchRequest`] / [`VaultSearchResponse`]
//! - `POST /api/v1/vault_read`    → [`VaultReadRequest`] / [`VaultReadResponse`]
//! - `POST /api/v1/vault_list`    → [`VaultListRequest`] / [`VaultListResponse`]
//! - `GET  /api/v1/vault_status`  → [`VaultStatusResponse`]
//! - `POST /api/v1/vault_graph`   → [`VaultGraphRequest`] / [`VaultGraphResponse`]
//! - `GET  /api/v1/vault_links`   → thin alias for `vault_graph` at depth=1
//! - `POST /api/v1/vault_trace`   → [`VaultTraceRequest`] / [`VaultTraceResponse`]
//! - `POST /api/v1/vault_context` → [`VaultContextRequest`] / [`VaultContextResponse`]
//! - `GET  /api/v1/vault_authors` → [`VaultAuthorsResponse`]
//! - `GET  /api/v1/vault_tags`    → [`VaultTagsResponse`]

use serde::Serialize;

// ── Re-exports depuis gradatum-dto (single source of truth) ───────────────────
pub use gradatum_dto::{
    SessionTraceRequest, SessionTraceResponse, VaultClassifyRequest, VaultContextRequest,
    VaultDowngradeRequest, VaultGraphRequest, VaultLinksRequest, VaultListRequest,
    VaultReadRequest, VaultSearchRequest, VaultTimelineRequest, VaultTraceRequest,
    VaultWriteRequest,
};

// ── vault_search ─────────────────────────────────────────────────────────────

/// Detailed breakdown of the composite score for a single search result.
///
/// Exposed **only** when the request carries `include_scores: true`.
/// Reflects every factor actually computed by the scoring pipeline
/// (`gradatum_search::scoring`), without reranking (the reranker is `NoopReranker`
/// by default). These fields allow the studio to unroll the formula:
///
/// `composite = rrf_score × (1 + α·recency_factor) × (1 + β·pagerank_factor) × (1 + γ·trust)`
///
/// with α=0.2, β=0.1, γ=0.15, k=60. No `rerank` column is exposed
/// (always a no-op and therefore misleading).
#[derive(Debug, Serialize)]
pub struct ScoreBreakdown {
    /// Raw RRF score (BM25 + semantic fusion) — `Σ 1/(k + rank_i)`, k=60.
    pub rrf_score: f64,
    /// Temporal decay factor (recency) ∈ `(0.0, 1.0]`.
    pub recency_factor: f64,
    /// Normalised PageRank in-degree factor ∈ `[0.0, 1.0]`.
    pub pagerank_factor: f64,
    /// Number of backlinks (in-degree) for the note in the graph.
    pub in_degree: u64,
    /// Raw trust score of the source (`0.0–1.0`), `None` if unresolved or decay disabled.
    ///
    /// Distinct from the legacy `SearchHit.trust` field (hardcoded `0.5`, deprecated):
    /// `trust_raw` is the **actual** value read from the note and fed into scoring.
    pub trust_raw: Option<f32>,
    /// Trust after temporal decay, `None` if trust is absent or decay is disabled.
    pub trust_decayed: Option<f64>,
    /// Final composite score (equals the value of `SearchHit.score`).
    pub composite: f64,
    /// Zero-based rank in the BM25 signal, `None` if the note is absent from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bm25_rank: Option<u32>,
    /// Zero-based rank in the semantic signal, `None` if the note is absent from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sem_rank: Option<u32>,
}

/// Individual search result, enriched with the H1 title extracted after curation.
///
/// Allows clients to know the human-readable title without reading the full note.
#[derive(Debug, Serialize)]
pub struct SearchHit {
    /// Path of the note (e.g. `"decisions/my-note"`).
    pub path: String,
    /// Relevance score — RRF (Reciprocal Rank Fusion): `Σ 1/(k + rank_i)` with k=60.
    ///
    /// Not bounded to `[0.0, 1.0]` — depends on the number of combined signals
    /// and the position in each ranked list. The maximum RRF score (rank=0,
    /// single signal) is `1/60 ≈ 0.0167`. Scores are comparable within a single
    /// query result set but not across different queries.
    pub score: f32,
    /// H1 title of the note (extracted after curation, may be absent — serialised as `null`).
    pub title: Option<String>,
    /// Note excerpt (native FTS5 snippet).
    pub snippet: Option<String>,
    /// **Legacy / deprecated (since v0.4.8)** — Trust score hardcoded to `0.5`.
    ///
    /// This field has always been `0.5` (neutral) and does NOT reflect the actual
    /// trust of the source. The real value is exposed in `scores.trust_raw`
    /// (and `scores.trust_decayed`) when `include_scores: true`.
    ///
    /// Kept for wire backward-compatibility (existing clients reading `trust`).
    /// Marked `#[deprecated]`: all new code should read `scores.trust_raw`. The
    /// serde serialisation is unaffected by the attribute (the field is still emitted
    /// on the wire). Final removal is planned for a future major version.
    #[deprecated(
        since = "0.4.8",
        note = "valeur legacy hardcodée 0.5 ; utiliser scores.trust_raw (v0.4.4+)"
    )]
    pub trust: f32,
    /// Raw SQL status of the note — kebab-case (`"live"`, `"pending-review"`,
    /// `"downgraded"`, …). Additive field: allows the studio to display a status
    /// badge without re-reading the note. Empty string if unresolved (rare degraded
    /// case: semantic-only hit whose batch `get_statuses` call failed).
    pub status: String,
    /// Score breakdown — present **only** when the request carries
    /// `include_scores: true`. Omitted otherwise (fully backward-compatible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scores: Option<ScoreBreakdown>,
}

/// Response for `vault_search`.
#[derive(Debug, Serialize)]
pub struct VaultSearchResponse {
    /// Search result list (may be empty).
    pub items: Vec<SearchHit>,
    /// Total number of notes matching the **FTS5/BM25 lexical** query within the
    /// filtered scope, unbounded by K. Present only when the request carries
    /// `include_corpus_count: true`; `None` otherwise (DTO wire format unchanged).
    ///
    /// Does NOT indicate relevance or ANN semantic hits.
    /// `corpus_match_count == 0` means the subject is absent from the corpus (lexically).
    /// `corpus_match_count > len(items)` means matches exist beyond the K limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corpus_match_count: Option<u64>,
    /// `true` when `corpus_match_count` was capped at 10 000 (the real corpus contains
    /// ≥ 10 000 matches). Omitted from the response when `false` (backward-compatible).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub corpus_count_capped: bool,
}

// ── vault_read ────────────────────────────────────────────────────────────────

/// Response for `vault_read`.
#[derive(Debug, Serialize)]
pub struct VaultReadResponse {
    /// Path of the note that was read.
    pub path: String,
    /// Title of the note.
    ///
    /// Primary source: the `title` column in the SQLite index via `get_titles_sections`
    /// — identical to what `vault_search` returns, ensuring round-trip consistency
    /// between `vault_read` and `vault_search`.
    ///
    /// Fallback: the first line of the body if it starts with `"# "` (no indentation),
    /// aligned with the canonical SQL definition of `title_lookup`
    /// (`body_text LIKE '# %'` — H1 at the very start of the body only).
    /// An H1 on line 2 or indented does NOT produce a title (SQL ↔ runtime consistency).
    ///
    /// `None` when neither the index nor a leading H1 line can derive a title.
    pub title: Option<String>,
    /// Markdown content of the note.
    pub content: String,
    /// YAML frontmatter metadata (serialised as JSON for transport).
    pub metadata: Option<serde_json::Value>,
    /// Content size in bytes.
    pub size_bytes: u64,
    /// SHA-256 of the content (hex, 64 chars).
    pub sha256: String,
}

// ── vault_list ────────────────────────────────────────────────────────────────

/// Individual entry in a vault listing.
#[derive(Debug, Serialize)]
pub struct VaultEntry {
    /// Path of the note.
    pub path: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Last-modified timestamp (ISO 8601 UTC).
    pub modified_at: String,
}

/// Response for `vault_list`.
#[derive(Debug, Serialize)]
pub struct VaultListResponse {
    /// Listed entries.
    pub entries: Vec<VaultEntry>,
    /// Cursor for the next page (absent when at the end of the list).
    pub next_cursor: Option<String>,
    /// Total number of notes (unpaginated).
    pub total: u64,
}

// ── vault_status ──────────────────────────────────────────────────────────────

/// Response for `vault_status` (GET, no request body).
#[derive(Debug, Serialize)]
pub struct VaultStatusResponse {
    /// Tenant identifier.
    pub tenant_id: String,
    /// Number of indexed notes.
    pub note_count: u64,
    /// Total size of all notes in bytes.
    pub total_size_bytes: u64,
    /// Index schema version (e.g. `"v1"`).
    pub index_version: String,
    /// Timestamp of the last re-index run (ISO 8601 UTC).
    pub last_indexed_at: Option<String>,
    /// Vault health (`"healthy"` / `"degraded"` / `"offline"`).
    pub health: String,
}

// ── vault_graph ───────────────────────────────────────────────────────────────

/// Graph edge (link between two notes).
#[derive(Debug, Serialize)]
pub struct GraphEdge {
    /// Source note.
    pub from: String,
    /// Target note.
    pub to: String,
    /// Link type (e.g. `"wikilink"`, `"embed"`).
    pub kind: String,
}

/// Response for `vault_graph`.
#[derive(Debug, Serialize)]
pub struct VaultGraphResponse {
    /// Graph nodes (note paths).
    pub nodes: Vec<String>,
    /// Graph edges.
    pub edges: Vec<GraphEdge>,
}

// ── vault_links (alias thin vault_graph depth=1) ──────────────────────────────

/// Response for `vault_links` — same structure as [`VaultGraphResponse`].
pub type VaultLinksResponse = VaultGraphResponse;

// ── vault_trace ───────────────────────────────────────────────────────────────

/// Individual trace result entry.
#[derive(Debug, Serialize)]
pub struct TraceEntry {
    /// Path of the note.
    pub path: String,
    /// Relevance score (0.0–1.0).
    pub score: f32,
    /// Contextual excerpt of the note.
    pub snippet: Option<String>,
    /// Tags attached to the note.
    pub tags: Vec<String>,
}

/// Response for `vault_trace`.
#[derive(Debug, Serialize)]
pub struct VaultTraceResponse {
    /// Trace result entries.
    pub entries: Vec<TraceEntry>,
}

// ── vault_context ─────────────────────────────────────────────────────────────

/// Response for `vault_context`.
#[derive(Debug, Serialize)]
pub struct VaultContextResponse {
    /// Context formatted for injection into an LLM prompt.
    pub context: String,
    /// Estimated token count for the context.
    pub estimated_tokens: u32,
    /// Source notes used to build the context (note paths).
    pub sources: Vec<String>,
}

// ── vault_authors ─────────────────────────────────────────────────────────────

/// Response for `vault_authors` (GET, no request body).
#[derive(Debug, Serialize)]
pub struct VaultAuthorsResponse {
    /// Distinct authors identified in the vault.
    pub authors: Vec<AuthorEntry>,
}

/// Author entry.
#[derive(Debug, Serialize)]
pub struct AuthorEntry {
    /// Author name or identifier.
    pub name: String,
    /// Number of notes attributed to this author.
    pub note_count: u64,
}

// ── vault_tags ────────────────────────────────────────────────────────────────

/// Response for `vault_tags` (GET, no request body).
#[derive(Debug, Serialize)]
pub struct VaultTagsResponse {
    /// Distinct tags with their usage frequency.
    pub tags: Vec<TagEntry>,
}

/// Tag entry.
#[derive(Debug, Serialize)]
pub struct TagEntry {
    /// Tag value (e.g. `"architecture"`, `"urgent"`).
    pub tag: String,
    /// Number of notes carrying this tag.
    pub note_count: u64,
}

// ── DTOs write (P2.0b — async 202 enqueue pattern) ────────────────────────────

/// 202 Accepted response — job enqueued (legacy queue, integer SQLite job ID).
#[derive(Debug, Serialize)]
pub struct EnqueuedResponse {
    /// Job identifier in the legacy queue (auto-incremented SQLite integer).
    pub job_id: i64,
    /// Immediate status (`"queued"`).
    pub status: &'static str,
    /// Poll URL to track job status (`/api/v1/jobs/<id>`).
    pub poll_url: String,
}

/// 202 Accepted response — job enqueued via `gradatum_jobs` (ULID string job ID).
///
/// Used by handlers bridged to `state.job_store`. Unlike [`EnqueuedResponse`],
/// the `job_id` is a 26-character alphanumeric ULID.
///
/// The `note_id` field is the ULID pre-allocated at enqueue time — it can be used
/// immediately in a `vault_read` call without waiting for job completion.
/// This ULID matches the `note_id` stored by the curate worker via
/// `write_note_with_id`. Additive and backward-compatible: clients that ignore
/// `note_id` are unaffected.
#[derive(Debug, Serialize)]
pub struct EnqueuedResponseUlid {
    /// Job identifier in `gradatum_jobs` (26-char ULID).
    pub job_id: String,
    /// Immediate status (`"queued"`).
    pub status: &'static str,
    /// Poll URL to track job status (`/api/v1/jobs/<id>/v2`).
    pub poll_url: String,
    /// Pre-allocated note ULID — usable directly in `vault_read`
    /// without waiting for the curate job to complete.
    ///
    /// Format: 26-char alphanumeric ULID (e.g. `"01HZ..."`).
    /// Guaranteed to match the `note_id` stored by the worker.
    pub note_id: String,
}

/// Response for `GET /api/v1/jobs/<id>` — job status.
#[derive(Debug, Serialize)]
pub struct JobStatusResponse {
    /// Job identifier.
    pub job_id: i64,
    /// Current status (`"pending"` | `"leased"` | `"done"` | `"dead"`).
    pub status: String,
    /// Number of attempts made so far.
    pub attempts: i32,
    /// Last error message, absent if none.
    pub last_error: Option<String>,
}
