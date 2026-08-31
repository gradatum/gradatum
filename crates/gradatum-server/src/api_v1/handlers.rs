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

use axum::response::{IntoResponse, Response};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use gradatum_core::error::GradatumError;
use gradatum_core::trust::TrustContext;

use crate::api_v1::compact::{self, CompactBody};

use crate::api_v1::dto::{
    VaultAuthorsResponse, VaultContextRequest, VaultContextResponse, VaultGraphRequest,
    VaultGraphResponse, VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse,
    VaultReadRequest, VaultSearchRequest, VaultStatusResponse, VaultTagsRequest, VaultTagsResponse,
    VaultTraceRequest, VaultTraceResponse,
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

/// Cite un jeton FTS5 unique lorsqu'il n'est pas sûr tel quel.
///
/// L'échappement se fait au niveau du **jeton**, et c'est cela qui préserve
/// l'immunité au HTTP 500 : aucun caractère opérateur (`- * : ^ ( ) . , ! ?` …)
/// ne peut atteindre le parseur FTS5 hors d'une phrase, et chaque `"` est doublé,
/// donc le compte de guillemets reste toujours pair. Ne PAS réintroduire une liste
/// noire de caractères opérateurs — c'était le bug pré-existant (`.`, `,`, `'`,
/// `!`, `?` manquaient → HTTP 500).
///
/// Un jeton entièrement sûr (alphanumérique Unicode + `_`, hors mot-clé) est rendu
/// nu ; sinon il est enveloppé en phrase exacte, `"` et `'` doublés.
fn quote_fts_token(token: &str) -> String {
    let is_safe = |c: char| c.is_alphanumeric() || c == '_';
    let is_keyword = matches!(token.to_uppercase().as_str(), "AND" | "OR" | "NOT" | "NEAR");
    if !is_keyword && token.chars().all(is_safe) {
        token.to_string()
    } else {
        // Phrase exacte : guillemets internes doublés + apostrophes doublées
        // (le tokeniseur `unicode61` traite `'` comme délimiteur en mode phrase).
        let escaped = token.replace('"', "\"\"").replace('\'', "''");
        format!("\"{escaped}\"")
    }
}

/// Normalise une requête pour SQLite FTS5 en enveloppant **chaque jeton** au besoin.
///
/// ## Logique (rev2, F-162 lot 0 T1)
///
/// La requête est découpée sur les espaces ; chaque jeton est traité indépendamment
/// par [`quote_fts_token`], puis les jetons sont rejoints par un espace — FTS5 y voit
/// un ET implicite. Un jeton n'est rendu nu que s'il est intégralement sûr
/// (alphanumérique Unicode ou `_`) et n'est pas un mot-clé réservé (`AND`, `OR`,
/// `NOT`, `NEAR`) ; sinon il est cité en phrase exacte.
///
/// ## Ce que corrige rev2
///
/// L'enveloppe de la requête **entière** en une seule phrase (le comportement
/// précédent) transformait `cargo-semver-checks baseline` en phrase contiguë
/// `"cargo-semver-checks baseline"` — un seul trait d'union faisait tomber le
/// décompte à 0 (« absence prouvée »). Désormais chaque jeton est cité seul :
/// `"cargo-semver-checks" baseline` → ET des deux → correspondances réelles.
///
/// ## Neutralisation des opérateurs — documentée, plus silencieuse
///
/// `OR`, `NOT`, `NEAR` deviennent des jetons cités, donc des mots littéraux cherchés :
/// un parseur de requêtes exposant ces opérateurs est hors périmètre (voir T2/T4).
///
/// ## Immunité au HTTP 500
///
/// Préservée par construction : voir [`quote_fts_token`]. Ne PAS revenir à une
/// liste noire de caractères opérateurs (bug pré-existant → HTTP 500).
///
/// ## Visibilité
///
/// `pub(crate)` pour les tests unitaires de ce module ; hors API publique.
pub(crate) fn build_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(quote_fts_token)
        .collect::<Vec<_>>()
        .join(" ")
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
                "vault_search: get_titles_sections (semantic section filter) failed \
                 — BM25-only degradation (semantic hits dropped this round)"
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
                "retrieve_candidates: get_titles_sections (semantic multi-section filter) \
                 failed — BM25-only degradation (semantic hits dropped this round)"
            );
            Vec::new()
        }
    }
}

/// Filters semantic hits OUT of the excluded sections (default-search exclusion).
///
/// F-246 — inverse de [`filter_semantic_by_sections`] : garde les hits dont la
/// section n'est PAS dans `excluded_sections`. Utilisé quand la requête ne porte
/// aucun filtre de section explicite (`req.section = None`) pour écarter les
/// sections `Section::DEFAULT_SEARCH_EXCLUDED` (raw capture) du chemin sémantique.
///
/// `sec_result` est la valeur de retour de `IndexStore::get_titles_sections` pour
/// les IDs des hits sémantiques. Sémantique symétrique du filtre inclusif :
///
/// - `Ok(map)`: seul un hit dont la section (lue dans `map`) est ABSENTE de
///   `excluded_sections` est gardé. Un hit absent de la map (section inconnue) est
///   **exclu** — conservatisme : on ne surface pas un hit dont on ne peut pas vérifier
///   qu'il n'est pas dans une section exclue par défaut.
/// - `Err(_)`: **dégradation BM25-only** — vecteur vide. Une fuite de section exclue
///   serait pire que la perte du signal sémantique : le chemin BM25 reste filtré
///   (en mémoire dans `vault_search_impl`, au SQL dans `count_fts_matches`), donc la
///   recherche reste fonctionnelle et sans fuite.
///
/// Pure (sans I/O, sans état) — directement testable unitairement, y compris `Err`.
pub(crate) fn filter_semantic_excluding_sections(
    semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)>,
    excluded_sections: &[&str],
    sec_result: Result<std::collections::HashMap<String, (Option<String>, String)>, GradatumError>,
) -> Vec<(gradatum_core::identity::NoteId, f32)> {
    match sec_result {
        Ok(sec_map) => semantic_hits
            .into_iter()
            .filter(|(id, _)| {
                sec_map
                    .get(&id.to_string())
                    .map(|(_, sec)| !excluded_sections.contains(&sec.as_str()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "vault_search: get_titles_sections (semantic default-exclusion filter) failed \
                 — BM25-only degradation (semantic hits dropped this round)"
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
                "vault_search: get_statuses (semantic status filter) failed \
                 — BM25-only degradation (semantic hits dropped this round)"
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
/// 5. Return `SearchHit { vault_id, path: "<section>/<id>", score, snippet }`.
///
/// ## Provenance
///
/// Every result carries the `vault_id` it was read from — the vault the search
/// actually ran against, which on the cross-vault path is the request's target
/// rather than the caller's own vault. Clients never have to infer the origin of
/// a hit from the request they sent.
///
/// ## Score
///
/// `score` is the **composite hybrid** value: `fusion(BM25, semantic) ×
/// (1 + α·recency) × (1 + β·pagerank)`, where `fusion` is the weighted normalised
/// magnitude `0.5·normalize_bm25 + 0.5·normalize_semantic` when both arms respond
/// (F-162 critère 10), or the responding arm's normalised score at single arm
/// (critère 6). The magnitude is kept — the upper bound is now ≈ 1.3
/// (`[0,1]` fusion × composite ≤ 1.32), not the former RRF ceiling ≈ 0.04.
/// Treat it as a relative ordering signal, not an absolute calibrated similarity.
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
) -> Result<Response, StatusCode> {
    // Read the opt-in flag and capture the query before `req` is moved into the impl.
    // The query is only needed to render the compact absence hint, so clone it only
    // on the compact path (avoids a per-request allocation on the default path).
    let want_compact = req.compact;
    let compact_query = want_compact.then(|| req.query.clone());
    let resp = crate::api_v1::logic::vault_search_impl(&state, &trust, req)
        .await
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_search: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })?;
    // `compact=false` returns exactly `Json(resp)` as before → byte-for-byte identical.
    Ok(if want_compact {
        Json(CompactBody {
            compact: compact::render_search(&resp, compact_query.as_deref().unwrap_or_default()),
        })
        .into_response()
    } else {
        Json(resp).into_response()
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
) -> Result<Response, StatusCode> {
    // Read the opt-in flag before `req` is moved into the impl.
    let want_compact = req.compact;
    let resp = crate::api_v1::logic::vault_read_impl(&state, &trust, req)
        .await
        .map_err(|e| {
            match &e {
                gradatum_core::error::GradatumError::NoteNotFound(_)
                | gradatum_core::error::GradatumError::Storage(_) => {}
                other => {
                    tracing::error!(err = %other, "vault_read: backend failed");
                }
            }
            crate::api_v1::logic::err_to_status(&e)
        })?;
    // `compact=false` returns exactly `Json(resp)` as before → byte-for-byte identical.
    Ok(if want_compact {
        Json(CompactBody {
            compact: compact::render_read(&resp),
        })
        .into_response()
    } else {
        Json(resp).into_response()
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

// ── vault_archives_list (F-100 1.6 — read-only) ────────────────────────────────

/// `POST /api/v1/vault_archives_list`
///
/// **Lecture seule** : liste le registre d'archives (notes archivées par delete
/// on-demand). Filtres section/temps/gc/restored + pagination. Aucune mutation — le
/// delete/restore/purge vivent uniquement dans le namespace interne (CLI opérateur).
/// ACL Read.
pub async fn vault_archives_list(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<gradatum_dto::VaultArchivesListRequest>,
) -> Result<Json<gradatum_dto::VaultArchivesListResponse>, StatusCode> {
    crate::api_v1::logic::vault_archives_list_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_archives_list: registry failed");
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
/// Lists distinct authors in the caller's effective vault.
/// Delegates to `vault_authors_impl`, which resolves the tenant-scoped vault
/// (JWT-derived when multi-tenant is enabled) and calls
/// `state.search.distinct_authors(vault_id)`.
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
/// Lists distinct tags in the caller's effective vault with their usage frequency,
/// **most frequent first** and **bounded by default** (`?limit=` lifts the bound —
/// F-216). Delegates to `vault_tags_impl`, which resolves the tenant-scoped vault
/// (JWT-derived when multi-tenant is enabled) and calls
/// `state.search.distinct_tags(vault_id)`.
///
/// `limit` arrive en paramètre de requête (`Query`) : un appel sans query string
/// reste valide et rend la réponse par défaut bornée (rétrocompatible).
pub async fn vault_tags(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(req): Query<VaultTagsRequest>,
) -> Result<Json<VaultTagsResponse>, StatusCode> {
    crate::api_v1::logic::vault_tags_impl(&state, &trust, req)
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
        filter_semantic_excluding_sections, validate_search_status,
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

    /// F-246 — Ok + map complète : les hits des sections exclues sont retirés, les
    /// autres sont conservés.
    #[test]
    fn filter_semantic_excluding_ok_keeps_non_excluded() {
        let snap = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "01HBBBBBBBBBBBBBBBBBBBBBBB";
        let hits = vec![(nid(snap), 0.9f32), (nid(b), 0.8f32)];
        let mut map = HashMap::new();
        map.insert(snap.to_string(), (None, "snapshot".to_string()));
        map.insert(
            b.to_string(),
            (Some("Titre B".to_string()), "reference".to_string()),
        );

        let out = filter_semantic_excluding_sections(hits, &["snapshot"], Ok(map));
        assert_eq!(out.len(), 1, "seul le hit hors 'snapshot' reste");
        assert_eq!(out[0].0.to_string(), b);
    }

    /// F-246 — Ok + map partielle : un hit sans entrée (section inconnue) est exclu
    /// (on ne surface pas un hit dont on ne peut pas vérifier qu'il n'est pas exclu).
    #[test]
    fn filter_semantic_excluding_ok_partial_excludes_unknown() {
        let snap = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let ref_note = "01HBBBBBBBBBBBBBBBBBBBBBBB";
        let unknown = "01HCCCCCCCCCCCCCCCCCCCCCCC";
        let hits = vec![
            (nid(snap), 0.9f32),
            (nid(ref_note), 0.8f32),
            (nid(unknown), 0.7f32),
        ];
        // map contient snap (section exclue) et ref_note (visible) — unknown a une
        // section inconnue (absente de la map).
        let mut map = HashMap::new();
        map.insert(snap.to_string(), (None, "snapshot".to_string()));
        map.insert(ref_note.to_string(), (None, "reference".to_string()));

        let out = filter_semantic_excluding_sections(hits, &["snapshot"], Ok(map));
        assert_eq!(out.len(), 1, "seule la note 'reference' résolue survit");
        assert_eq!(out[0].0.to_string(), ref_note);
    }

    /// F-246 — Err (échec batch) : dégradation BM25-only, aucun hit ne fuit.
    #[test]
    fn filter_semantic_excluding_err_degrades_to_bm25_only() {
        let a = "01HAAAAAAAAAAAAAAAAAAAAAAA";
        let hits = vec![(nid(a), 0.9f32)];
        let err = Err(GradatumError::Storage("batch failure simulée".to_string()));

        let out = filter_semantic_excluding_sections(hits, &["snapshot"], err);
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

    /// Mot-clé FTS5 `AND` → seul le mot-clé est cité (F-162 rev2 : enveloppe par jeton).
    ///
    /// Avant rev2, le mot-clé enveloppait TOUTE la requête en phrase. Désormais
    /// `AND` devient un jeton cité littéral ; `gradatum` et `notes` restent nus.
    #[test]
    fn build_fts_query_fts5_keyword_and_is_wrapped() {
        let q = build_fts_query("gradatum AND notes");
        assert_eq!(
            q, r#"gradatum "AND" notes"#,
            "seul AND est cité — le reste garde l'ET implicite"
        );
    }

    /// Mot-clé FTS5 `NOT` → seul le mot-clé est cité (F-162 rev2).
    #[test]
    fn build_fts_query_fts5_keyword_not_is_wrapped() {
        let q = build_fts_query("notes NOT debug");
        assert_eq!(
            q, r#"notes "NOT" debug"#,
            "seul NOT est cité — le reste garde l'ET implicite"
        );
    }

    /// Query avec guillemets internes → guillemets doublés (F-162 rev2 : par jeton).
    ///
    /// Input : `say "hello"` → jetons `say` et `"hello"`.
    /// `say` est sûr → nu. `"hello"` (7 chars, dont 2 guillemets) → non sûr →
    /// `replace('"', "\"\"")` = `""hello""` → phrase : `"""hello"""`.
    /// Résultat joint : `say """hello"""`. Le compte de guillemets reste pair.
    #[test]
    fn build_fts_query_internal_quotes_doubled() {
        let q = build_fts_query(r#"say "hello""#);
        // Attendu : `say """hello"""` (say nu + espace + phrase à guillemets doublés).
        assert_eq!(
            q, "say \"\"\"hello\"\"\"",
            "guillemets internes doublés dans le jeton cité"
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

    /// C1/F-162 rev2 — `NEAR(...)` n'est plus enveloppé en une phrase : il est
    /// découpé sur l'espace en deux jetons ponctués, chacun cité individuellement.
    ///
    /// `NEAR(gradatum 5)` → jetons `NEAR(gradatum` et `5)` (le `(` et le `)` ne
    /// sont pas sûrs) → `"NEAR(gradatum" "5)"`. L'ancien nom (`_is_wrapped`) et
    /// l'ancien assert souple (`starts_with('"') && ends_with('"')`) masquaient ce
    /// changement de sortie : ils restaient verts sur deux jetons cités. Assertion
    /// exacte désormais, pour pinner ce que le test prétend tester.
    #[test]
    fn build_fts_query_near_expression_is_split_into_two_quoted_tokens() {
        assert_eq!(
            build_fts_query("NEAR(gradatum 5)"),
            "\"NEAR(gradatum\" \"5)\"",
            "NEAR(...) est découpé en deux jetons cités, pas enveloppé en une phrase"
        );
    }

    /// C1 — deux-points (frontmatter) → seul le jeton porteur du `:` est cité (F-162 rev2).
    ///
    /// `section:` porte le `:` (opérateur de colonne FTS5) → cité en phrase.
    /// `reasoning` est sûr → nu. Attendu : `"section:" reasoning`.
    #[test]
    fn build_fts_query_frontmatter_colon_is_wrapped() {
        let q = build_fts_query("section: reasoning");
        assert_eq!(
            q, r#""section:" reasoning"#,
            "seul le jeton porteur du deux-points est cité"
        );
    }

    // ── F-162 lot 0 T1 — enveloppe PAR JETON (rev2) ──────────────────────────────

    /// F-162 — un jeton ponctué est cité seul, le reste de la requête garde
    /// l'ET implicite. `cargo-semver-checks baseline` doit devenir
    /// `"cargo-semver-checks" baseline`, pas une phrase contiguë (qui rendait 0 match).
    #[test]
    fn build_fts_query_hyphenated_token_is_quoted_not_whole_query() {
        assert_eq!(
            build_fts_query("cargo-semver-checks baseline"),
            "\"cargo-semver-checks\" baseline"
        );
    }

    /// F-162 — une conjonction de versions n'est plus une seule phrase : chaque
    /// jeton est cité individuellement (le mot-clé `OR` devient un littéral cité).
    #[test]
    fn build_fts_query_conjunction_of_versions_is_not_one_phrase() {
        assert_eq!(
            build_fts_query("2.0.7 OR 2.0.6"),
            "\"2.0.7\" \"OR\" \"2.0.6\""
        );
    }

    /// F-162 — un jeton ponctué unique reste cité seul (inchangé par le découpage).
    #[test]
    fn build_fts_query_single_punctuated_token_unchanged() {
        assert_eq!(build_fts_query("2.0.7"), "\"2.0.7\"");
    }

    /// F-162 — immunité HTTP 500 prouvée par EXÉCUTION FTS5 réelle, pas par la forme.
    ///
    /// Chaque sonde traverse une vraie table FTS5 (`unicode61`) via `rusqlite`.
    /// Une régression produisant une chaîne bien formée mais rejetée par le moteur
    /// serait ici rouge — c'est le motif de faux vert que ce lot supprime.
    /// Les sondes couvrent : opérateurs isolés/enchâssés, ponctuation pure,
    /// guillemets/apostrophes déséquilibrés en entrée, requête vide/espaces,
    /// et Unicode (emoji / CJK / marque combinante).
    #[test]
    fn build_fts_query_output_is_always_accepted_by_fts5() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(body);
             INSERT INTO probe(body) VALUES ('gradatum notes 2.0.7 cargo-semver-checks');",
        )
        .unwrap();

        for probe in [
            "a-b",
            "a*b",
            "a:b",
            "a^b",
            "a\"b",
            "a(b",
            "a)b",
            "a.b",
            "a,b",
            "a'b",
            "a!b",
            "a?b",
            "-",
            "*",
            "\"",
            "''",
            "NEAR(x y)",
            "",
            "   ",
            "2.0.7 OR 2.0.6",
            "cargo-semver-checks baseline",
            "🙂 中文 e\u{0301}",
        ] {
            let q = build_fts_query(probe);
            // Seules les sondes vides ou blanches (`""`, `"   "`) ont le droit de
            // produire une sortie vide. Toute autre sonde qui s'effondre vers vide
            // est un bug (un jeton non vide perdu au découpage) et DOIT échouer :
            // l'ancien `continue` muet aurait laissé passer une telle régression.
            if probe.trim().is_empty() {
                assert!(
                    q.trim().is_empty(),
                    "sonde vide `{probe}` doit produire une sortie vide, obtenu `{q}`"
                );
                continue; // sortie vide : pas de MATCH FTS5 (early-return côté appelant)
            }
            assert!(
                !q.trim().is_empty(),
                "sonde non vide `{probe}` s'est effondrée vers une sortie vide"
            );
            let res: Result<i64, _> = conn.query_row(
                "SELECT COUNT(*) FROM probe WHERE probe MATCH ?1",
                [&q],
                |r| r.get(0),
            );
            assert!(
                res.is_ok(),
                "FTS5 a rejeté `{q}` (sonde `{probe}`) : {res:?}"
            );
        }
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
