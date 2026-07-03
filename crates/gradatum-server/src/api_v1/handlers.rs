//! MCP read handlers for API v1 — 10 endpoints, wire-compatible with the legacy vault.
//!
//! Each handler:
//! 1. Verifies authentication via [`TrustContext::is_authenticated`].
//! 2. Evaluates the ACL via `AclEngine::evaluate` (Read, locus = `tenant_id/section_or_main`).
//! 3. Delegates to the real libraries (`state.vault` + `state.search`).
//!
//! ## Wired handlers (`state.search` / `SqliteIndex`)
//!
//! - `vault_authors` → `distinct_authors`
//! - `vault_tags` → `distinct_tags`
//! - `vault_links` → `backlinks` (direct incoming links)
//! - `vault_graph` → `neighbors` (recursive CTE, depth capped at 3)
//! - `vault_trace` → `trace_lineage` (parents + children)
//! - `vault_read` → `get_note` (404 if absent)
//! - `vault_context` → `get_note` body + backlink sources
//!
//! ## Path → note ID resolution
//!
//! `req.path` / `req.root` / `req.query` are interpreted as ULID identifiers.
//! Non-ULID paths fall back to `title_lookup` (resolved in `vault_read` / `vault_trace`).
//!
//! # Endpoints
//!
//! | Method | Path | Auth |
//! |--------|------|------|
//! | POST | `/api/v1/vault_search`  | bearer required |
//! | POST | `/api/v1/vault_read`    | bearer required |
//! | POST | `/api/v1/vault_list`    | bearer required |
//! | GET  | `/api/v1/vault_status`  | bearer required |
//! | POST | `/api/v1/vault_graph`   | bearer required |
//! | POST | `/api/v1/vault_links`   | bearer required |
//! | POST | `/api/v1/vault_trace`   | bearer required |
//! | POST | `/api/v1/vault_context` | bearer required |
//! | GET  | `/api/v1/vault_authors` | bearer required |
//! | GET  | `/api/v1/vault_tags`    | bearer required |

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_core::error::GradatumError;
use gradatum_core::trust::TrustContext;

use crate::api_v1::dto::{
    VaultAuthorsResponse, VaultContextRequest, VaultContextResponse, VaultGraphRequest,
    VaultGraphResponse, VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse,
    VaultReadRequest, VaultReadResponse, VaultSearchRequest, VaultSearchResponse,
    VaultStatusResponse, VaultTagsResponse, VaultTraceRequest, VaultTraceResponse,
};
use crate::state::AppState;

/// Allowed values for the `vault_search` `status` filter.
///
/// The 6 `NoteStatus` variants (kebab-case) plus the legacy SQL value `"downgraded"`
/// (written by `vault_downgrade`, outside the enum but present in the database —
/// accepted so that callers can filter on those notes).
const SEARCH_STATUS_ALLOWED: [&str; 7] = [
    "draft",
    "staging",
    "pending-review",
    "live",
    "deprecated",
    "garbage",
    "downgraded",
];

/// Validates and normalises the `status` filter of a search request.
///
/// - `None` → `Ok(None)` (no filter — unchanged behaviour).
/// - `Some(s)` trimmed and present in [`SEARCH_STATUS_ALLOWED`] → `Ok(Some(trimmed))`.
/// - `Some(s)` not in the list → `Err(())` (the handler maps this to `400 Bad Request`).
///
/// Pure (no I/O) — directly unit-testable.
pub(crate) fn validate_search_status(status: Option<&str>) -> Result<Option<String>, ()> {
    match status {
        None => Ok(None),
        Some(raw) => {
            let s = raw.trim();
            if SEARCH_STATUS_ALLOWED.contains(&s) {
                Ok(Some(s.to_string()))
            } else {
                Err(())
            }
        }
    }
}

/// Truncates text to `max_chars` Unicode codepoints.
///
/// Kept for regression unit tests (UTF-8 boundary, ZWJ emoji, short body)
/// that cover the char-safe invariant used by `vault_context`.
/// `vault_context` now inlines the `char_indices().nth()` pattern directly
/// without calling `build_snippet`.
///
/// Uses `char_indices` to guarantee a char-safe boundary (never mid-byte).
/// Appends `…` when the text is truncated.
///
/// `max_chars` counts codepoints, not bytes: a 4-byte emoji counts as 1.
/// A ZWJ sequence (e.g. 👨‍👩‍👧‍👦) is treated as multiple separate codepoints —
/// each codepoint is a safe boundary.
#[allow(dead_code)] // référencé par les tests unitaires (cf. mod tests build_snippet_*)
pub(crate) fn build_snippet(body: &str, max_chars: usize) -> String {
    let end = body
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(body.len());
    if end < body.len() {
        format!("{}…", &body[..end])
    } else {
        body.to_string()
    }
}

/// Normalises a query for SQLite FTS5, wrapping it in an exact phrase when necessary.
///
/// ## Logic
///
/// A query is passed as-is to FTS5 if and only if:
/// - It contains only Unicode alphanumeric characters, underscores, or spaces.
/// - It contains no reserved FTS5 keywords (`AND`, `OR`, `NOT`, `NEAR`).
///
/// In all other cases (presence of `.`, `,`, `-`, `'`, `!`, `?`, `:`, `*`, etc.),
/// the query is wrapped as an exact phrase: `"<query>"`.
///
/// ## Escaping
///
/// Inside an FTS5 phrase:
/// - Internal double-quotes are doubled (`"` → `""`).
/// - Internal apostrophes are doubled (`'` → `''`) — required by the FTS5 tokeniser
///   in phrase mode (the default `unicode61` tokeniser treats `'` as a delimiter).
///
/// ## Previous anti-pattern
///
/// Explicitly listing operator characters (`- * : ^ " ( )`) was incomplete by design:
/// `.`, `,`, `'`, `!`, `?` were missing, causing HTTP 500 on earlier versions.
///
/// ## Visibility
///
/// `pub(crate)` for unit tests in this module; not exposed in the public API.
pub(crate) fn build_fts_query(query: &str) -> String {
    // Caractères safe pour FTS5 sans guillemets : alphanumériques Unicode, underscore, espace.
    let is_safe = |c: char| c.is_alphanumeric() || c == '_' || c == ' ';

    // Mots-clés FTS5 réservés — déclenche le wrap même si tous les chars sont alphanumériques.
    let upper = query.to_uppercase();
    let has_fts5_keyword = upper
        .split_whitespace()
        .any(|t| matches!(t, "AND" | "OR" | "NOT" | "NEAR"));

    let needs_wrap = !query.chars().all(is_safe) || has_fts5_keyword;

    if needs_wrap {
        // Phrase exacte : guillemets doubles + internes doublés + apostrophes doublées.
        let escaped = query.replace('"', "\"\"").replace('\'', "''");
        format!("\"{escaped}\"")
    } else {
        query.to_string()
    }
}

/// Filters semantic hits by section.
///
/// `sec_result` is the return value of `IndexStore::get_titles_sections` for the
/// IDs of the semantic hits. Semantics:
///
/// - `Ok(map)`: only hits whose section (read from `map`) equals `wanted_section`
///   are kept. A hit absent from the map (unknown section) is **excluded** —
///   an empty or partial map is treated as a legitimate filter, not a failure.
/// - `Err(_)`: **BM25-only degradation** — returns an empty vector (no semantic
///   hits this round). A section leak would be worse than losing semantic signal:
///   the BM25 path remains section-filtered at the SQL level, so search stays
///   functional.
///
/// Pure (no I/O, no state) — directly unit-testable, including the `Err` path.
pub(crate) fn filter_semantic_by_section(
    semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)>,
    wanted_section: &str,
    sec_result: Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError>,
) -> Vec<(gradatum_core::identity::NoteId, f32)> {
    match sec_result {
        Ok(sec_map) => semantic_hits
            .into_iter()
            .filter(|(id, _)| {
                sec_map
                    .get(&id.to_string())
                    .map(|(_, sec)| sec == wanted_section)
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "vault_search: get_titles_sections (filtre section sémantique) échoué \
                 — dégradation BM25-only (hits sémantiques écartés ce tour)"
            );
            Vec::new()
        }
    }
}

/// Filters semantic hits to a set of sections (multi-section generalisation of
/// [`filter_semantic_by_section`]).
///
/// `sec_result` is the return value of `IndexStore::get_titles_sections` for the
/// IDs of the semantic hits. Semantics:
///
/// - `Ok(map)`: only hits whose section (read from `map`) is in `wanted_sections`
///   are kept. A hit absent from the map (unknown section) is **excluded** —
///   same conservative policy as [`filter_semantic_by_section`].
/// - `Err(_)`: **BM25-only degradation** — returns an empty vector.
///   A section leak would be worse than losing semantic signal.
///
/// Pure (no I/O, no state) — directly unit-testable.
///
/// # Note on single-element sets
///
/// When `wanted_sections` has exactly one element this behaves identically to
/// [`filter_semantic_by_section`] with that element. The caller may use either
/// function; this one is preferred when the set is dynamically sized.
pub(crate) fn filter_semantic_by_sections(
    semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)>,
    wanted_sections: &[&str],
    sec_result: Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError>,
) -> Vec<(gradatum_core::identity::NoteId, f32)> {
    match sec_result {
        Ok(sec_map) => semantic_hits
            .into_iter()
            .filter(|(id, _)| {
                sec_map
                    .get(&id.to_string())
                    .map(|(_, sec)| wanted_sections.contains(&sec.as_str()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "retrieve_candidates: get_titles_sections (filtre multi-sections sémantique) \
                 échoué — dégradation BM25-only (hits sémantiques écartés ce tour)"
            );
            Vec::new()
        }
    }
}

/// Filters semantic hits by status (symmetric to `filter_semantic_by_section`).
///
/// `status_result` is the return value of `IndexStore::get_statuses` (raw SQL status)
/// for the IDs of the semantic hits. Same semantics as [`filter_semantic_by_section`]:
///
/// - `Ok(map)`: only hits whose status equals `wanted_status` are kept.
///   A hit absent from the map (unknown status) is **excluded** (`unwrap_or(false)`).
/// - `Err(_)`: **BM25-only degradation** — empty vector (the BM25 path remains
///   status-filtered at the SQL level, so search stays functional and leak-free).
///
/// Pure (no I/O) — directly unit-testable.
pub(crate) fn filter_semantic_by_status(
    semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)>,
    wanted_status: &str,
    status_result: Result<std::collections::HashMap<String, String>, GradatumError>,
) -> Vec<(gradatum_core::identity::NoteId, f32)> {
    match status_result {
        Ok(status_map) => semantic_hits
            .into_iter()
            .filter(|(id, _)| {
                status_map
                    .get(&id.to_string())
                    .map(|st| st == wanted_status)
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "vault_search: get_statuses (filtre statut sémantique) échoué \
                 — dégradation BM25-only (hits sémantiques écartés ce tour)"
            );
            Vec::new()
        }
    }
}

// ── vault_search ──────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_search`
///
/// Full-text FTS5 search in the vault via `state.search.search_fts`.
///
/// ## Algorithm
///
/// 1. Auth + ACL (Read).
/// 2. Clamp `limit` to [1, 50], default 10.
/// 3. Call `search_fts(vault_id, query, limit)` → `Vec<NoteId>`.
/// 4. For each `NoteId`: `get_note(vault_id, id)` → snippet (first 50 chars of body).
/// 5. Return `SearchHit { path: "<section>/<id>", score, snippet }`.
///
/// ## Score
///
/// `score` is the **composite RRF** value: `rrf_fuse(BM25, semantic, k=60) ×
/// (1 + α·recency) × (1 + β·pagerank)`. Upper bound ≈ 0.04 (a hit ranked #1 in
/// both lists ≈ 2/(60+1) before factors). **This is NOT a [0-1] similarity** —
/// interpret it as a relative rank (hit order), never via an absolute threshold.
///
/// ## Errors
///
/// - `500` if the SQLite FTS5 query fails (logged and propagated).
/// - `400` if `query` is empty (FTS5 rejects empty queries).
///
/// ## Optional scoping — `locus` + `vault_id`
///
/// - `locus`: filters by physical path prefix (e.g. `"council/"`) — applied to
///   both the FTS and semantic paths. No effect on the response contract.
/// - `vault_id`: cross-vault read — `tenant_id` is still used for ACL;
///   `vault_id` is used for the actual read. Non-empty, max 128 chars.
///   Zero semantic hits on a vault ≠ `tenant_id` are logged at `info!` level
///   (no additional response field — backward-compatible).
pub async fn vault_search(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultSearchRequest>,
) -> Result<Json<VaultSearchResponse>, StatusCode> {
    crate::api_v1::logic::vault_search_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_search: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_read ────────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_read`
///
/// Reads the content of a note by ULID **or by Markdown H1 title**.
///
/// ## Algorithm
///
/// 1. If `req.path` parses as a ULID → delegates directly to `state.vault.read_note_by_id()`.
/// 2. Otherwise → `state.search.title_lookup(&req.tenant_id, &req.path)`:
///    - `Ok(Some(found_id))` → delegates to `state.vault.read_note_by_id(found_id)`.
///    - `Ok(None)` → 404 NOT_FOUND (title not found or note not live — see the
///      `AND status = 'live'` filter in `title_lookup`, `queries.rs`).
///    - `Err(e)` → 500 INTERNAL_SERVER_ERROR (logged).
///
/// Title resolution is transparent and built into `vault_read`, consistent with the
/// legacy vault which accepted wikilinks `[[title]]` directly.
///
/// Returns 200, 404, or 500.
pub async fn vault_read(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultReadRequest>,
) -> Result<Json<VaultReadResponse>, StatusCode> {
    crate::api_v1::logic::vault_read_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            match &e {
                gradatum_core::error::GradatumError::NoteNotFound(_)
                | gradatum_core::error::GradatumError::Storage(_) => {}
                other => {
                    tracing::error!(err = %other, "vault_read: backend failed");
                }
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_list ────────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_list`
///
/// Lists notes in a vault with ULID cursor-based pagination.
/// Delegates to [`crate::state::AppState::search`]`.list_notes()` (FTS5 SQLite).
/// The `pattern` field is accepted but ignored; pattern filtering is planned.
/// Returns a `next_cursor` when more pages are available.
pub async fn vault_list(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultListRequest>,
) -> Result<Json<VaultListResponse>, StatusCode> {
    crate::api_v1::logic::vault_list_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_list: list_notes failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_status ──────────────────────────────────────────────────────────────

/// `GET /api/v1/vault_status`
///
/// Returns the current vault state for the active tenant (`"main"`).
/// `note_count`: `COUNT(*) WHERE status = 'live'` via [`crate::state::AppState::search`]`.live_note_count()`.
/// `total_size_bytes`: `COALESCE(SUM(LENGTH(body_text)), 0)` via `.total_body_size_bytes()`.
/// On SQL error for a metric, the fallback value is `0` (the handler never panics).
pub async fn vault_status(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultStatusResponse>, StatusCode> {
    crate::api_v1::logic::vault_status_impl(&state, &trust)
        .await
        .map(Json)
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}

// ── vault_graph ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_graph`
///
/// Returns the link graph for a root note up to `depth` levels.
///
/// `req.root` is interpreted as a note ULID. `depth` defaults to 2, max effective value is 3.
/// Uses `state.search.neighbors` (recursive CTE) and `backlinks` when `include_backlinks` is set.
///
/// Returns `nodes` (list of neighbouring note IDs) and `edges` (from → to links).
pub async fn vault_graph(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultGraphRequest>,
) -> Result<Json<VaultGraphResponse>, StatusCode> {
    crate::api_v1::logic::vault_graph_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_graph: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_links ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_links`
///
/// Thin alias for `vault_graph` at `depth=1`.
/// Lists the direct incoming and outgoing links for a note.
///
/// `req.path` is interpreted as a note ULID.
/// Uses `backlinks` (incoming) and `neighbors(depth=1)` (outgoing).
pub async fn vault_links(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultLinksRequest>,
) -> Result<Json<VaultLinksResponse>, StatusCode> {
    crate::api_v1::logic::vault_links_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_links: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_trace ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_trace`
///
/// Traces the lineage of a note (parents + children via wikilinks).
///
/// ## Multi-mode resolution
///
/// 1. If `req.query` parses as a ULID → calls `trace_lineage` directly.
/// 2. Otherwise → `state.search.title_lookup(...)`:
///    - `Some(found_id)` → `trace_lineage(found_id)` (exact title match).
///    - `None` → FTS fallback: `search_fts_with_snippet(...)` returns up to
///      `min(limit, 5)` seeds, each passed to `trace_lineage` (N+1 SQLite,
///      typically ≤ 5 queries).
///    - `Err(e)` → 500.
///
/// All `lineage.parents` and `lineage.children` are concatenated, deduplicated,
/// and capped to `req.limit`. Score is fixed at 1.0 (no RRF applied).
pub async fn vault_trace(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultTraceRequest>,
) -> Result<Json<VaultTraceResponse>, StatusCode> {
    crate::api_v1::logic::vault_trace_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_trace: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_context ─────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_context`
///
/// Builds an LLM context from relevant notes.
///
/// ## Algorithm
///
/// 1. If `req.query` parses as a ULID → fetches the note + backlinks (sources)
///    and truncates the body to the token budget (ratio 3.0 chars/token).
/// 2. Otherwise → FTS text search via `search_fts_with_snippet` (top-10 notes by BM25,
///    section-filtered when provided). For each note, includes `body_text` (full or
///    char-safe truncated) as long as the remaining budget allows.
///
/// ## Token heuristic
///
/// - **`chars().count()`** (not `len()` bytes) — correct for Unicode FR/EN/multi-byte.
/// - Ratio **3.0 chars/token** — conservative for mixed FR/EN content.
///   Anthropic guidance: FR ≈ 3.5 chars/token, EN ≈ 4.0. The corpus is mixed
///   FR/EN/code/markdown/ULIDs, so the effective ratio is below 3.5. Using 3.0
///   never under-counts token cost, keeping `max_tokens` honoured with margin.
///
/// ## Char-safe truncation
///
/// `body_text.char_indices().nth(char_limit)` returns a valid byte offset for
/// `&body_text[..end]` — no UTF-8 boundary panic.
pub async fn vault_context(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultContextRequest>,
) -> Result<Json<VaultContextResponse>, StatusCode> {
    crate::api_v1::logic::vault_context_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_context: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_authors ─────────────────────────────────────────────────────────────

/// `GET /api/v1/vault_authors`
///
/// Lists distinct authors in the vault (tenant `"main"`).
/// Delegates to `state.search.distinct_authors("main")`.
/// Notes without an author (`author_id IS NULL`) are excluded.
pub async fn vault_authors(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultAuthorsResponse>, StatusCode> {
    crate::api_v1::logic::vault_authors_impl(&state, &trust)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_authors: distinct_authors failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// ── vault_tags ────────────────────────────────────────────────────────────────

/// `GET /api/v1/vault_tags`
///
/// Lists distinct tags in the vault (tenant `"main"`) with their usage frequency.
/// Delegates to `state.search.distinct_tags("main")`.
pub async fn vault_tags(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultTagsResponse>, StatusCode> {
    crate::api_v1::logic::vault_tags_impl(&state, &trust)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_tags: distinct_tags failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

// SearchHit est utilisé directement dans vault_search (T10 — FTS5 réel).
// Les autres types (GraphEdge, TraceEntry, AuthorEntry, TagEntry) sont utilisés
// dans les 7 handlers câblés en T3 P2.0c.

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        build_fts_query, build_snippet, filter_semantic_by_section, filter_semantic_by_status,
        validate_search_status,
    };
    use gradatum_core::error::GradatumError;
    use gradatum_core::identity::NoteId;
    use std::collections::HashMap;
    use ulid::Ulid;

    fn nid(s: &str) -> NoteId {
        NoteId(Ulid::from_string(s).expect("ulid test valide"))
    }

    /// Ok + map complète : seuls les hits de la section voulue sont conservés.
    #[test]
    fn filter_semantic_section_ok_keeps_only_matching() {
        let a = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "01HBBBBBBBBBBBBBBBBBBBBBBB";
        let hits = vec![(nid(a), 0.9f32), (nid(b), 0.8f32)];
        let mut map = HashMap::new();
        map.insert(
            a.to_string(),
            (Some("Titre A".to_string()), "reference".to_string()),
        );
        map.insert(b.to_string(), (None, "debug".to_string()));

        let out = filter_semantic_by_section(hits, "reference", Ok(map));
        assert_eq!(out.len(), 1, "seul le hit 'reference' reste");
        assert_eq!(out[0].0.to_string(), a);
    }

    /// Ok + map partielle (hit sans entrée) : le hit inconnu est exclu (pas un leak).
    #[test]
    fn filter_semantic_section_ok_partial_excludes_unknown() {
        let a = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "01HBBBBBBBBBBBBBBBBBBBBBBB";
        let hits = vec![(nid(a), 0.9f32), (nid(b), 0.8f32)];
        // map ne contient QUE a (b a une section inconnue).
        let mut map = HashMap::new();
        map.insert(a.to_string(), (None, "reference".to_string()));

        let out = filter_semantic_by_section(hits, "reference", Ok(map));
        assert_eq!(
            out.len(),
            1,
            "le hit sans entrée (section inconnue) est exclu"
        );
        assert_eq!(out[0].0.to_string(), a);
    }

    /// Ok + map vide légitime : aucun hit ne matche → tous exclus (pas de leak).
    #[test]
    fn filter_semantic_section_ok_empty_map_excludes_all() {
        let a = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let hits = vec![(nid(a), 0.9f32)];
        let out = filter_semantic_by_section(hits, "reference", Ok(HashMap::new()));
        assert!(
            out.is_empty(),
            "map vide Ok → aucun hit conservé (exclusion, pas leak)"
        );
    }

    /// Audit lot C P1.1 — Err (échec batch) : dégradation BM25-only, AUCUN hit hors
    /// section ne fuit (vecteur vide, jamais les hits non filtrés).
    #[test]
    fn filter_semantic_section_err_degrades_to_bm25_only() {
        let a = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "01HBBBBBBBBBBBBBBBBBBBBBBB";
        let hits = vec![(nid(a), 0.9f32), (nid(b), 0.8f32)];
        let err = Err(GradatumError::Storage("batch failure simulée".to_string()));

        let out = filter_semantic_by_section(hits, "reference", err);
        assert!(
            out.is_empty(),
            "sur Err, AUCUN hit sémantique ne doit passer (dégradation BM25-only, zéro leak)"
        );
    }

    /// Régression UTF-8 : body dont le 200e byte est à l'intérieur d'un char 'é'
    /// (2 bytes). Avec l'ancien `[..200]`, ce test panique à cause d'une slice
    /// non char-safe. Avec le fix `char_indices().nth(200)`, il doit passer.
    #[test]
    fn snippet_utf8_boundary_char_safe() {
        // Construction : 199 ASCII 'a' + 'é' (2 bytes pos 199-200) + suite.
        // Byte 200 = 2e byte de 'é' → l'ancien code paniquait ici.
        let body: String = "a".repeat(199) + "é" + &"b".repeat(10);
        // body.len() = 199 + 2 + 10 = 211 bytes, mais 210 chars Unicode.
        assert_eq!(body.len(), 211, "précondition longueur bytes");
        assert_eq!(body.chars().count(), 210, "précondition longueur chars");

        // Ne doit pas paniquer — c'est la régression principale.
        let snip = build_snippet(&body, 200);

        // Le snippet doit contenir exactement 200 chars + '…'
        // 200 chars = 199 'a' + 'é'
        let expected_chars = 199 + 1; // 'é' compte pour 1 char
        let snip_without_ellipsis: &str = snip.trim_end_matches('…');
        assert_eq!(
            snip_without_ellipsis.chars().count(),
            expected_chars,
            "snippet doit contenir exactement 200 chars Unicode"
        );
        assert!(snip.ends_with('…'), "snippet doit se terminer par '…'");
        // Vérifier que 'é' est bien inclus (pas tronqué au milieu)
        assert!(
            snip_without_ellipsis.ends_with('é'),
            "le dernier char du snippet doit être 'é' entier"
        );
    }

    /// Corps court (< 200 chars) : pas d'ellipsis, texte intégral retourné.
    #[test]
    fn snippet_short_body_no_ellipsis() {
        let body = "Ceci est un texte court avec des accents : éàü.";
        let snip = build_snippet(body, 200);
        assert_eq!(snip, body, "corps court doit être retourné intégral");
        assert!(!snip.ends_with('…'), "pas d'ellipsis sur corps court");
    }

    /// Corps exactement 200 chars ASCII : pas d'ellipsis (boundary exacte).
    #[test]
    fn snippet_exact_200_ascii_no_ellipsis() {
        let body: String = "x".repeat(200);
        let snip = build_snippet(&body, 200);
        assert_eq!(
            snip, body,
            "corps de 200 chars exact retourné sans ellipsis"
        );
    }

    /// Corps avec emoji (4 bytes par char) : ne doit jamais paniquer.
    #[test]
    fn snippet_emoji_boundary_char_safe() {
        // 199 'a' + emoji 🦀 (4 bytes) + suite — bytes 199-202 = emoji
        let body: String = "a".repeat(199) + "🦀" + &"z".repeat(10);
        // byte 200 = 2e byte de 🦀 → ancien code paniquait
        let snip = build_snippet(&body, 200);
        // Ne doit pas paniquer — c'est l'assertion principale
        assert!(
            snip.ends_with('…'),
            "snippet avec emoji doit avoir ellipsis"
        );
    }

    /// C1 — Régression ZWJ : boundary au milieu d'une séquence ZWJ ne produit
    /// pas de ZWJ orphelin. La séquence famille 👨‍👩‍👧‍👦 contient 7 codepoints
    /// (4 emojis + 3 U+200D). `char_indices().nth()` coupe à un codepoint, jamais
    /// à l'intérieur d'un scalaire — donc pas de panique UTF-8.
    #[test]
    fn build_snippet_zwj_emoji_preserves_utf8_boundary() {
        // Famille avec ZWJ (U+200D) : 👨‍👩‍👧‍👦 = man + ZWJ + woman + ZWJ + girl + ZWJ + boy
        // 7 codepoints (4 emojis + 3 ZWJ), ~25 bytes UTF-8.
        let body = "Famille 👨\u{200D}👩\u{200D}👧\u{200D}👦 explore le monde";

        // Boundary à 9 chars (au milieu de la famille zwj : après man+ZWJ).
        let snippet = build_snippet(body, 9);

        // 1. Slice doit être UTF-8 valide (str::from_utf8 ne paniquerait pas).
        assert!(
            std::str::from_utf8(snippet.as_bytes()).is_ok(),
            "snippet doit être UTF-8 valide : {:?}",
            snippet
        );

        // 2. Snippet ne doit pas finir par un ZWJ orphelin (U+200D = 0x200D).
        let last_char = snippet.trim_end_matches('…').chars().last();
        if let Some(c) = last_char {
            assert_ne!(
                c as u32, 0x200D,
                "snippet finit par ZWJ orphelin : {:?}",
                snippet
            );
        }
    }

    /// C2 — build_snippet avec corps court : retourne texte intégral, pas d'ellipsis.
    #[test]
    fn build_snippet_short_body_returns_full() {
        let body = "court";
        let snippet = build_snippet(body, 200);
        assert_eq!(snippet, "court");
        assert!(!snippet.contains('…'));
    }

    /// C2 — build_snippet avec corps long : tronque à max_chars + ajoute ellipsis.
    #[test]
    fn build_snippet_long_body_truncates_with_ellipsis() {
        let body = "a".repeat(300);
        let snippet = build_snippet(&body, 200);
        // snippet = 200 'a' + '…' → chars().count() = 201
        assert_eq!(
            snippet.chars().count(),
            201,
            "snippet long : 200 chars + 1 ellipsis = 201 codepoints"
        );
        assert!(snippet.ends_with('…'));
    }

    // ── Tests unitaires build_fts_query (fix #32) ─────────────────────────────────

    /// Régression #32 — query avec point doit être wrappée en phrase FTS5.
    ///
    /// `2.1.1` non-wrappé → `fts5 syntax error near "."` → HTTP 500.
    /// Après fix : `"2.1.1"` (phrase exacte) → FTS5 OK.
    #[test]
    fn build_fts_query_dot_is_wrapped() {
        let q = build_fts_query("2.1.1");
        assert_eq!(
            q, r#""2.1.1""#,
            "query avec point doit être wrappée en phrase FTS5"
        );
    }

    /// Régression #32 — query avec token contenant un point → wrappée.
    #[test]
    fn build_fts_query_alpha_dot_is_wrapped() {
        let q = build_fts_query("alpha.8");
        assert_eq!(q, r#""alpha.8""#, "alpha.8 doit être wrappé");
    }

    /// Régression #32 — query avec apostrophe → wrappée, apostrophe doublée.
    ///
    /// FTS5 interprète `'` comme délimiteur dans les phrases — doit être doublé.
    #[test]
    fn build_fts_query_apostrophe_is_doubled() {
        let q = build_fts_query("O'Reilly");
        // Apostrophe doublée dans la phrase : "O''Reilly"
        assert_eq!(
            q, r#""O''Reilly""#,
            "apostrophe doit être doublée dans la phrase FTS5"
        );
    }

    /// Query alphanumérique simple → pas de wrap (path tokenizer FTS5 direct).
    #[test]
    fn build_fts_query_alphanumeric_not_wrapped() {
        let q = build_fts_query("gradatum");
        assert_eq!(
            q, "gradatum",
            "query alphanumérique ne doit pas être wrappée"
        );
    }

    /// Query avec underscore → pas de wrap (underscore = char safe FTS5).
    #[test]
    fn build_fts_query_underscore_not_wrapped() {
        let q = build_fts_query("vault_search");
        assert_eq!(q, "vault_search", "underscore est safe, pas de wrap");
    }

    /// Mot-clé FTS5 `AND` → wrap phrase (même si que des chars alphanumériques).
    ///
    /// Préserve le comportement existant : `AND` opérateur FTS5 → phrase littérale.
    #[test]
    fn build_fts_query_fts5_keyword_and_is_wrapped() {
        let q = build_fts_query("gradatum AND notes");
        assert_eq!(
            q, r#""gradatum AND notes""#,
            "AND keyword doit déclencher le wrap phrase"
        );
    }

    /// Mot-clé FTS5 `NOT` → wrap phrase.
    #[test]
    fn build_fts_query_fts5_keyword_not_is_wrapped() {
        let q = build_fts_query("notes NOT debug");
        assert_eq!(
            q, r#""notes NOT debug""#,
            "NOT keyword doit déclencher le wrap phrase"
        );
    }

    /// Query avec guillemets internes → guillemets doublés dans la phrase FTS5.
    ///
    /// Input : `say "hello"` (11 chars, dont 2 guillemets)
    /// Après `replace('"', "\"\"")` : `say ""hello""`
    /// Wrappé en phrase : `"say ""hello"""` — guillemets ouvrant/fermant + doublage interne.
    #[test]
    fn build_fts_query_internal_quotes_doubled() {
        let q = build_fts_query(r#"say "hello""#);
        // Valeur attendue : `"say ""hello"""` (ouverture + say + espace + "" + hello + "" + fermeture)
        assert_eq!(
            q, r#""say ""hello""""#,
            "guillemets internes doublés dans la phrase"
        );
    }

    /// Query `phase-2.x` (tiret + point) → wrappée.
    /// Les deux caractères spéciaux déclenchent le wrap.
    #[test]
    fn build_fts_query_dash_and_dot_wrapped() {
        let q = build_fts_query("phase-2.x");
        assert_eq!(
            q, r#""phase-2.x""#,
            "tiret+point doivent déclencher le wrap"
        );
    }

    // ── Tests C1 council backlog Phase 2.1.2 (alpha.15) ─────────────────────────

    /// C1 — accents Unicode : pas de wrap (is_alphanumeric traite les accents comme alphanum).
    ///
    /// `éàü` sont des caractères alphanumériques Unicode → `char::is_alphanumeric()` = true.
    /// Pas de wrap nécessaire — FTS5 les tokenize correctement.
    #[test]
    fn build_fts_query_accented_chars_not_wrapped() {
        let q = build_fts_query("éàü gradatum");
        assert_eq!(
            q, "éàü gradatum",
            "accents ne doivent pas déclencher le wrap"
        );
    }

    /// C1 — query vide → chaîne vide (400 géré en amont par le handler).
    ///
    /// `build_fts_query` retourne `""` sur input vide — le handler vérifie
    /// `query.trim().is_empty()` avant d'appeler FTS5.
    #[test]
    fn build_fts_query_empty_is_empty_string() {
        let q = build_fts_query("");
        assert_eq!(q, "", "query vide retourne chaîne vide");
    }

    /// C1 — NEAR avec parenthèses → wrappé (parens = unsafe pour FTS5).
    ///
    /// La parenthèse `(` déclenche le wrap car `is_alphanumeric()` retourne false.
    #[test]
    fn build_fts_query_near_with_parens_is_wrapped() {
        let q = build_fts_query("NEAR(gradatum 5)");
        assert!(
            q.starts_with('"') && q.ends_with('"'),
            "NEAR(gradatum 5) doit être wrappé — parens = char spécial FTS5"
        );
    }

    /// C1 — deux-points (frontmatter) → wrappé.
    ///
    /// Le `:` après `section` est unsafe FTS5 (opérateur de colonne). Doit être wrappé.
    #[test]
    fn build_fts_query_frontmatter_colon_is_wrapped() {
        let q = build_fts_query("section: reasoning");
        assert!(
            q.starts_with('"') && q.ends_with('"'),
            "deux-points dans la query doivent déclencher le wrap (opérateur de colonne FTS5)"
        );
    }

    /// Vérifie que le mapping BM25 → score [0..1] utilisé dans `vault_search` est
    /// monotone décroissant et borné : meilleur match (bm25 proche de 0) → score
    /// proche de 1.0, mauvais match (bm25 très négatif) → score proche de 0.0.
    #[test]
    fn bm25_score_mapping_is_monotone_decreasing_in_zero_one_range() {
        // Mapping interne handler vault_search :
        // score = 1.0 / (1.0 + bm25_raw.abs()) cast en f32.
        // bm25 SQLite est négatif (meilleur match → plus proche de 0).
        fn map(bm25_raw: f64) -> f32 {
            (1.0_f64 / (1.0 + bm25_raw.abs())) as f32
        }

        let s_excellent = map(-0.1);
        let s_good = map(-0.5);
        let s_poor = map(-10.0);

        assert!((s_excellent - 0.909).abs() < 0.01, "got {}", s_excellent);
        assert!((s_good - 0.667).abs() < 0.01, "got {}", s_good);
        assert!((s_poor - 0.091).abs() < 0.01, "got {}", s_poor);

        assert!(s_excellent > s_good);
        assert!(s_good > s_poor);

        assert!((0.0..=1.0).contains(&s_excellent));
        assert!(s_poor >= 0.0);

        assert_eq!(map(0.0), 1.0_f32);
        let s_terrible = map(-1000.0);
        assert!(s_terrible < 0.01);
    }

    // ── F-37 notes fix — validate_search_status ────────────────────────────────

    #[test]
    fn validate_search_status_none_is_ok_none() {
        assert_eq!(validate_search_status(None), Ok(None));
    }

    #[test]
    fn validate_search_status_accepts_enum_and_legacy() {
        for ok in [
            "draft",
            "staging",
            "pending-review",
            "live",
            "deprecated",
            "garbage",
            "downgraded",
        ] {
            assert_eq!(
                validate_search_status(Some(ok)),
                Ok(Some(ok.to_string())),
                "{ok:?} doit être accepté"
            );
        }
        // Trim appliqué.
        assert_eq!(
            validate_search_status(Some("  live  ")),
            Ok(Some("live".to_string()))
        );
    }

    #[test]
    fn validate_search_status_rejects_unknown() {
        for bad in [
            "",
            "Live",
            "pendingreview",
            "archived",
            "needs-review",
            "garbages",
        ] {
            assert_eq!(
                validate_search_status(Some(bad)),
                Err(()),
                "{bad:?} doit être rejeté"
            );
        }
    }

    // ── F-37 notes fix — filter_semantic_by_status ─────────────────────────────

    #[test]
    fn filter_semantic_by_status_keeps_only_matching() {
        let a = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let b = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
        let hits = vec![(nid(a), 0.9f32), (nid(b), 0.8f32)];
        let mut map = std::collections::HashMap::new();
        map.insert(a.to_string(), "live".to_string());
        map.insert(b.to_string(), "pending-review".to_string());

        let kept = filter_semantic_by_status(hits, "live", Ok(map));
        assert_eq!(kept.len(), 1, "seul le hit 'live' est conservé");
        assert_eq!(kept[0].0, nid(a));
    }

    #[test]
    fn filter_semantic_by_status_excludes_unknown_id() {
        let a = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let hits = vec![(nid(a), 0.9f32)];
        // map vide → id inconnu → exclu.
        let kept = filter_semantic_by_status(hits, "live", Ok(std::collections::HashMap::new()));
        assert!(kept.is_empty(), "id absent de la map est exclu");
    }

    #[test]
    fn filter_semantic_by_status_err_degrades_to_empty() {
        let a = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let hits = vec![(nid(a), 0.9f32)];
        let kept = filter_semantic_by_status(
            hits,
            "live",
            Err(GradatumError::Storage("boom".to_string())),
        );
        assert!(
            kept.is_empty(),
            "Err → dégradation BM25-only (vecteur vide)"
        );
    }
}
