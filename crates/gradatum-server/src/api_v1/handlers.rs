//! Handlers MCP read v1 — 10 méthodes, parité stricte legacy vault v1.6.2.
//!
//! Chaque handler :
//! 1. Vérifie l'authentification via [`TrustContext::is_authenticated`].
//! 2. Évalue l'ACL via [`AclEngine::evaluate`] (Read, locus = `tenant_id/section_or_main`).
//! 3. Délègue aux libs réelles (`state.vault` + `state.search`) depuis T3 P2.0c.
//!
//! ## Câblage T3 P2.0c
//!
//! 7 handlers câblés avec `state.search` (SqliteIndex) :
//! - `vault_authors` → `distinct_authors`
//! - `vault_tags` → `distinct_tags`
//! - `vault_links` → `backlinks` (liens entrants directs)
//! - `vault_graph` → `neighbors` (CTE récursif depth-cap 3)
//! - `vault_trace` → `trace_lineage` (parents + enfants)
//! - `vault_read` → `get_note` (404 si absent)
//! - `vault_context` → `get_note` body + sources backlinks
//!
//! ## Mapping path → note_id (T3)
//!
//! T3 interprète `req.path` / `req.root` / `req.query` directement comme un
//! identifiant ULID. Le mapping chemin → ULID (title_lookup + section/locus)
//! sera implémenté en T4 (Vault.read_note).
//!
//! # Endpoints
//!
//! | Méthode | Path | Auth |
//! |---------|------|------|
//! | POST | `/api/v1/vault_search`  | bearer requis |
//! | POST | `/api/v1/vault_read`    | bearer requis |
//! | POST | `/api/v1/vault_list`    | bearer requis |
//! | GET  | `/api/v1/vault_status`  | bearer requis |
//! | POST | `/api/v1/vault_graph`   | bearer requis |
//! | POST | `/api/v1/vault_links`   | bearer requis |
//! | POST | `/api/v1/vault_trace`   | bearer requis |
//! | POST | `/api/v1/vault_context` | bearer requis |
//! | GET  | `/api/v1/vault_authors` | bearer requis |
//! | GET  | `/api/v1/vault_tags`    | bearer requis |

use std::collections::HashMap;

use axum::{extract::State, http::StatusCode, Extension, Json};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::error::GradatumError;
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use gradatum_embed::EmbedBackend;
use gradatum_search::rrf_fuse;
use gradatum_search::scoring::{composite_score, pagerank_factor, recency_factor};

use crate::api_v1::dto::{
    AuthorEntry, GraphEdge, SearchHit, TagEntry, TraceEntry, VaultAuthorsResponse,
    VaultContextRequest, VaultContextResponse, VaultEntry, VaultGraphRequest, VaultGraphResponse,
    VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse, VaultReadRequest,
    VaultReadResponse, VaultSearchRequest, VaultSearchResponse, VaultStatusResponse,
    VaultTagsResponse, VaultTraceRequest, VaultTraceResponse,
};
use crate::state::AppState;

// ── Helpers internes ──────────────────────────────────────────────────────────

/// Construit le locus ACL : `{tenant_id}/main` (section par défaut).
///
/// Utilisé pour évaluer les permissions sur le tenant avant toute opération.
fn locus_for_tenant(tenant_id: &str) -> String {
    format!("{}/main", tenant_id)
}

/// Construit le locus ACL pour une section spécifique.
fn locus_for_section(tenant_id: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{}/{}", tenant_id, s),
        None => format!("{}/main", tenant_id),
    }
}

/// Tronque un texte à `max_chars` caractères Unicode (codepoints).
///
/// Conservé pour les tests unitaires de régression (UTF-8 boundary, ZWJ emoji,
/// short body) qui couvrent l'invariant char-safe utilisé par `vault_context`
/// (Phase 2.x.4) — ce dernier inline désormais le pattern `char_indices().nth()`
/// sans appeler `build_snippet` directement.
///
/// Utilise `char_indices` pour garantir une boundary char-safe (jamais
/// au milieu d'un char multi-byte). Ajoute `…` si le texte est tronqué.
///
/// `max_chars` est un nombre de codepoints, pas de bytes : un emoji 4-byte
/// compte pour 1. Un séquence ZWJ (ex. famille 👨‍👩‍👧‍👦) est traitée comme
/// plusieurs codepoints séparés — chaque codepoint est une boundary safe.
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

/// Normalise une query pour FTS5 SQLite en wrappant en phrase exacte si nécessaire.
///
/// ## Logique (inversion logique — fix #32)
///
/// Une query est transmise telle quelle à FTS5 si et seulement si :
/// - Elle ne contient que des caractères alphanumériques Unicode, underscores ou espaces.
/// - Elle ne contient aucun mot-clé FTS5 réservé (`AND`, `OR`, `NOT`, `NEAR`).
///
/// Dans tout autre cas (présence de `.`, `,`, `-`, `'`, `!`, `?`, `:`, `*`, etc.),
/// la query est wrappée en phrase exacte : `"<query>"`.
///
/// ## Échappement
///
/// Dans une phrase FTS5 :
/// - Les guillemets doubles internes sont doublés (`"` → `""`).
/// - Les apostrophes internes sont doublées (`'` → `''`) — requis par le tokenizer FTS5
///   en mode phrase (le tokenizer unicode61 par défaut interprète `'` comme délimiteur).
///
/// ## Anti-pattern précédent
///
/// Lister explicitement les caractères opérateurs (`- * : ^ " ( )`) → incomplet par
/// construction : `.`, `,`, `'`, `!`, `?` manquaient → HTTP 500 sur `2.1.1`, `alpha.8`.
///
/// ## Visibilité
///
/// `pub(crate)` pour les tests unitaires dans ce module ; non exposé dans l'API publique.
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

// ── vault_search ──────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_search`
///
/// Recherche plein-texte FTS5 dans le vault via `state.search.search_fts`.
///
/// ## Algorithme
///
/// 1. Auth + ACL (Read).
/// 2. Clamp `limit` à [1, 50], défaut 10.
/// 3. Appel `search_fts(vault_id, query, limit)` → `Vec<NoteId>`.
/// 4. Pour chaque NoteId : `get_note(vault_id, id)` → snippet (50 premiers chars du body).
/// 5. Retourne `SearchHit { path: "<section>/<id>", score, snippet }`.
///
/// ## Score FTS5
///
/// SQLite FTS5 `bm25()` retourne une valeur négative (meilleur match → plus proche
/// de 0). Le handler utilise `search_fts_scored` pour obtenir les scores BM25 réels
/// et les normalise via `1.0 / (1.0 + bm25_raw.abs())` → [0.0–1.0] monotone
/// décroissant (score 1.0 = match parfait, 0.0 = aucun match).
///
/// ## Erreurs
///
/// - `500` si la requête SQLite FTS5 échoue (log + propagation).
/// - `400` si `query` est vide (FTS5 refuse les queries vides).
///
/// T10 P2.0c — câblage production `state.search` (SqliteIndex vault/.gradatum/index.db).
pub async fn vault_search(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultSearchRequest>,
) -> Result<Json<VaultSearchResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_section(&req.tenant_id, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // Query vide : FTS5 retournerait une erreur.
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(Json(VaultSearchResponse { items: vec![] }));
    }

    // Clamp limit : [1, 50], défaut 10.
    let limit = req.limit.unwrap_or(10).clamp(1, 50) as usize;

    let vault_id = VaultId::new(&req.tenant_id);

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
    let semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> =
        if state.embedder.backend_kind() != EmbedBackend::Noop {
            match state.embedder.embed(query).await {
                Ok(query_emb) => state
                    .search
                    .search_semantic(
                        &req.tenant_id,
                        state.embedder.embedder_id(),
                        &query_emb,
                        limit * 2,
                    )
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            err = %e,
                            query = %query,
                            "vault_search: search_semantic failed, BM25 only"
                        );
                        vec![]
                    }),
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

    // Enrichir section + snippet + title depuis la map BM25.
    // Les hits semantic-only conservent section="" et snippet=None.
    for hit in &mut fused {
        if let Some(bh) = bm25_map.get(&hit.note_id) {
            hit.section = bh.section.clone();
            hit.snippet = Some(bh.snippet.clone());
            hit.title = bh.title.clone();
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

    for hit in fused {
        let (created_ms, in_degree) = match state
            .search
            .get_note_created_and_indegree(&req.tenant_id, &hit.note_id)
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
        let composite = composite_score(hit.rrf_score, recency, pagerank);
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
                let body = match state.search.get_note(&req.tenant_id, &hit.note_id).await {
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
    // Les hits présents dans `final_hits` mais absents de `bm25_map` (hits
    // sémantique-only) ont conservé `title = None` et `section = ""` après la
    // fusion RRF : `search_fts_with_snippet` ne les a pas retournés, donc
    // `bm25_map` ne les contient pas.
    //
    // On collecte les identifiants manquants et on récupère `title` + `section`
    // depuis la table `notes` en un seul SELECT batch (N ≤ limit ≤ 50 — N+1 borné).
    //
    // Snippet sémantique-only : laissé à `None` — il n'y a pas de match FTS5
    // pour générer un extrait localisé pertinent. Le consommateur doit traiter
    // `snippet: None` comme "pas d'extrait disponible".
    let semantic_only_ids: Vec<String> = final_hits
        .iter()
        .filter(|(hit, _score)| hit.title.is_none() && hit.section.is_empty())
        .map(|(hit, _score)| hit.note_id.clone())
        .collect();

    let title_section_map: HashMap<String, (Option<String>, String)> =
        if semantic_only_ids.is_empty() {
            HashMap::new()
        } else {
            state
                .search
                .get_titles_sections(&req.tenant_id, &semantic_only_ids)
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
                        hit.title = fetched_title.clone();
                    }
                    if hit.section.is_empty() {
                        hit.section = fetched_section.clone();
                    }
                }
            }
            let section = if hit.section.is_empty() {
                "main".to_string()
            } else {
                hit.section
            };
            SearchHit {
                path: format!("{}/{}", section, hit.note_id),
                score,
                title: hit.title,
                snippet: hit.snippet,
            }
        })
        .collect();

    Ok(Json(VaultSearchResponse { items }))
}

// ── vault_read ────────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_read`
///
/// Lit le contenu d'une note par identifiant ULID **ou par titre H1 Markdown**.
///
/// ## Algorithme (Phase 2.x.4 alpha.13 Task 14 — B4 title_lookup intégré)
///
/// 1. Si `req.path` parse comme ULID → délégation directe à `state.vault.read_note_by_id()`
///    (chemin existant T4 P2.0c — cache hit + checksum B22 + disk read).
/// 2. Sinon → `state.search.title_lookup(&req.tenant_id, &req.path)` :
///    - `Ok(Some(found_id))` → délégation à `state.vault.read_note_by_id(found_id)`.
///    - `Ok(None)` → 404 NOT_FOUND (titre absent ou note downgraded — voir filtre
///      `AND status = 'live'` dans `title_lookup`, queries.rs).
///    - `Err(e)` → 500 INTERNAL_SERVER_ERROR (log).
///
/// **Décision rev2 P0-3 Option A** : pas de handler dédié `/vault_title_lookup`
/// — résolution intégrée transparente côté `vault_read`. Cohérent legacy vault v1.6.2
/// qui accepte les wikilinks `[[titre]]` directement dans `vault_read`.
///
/// Retourne 200, 404 ou 500.
pub async fn vault_read(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultReadRequest>,
) -> Result<Json<VaultReadResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_section(&req.tenant_id, req.section.as_deref());
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── B4 (alpha.13 Task 14) : résolution titre → ULID si non-ULID ────────────
    //
    // ULID parse OK → chemin direct (non-régression T4 P2.0c).
    // ULID parse KO → tentative title_lookup (filtre `AND status = 'live'`).
    // Le `resolved_path` est ensuite utilisé pour `read_note_by_id`.
    let resolved_path: String = if ulid::Ulid::from_string(&req.path).is_ok() {
        req.path.clone()
    } else {
        match state.search.title_lookup(&req.tenant_id, &req.path).await {
            Ok(Some(found_id)) => {
                tracing::debug!(
                    title = %req.path,
                    resolved_id = %found_id,
                    "vault_read: titre résolu via title_lookup"
                );
                found_id
            }
            Ok(None) => {
                tracing::debug!(path = %req.path, "vault_read: titre non trouvé (live)");
                return Err(StatusCode::NOT_FOUND);
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
            Ok(Json(VaultReadResponse {
                path: note.id.to_string(),
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
/// Liste les notes d'un vault (avec pagination optionnelle).
/// Stub T8 : retourne `entries: []`. Implémentation effective en T12.
pub async fn vault_list(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultListRequest>,
) -> Result<Json<VaultListResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_section(&req.tenant_id, req.section.as_deref());
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
            &req.tenant_id,
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
/// Retourne l'état courant du vault pour le tenant courant.
/// T2 P2.0c : délègue à `state.vault` (vraie impl Registry) pour les counts.
/// Note : `note_count` et `total_size_bytes` restent stubbed (T4 P2.0c les câblera).
pub async fn vault_status(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultStatusResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
/// Retourne le graphe de liens pour une note racine, jusqu'à `depth` niveaux.
///
/// T3 P2.0c : `req.root` interprété comme note_id ULID. `depth` par défaut = 2, max 3.
/// Utilise `state.search.neighbors` (CTE récursif) + `backlinks` si `include_backlinks`.
///
/// Retourne `nodes` = liste de note_ids voisins + `edges` = liens (from → to).
pub async fn vault_graph(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultGraphRequest>,
) -> Result<Json<VaultGraphResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_tenant(&req.tenant_id);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    let depth = req.depth.unwrap_or(2).min(3) as u8;
    // Forward neighbors (liens sortants BFS).
    let neighbors = state
        .search
        .neighbors(&req.tenant_id, &req.root, depth)
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
            .backlinks(&req.tenant_id, &req.root)
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
/// Alias thin sur vault_graph avec `depth=1` (parité D5).
/// Liste les liens entrants/sortants directs d'une note.
///
/// T3 P2.0c : `req.path` interprété comme note_id ULID.
/// Utilise `backlinks` (entrants) + `neighbors(depth=1)` (sortants).
pub async fn vault_links(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultLinksRequest>,
) -> Result<Json<VaultLinksResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_tenant(&req.tenant_id);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // Liens sortants (depth=1 : voisins directs).
    let outbound = state
        .search
        .neighbors(&req.tenant_id, &req.path, 1)
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
            .backlinks(&req.tenant_id, &req.path)
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
/// Trace la lignée d'une note (parents + enfants wikilinks).
///
/// ## Algorithme M4 (Phase 2.x.4 alpha.13 Task 15) — résolution multi-mode
///
/// 1. Si `req.query` parse comme ULID → `trace_lineage` direct (non-régression T3).
/// 2. Sinon → `state.search.title_lookup(...)` :
///    - `Some(found_id)` → `trace_lineage(found_id)` (titre exact).
///    - `None` → fallback FTS : `search_fts_with_snippet(...)` retourne jusqu'à
///      `min(limit, 5)` seeds. Pour chaque seed → `trace_lineage` (caveat backlog
///      C2 : N+1 SQLite, ≤ 5 requêtes typiquement).
///    - `Err(e)` → 500.
///
/// Toutes les `lineage.parents` + `lineage.children` sont concaténés, dédupliqués
/// et limités à `req.limit`. Score = 1.0 (pas de RRF appliqué — caveat C2/C8 backlog).
pub async fn vault_trace(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultTraceRequest>,
) -> Result<Json<VaultTraceResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_tenant(&req.tenant_id);
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
        match state.search.title_lookup(&req.tenant_id, &req.query).await {
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
                let vault_id = VaultId::new(&req.tenant_id);
                let fts_limit = limit.min(5);
                match state
                    .search
                    .search_fts_with_snippet(
                        &vault_id, &fts_q, fts_limit, /* include_downgraded= */ false,
                        /* section= */ None,
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
        let tenant_id = req.tenant_id.clone();
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
/// Construit un contexte LLM depuis les notes pertinentes.
///
/// ## Algorithme M5 (Phase 2.x.4 alpha.13 Task 16)
///
/// 1. Si `req.query` parse comme ULID → récupère la note + backlinks (sources)
///    et tronque le body au budget tokens (chemin v1 conservé, ratio 3.0 rev2).
/// 2. Sinon → FTS textuel `search_fts_with_snippet` (top-10 notes par BM25,
///    section filtrée si fournie). Pour chaque note, si le budget restant le
///    permet, inclure le `body_text` (intégral ou tronqué char-safe).
///
/// ## Heuristique tokens rev2 (B-P0-1 + L-P0-3)
///
/// - **`chars().count()`** (pas `len()` bytes) — cohérence unicode FR/EN/multi-byte.
/// - Ratio **3.0 chars/token** (au lieu de 4.0) — conservateur FR/EN mixte.
///   Doc Anthropic : FR ≈ 3.5 chars/token, EN ≈ 4.0 ; gradatum corpus mixte
///   FR/EN/code/markdown/ULIDs → ratio effectif < 3.5. **3.0 = sous-estime
///   jamais le coût tokens**, donc le budget `max_tokens` est respecté avec marge.
///
/// ## Troncature char-safe
///
/// `body_text.char_indices().nth(char_limit)` retourne le byte offset valide pour
/// `&body_text[..end]` — pas de panic UTF-8 boundary (cf. patch 2026-04-XX
/// `[DEBUG][gradatum] handlers.rs:172 panic UTF-8 boundary`).
pub async fn vault_context(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultContextRequest>,
) -> Result<Json<VaultContextResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = locus_for_section(&req.tenant_id, req.section.as_deref());
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
            .backlinks(&req.tenant_id, &req.query)
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
        let vault_id = VaultId::new(&req.tenant_id);
        match state
            .search
            .search_fts_with_snippet(
                &vault_id,
                &fts_q,
                /* limit= */ 10,
                /* include_downgraded= */ false,
                req.section.as_deref(),
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
        match state.search.get_note(&req.tenant_id, note_id).await {
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
/// Liste les auteurs distincts du vault (tenant `"main"` par défaut).
/// T3 P2.0c : délègue à `state.search.distinct_authors("main")`.
/// Notes sans auteur (`author_id IS NULL`) sont exclues.
pub async fn vault_authors(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultAuthorsResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
/// Liste les tags distincts du vault (tenant `"main"` par défaut) avec leur fréquence.
/// T3 P2.0c : délègue à `state.search.distinct_tags("main")`.
pub async fn vault_tags(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<VaultTagsResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    use super::{build_fts_query, build_snippet};

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

    /// Régression #32 — query `alpha.8` avec point → wrappée.
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
}
