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

use gradatum_core::scope::VaultId;
use serde::Serialize;

// ── Re-exports depuis gradatum-dto (single source of truth) ───────────────────
pub use gradatum_dto::{
    CreateFeatureCardRequest, CreateFeatureCardResponse, SessionTraceRequest, SessionTraceResponse,
    VaultClassifyRequest, VaultClassifyResponse, VaultContextRequest, VaultGraphRequest,
    VaultLinksRequest, VaultListRequest, VaultReadRequest, VaultSearchRequest,
    VaultTimelineRequest, VaultTraceRequest, VaultWriteRequest,
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
    /// Weighted usage sum `Σ w_kind·count`, `None` when salience is disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salience_weighted_sum: Option<f64>,
    /// Normalised salience `s/(s+k_norm)` ∈ `[0,1)`, `None` when disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salience_factor: Option<f64>,
}

/// Individual search result, enriched with the H1 title extracted after curation.
///
/// Allows clients to know the human-readable title without reading the full note.
///
/// `vault_id` + `path` together form the full address of the note: `path` is only
/// unique within a vault, so a hit is only unambiguous once both are read.
///
/// `#[non_exhaustive]`: this is a response type, produced by the server and never
/// constructed by a consumer, so further fields can be added within the `2.x` line
/// without a major bump. Downstream code reads it — including through
/// `serde_json` — which the attribute leaves untouched; only literal construction
/// and exhaustive destructuring from another crate are forbidden.
#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct SearchHit {
    /// Vault the result was read from.
    ///
    /// Always present, on every result, whether or not the request carried a
    /// `vault_id`. It states the origin of the content instead of leaving the
    /// caller to infer it from the request it sent — an inference that breaks as
    /// soon as a single hit is quoted, cached or merged away from its response.
    ///
    /// Together with `path` it forms the full address of the note (`path` alone is
    /// only unique within a vault).
    ///
    /// Serialised as a plain JSON string, e.g. `"main"`.
    pub vault_id: VaultId,
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
        note = "hardcoded legacy value 0.5; use scores.trust_raw (v0.4.4+)"
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
    /// Temporal anchor (`temporal_index.anchor_ms`), Unix epoch milliseconds.
    ///
    /// Present when the note has a `temporal_index` entry; `null` otherwise.
    /// Allows clients to display the document date without re-reading the note.
    /// Additive field (F-65) — existing clients that ignore unknown fields are unaffected.
    pub anchor_ms: Option<i64>,
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

/// Source note incluse dans la réponse `vault_context`.
#[derive(Debug, Serialize)]
pub struct IncludedNote {
    /// ULID de la note source.
    pub ulid: String,
    /// Titre Markdown H1 (ULID de repli si le titre est absent).
    pub title: String,
    /// Section de la note (ex. `"decisions"`, `"reference"`).
    pub section: String,
    /// Date de création ISO 8601 UTC (ex. `"2026-06-26T12:00:00+00:00"`).
    pub date: String,
    /// Score de pertinence (0.0 en mode Raw ; score composite en mode Assembled).
    pub score: f64,
}

/// Diagnostics d'assemblage retournés dans chaque réponse `vault_context`.
#[derive(Debug, Serialize)]
pub struct ContextDiagnostics {
    /// Nombre de candidats évalués avant la sélection budgétaire.
    pub candidates_considered: u32,
    /// Nombre de notes effectivement incluses dans le contexte produit.
    pub included_count: u32,
    /// `true` si l'embed a échoué / timeout et que le RRF s'est dégradé en BM25-only.
    pub embed_fallback: bool,
    /// Nombre de skills injectés dans le contexte (F-58, `inject_skills=true`).
    pub skills_injected: u32,
}

/// Stub d'une note référencée — miroir sérialisable de [`crate::context::reference::Stub`].
///
/// Retourné dans [`VaultContextResponse::references`] quand `reference_mode=true`.
/// Champs en **ordre fixe** (cache-stable, contrainte §5 spec v0.7.2).
/// Score and date excluded: canonical byte-stable stub (cache stability constraint).
#[derive(Debug, Serialize)]
pub struct StubDto {
    /// ULID de la note (identifiant pour déréférencement via `vault_read`).
    pub ulid: String,
    /// Titre de la note.
    pub title: String,
    /// Section thématique (ex. `"decisions"`, `"reference"`).
    pub section: String,
    /// Extrait figé du corps (tronqué char-safe, sans newline).
    pub snippet: String,
}

/// Distribution counters for inline/stub/dropped notes in a context assembly.
///
/// Invariant : `inline + stub + dropped == diagnostics.candidates_considered`.
///
/// - `inline` : notes dont le corps complet figure dans `assembled_text`.
/// - `stub` : notes condensées en stubs déréférençables dans `references`.
/// - `dropped` : notes ignorées (hors budget inline + stub).
#[derive(Debug, Serialize)]
pub struct ContextCounts {
    /// Notes retenues inline (corps complet dans `assembled_text`).
    pub inline: usize,
    /// Notes retournées en stubs déréférençables (`references`).
    pub stub: usize,
    /// Notes droppées (hors budget inline + stub).
    pub dropped: usize,
}

/// Response for `vault_context` (v0.7.0+).
///
/// Remplace l'ancienne forme `{ context, estimated_tokens, sources }` (v0.6.x).
/// Le mode Raw produit `assembled_text` = exactement l'ancien `context` (parité
/// bit-pour-bit : jointure `"\n\n---\n\n"`, troncature char-safe, budget `chars/3`).
///
/// ## Additional fields (since v0.7.2)
///
/// `references` and `counts` are **always present** (never `null`).
/// When `reference_mode=false` (default): `references = []`, `counts.stub = 0` —
/// fully backward-compatible behavior.
#[derive(Debug, Serialize)]
pub struct VaultContextResponse {
    /// Texte assemblé prêt pour injection dans un prompt LLM.
    pub assembled_text: String,
    /// Notes sources incluses dans le contexte.
    pub included: Vec<IncludedNote>,
    /// Budget tokens consommé par le contexte assemblé.
    ///
    /// - Mode **Assembled** : `HeuristicEstimator::estimate(&assembled_text)` mesuré après
    ///   rendu final complet (scaffolding Markdown, métadonnées, header skills éventuel inclus).
    ///   Représente le vrai coût d'injection — plus précis que la somme des seuls corps (P2-b).
    /// - Mode **Raw** : `assembled_text.chars().count() / 3` (division entière, parité legacy).
    pub budget_used: u32,
    /// Diagnostics d'assemblage.
    pub diagnostics: ContextDiagnostics,
    /// Notes en stubs déréférençables (F-29).
    ///
    /// Empty (`[]`) when `reference_mode=false` (default) — fully backward-compatible.
    /// Chaque stub contient `ulid`/`title`/`section`/`snippet` (byte-stable, sans score).
    pub references: Vec<StubDto>,
    /// Répartition inline/stub/dropped pour cette requête (F-29).
    ///
    /// Invariant : `counts.inline + counts.stub + counts.dropped == diagnostics.candidates_considered`.
    pub counts: ContextCounts,

    /// Prompt cache signal.
    ///
    /// `true` si `budget_used > cache_breakpoint_threshold_tokens` (config `[context]`).
    /// Le consommateur peut utiliser ce signal pour poser un `cache_control` sur le
    /// `tool_result` et optimiser le prompt cache LCP.
    /// `false` si `budget_used == 0` (assemblage vide) ou si le seuil n'est pas atteint.
    pub cache_breakpoint_hint: bool,
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

/// 202 Accepted response — job enqueued via `gradatum_jobs` (ULID string job ID).
///
/// Used by handlers bridged to `state.job_store`. Contrairement au format hérité retiré avec l'ancienne file,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `StubDto` se sérialise avec les 4 champs en ordre fixe (cache-stable).
    #[test]
    fn stub_dto_serializes_four_fields() {
        let stub = StubDto {
            ulid: "01JXABCDE12345678901234567".to_string(),
            title: "Ma note".to_string(),
            section: "decisions".to_string(),
            snippet: "Un extrait concis".to_string(),
        };
        let json = serde_json::to_value(&stub).unwrap();
        assert_eq!(json["ulid"], "01JXABCDE12345678901234567");
        assert_eq!(json["title"], "Ma note");
        assert_eq!(json["section"], "decisions");
        assert_eq!(json["snippet"], "Un extrait concis");
        // Pas de champ `score` ni `date` (cache-stable, contrainte F-29).
        assert!(
            json.get("score").is_none(),
            "score ne doit pas être dans StubDto"
        );
        assert!(
            json.get("date").is_none(),
            "date ne doit pas être dans StubDto"
        );
    }

    /// `ContextCounts` se sérialise avec les 3 compteurs.
    #[test]
    fn context_counts_serializes_three_fields() {
        let counts = ContextCounts {
            inline: 3,
            stub: 5,
            dropped: 2,
        };
        let json = serde_json::to_value(&counts).unwrap();
        assert_eq!(json["inline"], 3);
        assert_eq!(json["stub"], 5);
        assert_eq!(json["dropped"], 2);
    }

    /// `VaultContextResponse` sérialise `references`, `counts` et `cache_breakpoint_hint`.
    #[test]
    fn vault_context_response_serializes_references_and_counts() {
        let resp = VaultContextResponse {
            assembled_text: "texte".to_string(),
            included: vec![],
            budget_used: 42,
            diagnostics: ContextDiagnostics {
                candidates_considered: 10,
                included_count: 3,
                embed_fallback: false,
                skills_injected: 0,
            },
            references: vec![StubDto {
                ulid: "01JXTEST".to_string(),
                title: "T".to_string(),
                section: "s".to_string(),
                snippet: "snip".to_string(),
            }],
            counts: ContextCounts {
                inline: 3,
                stub: 1,
                dropped: 6,
            },
            cache_breakpoint_hint: true,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["references"].as_array().unwrap().len(), 1);
        assert_eq!(json["counts"]["inline"], 3);
        assert_eq!(json["counts"]["stub"], 1);
        assert_eq!(json["counts"]["dropped"], 6);
        assert_eq!(json["cache_breakpoint_hint"], serde_json::json!(true));
    }
}
