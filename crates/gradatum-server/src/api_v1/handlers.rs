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

use std::collections::HashMap;

use axum::{extract::State, http::StatusCode, Extension, Json};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::error::GradatumError;
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;

use gradatum_embed::EmbedBackend;
use gradatum_index::extract_h1_title;
use gradatum_index::links::title_to_slug;
use gradatum_search::rrf_fuse;
use gradatum_search::scoring::{
    composite_score_with_trust, pagerank_factor, recency_factor, trust_decay_factor,
};

use crate::api_v1::dto::{
    AuthorEntry, GraphEdge, ScoreBreakdown, SearchHit, TagEntry, TraceEntry, VaultAuthorsResponse,
    VaultContextRequest, VaultContextResponse, VaultEntry, VaultGraphRequest, VaultGraphResponse,
    VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse, VaultReadRequest,
    VaultReadResponse, VaultSearchRequest, VaultSearchResponse, VaultStatusResponse,
    VaultTagsResponse, VaultTraceRequest, VaultTraceResponse,
};
use crate::api_v1::tenant_guard::effective_tenant;
use crate::state::AppState;

// ── Helpers internes ──────────────────────────────────────────────────────────

/// Builds the ACL locus for a tenant: `{tenant_id}/main` (default section).
fn locus_for_tenant(tenant_id: &str) -> String {
    format!("{}/main", tenant_id)
}

/// Builds the ACL locus for a specific section.
fn locus_for_section(tenant_id: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{}/{}", tenant_id, s),
        None => format!("{}/main", tenant_id),
    }
}

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
fn validate_search_status(status: Option<&str>) -> Result<Option<String>, ()> {
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
fn filter_semantic_by_section(
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
fn filter_semantic_by_status(
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let acl_locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // Validation F-31 : vault_id optionnel — non-vide, max 128 chars.
    if let Some(vid) = req.vault_id.as_deref() {
        if vid.is_empty() || vid.len() > 128 {
            tracing::warn!(
                vault_id = %vid,
                "vault_search: vault_id invalide (vide ou > 128 chars)"
            );
            return Err(StatusCode::BAD_REQUEST);
        }
        // P0 cross-tenant (Lot 4) : le cross-read vault_id n'est PAS supporté tant
        // que le VaultRegistry mono-vault est en place. Tout vault_id ≠ "main" → 403
        // (plus de "warn et continue" — l'ancien fallback était la faille F-31).
        if vid != "main" {
            tracing::warn!(
                vault_id = %vid,
                "vault_search: cross-read vault_id ≠ main non supporté (mono-vault) — 403"
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Validation F-37 notes fix : status optionnel — doit appartenir à la liste
    // fermée (6 NoteStatus + downgraded legacy), sinon 400.
    let status_filter = match validate_search_status(req.status.as_deref()) {
        Ok(s) => s,
        Err(()) => {
            tracing::warn!(
                status = ?req.status,
                "vault_search: status invalide (hors liste autorisée)"
            );
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    // vault_id effectif pour la lecture. Après le clamp Lot 4, vault_id (si présent)
    // == "main" == tenant : le cross-read est neutralisé en mono-vault. On retient
    // donc le tenant dérivé du JWT comme source unique.
    let read_vault_id = req.vault_id.as_deref().unwrap_or(&tenant).to_owned();

    // locus : préfixe brut passé aux fonctions sqlite qui appliquent elles-mêmes
    // l'échappement LIKE via escape_like (cf. doc sqlite.rs ligne ~750 :
    // "l'appelant n'a pas à le faire"). Double-escape antérieur corrigé B5 audit P0.

    // Query vide : FTS5 retournerait une erreur.
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Json(VaultSearchResponse {
            items: vec![],
            corpus_match_count: None,
            corpus_count_capped: false,
        }));
    }

    // Clamp limit : [1, 50], défaut 10.
    let limit = req.limit.unwrap_or(10).clamp(1, 50) as usize;

    let vault_id = VaultId::new(&read_vault_id);

    // Normalisation FTS5 — fix #32.
    let fts_query = build_fts_query(query);

    // ── Signal 1 : BM25 (limit * 2 pour la fusion RRF) ─────────────────────────
    //
    // search_fts_with_snippet retourne snippets FTS5 natifs + section + bm25 score.
    // Utilisé comme signal primaire ET comme source d'enrichissement (section, snippet, title).
    let bm25_hits = state
        .search
        .search_fts_with_snippet(
            &vault_id,
            &fts_query,
            limit * 2,
            req.include_downgraded,
            req.section.as_deref(),
            req.locus.as_deref(),
            status_filter.as_deref(),
        )
        .await
        .map_err(|e| {
            tracing::error!(err = %e, query = %query, "vault_search: search_fts_with_snippet failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // ── Signal 2 : sémantique (si embedder non-Noop) ───────────────────────────
    //
    // Dégradation gracieuse :
    // - Noop → skip silencieux.
    // - Erreur embed() → WARN log + vecteur vide (BM25 seul).
    // - Erreur search_semantic() → WARN log + vecteur vide (BM25 seul).
    //
    // F-31 : read_vault_id est passé à search_semantic (cross-vault lecture).
    // Si vault_id ≠ tenant_id ET 0 hits sémantiques → signalé en tracing::info!
    // (caveat C spec : pas de champ breaking dans VaultSearchResponse).
    let semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> = if state
        .embedder
        .backend_kind()
        != EmbedBackend::Noop
    {
        match state.embedder.embed(query).await {
            Ok(query_emb) => {
                let hits = state
                    .search
                    .search_semantic(
                        &read_vault_id,
                        state.embedder.embedder_id(),
                        &query_emb,
                        limit * 2,
                        req.locus.as_deref(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            err = %e,
                            query = %query,
                            "vault_search: search_semantic failed, BM25 only"
                        );
                        vec![]
                    });
                // F-31 : signaler 0 hits sémantiques sur vault ≠ tenant. En mono-vault
                // (Lot 4) cette branche est inerte (vault_id == "main" == tenant), conservée
                // pour le jour où le cross-read multi-vault sera réactivé.
                if hits.is_empty() && req.vault_id.as_deref().is_some() && read_vault_id != tenant {
                    tracing::info!(
                        vault_id = %read_vault_id,
                        query = %query,
                        "vault_search: 0 hits sémantiques sur vault cross-tenant \
                         (embeddings absents ou vault vide)"
                    );
                }
                hits
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    query = %query,
                    "vault_search: embed() failed, BM25 only"
                );
                vec![]
            }
        }
    } else {
        vec![]
    };

    // ── Fix C2 (v0.4.4) — filtre section sur le chemin sémantique ──────────────
    //
    // Bug d'origine : `search_semantic` ne reçoit que `locus`, jamais `section`.
    // Le chemin BM25 (`search_fts_with_snippet`) filtrait bien par section, mais
    // les hits sémantique-only d'autres sections fuyaient dans la fusion RRF dès
    // qu'un embedder était actif (cas LIVE bge-m3). Constaté empiriquement :
    // `section=lessons-learned` retournait des notes `debug`/`decisions`.
    //
    // Correctif (server-side, pré-RRF — pas de perte de résultats par troncature) :
    // si `section` est demandée, on récupère la section de chaque hit sémantique en
    // un seul SELECT batch (`get_titles_sections`) et on ne garde que celles qui
    // matchent. Filtrer AVANT la fusion préserve la complétude des `limit` résultats.
    // (Alternative écartée : changer la signature `search_semantic` → churn du trait
    // VectorStore + 4 branches SQL section×locus pour un gain identique.)
    //
    // Audit lot C P1 (1.1) : un ÉCHEC du batch (Err) dégrade en **BM25-only** (on jette
    // tous les hits sémantiques) plutôt que de les laisser fuir non filtrés — un leak de
    // section est pire qu'une perte de signal sémantique sur ce tour. À distinguer d'une
    // map vide/partielle LÉGITIME (Ok) : dans ce cas on filtre normalement, et un hit
    // sans entrée dans la map (section inconnue) est exclu (`unwrap_or(false)`).
    let semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> = if let Some(wanted_section) =
        req.section.as_deref()
    {
        if semantic_hits.is_empty() {
            semantic_hits
        } else {
            let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
            let sec_result = state
                .search
                .get_titles_sections(&read_vault_id, &sem_ids)
                .await;
            filter_semantic_by_section(semantic_hits, wanted_section, sec_result)
        }
    } else {
        semantic_hits
    };

    // ── F-37 notes fix — filtre status sur le chemin sémantique (symétrique C2) ──
    //
    // `search_semantic` ne reçoit pas le filtre status ; sans ce filtre les hits
    // sémantique-only d'autres statuts fuiraient dans la fusion RRF (même classe de
    // bug que le leak de section C2). On récupère le statut SQL brut de chaque hit
    // sémantique en un seul batch (`get_statuses`) et on ne garde que ceux qui matchent.
    let semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> = if let Some(wanted_status) =
        status_filter.as_deref()
    {
        if semantic_hits.is_empty() {
            semantic_hits
        } else {
            let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
            let status_result = state.search.get_statuses(&read_vault_id, &sem_ids).await;
            filter_semantic_by_status(semantic_hits, wanted_status, status_result)
        }
    } else {
        semantic_hits
    };

    // ── Fusion RRF ─────────────────────────────────────────────────────────────
    //
    // bm25_hits : trié ASC (plus négatif = meilleur BM25) — conforme à rrf_fuse.
    // semantic_hits : trié DESC (plus grand cosine = meilleur) — conforme à rrf_fuse.
    let bm25_for_rrf: Vec<(String, f64)> = bm25_hits
        .iter()
        .map(|h| (h.note_id.to_string(), h.bm25))
        .collect();

    let sem_for_rrf: Vec<(String, f32)> = semantic_hits
        .iter()
        .map(|(id, score)| (id.to_string(), *score))
        .collect();

    // Map BM25 pour enrichissement O(1) (section, snippet, title).
    let bm25_map: HashMap<String, &gradatum_index::SearchHitRaw> = bm25_hits
        .iter()
        .map(|h| (h.note_id.to_string(), h))
        .collect();

    // k=60 (standard Cormack et al. 2009). On demande limit*4 pour avoir
    // un buffer suffisant avant le scoring composite et le re-tri (top-20).
    let rrf_buffer = (limit * 4).clamp(20, 200);
    let mut fused = rrf_fuse(&bm25_for_rrf, &sem_for_rrf, 60.0, rrf_buffer);

    // Enrichir section + snippet + title + status depuis la map BM25.
    // Les hits semantic-only conservent section="" / snippet=None / status="" (enrichis plus bas).
    //
    // Invariant vault_read == vault_search (P2-R1) : filtrer les titres vides/whitespace
    // de la colonne SQL (`title = ""` possible sur notes legacy) pour rester symétrique
    // au filtre `.filter(!trim().is_empty())` appliqué dans vault_read.
    for hit in &mut fused {
        if let Some(bh) = bm25_map.get(&hit.note_id) {
            hit.section = bh.section.clone();
            hit.snippet = Some(bh.snippet.clone());
            hit.title = bh.title.clone().filter(|s| !s.trim().is_empty());
            hit.status = bh.status.clone();
        }
    }

    // ── alpha.12 Task 13 — Scoring composite multi-facteur ───────────────────
    //
    // Pour chaque RrfHit : recency_factor + pagerank_factor → composite_score.
    // N+1 acceptable pour N ≤ 200 (clamp ci-dessus). Chaque
    // get_note_created_and_indegree = 2 requêtes O(log N) via index B-tree.
    // Batchage différé Phase 2.x.4 (caveat C2 backlog).
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut composite_hits: Vec<(gradatum_search::RrfHit, f64)> = Vec::with_capacity(fused.len());

    // F-37 S1.1 — décomposition de score opt-in. Construite ici (point unique où tous
    // les facteurs sont en main) et indexée par note_id pour survivre aux deux chemins
    // de tri ultérieurs (reranker / noop). HashMap vide si `include_scores = false`.
    let mut score_breakdowns: HashMap<String, ScoreBreakdown> = HashMap::new();

    for hit in fused {
        let (created_ms, in_degree) = match state
            .search
            .get_note_created_and_indegree(&tenant, &hit.note_id)
            .await
        {
            Ok(v) => v,
            Err(GradatumError::NoteNotFound(_)) => {
                // Note absente de l'index (RRF a ramené un orphan) — fallback gracieux.
                tracing::debug!(
                    note_id = %hit.note_id,
                    "vault_search: note absente, fallback (now_ms, 0)"
                );
                (now_ms, 0u64)
            }
            Err(e) => {
                tracing::error!(
                    err = %e,
                    note_id = %hit.note_id,
                    "vault_search: get_note_created_and_indegree storage error"
                );
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

        let recency = recency_factor(created_ms, now_ms);
        let pagerank = pagerank_factor(in_degree);

        // ── F-17 trust decay — couche RRF uniquement (jamais BM25) ────────────
        // Résolution des paramètres trust : Some(...) si decay activé + trust présent,
        // None ⇒ composite_score_with_trust neutralise le facteur (scores v0.4.3).
        // L'ordre forgotten > downgraded est appliqué EN AMONT (couche SQL search_fts) ;
        // ici on applique [RRF × recency × pagerank × trust_decay].
        let trust_params = if state.scoring.enabled {
            // P3-1 : ne pas avaler silencieusement l'erreur — un échec de lecture
            // trust dégrade le scoring (fallback neutre v0.4.3) mais doit être tracé.
            let (trust, provenance) = match state
                .search
                .get_trust_and_provenance(&tenant, &hit.note_id)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        note_id = %hit.note_id,
                        "vault_search: get_trust_and_provenance échoué — fallback sans decay trust"
                    );
                    (None, None)
                }
            };
            let age_days = ((now_ms - created_ms).max(0) as f64) / 86_400_000.0;
            state
                .scoring
                .resolve(trust, provenance.as_deref(), age_days)
        } else {
            None
        };

        let composite = composite_score_with_trust(hit.rrf_score, recency, pagerank, trust_params);

        // F-37 S1.1 — capturer la décomposition AVANT de déplacer `hit`.
        // `trust_params = Some((trust, age_days, half_life))` ⇒ trust réel résolu ;
        // on recalcule `trust_decayed` via la même fonction pure que le scoring.
        if req.include_scores {
            let (trust_raw, trust_decayed) = match trust_params {
                Some((trust, age_days, half_life)) => (
                    Some(trust as f32),
                    Some(trust_decay_factor(trust, age_days, half_life)),
                ),
                None => (None, None),
            };
            score_breakdowns.insert(
                hit.note_id.clone(),
                ScoreBreakdown {
                    rrf_score: hit.rrf_score,
                    recency_factor: recency,
                    pagerank_factor: pagerank,
                    in_degree,
                    trust_raw,
                    trust_decayed,
                    composite,
                    bm25_rank: hit.bm25_rank,
                    sem_rank: hit.sem_rank,
                },
            );
        }

        composite_hits.push((hit, composite));
    }

    // Re-tri DESC stable par composite_score.
    composite_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // ── alpha.12 Task 14 — Cross-encoder reranker (top-20) ────────────────────
    //
    // Si le reranker câblé exige le body_text (cf. `Reranker::requires_body`),
    // on charge top-20, on rerank, et on truncate(limit).
    // Sinon (NoopReranker par défaut) : skip — ordre composite préservé directement.
    let rerank_n = state.reranker.max_batch_size().min(20);
    let final_hits: Vec<(gradatum_search::RrfHit, f32)> =
        if state.reranker.requires_body() && !composite_hits.is_empty() && rerank_n > 0 {
            // Charger les body_text des top-rerank_n candidats (caveat C2 backlog : batchage).
            let mut top_for_rerank: Vec<(gradatum_search::RrfHit, f64)> =
                composite_hits.into_iter().take(rerank_n).collect();

            let mut rerank_candidates: Vec<(String, String)> =
                Vec::with_capacity(top_for_rerank.len());
            for (hit, _composite) in &top_for_rerank {
                // get_note retourne Option<NoteRecord> — None si note absente (orphan RRF).
                let body = match state.search.get_note(&tenant, &hit.note_id).await {
                    Ok(Some(rec)) => rec.body_text,
                    Ok(None) => {
                        tracing::debug!(
                            note_id = %hit.note_id,
                            "vault_search reranker: note absente, body=\"\""
                        );
                        String::new()
                    }
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            note_id = %hit.note_id,
                            "vault_search reranker: get_note storage error, body=\"\""
                        );
                        String::new()
                    }
                };
                rerank_candidates.push((hit.note_id.clone(), body));
            }

            let reranker = std::sync::Arc::clone(&state.reranker);
            let query_owned = query.to_string();
            let cand_clone = rerank_candidates.clone();

            let rerank_start = std::time::Instant::now();
            // Caveat L-P0-5 — fallback gracieux WARN, pas de panic.
            // block_in_place : runtime multi_thread garanti en prod (caveat C8 / R4).
            let rerank_result =
                tokio::task::block_in_place(move || reranker.rerank(&query_owned, &cand_clone));
            let rerank_elapsed = rerank_start.elapsed();

            let scores: Vec<f32> = match rerank_result {
                Ok(s) => {
                    tracing::info!(
                        rerank_n = top_for_rerank.len(),
                        elapsed_ms = rerank_elapsed.as_millis(),
                        "vault_search: reranker OK"
                    );
                    s
                }
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        "vault_search: reranker failed, falling back to composite order"
                    );
                    // Génère scores synthétiques décroissants alignés sur l'ordre composite
                    let n = top_for_rerank.len();
                    let denom = n as f32 + 1.0;
                    (0..n).map(|i| 1.0 - (i as f32) / denom).collect()
                }
            };

            // Re-trier par reranker score DESC (stable).
            let mut zipped: Vec<(gradatum_search::RrfHit, f32)> = top_for_rerank
                .drain(..)
                .map(|(hit, _composite)| hit)
                .zip(scores)
                .collect();
            zipped.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            zipped.truncate(limit);
            zipped
        } else {
            // NoopReranker / pas de body requis : prendre composite top `limit` directement,
            // utiliser composite_score (en f32) comme score retourné.
            composite_hits
                .into_iter()
                .take(limit)
                .map(|(hit, composite)| (hit, composite as f32))
                .collect()
        };

    // ── Passe d'enrichissement finale — hits sémantique-only ──────────────────
    //
    // Les hits `is_semantic_only = true` (marqués à la fusion RRF) sont absents
    // de `bm25_map` : `search_fts_with_snippet` ne les a pas retournés, donc
    // ils ont conservé `title = None` et `section = ""` après l'enrichissement BM25.
    //
    // On collecte ces identifiants et on récupère `title` + `section` depuis
    // la table `notes` en un seul SELECT batch (N ≤ limit ≤ 50 — N+1 borné).
    //
    // Snippet sémantique-only : laissé à `None` — il n'y a pas de match FTS5
    // pour générer un extrait localisé pertinent. Le consommateur doit traiter
    // `snippet: None` comme "pas d'extrait disponible".
    //
    // Note : `hit.is_semantic_only` est calculé à la fusion RRF (source de vérité)
    // et remplace l'ancienne heuristique `title.is_none() && section.is_empty()`.
    // Les deux sont équivalentes au moment du filtre (un hit BM25 a toujours été
    // enrichi section+title ci-dessus), mais `is_semantic_only` est robuste aux
    // évolutions futures (ex : enrichissement partiel d'un hit mixte).
    let semantic_only_ids: Vec<String> = final_hits
        .iter()
        .filter(|(hit, _score)| hit.is_semantic_only)
        .map(|(hit, _score)| hit.note_id.clone())
        .collect();

    let title_section_map: HashMap<String, (Option<String>, String)> =
        if semantic_only_ids.is_empty() {
            HashMap::new()
        } else {
            state
                .search
                .get_titles_sections(&tenant, &semantic_only_ids)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        err = %e,
                        count = semantic_only_ids.len(),
                        "vault_search: get_titles_sections failed, semantic-only hits sans titre"
                    );
                    HashMap::new()
                })
        };

    // F-37 notes fix — statut SQL brut des hits sémantique-only (absents de bm25_map).
    // Batch unique aligné sur title_section_map. Échec → map vide (status="" en sortie).
    let status_map: HashMap<String, String> = if semantic_only_ids.is_empty() {
        HashMap::new()
    } else {
        state
            .search
            .get_statuses(&tenant, &semantic_only_ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    err = %e,
                    count = semantic_only_ids.len(),
                    "vault_search: get_statuses failed, semantic-only hits sans status"
                );
                HashMap::new()
            })
    };

    // ── Construire la réponse ──────────────────────────────────────────────────
    let items: Vec<SearchHit> = final_hits
        .into_iter()
        .map(|(mut hit, score)| {
            // Enrichir les hits sémantique-only (title absent OU section vide)
            // depuis le batch récupéré ci-dessus. Ne pas écraser les enrichissements
            // BM25 existants (section non vide ou title déjà présent).
            if hit.title.is_none() || hit.section.is_empty() {
                if let Some((fetched_title, fetched_section)) = title_section_map.get(&hit.note_id)
                {
                    if hit.title.is_none() {
                        // Même filtre que vault_read (invariant P2-R1) : titre vide → None.
                        hit.title = fetched_title.clone().filter(|s| !s.trim().is_empty());
                    }
                    if hit.section.is_empty() {
                        hit.section = fetched_section.clone();
                    }
                }
            }
            // F-37 notes fix — status : enrichir les hits sémantique-only depuis le batch.
            if hit.status.is_empty() {
                if let Some(fetched_status) = status_map.get(&hit.note_id) {
                    hit.status = fetched_status.clone();
                }
            }
            let section = if hit.section.is_empty() {
                "main".to_string()
            } else {
                hit.section
            };
            // F-37 S1.1 — rattacher la décomposition (opt-in). `remove` déplace
            // l'ownership : chaque note apparaît au plus une fois dans `final_hits`.
            let scores = score_breakdowns.remove(&hit.note_id);
            // D1.4 : SearchHit.trust est #[deprecated] (legacy hardcodé 0.5). On le
            // peuple encore pour la rétrocompat wire → allow(deprecated) ciblé sur le
            // seul site de construction légitime (le vrai trust vit dans scores.trust_raw).
            #[allow(deprecated)]
            SearchHit {
                path: format!("{}/{}", section, hit.note_id),
                score,
                title: hit.title,
                snippet: hit.snippet,
                // F-47 v0.4.0 : trust legacy hardcodé (déprécié — cf. scores.trust_raw).
                trust: 0.5,
                // F-37 notes fix — statut SQL brut (additif ; valeur réelle de la note).
                status: hit.status,
                scores,
            }
        })
        .collect();

    // ── corpus_match_count (R1-R4 spec corpus-hits) ───────────────────────────
    //
    // Exécuté SEULEMENT si include_corpus_count=true — zéro surcoût par défaut (R5).
    // COUNT FTS5/BM25 uniquement (pas ANN) → corpus_match_count < len(items) est
    // NOMINAL avec embedder actif (hits sémantiques-purs). Invariant R2 :
    // corpus_match_count >= count(items où !is_semantic_only).
    let (corpus_match_count, corpus_count_capped) = if req.include_corpus_count {
        match state
            .search
            .count_fts_matches(
                &vault_id,
                &fts_query,
                req.include_downgraded,
                req.section.as_deref(),
                req.locus.as_deref(),
                status_filter.as_deref(),
            )
            .await
        {
            Ok((count, capped)) => (Some(count), capped),
            Err(e) => {
                // Dégradation gracieuse : log + réponse sans corpus_count
                // (pas de 500 sur une feature opt-in additif).
                tracing::warn!(
                    err = %e,
                    query = %query,
                    "vault_search: count_fts_matches failed, corpus_match_count absent"
                );
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    Ok(Json(VaultSearchResponse {
        items,
        corpus_match_count,
        corpus_count_capped,
    }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── B4 (alpha.13 Task 14) : résolution titre → ULID si non-ULID ────────────
    //
    // ULID parse OK → chemin direct (non-régression T4 P2.0c).
    // ULID parse KO → tentative title_lookup (filtre `AND status = 'live'`).
    // Le `resolved_path` est ensuite utilisé pour `read_note_by_id`.
    //
    // Item B (2026-06-05) : vault_search émet path="<section>/<ulid>". On extrait
    // le dernier segment comme candidat ULID avant le fallback title_lookup.
    // title_lookup reste sur req.path original (un titre sans '/' est inchangé).
    let ulid_candidate = req.path.rsplit('/').next().unwrap_or(req.path.as_str());
    let resolved_path: String = if ulid::Ulid::from_string(ulid_candidate).is_ok() {
        ulid_candidate.to_string()
    } else {
        match state.search.title_lookup(&tenant, &req.path).await {
            Ok(Some(found_id)) => {
                tracing::debug!(
                    title = %req.path,
                    resolved_id = %found_id,
                    "vault_read: titre résolu via title_lookup"
                );
                found_id
            }
            // F-39 — fallback redirect : title_lookup a échoué, tenter resolve_redirect.
            // Cas : la note a été renommée — l'ancien slug est dans redirect_table.
            Ok(None) => {
                let slug = title_to_slug(&req.path);
                match state.search.resolve_redirect(&slug).await {
                    Ok(Some(ulid)) => {
                        tracing::debug!(
                            title = %req.path,
                            slug = %slug,
                            resolved_id = %ulid,
                            "vault_read: titre résolu via redirect_table (F-39)"
                        );
                        ulid.to_string()
                    }
                    Ok(None) => {
                        tracing::debug!(
                            path = %req.path,
                            slug = %slug,
                            "vault_read: titre non trouvé (ni live, ni redirect)"
                        );
                        return Err(StatusCode::NOT_FOUND);
                    }
                    Err(e) => {
                        tracing::error!(
                            err = %e,
                            path = %req.path,
                            "vault_read: resolve_redirect failed"
                        );
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            }
            Err(e) => {
                tracing::error!(err = %e, path = %req.path, "vault_read: title_lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    // T4 P2.0c : délégation via Registry::read_note_by_id sur le `resolved_path`.
    // Une erreur Storage avec "ULID invalide" est traitée comme NoteNotFound (404).
    match state.vault.read_note_by_id(&resolved_path).await {
        Ok(note) => {
            let body = note.body.markdown;
            let size_bytes = body.len() as u64;
            // Convertit le ContentHash (32 bytes) en hex.
            let sha256: String = note
                .content_hash
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();

            // ── Enrichissement `title` (fix TODO 01KV39C8A5) ─────────────────────────────
            //
            // Garantit la cohérence round-trip `vault_read` / `vault_search` :
            // la même source `get_titles_sections` est utilisée par les deux endpoints.
            //
            // Stratégie best-effort (jamais bloquante) :
            // 1. `get_titles_sections` → colonne `title` de l'index SQLite.
            // 2. Fallback H1 : si colonne vide/None, extraire la 1ʳᵉ ligne `# ...` du body.
            // 3. Err de `get_titles_sections` → warn + title=None (read reste 200 OK).
            //
            // Contrainte ACL : le `tenant` est déjà dérivé et validé via `effective_tenant`
            // + `AclOp::Read` ci-dessus — on le réutilise sans re-dériver.
            let title: Option<String> = {
                let ids = std::slice::from_ref(&resolved_path);
                match state.search.get_titles_sections(&tenant, ids).await {
                    Ok(map) => {
                        // `.0` = `Option<String>` (titre), `.1` = section (ignoré ici).
                        map.get(&resolved_path)
                            .and_then(|(t, _)| t.clone())
                            .filter(|s| !s.trim().is_empty())
                    }
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            note_id = %resolved_path,
                            "vault_read: get_titles_sections échoué — title=None (best-effort)"
                        );
                        None
                    }
                }
            };
            // Fallback H1 : si l'index ne donne pas de titre, déléguer à
            // `extract_h1_title` (gradatum-index::queries) — définition canonique
            // alignée sur le prédicat SQL de `title_lookup` (`body_text LIKE '# %'`).
            // Un H1 en ligne 2 ou indenté retourne None (cohérence SQL ↔ runtime).
            let title: Option<String> = title.or_else(|| extract_h1_title(&body));

            Ok(Json(VaultReadResponse {
                path: note.id.to_string(),
                title,
                content: body,
                metadata: Some(serde_json::json!({
                    "section": note.frontmatter.section.to_string(),
                    "status": note.frontmatter.status.to_string(),
                    "author": note.frontmatter.author.as_ref().map(|a| a.id.as_str()),
                    "tags": note.frontmatter.tags.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                    "vault_id": note.frontmatter.vault_id.as_str(),
                    "created": note.frontmatter.created.timestamp_millis(),
                    "updated": note.frontmatter.updated.map(|d| d.timestamp_millis()),
                })),
                size_bytes,
                sha256,
            }))
        }
        Err(GradatumError::NoteNotFound(_)) => Err(StatusCode::NOT_FOUND),
        // ULID invalide = identifiant inexistant → 404 (pas d'erreur serveur)
        Err(GradatumError::Storage(ref msg)) if msg.contains("ULID invalide") => {
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            tracing::error!(err = %e, note_id = %resolved_path, "vault_read: read_note_by_id failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // M6 fix (alpha.10) : impl réelle vault_list avec pagination ULID curseur.
    // Pattern ignoré pour l'instant (T12 pattern filter à brancher en Phase 2.x.5).
    let _ = req.pattern;

    let limit = req.limit.unwrap_or(20).clamp(1, 200) as usize;

    let (records, total) = state
        .search
        .list_notes(
            &tenant,
            req.section.as_deref(),
            limit,
            req.cursor.as_deref(),
        )
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "vault_list: list_notes failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Curseur : présent si on a reçu autant de résultats que demandés (plus de pages).
    let next_cursor = if records.len() == limit {
        records.last().map(|r| r.id.clone())
    } else {
        None
    };

    let entries: Vec<VaultEntry> = records
        .into_iter()
        .map(|r| {
            let modified_at = {
                let ms = r.updated.unwrap_or(r.created);
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_default()
            };
            VaultEntry {
                path: format!("{}/{}", r.section, r.id),
                size_bytes: r.body_text.len() as u64,
                modified_at,
            }
        })
        .collect();

    Ok(Json(VaultListResponse {
        entries,
        next_cursor,
        total,
    }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Single-tenant v0.4.x: tenant_id always "main". Multi-tenant deferred to v0.5.1.
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // Bug1 fix (alpha.10) : note_count via COUNT(*) WHERE status='live' (plus tenant_count)
    // Bug2 fix (alpha.10) : total_size_bytes via COALESCE(SUM(LENGTH(body_text)),0) (plus locus_count)
    // Fallback à 0 en cas d'erreur — le handler ne doit jamais crasher pour une métrique.
    let note_count = state.search.live_note_count("main").await.unwrap_or(0);
    let total_size_bytes = state
        .search
        .total_body_size_bytes("main")
        .await
        .unwrap_or(0);
    Ok(Json(VaultStatusResponse {
        tenant_id: "main".to_string(),
        note_count,
        total_size_bytes,
        index_version: "v1".to_string(),
        last_indexed_at: None,
        health: "healthy".to_string(),
    }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // depth cap : index CTE récursif limité à 3 niveaux (SQLite RECURSIVE perf).
    // Valeurs > 5 sont refusées explicitement (400) — erreur client non ambiguë.
    // Valeurs 4-5 sont silencieusement ramenées à 3 (le max effectif du CTE).
    let raw_depth = req.depth.unwrap_or(2);
    if raw_depth > 5 {
        tracing::warn!(
            depth = raw_depth,
            "vault_graph: depth > 5 refusé (max effectif = 3)"
        );
        return Err(StatusCode::BAD_REQUEST);
    }
    let depth = raw_depth.min(3) as u8;
    // Forward neighbors (liens sortants BFS).
    let neighbors = state
        .search
        .neighbors(&tenant, &req.root, depth)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "vault_graph: neighbors failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Edges forward : root → chaque voisin direct (depth=1 seulement pour les arcs explicites).
    let mut edges: Vec<GraphEdge> = neighbors
        .iter()
        .map(|n| GraphEdge {
            from: req.root.clone(),
            to: n.clone(),
            kind: "wikilink".to_string(),
        })
        .collect();

    let mut nodes: Vec<String> = neighbors;
    nodes.push(req.root.clone());
    nodes.sort();
    nodes.dedup();

    // Backlinks si demandés.
    if req.include_backlinks.unwrap_or(false) {
        let backlinks = state
            .search
            .backlinks(&tenant, &req.root)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, "vault_graph: backlinks failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        for bl in &backlinks {
            edges.push(GraphEdge {
                from: bl.clone(),
                to: req.root.clone(),
                kind: "wikilink".to_string(),
            });
            if !nodes.contains(bl) {
                nodes.push(bl.clone());
            }
        }
        nodes.sort();
        nodes.dedup();
    }

    Ok(Json(VaultGraphResponse { nodes, edges }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // Liens sortants (depth=1 : voisins directs).
    let outbound = state
        .search
        .neighbors(&tenant, &req.path, 1)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "vault_links: neighbors failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut edges: Vec<GraphEdge> = outbound
        .iter()
        .map(|n| GraphEdge {
            from: req.path.clone(),
            to: n.clone(),
            kind: "wikilink".to_string(),
        })
        .collect();

    let mut nodes: Vec<String> = outbound;
    nodes.push(req.path.clone());

    // Backlinks si demandés.
    if req.include_backlinks.unwrap_or(true) {
        let backlinks = state
            .search
            .backlinks(&tenant, &req.path)
            .await
            .map_err(|e| {
                tracing::error!(err = %e, "vault_links: backlinks failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        for bl in &backlinks {
            edges.push(GraphEdge {
                from: bl.clone(),
                to: req.path.clone(),
                kind: "wikilink".to_string(),
            });
            if !nodes.contains(bl) {
                nodes.push(bl.clone());
            }
        }
    }

    nodes.sort();
    nodes.dedup();
    Ok(Json(VaultLinksResponse { nodes, edges }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let limit = req.limit.unwrap_or(20).clamp(1, 200) as usize;

    // ── M4 : résolution multi-mode req.query → seeds ──────────────────────────
    //
    // Priorité : (1) ULID direct → (2) titre exact → (3) FTS textuel.
    let resolved_seeds: Vec<String> = if ulid::Ulid::from_string(&req.query).is_ok() {
        vec![req.query.clone()]
    } else {
        match state.search.title_lookup(&tenant, &req.query).await {
            Ok(Some(note_id)) => {
                tracing::debug!(
                    title = %req.query,
                    id = %note_id,
                    "vault_trace: titre résolu via title_lookup"
                );
                vec![note_id]
            }
            Ok(None) => {
                // FTS textuel : top-N seeds (limité pour éviter N+1 trace_lineage).
                let fts_q = build_fts_query(&req.query);
                if fts_q.trim_matches(['"', ' ']).is_empty() {
                    return Ok(Json(VaultTraceResponse { entries: vec![] }));
                }
                let vault_id = VaultId::new(&tenant);
                let fts_limit = limit.min(5);
                match state
                    .search
                    .search_fts_with_snippet(
                        &vault_id, &fts_q, fts_limit, /* include_downgraded= */ false,
                        /* section= */ None, /* locus= */ None, /* status= */ None,
                    )
                    .await
                {
                    Ok(hits) => {
                        let ids: Vec<String> =
                            hits.into_iter().map(|h| h.note_id.to_string()).collect();
                        tracing::debug!(
                            query = %req.query,
                            seeds = ids.len(),
                            "vault_trace: FTS textuel — seeds trouvées"
                        );
                        ids
                    }
                    Err(e) => {
                        tracing::error!(
                            err = %e,
                            query = %req.query,
                            "vault_trace: search_fts_with_snippet failed"
                        );
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }
            }
            Err(e) => {
                tracing::error!(err = %e, query = %req.query, "vault_trace: title_lookup failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    if resolved_seeds.is_empty() {
        return Ok(Json(VaultTraceResponse { entries: vec![] }));
    }

    // Task 20 C2 (alpha.15 Phase 2) : trace_lineage parallélisé via tokio::task::JoinSet.
    // N ≤ 5 seeds — chaque seed spawne une task async. Le mutex SQLite interne à
    // trace_lineage sérialise les accès DB (~1ms/acq), mais les tasks Tokio sont
    // planifiées en parallèle, réduisant la latence perçue côté HTTP.
    // Pas de nouvelle dépendance : JoinSet est fourni par tokio (déjà .workspace=true).
    let mut join_set = tokio::task::JoinSet::new();
    for seed_id in resolved_seeds {
        let search = state.search.clone();
        let tenant_id = tenant.clone();
        join_set.spawn(async move {
            search
                .trace_lineage(&tenant_id, &seed_id)
                .await
                .map_err(|e| {
                    tracing::error!(
                        err = %e,
                        seed = %seed_id,
                        "vault_trace: trace_lineage failed"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        });
    }

    let mut all_ids: Vec<String> = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        // join_result : Result<Result<Lineage, StatusCode>, JoinError>
        let lineage = join_result.map_err(|e| {
            tracing::error!(err = %e, "vault_trace: JoinSet task panicked");
            StatusCode::INTERNAL_SERVER_ERROR
        })??;
        all_ids.extend(lineage.parents);
        all_ids.extend(lineage.children);
    }

    // Dédupliquer (préserve l'ordre — important si on enrichit avec score plus tard)
    // et limiter à `req.limit`.
    let mut seen = std::collections::HashSet::new();
    all_ids.retain(|id| seen.insert(id.clone()));
    all_ids.truncate(limit);

    let entries = all_ids
        .into_iter()
        .map(|id| TraceEntry {
            path: id,
            score: 1.0,
            snippet: None,
            tags: vec![],
        })
        .collect();

    Ok(Json(VaultTraceResponse { entries }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let max_tokens = req.max_tokens.unwrap_or(2000).clamp(1, 8000) as usize;

    // ── M5 (alpha.13 Task 16) : résolution multi-mode req.query → note_ids ────
    //
    // Chemin 1 : ULID direct → note + backlinks (sources). Budget appliqué sur
    //            le body_text de la note principale uniquement (les backlinks
    //            sont dans `sources`, pas concaténés au context).
    // Chemin 2 : query textuelle → FTS top-10 → boucle budget par note.
    let top_note_ids: Vec<String> = if ulid::Ulid::from_string(&req.query).is_ok() {
        let backlinks = state
            .search
            .backlinks(&tenant, &req.query)
            .await
            .unwrap_or_default();
        let mut ids = vec![req.query.clone()];
        ids.extend(backlinks);
        ids
    } else {
        let fts_q = build_fts_query(&req.query);
        if fts_q.trim_matches(['"', ' ']).is_empty() {
            return Ok(Json(VaultContextResponse {
                context: String::new(),
                estimated_tokens: 0,
                sources: vec![],
            }));
        }
        let vault_id = VaultId::new(&tenant);
        match state
            .search
            .search_fts_with_snippet(
                &vault_id,
                &fts_q,
                /* limit= */ 10,
                /* include_downgraded= */ false,
                req.section.as_deref(),
                /* locus= */ None,
                /* status= */ None,
            )
            .await
        {
            Ok(hits) => hits.into_iter().map(|h| h.note_id.to_string()).collect(),
            Err(e) => {
                tracing::error!(err = %e, "vault_context: search_fts_with_snippet failed");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    };

    // Budget tokens (rev2 ratio 3.0 chars/token, chars().count() unicode-safe)
    let mut context_parts: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut used_tokens: usize = 0;

    for note_id in &top_note_ids {
        if used_tokens >= max_tokens {
            break;
        }
        match state.search.get_note(&tenant, note_id).await {
            Ok(Some(record)) => {
                let note_chars = record.body_text.chars().count();
                let note_tokens = note_chars.div_ceil(3).max(1);
                let remaining = max_tokens.saturating_sub(used_tokens);

                let body_part = if note_tokens > remaining {
                    // Tronquer à `remaining * 3` chars (char-safe via char_indices).
                    let char_limit = remaining.saturating_mul(3);
                    let end = record
                        .body_text
                        .char_indices()
                        .nth(char_limit)
                        .map(|(i, _)| i)
                        .unwrap_or(record.body_text.len());
                    record.body_text[..end].to_string()
                } else {
                    record.body_text.clone()
                };

                let consumed = body_part.chars().count().div_ceil(3).max(1);
                context_parts.push(body_part);
                sources.push(note_id.clone());
                used_tokens = used_tokens.saturating_add(consumed);
            }
            Ok(None) => {
                tracing::debug!(note_id = %note_id, "vault_context: note absente, ignorée");
            }
            Err(e) => {
                tracing::warn!(err = %e, note_id = %note_id, "vault_context: get_note failed, ignoré");
            }
        }
    }

    let context = context_parts.join("\n\n---\n\n");
    // estimated_tokens : chars().count() / 3 (rev2 — cohérent ratio 3.0).
    let estimated_tokens = (context.chars().count() / 3) as u32;

    Ok(Json(VaultContextResponse {
        context,
        estimated_tokens,
        sources,
    }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Single-tenant v0.4.x: tenant_id always "main". Multi-tenant deferred to v0.5.1.
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // T3 P2.0c : délégation réelle via SqliteIndex.distinct_authors.
    let rows = state.search.distinct_authors("main").await.map_err(|e| {
        tracing::error!(err = %e, "vault_authors: distinct_authors failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let authors = rows
        .into_iter()
        .map(|r| AuthorEntry {
            name: r.name,
            note_count: r.note_count,
        })
        .collect();
    Ok(Json(VaultAuthorsResponse { authors }))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Single-tenant v0.4.x: tenant_id always "main". Multi-tenant deferred to v0.5.1.
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // T3 P2.0c : délégation réelle via SqliteIndex.distinct_tags.
    let rows = state.search.distinct_tags("main").await.map_err(|e| {
        tracing::error!(err = %e, "vault_tags: distinct_tags failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let tags = rows
        .into_iter()
        .map(|(tag, count)| TagEntry {
            tag,
            note_count: count,
        })
        .collect();
    Ok(Json(VaultTagsResponse { tags }))
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
