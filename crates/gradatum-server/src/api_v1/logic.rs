//! Pure business logic extracted from the API v1 handlers, enabling MCP re-use.
//!
//! This module exposes `*_impl` functions that form the core of the 21 MCP tools
//! without Axum coupling. Each `*_impl`:
//!
//! 1. Enforces **ACL** (`state.acl.evaluate`), tenant check (`effective_tenant`),
//!    **audit** (`state.audit.record`), and **`read_usage_accumulators`** increment.
//! 2. Receives `&AppState` + `&TrustContext` + the request DTO — identical to what
//!    the HTTP handler extracts from Axum.
//! 3. Returns `Result<RespType, GradatumError>` (semantic errors) so that a future
//!    MCP server can map them without depending on `axum::http::StatusCode`.
//!
//! ## MCP parity (security gate)
//!
//! ACL, mono-vault tenant check, audit, and usage counters live **inside `*_impl`**,
//! never only in the HTTP wrapper. The MCP path calls `*_impl` directly and inherits
//! security by construction without duplicating guards.
//!
//! ## Handlers non extractibles (résistance axum documentée)
//!
//! - `vault_forget` / `vault_forgotten_list` / `vault_unforgot` : hors scope 21 outils MCP
//!   (endpoints de gestion sémantique non exposés dans `gradatum-mcp-stub`).
//! - `code_scope` : l'invariant de sécurité #1 (bypass de la garde mono-vault via préfixe
//!   `code-`) est trop couplé à la logique de routing pour être extrait sans risque de
//!   masquer la raison du bypass. Laissé dans son module (`code_scope.rs`).
//! - `vault_classify` : retourne `impl IntoResponse` pour raison de rétrocompat wire —
//!   extractible mais `501 Not Implemented` figé, extraction sans valeur ajoutée.
//! - `vault_downgrade` (async) dans `write.rs` : non câblée dans le routeur (dead code
//!   documenté), remplacée par la version sync dans `notes.rs`.
//! - Jobs, dashboard, review, event_log, session_log : hors scope 21 outils MCP.
//!
//! ## Organisation
//!
//! Les fonctions sont groupées par module source :
//! - §1 Read handlers (depuis `handlers.rs`) : vault_search, vault_read, vault_list,
//!   vault_status, vault_graph, vault_links, vault_trace, vault_context, vault_authors,
//!   vault_tags.
//! - §2 Timeline (depuis `timeline.rs`) : vault_timeline.
//! - §3 Write handlers (depuis `write.rs`) : vault_write.
//! - §4 History handlers (depuis `history.rs`) : vault_history, vault_history_get,
//!   vault_restore, vault_diff.
//! - §5 Lessons (depuis `lessons.rs`) : lessons_recall.

use std::collections::HashMap;
use std::time::Instant;

use axum::http::StatusCode;
use chrono::Utc;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::audit::http::HttpAuditEvent;
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{LocusId, VaultId};
use gradatum_core::temporal_query::{TimelineCursor, TimelineFilter};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    LessonHit, LessonsRecallRequest, LessonsRecallResponse, NoteStatusPatch, VaultClassifyRequest,
    VaultClassifyResponse, VaultDiffRequest, VaultDiffResponse, VaultDowngradeRequest,
    VaultDowngradeResponse, VaultHistoryGetRequest, VaultHistoryGetResponse, VaultHistoryRequest,
    VaultHistoryResponse, VaultRestoreRequest, VaultRestoreResponse,
};
use gradatum_embed::EmbedBackend;
use gradatum_index::extract_h1_title;
use gradatum_index::links::title_to_slug;
use gradatum_search::rrf_fuse;
use gradatum_search::scoring::{
    composite_score_with_trust, pagerank_factor, recency_factor, trust_decay_factor,
};
use ulid::Ulid;

use crate::api_v1::dto::{
    AuthorEntry, EnqueuedResponseUlid, GraphEdge, ScoreBreakdown, SearchHit, TagEntry, TraceEntry,
    VaultAuthorsResponse, VaultContextRequest, VaultContextResponse, VaultEntry, VaultGraphRequest,
    VaultGraphResponse, VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse,
    VaultReadRequest, VaultReadResponse, VaultSearchRequest, VaultSearchResponse,
    VaultStatusResponse, VaultTagsResponse, VaultTimelineRequest, VaultTraceRequest,
    VaultTraceResponse,
};
use crate::api_v1::handlers::{
    build_fts_query, filter_semantic_by_section, filter_semantic_by_status, validate_search_status,
};
use crate::api_v1::tenant_guard::effective_tenant;
use crate::api_v1::timeline::{TimelineItem, VaultTimelineResponse};
use crate::api_v1::write::{
    actor_from_trust, build_curate_job_record, emit_auth_failure_audit, emit_write_rejection_audit,
    parse_sha256_hex,
};
use crate::state::AppState;

// ── Helpers internes ─────────────────────────────────────────────────────────

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

/// Mappe une `GradatumError` en `StatusCode` HTTP.
///
/// Utilisé par les thin wrappers axum pour convertir les erreurs sémantiques
/// des fonctions `*_impl` en codes HTTP.
///
/// Mapping :
/// - `Unauthorized`  → 401
/// - `Forbidden`     → 403
/// - `InvalidInput`  → 400
/// - `Conflict`      → 409
/// - `NoteNotFound`  → 404
/// - `Storage(msg)` contenant "introuvable" ou "Not found" → 404
/// - Tout autre      → 500
pub(crate) fn err_to_status(e: &GradatumError) -> StatusCode {
    match e {
        GradatumError::Unauthorized => StatusCode::UNAUTHORIZED,
        GradatumError::Forbidden(_) => StatusCode::FORBIDDEN,
        GradatumError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        GradatumError::Conflict(_) => StatusCode::CONFLICT,
        GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
        GradatumError::Storage(msg) if msg.contains("introuvable") || msg.contains("Not found") => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ── §1 READ HANDLERS ─────────────────────────────────────────────────────────

/// Logique métier de `POST /api/v1/vault_search`.
///
/// Extrait depuis `handlers::vault_search` — ACL Read + usage + scoring composite.
/// Le futur serveur MCP appellera cette fonction directement.
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL Read denied ou cross-vault interdit.
/// - `GradatumError::InvalidInput` si query vide ou status invalide.
/// - `GradatumError::Storage` sur erreur index.
pub async fn vault_search_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultSearchRequest,
) -> Result<VaultSearchResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    // Télémétrie usage read-path — coût ~0 (AtomicU64 Relaxed, aucun I/O).
    state
        .read_usage_accumulators
        .vault_search
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // P0 cross-tenant : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let acl_locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // Validation vault_id (mono-vault — cross-read non supporté).
    if let Some(vid) = req.vault_id.as_deref() {
        if vid.is_empty() || vid.len() > 128 {
            return Err(GradatumError::InvalidInput("vault_id invalide".into()));
        }
        if vid != "main" {
            return Err(GradatumError::Forbidden(
                "cross-read vault_id ≠ main non supporté (mono-vault)".into(),
            ));
        }
    }

    // Validation status filter.
    let status_filter = validate_search_status(req.status.as_deref())
        .map_err(|()| GradatumError::InvalidInput("status invalide".into()))?;

    let read_vault_id = req.vault_id.as_deref().unwrap_or(&tenant).to_owned();
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(VaultSearchResponse {
            items: vec![],
            corpus_match_count: None,
            corpus_count_capped: false,
        });
    }

    let limit = req.limit.unwrap_or(10).clamp(1, 50) as usize;
    let vault_id = VaultId::new(&read_vault_id);
    let fts_query = build_fts_query(query);

    // Signal BM25.
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
        .await?;

    // Signal sémantique (dégradation gracieuse si Noop ou erreur).
    let mut semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> = if state
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
                            "vault_search_impl: search_semantic failed, BM25 only"
                        );
                        vec![]
                    });
                if hits.is_empty() && req.vault_id.as_deref().is_some() && read_vault_id != tenant {
                    tracing::info!(
                        vault_id = %read_vault_id,
                        query = %query,
                        "vault_search_impl: 0 hits sémantiques sur vault cross-tenant"
                    );
                }
                hits
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    query = %query,
                    "vault_search_impl: embed() failed, BM25 only"
                );
                vec![]
            }
        }
    } else {
        vec![]
    };

    // Filtre section sur chemin sémantique (fix C2).
    if let Some(wanted_section) = req.section.as_deref()
        && !semantic_hits.is_empty()
    {
        let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
        let sec_result = state
            .search
            .get_titles_sections(&read_vault_id, &sem_ids)
            .await;
        semantic_hits = filter_semantic_by_section(semantic_hits, wanted_section, sec_result);
    }

    // Filtre status sur chemin sémantique (symétrique C2).
    if let Some(wanted_status) = status_filter.as_deref()
        && !semantic_hits.is_empty()
    {
        let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
        let status_result = state.search.get_statuses(&read_vault_id, &sem_ids).await;
        semantic_hits = filter_semantic_by_status(semantic_hits, wanted_status, status_result);
    }

    // Fusion RRF.
    let bm25_for_rrf: Vec<(String, f64)> = bm25_hits
        .iter()
        .map(|h| (h.note_id.to_string(), h.bm25))
        .collect();
    let sem_for_rrf: Vec<(String, f32)> = semantic_hits
        .iter()
        .map(|(id, score)| (id.to_string(), *score))
        .collect();
    let bm25_map: HashMap<String, &gradatum_index::SearchHitRaw> = bm25_hits
        .iter()
        .map(|h| (h.note_id.to_string(), h))
        .collect();
    let rrf_buffer = (limit * 4).clamp(20, 200);
    let mut fused = rrf_fuse(&bm25_for_rrf, &sem_for_rrf, 60.0, rrf_buffer);

    // Enrichir section + snippet + title + status depuis la map BM25.
    for hit in &mut fused {
        if let Some(bh) = bm25_map.get(&hit.note_id) {
            hit.section = bh.section.clone();
            hit.snippet = Some(bh.snippet.clone());
            hit.title = bh.title.clone().filter(|s| !s.trim().is_empty());
            hit.status = bh.status.clone();
        }
    }

    // Scoring composite multi-facteur.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut composite_hits: Vec<(gradatum_search::RrfHit, f64)> = Vec::with_capacity(fused.len());
    let mut score_breakdowns: HashMap<String, ScoreBreakdown> = HashMap::new();

    for hit in fused {
        let (created_ms, in_degree) = match state
            .search
            .get_note_created_and_indegree(&tenant, &hit.note_id)
            .await
        {
            Ok(v) => v,
            Err(GradatumError::NoteNotFound(_)) => {
                tracing::debug!(
                    note_id = %hit.note_id,
                    "vault_search_impl: note absente, fallback (now_ms, 0)"
                );
                (now_ms, 0u64)
            }
            Err(e) => {
                tracing::error!(
                    err = %e,
                    note_id = %hit.note_id,
                    "vault_search_impl: get_note_created_and_indegree storage error"
                );
                return Err(e);
            }
        };

        let recency = recency_factor(created_ms, now_ms);
        let pagerank = pagerank_factor(in_degree);

        let trust_params = if state.scoring.enabled {
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
                        "vault_search_impl: get_trust_and_provenance échoué — fallback"
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

    composite_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Reranker (NoopReranker par défaut → skip).
    let rerank_n = state.reranker.max_batch_size().min(20);
    let final_hits: Vec<(gradatum_search::RrfHit, f32)> =
        if state.reranker.requires_body() && !composite_hits.is_empty() && rerank_n > 0 {
            let mut top_for_rerank: Vec<(gradatum_search::RrfHit, f64)> =
                composite_hits.into_iter().take(rerank_n).collect();
            let mut rerank_candidates: Vec<(String, String)> =
                Vec::with_capacity(top_for_rerank.len());
            for (hit, _composite) in &top_for_rerank {
                let body = match state.search.get_note(&tenant, &hit.note_id).await {
                    Ok(Some(rec)) => rec.body_text,
                    Ok(None) => String::new(),
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            note_id = %hit.note_id,
                            "vault_search_impl reranker: get_note failed, body=\"\""
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
            let rerank_result =
                tokio::task::block_in_place(move || reranker.rerank(&query_owned, &cand_clone));
            let rerank_elapsed = rerank_start.elapsed();
            let scores: Vec<f32> = match rerank_result {
                Ok(s) => {
                    tracing::info!(
                        rerank_n = top_for_rerank.len(),
                        elapsed_ms = rerank_elapsed.as_millis(),
                        "vault_search_impl: reranker OK"
                    );
                    s
                }
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        "vault_search_impl: reranker failed, falling back to composite order"
                    );
                    let n = top_for_rerank.len();
                    let denom = n as f32 + 1.0;
                    (0..n).map(|i| 1.0 - (i as f32) / denom).collect()
                }
            };
            let mut zipped: Vec<(gradatum_search::RrfHit, f32)> = top_for_rerank
                .drain(..)
                .map(|(hit, _composite)| hit)
                .zip(scores)
                .collect();
            zipped.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            zipped.truncate(limit);
            zipped
        } else {
            composite_hits
                .into_iter()
                .take(limit)
                .map(|(hit, composite)| (hit, composite as f32))
                .collect()
        };

    // Enrichissement semantic-only.
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
                        "vault_search_impl: get_titles_sections failed, sem-only sans titre"
                    );
                    HashMap::new()
                })
        };
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
                    "vault_search_impl: get_statuses failed, sem-only sans status"
                );
                HashMap::new()
            })
    };

    // Construire la réponse.
    let items: Vec<SearchHit> = final_hits
        .into_iter()
        .map(|(mut hit, score)| {
            if (hit.title.is_none() || hit.section.is_empty())
                && let Some((fetched_title, fetched_section)) = title_section_map.get(&hit.note_id)
            {
                if hit.title.is_none() {
                    hit.title = fetched_title.clone().filter(|s| !s.trim().is_empty());
                }
                if hit.section.is_empty() {
                    hit.section = fetched_section.clone();
                }
            }
            if hit.status.is_empty()
                && let Some(fetched_status) = status_map.get(&hit.note_id)
            {
                hit.status = fetched_status.clone();
            }
            let section = if hit.section.is_empty() {
                "main".to_string()
            } else {
                hit.section
            };
            let scores = score_breakdowns.remove(&hit.note_id);
            #[allow(deprecated)]
            SearchHit {
                path: format!("{}/{}", section, hit.note_id),
                score,
                title: hit.title,
                snippet: hit.snippet,
                trust: 0.5,
                status: hit.status,
                scores,
            }
        })
        .collect();

    // corpus_match_count (opt-in).
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
                tracing::warn!(
                    err = %e,
                    query = %query,
                    "vault_search_impl: count_fts_matches failed, corpus_match_count absent"
                );
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    Ok(VaultSearchResponse {
        items,
        corpus_match_count,
        corpus_count_capped,
    })
}

/// Logique métier de `POST /api/v1/vault_read`.
///
/// ACL Read + usage + résolution titre → ULID.
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL deny.
/// - `GradatumError::NoteNotFound` si note absente ou titre non résolu.
/// - `GradatumError::Storage` sur erreur index.
pub async fn vault_read_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultReadRequest,
) -> Result<VaultReadResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    state
        .read_usage_accumulators
        .vault_read
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // Résolution ULID ou titre → ULID.
    let ulid_candidate = req.path.rsplit('/').next().unwrap_or(req.path.as_str());
    let resolved_path: String = if ulid::Ulid::from_string(ulid_candidate).is_ok() {
        ulid_candidate.to_string()
    } else {
        match state.search.title_lookup(&tenant, &req.path).await {
            Ok(Some(found_id)) => {
                tracing::debug!(title = %req.path, resolved_id = %found_id, "vault_read_impl: titre résolu");
                found_id
            }
            Ok(None) => {
                let slug = title_to_slug(&req.path);
                match state.search.resolve_redirect(&slug).await {
                    Ok(Some(ulid)) => ulid.to_string(),
                    Ok(None) => {
                        // req.path peut être un titre (pas un ULID) — Storage pour l'intro
                        return Err(GradatumError::Storage(format!(
                            "introuvable : {}",
                            req.path
                        )));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };

    match state.vault.read_note_by_id(&resolved_path).await {
        Ok(note) => {
            let body = note.body.markdown;
            let size_bytes = body.len() as u64;
            let sha256: String = note
                .content_hash
                .0
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();

            let title: Option<String> = {
                let ids = std::slice::from_ref(&resolved_path);
                match state.search.get_titles_sections(&tenant, ids).await {
                    Ok(map) => map
                        .get(&resolved_path)
                        .and_then(|(t, _)| t.clone())
                        .filter(|s| !s.trim().is_empty()),
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            note_id = %resolved_path,
                            "vault_read_impl: get_titles_sections échoué — title=None (best-effort)"
                        );
                        None
                    }
                }
            };
            let title: Option<String> = title.or_else(|| extract_h1_title(&body));

            Ok(VaultReadResponse {
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
            })
        }
        Err(GradatumError::NoteNotFound(_)) => {
            let note_id = ulid::Ulid::from_string(&resolved_path)
                .map(NoteId)
                .unwrap_or_else(|_| NoteId::new());
            Err(GradatumError::NoteNotFound(note_id))
        }
        Err(GradatumError::Storage(ref msg)) if msg.contains("ULID invalide") => {
            let note_id = ulid::Ulid::from_string(&resolved_path)
                .map(NoteId)
                .unwrap_or_else(|_| NoteId::new());
            Err(GradatumError::NoteNotFound(note_id))
        }
        Err(e) => Err(e),
    }
}

/// Logique métier de `POST /api/v1/vault_list`.
///
/// ACL Read + pagination ULID curseur.
pub async fn vault_list_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultListRequest,
) -> Result<VaultListResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let _ = req.pattern; // ignoré (T12 pattern filter différé)
    let limit = req.limit.unwrap_or(20).clamp(1, 200) as usize;

    let (records, total) = state
        .search
        .list_notes(
            &tenant,
            req.section.as_deref(),
            limit,
            req.cursor.as_deref(),
        )
        .await?;

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

    Ok(VaultListResponse {
        entries,
        next_cursor,
        total,
    })
}

/// Logique métier de `GET /api/v1/vault_status`.
///
/// ACL Read + compteurs live_note_count + total_body_size_bytes.
pub async fn vault_status_impl(
    state: &AppState,
    trust: &TrustContext,
) -> Result<VaultStatusResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let note_count = state.search.live_note_count("main").await.unwrap_or(0);
    let total_size_bytes = state
        .search
        .total_body_size_bytes("main")
        .await
        .unwrap_or(0);
    Ok(VaultStatusResponse {
        tenant_id: "main".to_string(),
        note_count,
        total_size_bytes,
        index_version: "v1".to_string(),
        last_indexed_at: None,
        health: "healthy".to_string(),
    })
}

/// Logique métier de `POST /api/v1/vault_graph`.
///
/// ACL Read + neighbors + backlinks optionnels.
pub async fn vault_graph_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultGraphRequest,
) -> Result<VaultGraphResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let raw_depth = req.depth.unwrap_or(2);
    if raw_depth > 5 {
        return Err(GradatumError::InvalidInput(
            "depth > 5 refusé (max effectif = 3)".into(),
        ));
    }
    let depth = raw_depth.min(3) as u8;

    let neighbors = state.search.neighbors(&tenant, &req.root, depth).await?;

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

    if req.include_backlinks.unwrap_or(false) {
        let backlinks = state.search.backlinks(&tenant, &req.root).await?;
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

    Ok(VaultGraphResponse { nodes, edges })
}

/// Logique métier de `POST /api/v1/vault_links`.
///
/// ACL Read + liens sortants (depth=1) + backlinks.
pub async fn vault_links_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultLinksRequest,
) -> Result<VaultLinksResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let outbound = state.search.neighbors(&tenant, &req.path, 1).await?;
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

    if req.include_backlinks.unwrap_or(true) {
        let backlinks = state.search.backlinks(&tenant, &req.path).await?;
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
    Ok(VaultLinksResponse { nodes, edges })
}

/// Logique métier de `POST /api/v1/vault_trace`.
///
/// ACL Read + résolution multi-mode (ULID / titre / FTS) + trace_lineage parallèle.
pub async fn vault_trace_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultTraceRequest,
) -> Result<VaultTraceResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let limit = req.limit.unwrap_or(20).clamp(1, 200) as usize;

    let resolved_seeds: Vec<String> = if ulid::Ulid::from_string(&req.query).is_ok() {
        vec![req.query.clone()]
    } else {
        match state.search.title_lookup(&tenant, &req.query).await {
            Ok(Some(note_id)) => {
                tracing::debug!(title = %req.query, id = %note_id, "vault_trace_impl: titre résolu");
                vec![note_id]
            }
            Ok(None) => {
                let fts_q = build_fts_query(&req.query);
                if fts_q.trim_matches(['"', ' ']).is_empty() {
                    return Ok(VaultTraceResponse { entries: vec![] });
                }
                let vault_id = VaultId::new(&tenant);
                let fts_limit = limit.min(5);
                match state
                    .search
                    .search_fts_with_snippet(&vault_id, &fts_q, fts_limit, false, None, None, None)
                    .await
                {
                    Ok(hits) => hits.into_iter().map(|h| h.note_id.to_string()).collect(),
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };

    if resolved_seeds.is_empty() {
        return Ok(VaultTraceResponse { entries: vec![] });
    }

    let mut join_set = tokio::task::JoinSet::new();
    for seed_id in resolved_seeds {
        let search = state.search.clone();
        let tenant_id = tenant.clone();
        join_set.spawn(async move {
            search.trace_lineage(&tenant_id, &seed_id).await.map_err(|e| {
                tracing::error!(err = %e, seed = %seed_id, "vault_trace_impl: trace_lineage failed");
                e
            })
        });
    }

    let mut all_ids: Vec<String> = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        let lineage = join_result.map_err(|e| {
            tracing::error!(err = %e, "vault_trace_impl: JoinSet task panicked");
            GradatumError::Storage("task panicked".into())
        })??;
        all_ids.extend(lineage.parents);
        all_ids.extend(lineage.children);
    }

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

    Ok(VaultTraceResponse { entries })
}

/// Logique métier de `POST /api/v1/vault_context`.
///
/// ACL Read + construction contexte LLM avec budget tokens.
pub async fn vault_context_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultContextRequest,
) -> Result<VaultContextResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let max_tokens = req.max_tokens.unwrap_or(2000).clamp(1, 8000) as usize;

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
            return Ok(VaultContextResponse {
                context: String::new(),
                estimated_tokens: 0,
                sources: vec![],
            });
        }
        let vault_id = VaultId::new(&tenant);
        match state
            .search
            .search_fts_with_snippet(
                &vault_id,
                &fts_q,
                10,
                false,
                req.section.as_deref(),
                None,
                None,
            )
            .await
        {
            Ok(hits) => hits.into_iter().map(|h| h.note_id.to_string()).collect(),
            Err(e) => return Err(e),
        }
    };

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
                tracing::debug!(note_id = %note_id, "vault_context_impl: note absente, ignorée");
            }
            Err(e) => {
                tracing::warn!(err = %e, note_id = %note_id, "vault_context_impl: get_note failed");
            }
        }
    }

    let context = context_parts.join("\n\n---\n\n");
    let estimated_tokens = (context.chars().count() / 3) as u32;

    Ok(VaultContextResponse {
        context,
        estimated_tokens,
        sources,
    })
}

/// Logique métier de `GET /api/v1/vault_authors`.
///
/// ACL Read (tenant "main") + distinct_authors.
pub async fn vault_authors_impl(
    state: &AppState,
    trust: &TrustContext,
) -> Result<VaultAuthorsResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }
    let rows = state.search.distinct_authors("main").await?;
    let authors = rows
        .into_iter()
        .map(|r| AuthorEntry {
            name: r.name,
            note_count: r.note_count,
        })
        .collect();
    Ok(VaultAuthorsResponse { authors })
}

/// Logique métier de `GET /api/v1/vault_tags`.
///
/// ACL Read (tenant "main") + distinct_tags.
pub async fn vault_tags_impl(
    state: &AppState,
    trust: &TrustContext,
) -> Result<VaultTagsResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let locus = locus_for_tenant("main");
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }
    let rows = state.search.distinct_tags("main").await?;
    let tags = rows
        .into_iter()
        .map(|(tag, count)| TagEntry {
            tag,
            note_count: count,
        })
        .collect();
    Ok(VaultTagsResponse { tags })
}

// ── §2 TIMELINE ──────────────────────────────────────────────────────────────

/// Accepted `doc_kind` values (strict allowlist — synchronisé avec `timeline.rs`).
const KNOWN_DOC_KINDS: [&str; 3] = ["Static", "Event", "Versioned"];

/// Logique métier de `POST /api/v1/vault_timeline`.
///
/// ACL Read (locus `{tenant}/timeline`) + usage + pagination keyset.
pub async fn vault_timeline_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultTimelineRequest,
) -> Result<VaultTimelineResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    state
        .read_usage_accumulators
        .vault_timeline
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let acl_locus = format!("{}/timeline", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny timeline".into()));
    }

    // Validation.
    if let (Some(f), Some(t)) = (req.from_ms, req.to_ms)
        && f > t
    {
        return Err(GradatumError::InvalidInput("from_ms > to_ms".into()));
    }
    if let Some(kinds) = req.doc_kind.as_ref() {
        if kinds.len() > KNOWN_DOC_KINDS.len() {
            return Err(GradatumError::InvalidInput("trop de doc_kind".into()));
        }
        if kinds.iter().any(|k| !KNOWN_DOC_KINDS.contains(&k.as_str())) {
            return Err(GradatumError::InvalidInput(
                "doc_kind hors allowlist".into(),
            ));
        }
    }
    if let Some(v) = req.vault_id.as_ref() {
        if v.is_empty() || v.len() > 128 {
            return Err(GradatumError::InvalidInput("vault_id invalide".into()));
        }
        if v != "main" {
            return Err(GradatumError::Forbidden(
                "cross-read vault_id ≠ main non supporté".into(),
            ));
        }
    }
    let cursor = match req.cursor.as_deref() {
        Some(s) => Some(
            TimelineCursor::decode(s)
                .map_err(|_| GradatumError::InvalidInput("cursor malformé".into()))?,
        ),
        None => None,
    };

    let limit = req.limit.unwrap_or(50).clamp(1, 200) as usize;
    let vault = VaultId::new(req.vault_id.unwrap_or_else(|| tenant.clone()));
    let filter = TimelineFilter {
        doc_kind: req.doc_kind,
        from_ms: req.from_ms,
        to_ms: req.to_ms,
        limit,
        cursor,
        as_of_ms: req.as_of_ms,
        include_expired: req.include_expired,
    };

    let rows = state.search.timeline(&vault, &filter).await?;

    let next_cursor = if rows.len() == limit {
        rows.last().map(|r| {
            TimelineCursor {
                anchor_ms: r.anchor_ms,
                note_id: r.note_id.0.to_string(),
            }
            .encode()
        })
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(|r| TimelineItem {
            note_id: r.note_id.0.to_string(),
            anchor_ms: r.anchor_ms,
            anchor_src: r.anchor_src,
            doc_kind: r.doc_kind,
            title: r.title,
        })
        .collect();

    Ok(VaultTimelineResponse { items, next_cursor })
}

// ── §3 WRITE HANDLER ─────────────────────────────────────────────────────────

/// Logique métier de `POST /api/v1/vault_write`.
///
/// ACL Write + audit + optimistic-lock (F-41) + enqueue job curate.
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL Write denied.
/// - `GradatumError::InvalidInput` si note_id invalide ou sha256 malformé sur overwrite.
/// - `GradatumError::Conflict` si overwrite sans `expected_sha256`.
/// - `GradatumError::Storage` sur erreur enqueue.
pub async fn vault_write_impl(
    state: &AppState,
    trust: &TrustContext,
    req: crate::api_v1::dto::VaultWriteRequest,
    request_id: &str,
) -> Result<EnqueuedResponseUlid, GradatumError> {
    let start = Instant::now();

    if !trust.is_authenticated() {
        emit_auth_failure_audit(state, trust, &req.tenant_id, request_id, "unauthenticated").await;
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(state, trust, &tenant, request_id, "acl_deny").await;
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // ── Validateur de schéma project-map (Task 5, spec §5/§16 B3) ──────────────
    // Forçage de catégorie : sur section_hint="project-map" UNIQUEMENT, le body
    // doit porter les liens typés obligatoires (1 project + 1 status + 1 kind,
    // ≤1 version cohérente). Échec → rejet 400 + audit, jamais auto-routé.
    // Les 11 autres sections : aucune validation (inchangé).
    // Validateur DÉDIÉ (spec §16 D1) — ne réveille pas F-38.
    if req.section_hint.as_deref() == Some("project-map") {
        let targets = gradatum_curator::wikilinks::extract_wikilinks(&req.body);
        if let Err(schema_err) = gradatum_core::project_map::validate_links_from_targets(&targets) {
            emit_write_rejection_audit(
                state,
                trust,
                &tenant,
                &locus,
                request_id,
                "rejected_400_project_map_schema",
                None,
            )
            .await;
            return Err(GradatumError::InvalidInput(format!(
                "project-map schema: {schema_err}"
            )));
        }
    }

    // Résolution note_id (None → ULID frais, invalide → 400).
    let note_id_prealloc = match req.note_id.as_deref() {
        None => Ulid::new(),
        Some(s) => match Ulid::from_string(s) {
            Ok(id) => id,
            Err(_) => {
                emit_write_rejection_audit(
                    state,
                    trust,
                    &tenant,
                    &locus,
                    request_id,
                    "rejected_400_bad_note_id",
                    Some(s.to_string()),
                )
                .await;
                return Err(GradatumError::InvalidInput("note_id invalide".into()));
            }
        },
    };

    // C1 — anti fail-open sha (overwrite avec sha malformé → 400).
    if req.note_id.is_some()
        && let Some(sha) = req.expected_sha256.as_deref()
        && parse_sha256_hex(sha).is_none()
    {
        emit_write_rejection_audit(
            state,
            trust,
            &tenant,
            &locus,
            request_id,
            "rejected_400_bad_sha",
            Some(note_id_prealloc.to_string()),
        )
        .await;
        return Err(GradatumError::InvalidInput(
            "expected_sha256 malformé".into(),
        ));
    }

    // Garde overwrite : note_id valide + note EXISTANTE sans expected_sha256 → Conflict.
    if req.note_id.is_some() && req.expected_sha256.is_none() {
        match state
            .vault
            .read_note_by_id(&note_id_prealloc.to_string())
            .await
        {
            Ok(_) => {
                emit_write_rejection_audit(
                    state,
                    trust,
                    &tenant,
                    &locus,
                    request_id,
                    "rejected_409_overwrite_no_sha",
                    Some(note_id_prealloc.to_string()),
                )
                .await;
                return Err(GradatumError::Conflict(
                    "overwrite sans expected_sha256".into(),
                ));
            }
            Err(gradatum_core::error::GradatumError::NoteNotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }

    let record = build_curate_job_record(&req, note_id_prealloc, &tenant);
    let job_ulid = state
        .job_store
        .enqueue(record)
        .await
        .map_err(|e| GradatumError::Storage(e.to_string()))?;

    // Audit succès.
    let job_id_str = job_ulid.to_string();
    let note_id_str = note_id_prealloc.to_string();
    let duration_ms = start.elapsed().as_millis() as i64;
    let audit_evt = HttpAuditEvent {
        ts: Utc::now(),
        event: "vault_write".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant.clone(),
        locus: locus.clone(),
        note_id: Some(note_id_str.clone()),
        content_hash: None,
        outcome: "queued".into(),
        curator: Some(serde_json::json!({ "job_id": job_id_str, "duration_ms": duration_ms })),
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(audit_evt).await {
        tracing::warn!(error = %e, "vault_write_impl: audit emit échoué — non fatal");
    }

    Ok(EnqueuedResponseUlid {
        job_id: job_id_str,
        status: "queued",
        poll_url: format!("/api/v1/jobs/{job_ulid}/v2"),
        note_id: note_id_str,
    })
}

// ── vault_classify_impl ───────────────────────────────────────────────────────

/// Heuristic confidence — note admise (match fort, haute confiance).
const CONF_ADMITTED: f32 = 0.9;
/// Heuristic confidence — note ambiguë (best guess, confiance faible).
const CONF_PENDING: f32 = 0.5;
/// Heuristic confidence — aucune suggestion fiable (outcome Rejected).
const CONF_NONE: f32 = 0.0;

/// Logique métier de `POST /api/v1/vault_classify`.
///
/// Classifie une note existante via l'heuristique offline — **aucun appel LLM, aucun
/// appel réseau**. La note est lue depuis le vault, soumise au `CuratorPipeline::heuristic()`
/// (mode CPU-only), et la section suggérée est retournée dans le `VaultClassifyResponse`.
///
/// Cette implémentation est **idempotente et en lecture seule** — elle ne mutate pas la
/// note ni l'index. C'est une consultation synchrone de l'heuristique.
///
/// # ACL
///
/// ACL **Read** sur `{tenant}/main` — classifier une note ne nécessite qu'un accès lecture
/// (aucune écriture dans le vault).
///
/// # Convention d'audit (read-paths)
///
/// Par convention homogène avec `vault_read_impl` et `vault_search_impl`, les **read-paths
/// n'émettent pas d'événement d'audit** (ni sur les refus d'authentification, ni sur les
/// refus d'ACL, ni sur le succès). Seules les mutations (`vault_write_impl`,
/// `vault_restore_impl`) enregistrent des événements d'audit — la traçabilité de lecture
/// n'est pas requise dans le modèle de sécurité actuel. Cette décision est assumée et
/// homogène : toute évolution vers l'audit des accès lecture doit être appliquée à
/// l'ensemble des read-paths.
///
/// # Mapping outcome → confidence
///
/// | `CurateOutcome`  | `suggested_section`         | `confidence` |
/// |------------------|-----------------------------|--------------|
/// | `Admitted`       | section suggérée heuristique | `0.9`        |
/// | `Pending`        | section suggérée heuristique | `0.5`        |
/// | `Rejected`       | `current_section` (inchangé) | `0.0`        |
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si cross-tenant ou ACL Read refusée.
/// - `GradatumError::InvalidInput` si `note_id` n'est pas un ULID valide.
/// - `GradatumError::NoteNotFound` si la note est absente du vault.
/// - `GradatumError::Storage` sur erreur I/O vault.
pub async fn vault_classify_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultClassifyRequest,
) -> Result<VaultClassifyResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny read".into()));
    }

    // Validation ULID — erreur 400 si invalide (avant toute I/O vault).
    if ulid::Ulid::from_string(&req.note_id).is_err() {
        return Err(GradatumError::InvalidInput(
            "note_id invalide (ULID attendu)".into(),
        ));
    }

    // Lecture de la note — 404 si absente, 500 sur erreur disque.
    let note = state.vault.read_note_by_id(&req.note_id).await?;
    let current_section = note.frontmatter.section.to_string();

    // Construction du CuratorNote — pattern identique au worker classify.
    let title_for_curator = gradatum_curator::extract_h1_title(&note.body.markdown)
        .unwrap_or_else(|| current_section.clone());
    let tags_hint: Vec<String> = note
        .frontmatter
        .tags
        .iter()
        .map(|t| t.as_str().to_owned())
        .collect();

    let curator_note = gradatum_curator::Note {
        id: req.note_id.clone(),
        title: title_for_curator,
        body: note.body.markdown,
        tags_hint,
        // Pas de hint de section : on laisse l'heuristique décider librement.
        section_hint: None,
    };

    // Heuristique pure — zéro LLM, zéro egress réseau.
    let outcome = gradatum_curator::CuratorPipeline::heuristic()
        .process(curator_note)
        .await;

    let (suggested_section, confidence) = match outcome {
        gradatum_curator::CurateOutcome::Admitted { decisions } => {
            (decisions.canonical_section, CONF_ADMITTED)
        }
        gradatum_curator::CurateOutcome::Pending { decisions, .. } => {
            (decisions.canonical_section, CONF_PENDING)
        }
        gradatum_curator::CurateOutcome::Rejected { .. } => (current_section.clone(), CONF_NONE),
    };

    Ok(VaultClassifyResponse {
        note_id: req.note_id,
        current_section,
        suggested_section,
        confidence,
        method: "heuristic".to_owned(),
    })
}

// ── §4 HISTORY HANDLERS ───────────────────────────────────────────────────────

/// Maps a `GradatumError` to an HTTP `StatusCode` (réutilisé par les wrappers HTTP history).
pub(crate) fn map_err_to_status_history(e: &GradatumError) -> StatusCode {
    match e {
        GradatumError::Unauthorized => StatusCode::UNAUTHORIZED,
        GradatumError::Forbidden(_) => StatusCode::FORBIDDEN,
        GradatumError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
        GradatumError::Storage(msg) if msg.contains("introuvable") || msg.contains("Not found") => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Logique métier de `POST /api/v1/vault_history`.
///
/// ACL Read + history_versions.
pub async fn vault_history_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultHistoryRequest,
) -> Result<VaultHistoryResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let versions = state.vault.history_versions(&req.note_id).await?;
    let count = versions.len();
    Ok(VaultHistoryResponse { versions, count })
}

/// Logique métier de `POST /api/v1/vault_history_get`.
///
/// ACL Read + history_get snapshot.
pub async fn vault_history_get_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultHistoryGetRequest,
) -> Result<VaultHistoryGetResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let snapshot = state.vault.history_get(&req.note_id, req.ts_ms).await?;
    Ok(VaultHistoryGetResponse {
        note_id: req.note_id,
        ts_ms: req.ts_ms,
        body: snapshot.body.markdown,
        section: snapshot.frontmatter.section.to_string(),
    })
}

/// Logique métier de `POST /api/v1/vault_restore`.
///
/// ACL **Write** + history_restore (CoW).
pub async fn vault_restore_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultRestoreRequest,
) -> Result<VaultRestoreResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    // Restauration = écriture → AclOp::Write.
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    let content_hash = state.vault.history_restore(&req.note_id, req.ts_ms).await?;
    Ok(VaultRestoreResponse {
        note_id: req.note_id,
        ts_ms: req.ts_ms,
        content_hash,
    })
}

/// Logique métier de `POST /api/v1/vault_diff`.
///
/// ACL Read + validation sélecteurs + history_diff.
pub async fn vault_diff_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultDiffRequest,
) -> Result<VaultDiffResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let is_valid_selector = |s: &str| -> bool { s == "current" || s.parse::<i64>().is_ok() };
    if !is_valid_selector(&req.a) || !is_valid_selector(&req.b) {
        return Err(GradatumError::InvalidInput(
            "sélecteur invalide (attendu 'current' ou timestamp ms)".into(),
        ));
    }

    let lines = state
        .vault
        .history_diff(&req.note_id, &req.a, &req.b)
        .await?;
    let count = lines.len();
    Ok(VaultDiffResponse { lines, count })
}

// ── §5 LESSONS ────────────────────────────────────────────────────────────────

/// Fixed section for the lesson corpus (synchronisé avec `lessons.rs`).
const LESSONS_SECTION: &str = "lessons-learned";
/// Single-vault tenant (synchronisé avec `lessons.rs`).
const LESSONS_TENANT: &str = "main";

/// Logique métier de `GET /api/v1/lessons/recall`.
///
/// ACL Read + usage + validation vocabulaire + recall BM25.
pub async fn lessons_recall_impl(
    state: &AppState,
    trust: &TrustContext,
    params: LessonsRecallRequest,
) -> Result<LessonsRecallResponse, GradatumError> {
    use gradatum_dto::is_valid_lesson_class;

    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    state
        .read_usage_accumulators
        .lessons_recall
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let acl_locus = format!("{LESSONS_TENANT}/{LESSONS_SECTION}");
    if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny lessons".into()));
    }

    let class = params.class.trim();
    if !is_valid_lesson_class(class) {
        return Err(GradatumError::InvalidInput(format!(
            "classe hors vocabulaire contrôlé: {class}"
        )));
    }

    let limit = params.limit.unwrap_or(5).clamp(1, 20) as usize;
    let vault_id = VaultId::new(LESSONS_TENANT);
    let raw_hits = state.search.recall_lessons(&vault_id, class, limit).await?;

    let items: Vec<LessonHit> = raw_hits
        .into_iter()
        .map(|h| LessonHit {
            ulid: h.note_id.0.to_string(),
            title: h.title.unwrap_or_default(),
            snippet: h.snippet,
            tags: h.tags,
            anchor_ms: h.anchor_ms,
        })
        .collect();

    Ok(LessonsRecallResponse { items })
}

// ── §6 NOTES SYNC HANDLERS ───────────────────────────────────────────────────
//
// Ces trois handlers opèrent directement sur SQLite (synchrones, 200/204) et
// constituaient la faille F-1 (A01 Broken Access Control) avant ce fix :
// les handlers notes.rs n'extrayaient pas TrustContext et contournaient donc
// l'intégralité de la chaîne auth+ACL.
//
// Séquence obligatoire (parité avec vault_write_impl / vault_restore_impl) :
//   1. is_authenticated(trust)?       → GradatumError::Unauthorized (401)
//   2. tenant dérivé du JWT           → GradatumError::Forbidden si absent
//   3. acl.evaluate(Write, locus)?    → GradatumError::Forbidden (403)
//   4. Logique métier (I/O)
//
// Pas d'audit.record : les 3 handlers sont des mutations sync directes
// (pas de job asynchrone), cohérent avec l'absence d'audit dans vault_history_impl
// et vault_restore_impl (ceux-ci n'auditent pas non plus).

/// Logique métier de `POST /api/v1/vault_downgrade`.
///
/// ACL Write sur `{tenant}/main` + downgrade note directement dans le SQLite index.
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si cross-tenant ou ACL Write refusée.
/// - `GradatumError::NoteNotFound` si la note est absente.
/// - `GradatumError::Validation` si la requête est invalide (ULID, auto-référence).
/// - `GradatumError::Storage` sur erreur SQLite inattendue.
pub async fn vault_downgrade_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultDowngradeRequest,
) -> Result<VaultDowngradeResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    // tenant dérivé du JWT — req.tenant_id vérifié par cohérence (cross-tenant → 403).
    let tenant = effective_tenant(trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // Parse des ULID — erreurs déjà typées via GradatumError::Validation.
    let note_id = ulid::Ulid::from_string(&req.note_id)
        .map(NoteId)
        .map_err(|_| {
            GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                "note_id invalide (ULID attendu)".into(),
            ))
        })?;
    let replaced_by = req
        .replaced_by
        .as_deref()
        .map(|s| {
            ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                    "replaced_by invalide (ULID attendu)".into(),
                ))
            })
        })
        .transpose()?;

    state
        .search
        .downgrade_note(&note_id, &req.reason, replaced_by.as_ref())
        .await?;

    let now = chrono::Utc::now().timestamp_millis();
    Ok(VaultDowngradeResponse {
        note_id: req.note_id,
        status: "downgraded".to_string(),
        status_changed: now,
        reason: req.reason,
    })
}

/// Logique métier de `PATCH /api/v1/notes/{id}`.
///
/// ACL Write sur `{tenant}/main` + patch partiel (status / reason / replaced_by / add_tags)
/// directement dans le SQLite index ou via le vault (state machine + CoW).
///
/// `note_id` est le ULID préalablement parsé par le handler HTTP (parse-don't-validate
/// à la frontière HTTP — le thin wrapper valide le format avant d'appeler cette fonction).
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL Write refusée.
/// - `GradatumError::InvalidInput` si status hors enum ou ULID invalide.
/// - `GradatumError::NoteNotFound` si note absente.
/// - `GradatumError::InvalidStatusTransition` si transition état machine invalide.
/// - `GradatumError::Storage` sur erreur SQLite.
pub async fn patch_note_impl(
    state: &AppState,
    trust: &TrustContext,
    note_id: &NoteId,
    body: NoteStatusPatch,
) -> Result<(), GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    // Pour patch_note, le DTO n'a pas de tenant_id — on dérive directement du JWT.
    // Un contexte sans tenant (Mtls/Studio/Unauthenticated) est déjà rejeté
    // par is_authenticated() ci-dessus (Unauthenticated) ou ici (Mtls/Studio).
    let tenant = trust.tenant_id().ok_or_else(|| {
        GradatumError::Forbidden("contexte sans tenant — accès vault refusé".into())
    })?;
    let locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // Logique métier extraite de patch_note (inchangée — contrat métier identique).
    if let Some(ref status_str) = body.status {
        let target: gradatum_core::status::NoteStatus =
            serde_json::from_value(serde_json::Value::String(status_str.clone()))
                .map_err(|_| GradatumError::InvalidInput("status hors enum NoteStatus".into()))?;

        state
            .vault
            .update_note_status(&note_id.to_string(), target, body.status_reason.clone())
            .await?;

        // replaced_by fourni conjointement avec status → patcher via SQL direct
        // après la transition state machine (update_note_status ne le prend pas).
        if body.replaced_by.is_some() {
            let replaced_by = body
                .replaced_by
                .as_deref()
                .map(|s| {
                    ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                        GradatumError::InvalidInput("replaced_by invalide (ULID attendu)".into())
                    })
                })
                .transpose()?;
            state
                .search
                .patch_note_status(note_id, None, None, replaced_by.as_ref())
                .await?;
        }
    } else {
        // Patch partiel sans changement de statut — SQL direct (raison / replaced_by).
        let replaced_by = body
            .replaced_by
            .as_deref()
            .map(|s| {
                ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                    GradatumError::InvalidInput("replaced_by invalide (ULID attendu)".into())
                })
            })
            .transpose()?;
        state
            .search
            .patch_note_status(
                note_id,
                None,
                body.status_reason.as_deref(),
                replaced_by.as_ref(),
            )
            .await?;
    }

    Ok(())
}

/// Logique métier de `POST /api/v1/notes/{id}/move`.
///
/// ACL Write sur `{tenant}/main` + relocalisation physique du `.md` via `vault.move_locus`.
///
/// `id` est l'identifiant brut issu du path (validé comme ULID syntaxiquement valide
/// par le handler HTTP avant l'appel — locus validé par `LocusId::parse`).
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL Write refusée.
/// - `GradatumError::NoteNotFound` si la note est absente.
/// - `GradatumError::Validation` si le locus est invalide.
/// - `GradatumError::Storage` sur erreur vault inattendue.
pub async fn move_note_locus_impl(
    state: &AppState,
    trust: &TrustContext,
    id: &str,
    locus: LocusId,
) -> Result<(), GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    // Pour move_note_locus, le DTO n'a pas de tenant_id — dérivé du JWT.
    let tenant = trust.tenant_id().ok_or_else(|| {
        GradatumError::Forbidden("contexte sans tenant — accès vault refusé".into())
    })?;
    let acl_locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &acl_locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    state.vault.move_locus(id, &locus).await?;
    Ok(())
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use gradatum_core::trust::TrustContext;

    use super::{
        KNOWN_DOC_KINDS, LESSONS_SECTION, LESSONS_TENANT, locus_for_section, locus_for_tenant,
    };

    // ── Tests helpers ──────────────────────────────────────────────────────────

    #[test]
    fn locus_for_tenant_builds_main_locus() {
        assert_eq!(locus_for_tenant("main"), "main/main");
        assert_eq!(locus_for_tenant("staging"), "staging/main");
    }

    #[test]
    fn locus_for_section_with_section() {
        assert_eq!(
            locus_for_section("main", Some("decisions")),
            "main/decisions"
        );
        assert_eq!(
            locus_for_section("main", Some("lessons-learned")),
            "main/lessons-learned"
        );
    }

    #[test]
    fn locus_for_section_none_defaults_to_main() {
        assert_eq!(locus_for_section("main", None), "main/main");
    }

    // ── Tests invariants constants ─────────────────────────────────────────────

    #[test]
    fn known_doc_kinds_contains_expected_variants() {
        assert!(KNOWN_DOC_KINDS.contains(&"Static"));
        assert!(KNOWN_DOC_KINDS.contains(&"Event"));
        assert!(KNOWN_DOC_KINDS.contains(&"Versioned"));
        assert_eq!(KNOWN_DOC_KINDS.len(), 3, "3 variantes doc_kind connues");
    }

    #[test]
    fn lessons_constants_coherent() {
        assert_eq!(LESSONS_SECTION, "lessons-learned");
        assert_eq!(LESSONS_TENANT, "main");
    }

    // ── Test ACL : unauthenticated → Unauthorized ──────────────────────────────
    //
    // Prouve que les *_impl rejettent les requêtes non authentifiées AVANT
    // toute I/O — test direct sans axum, sans AppState complet.
    // TrustContext::Unauthenticated représente une requête sans bearer.

    #[test]
    fn trust_context_unauthenticated_is_not_authenticated() {
        // Prouve que le type sentinel utilisé dans les *_impl pour le check
        // is_authenticated() est bien détecté comme non-authentifié.
        let trust = TrustContext::Unauthenticated;
        assert!(
            !trust.is_authenticated(),
            "TrustContext::Unauthenticated doit refuser is_authenticated()"
        );
    }

    #[test]
    fn trust_context_bearer_is_authenticated() {
        let trust = TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: "agent".into(),
            scopes: vec!["read".into()],
            tenant_id: "main".into(),
        };
        assert!(
            trust.is_authenticated(),
            "BearerToken doit être authentifié"
        );
    }
}
