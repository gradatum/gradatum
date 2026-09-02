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
//! - `vault_downgrade` : servie par la version sync dans `notes.rs` (la variante async
//!   par queue a été retirée avec le moteur legacy).
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
use gradatum_acl_auth::IDENTITY_WRITE_SCOPE;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::audit::http::HttpAuditEvent;
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AgentId, LocusId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::temporal_query::{TimelineCursor, TimelineFilter};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    LessonHit, LessonsRecallRequest, LessonsRecallResponse, NoteStatusPatch, RankMode,
    VaultClassifyRequest, VaultClassifyResponse, VaultDiffRequest, VaultDiffResponse,
    VaultDowngradeRequest, VaultDowngradeResponse, VaultHistoryGetRequest, VaultHistoryGetResponse,
    VaultHistoryRequest, VaultHistoryResponse, VaultRestoreRequest, VaultRestoreResponse,
};
use gradatum_embed::EmbedBackend;
use gradatum_index::extract_h1_title;
use gradatum_index::links::title_to_slug;
use gradatum_search::scoring::{
    composite_score_with_trust, pagerank_factor, recency_factor, trust_decay_factor,
};
use gradatum_search::{rrf_fuse, rrf_fuse_short_circuit};
use ulid::Ulid;

use crate::api_v1::dto::{
    AuthorEntry, EnqueuedResponseUlid, GraphEdge, ScoreBreakdown, SearchHit, TagEntry, TraceEntry,
    VaultAuthorsResponse, VaultContextRequest, VaultContextResponse, VaultEntry, VaultGraphRequest,
    VaultGraphResponse, VaultLinksRequest, VaultLinksResponse, VaultListRequest, VaultListResponse,
    VaultReadRequest, VaultReadResponse, VaultSearchRequest, VaultSearchResponse,
    VaultStatusResponse, VaultTagsRequest, VaultTagsResponse, VaultTimelineRequest,
    VaultTraceRequest, VaultTraceResponse,
};
use crate::api_v1::handlers::{
    build_fts_query, filter_semantic_by_section, filter_semantic_by_status,
    filter_semantic_excluding_sections, validate_search_status,
};
use crate::api_v1::tenant_guard::{effective_read_vault, effective_tenant, effective_write_vault};
use crate::api_v1::timeline::{TimelineItem, VaultTimelineResponse};
use crate::api_v1::write::{
    actor_from_trust, build_curate_job_record, emit_auth_failure_audit, emit_drift_audit,
    emit_read_rejection_audit, emit_write_rejection_audit, parse_sha256_hex,
};
use crate::context::retrieval::retrieve_candidates;
use crate::note_usage_store::{KIND_READ, KIND_SEARCH_HIT, KIND_SEARCH_HIT_TOP3};
use crate::state::AppState;

// ── Helpers internes ─────────────────────────────────────────────────────────

/// Logs an ACL denial with the identity and the locus that was evaluated (B6′b).
///
/// ## Pourquoi ceci existe
///
/// `require_read_grant`, `require_write_grant`, `require_active_target` et
/// `write_scope_allowed` loggent tous leur refus ; l'évaluation ACL, elle, ne loggeait
/// rien. Sur la route qui les enchaîne, un `403` pouvait donc sortir sans laisser
/// **aucune trace** : l'opérateur voyait un refus sans corps ni ligne de journal,
/// indistinguable d'une panne. C'est ce qui a coûté une journée d'instruction sur
/// l'incident `engine` du 2026-07-27. L'asymétrie était pure — même barrière, même
/// statut, un seul barreau muet.
///
/// ## Ce qu'il porte
///
/// L'identité (`sub` / `user` / `cn` selon la variante) et le **locus évalué** : sans le
/// locus, la ligne dit qu'un refus a eu lieu sans dire sur quoi, ce qui ne réduit pas le
/// temps de diagnostic. Ce sont exactement les deux valeurs qu'il faut confronter au
/// preset pour conclure.
///
/// Niveau `warn!` : un refus ACL est un fait d'exploitation attendu (le défaut est deny),
/// pas une défaillance du service.
///
/// ## Ce qu'il ne fait pas
///
/// Il ne décide rien — l'appelant reste maître du refus. Il n'est PAS appelé depuis
/// [`gradatum_acl_policy::AclEngine::evaluate`] : ce point central est aussi traversé par
/// des chemins de **filtrage** (sélection des sections lisibles) où un deny est nominal et
/// massif ; y loguer noierait le signal que ce helper existe précisément pour rendre
/// audible.
pub(crate) fn log_acl_deny(trust: &TrustContext, op: AclOp, locus: &str, site: &str) {
    let identity = match trust {
        TrustContext::BearerToken { sub, .. } => sub.as_str(),
        TrustContext::Studio { user, .. } => user.as_str(),
        TrustContext::Mtls { cn, .. } => cn.as_str(),
        _ => "<unauthenticated>",
    };
    tracing::warn!(
        sub = %identity,
        locus = %locus,
        op = ?op,
        site = %site,
        "acl deny — identity not granted on the evaluated locus (403)"
    );
}

/// Builds the ACL locus for a tenant: `{tenant_id}/main` (default section).
pub(crate) fn locus_for_tenant(tenant_id: &str) -> String {
    format!("{}/main", tenant_id)
}

/// Builds the ACL locus for a specific section.
pub(crate) fn locus_for_section(tenant_id: &str, section: Option<&str>) -> String {
    match section {
        Some(s) => format!("{}/{}", tenant_id, s),
        None => format!("{}/main", tenant_id),
    }
}

/// Author effectif d'une note en écriture — **l'identité vient du credential** (v2.0.0).
///
/// L'author dérive exclusivement de l'agent authentifié (`trust.subject()` — le `sub` du
/// credential, source serveur). Il ne se déclare pas côté client et n'a aucun défaut :
///
/// - `req_author` `Some` → **REFUS** : l'identité ne se déclare pas (R2). Un client ne peut
///   pas choisir sous quelle identité sa note est attribuée.
/// - `req_author` `None` + `subject` `Some(id)` → chemin NOMINAL : l'author est le nom NU
///   du sujet (jamais préfixé `kind:` — le charset d'`AgentId`, cf. `scope.rs`, interdit le
///   `:` ; ce nom nu est l'identité résolue, accepté tel quel par `parse_author`).
/// - `req_author` `None` + `subject` `None` → **REFUS** : aucune identité résolue, jamais une
///   note sans auteur (R2, fail-closed).
///
/// **Locus unique du refus** : la garde vit ici et nulle part ailleurs. Deux appelants
/// de production la traversent — [`vault_write_impl`] (propage l'erreur via `?`) et le
/// handler de capture (`capture.rs`, mappe l'erreur en statut HTTP) — et **tous deux sont
/// fail-closed sur l'identité du credential** : ils passent `req_author = None` en dur et
/// n'obtiennent un author que du sujet authentifié.
///
/// # Errors
///
/// - [`GradatumError::InvalidInput`] si un `req_author` est fourni (l'identité ne se déclare pas).
/// - [`GradatumError::Unauthorized`] si aucune identité n'est résolue (ni author, ni sujet).
pub(crate) fn effective_author(
    req_author: &Option<String>,
    subject: Option<&AgentId>,
) -> Result<String, GradatumError> {
    if req_author.is_some() {
        return Err(GradatumError::InvalidInput(
            "author provided: identity comes from the credential, it is not self-declared (R2)"
                .into(),
        ));
    }
    match subject {
        Some(id) => Ok(id.as_str().to_string()),
        None => Err(GradatumError::Unauthorized),
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
/// - `Storage(msg)` contenant "not found" ou "Not found" → 404
/// - Tout autre      → 500
pub(crate) fn err_to_status(e: &GradatumError) -> StatusCode {
    match e {
        GradatumError::Unauthorized => StatusCode::UNAUTHORIZED,
        GradatumError::Forbidden(_) => StatusCode::FORBIDDEN,
        GradatumError::InvalidInput(_) => StatusCode::BAD_REQUEST,
        GradatumError::Conflict(_) => StatusCode::CONFLICT,
        GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
        GradatumError::Storage(msg) if msg.contains("not found") || msg.contains("Not found") => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Résout le handle de read-back d'un handler `api/v1`, **gaté sur `multi_tenant`**
/// (évite le split-brain read-back).
///
/// - `multi_tenant.enabled = false` (défaut LIVE) → singleton `state.vault` inchangé
///   (byte-identical).
/// - `enabled = true` → route via `state.vaults.resolve` sur le vault effectif `tenant`
///   (déjà résolu par `effective_write_vault`/`effective_tenant` — source de confiance →
///   [`VaultId::new`]). **Fail-closed** : vault inconnu → [`GradatumError::VaultNotFound`]
///   (500 via `err_to_status`), jamais un repli silencieux sur `main`.
///
/// # Errors
///
/// [`GradatumError::VaultNotFound`] à `enabled = true` si `tenant` n'est pas dans le registre.
#[allow(clippy::result_large_err)]
fn read_back_reader(
    state: &AppState,
    tenant: &str,
) -> Result<std::sync::Arc<dyn gradatum_vault::Registry>, GradatumError> {
    if state.server_config.multi_tenant.enabled {
        state.vaults.resolve(&VaultId::new(tenant))
    } else {
        Ok(std::sync::Arc::clone(&state.vault))
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();

    // EX-C2-1 : à `multi_tenant.enabled = true`, l'ACL est recalculée sur la CIBLE
    // (`read_vault_id`) + grant read per-vault. À OFF, le chemin legacy ci-dessous
    // (ACL appelant + garde mono-vault) est conservé inchangé.
    let checked_on: Option<gradatum_core::scope::AclCheckedVaultId> =
        if state.server_config.multi_tenant.enabled {
            Some(
                effective_read_vault(
                    state,
                    trust,
                    &tenant,
                    req.vault_id.as_ref(),
                    req.section.as_deref(),
                    "acl deny",
                )
                .await?,
            )
        } else {
            let acl_locus = locus_for_section(&tenant, req.section.as_deref());
            if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
                return Err(GradatumError::Forbidden("acl deny".into()));
            }

            // Validation vault_id (legacy mono-vault — cross-read non supporté à OFF).
            // `req.vault_id: Option<VaultId>` (newtype transparent, non re-validé à la
            // désérialisation) → `.as_str()` restitue la même chaîne que l'ancien `&str`.
            if let Some(vid) = req.vault_id.as_ref() {
                if vid.as_str().is_empty() || vid.as_str().len() > 128 {
                    return Err(GradatumError::InvalidInput("invalid vault_id".into()));
                }
                if vid.as_str() != "main" {
                    return Err(GradatumError::Forbidden(
                        "cross-read vault_id ≠ main not supported (mono-vault)".into(),
                    ));
                }
            }
            None
        };

    // Validation status filter.
    let status_filter = validate_search_status(req.status.as_deref())
        .map_err(|()| GradatumError::InvalidInput("invalid status".into()))?;

    // Validation temporal bounds (F-65).
    if matches!((req.from_ms, req.to_ms), (Some(f), Some(t)) if f > t) {
        return Err(GradatumError::InvalidInput("from_ms > to_ms".into()));
    }

    // Témoin de lecture (EX-C2-2) : à OFF la cible est "main" == tenant, dont l'ACL
    // vient d'être évaluée ci-dessus — l'attestation est donc exacte sur les 2 chemins.
    let read_vault = match checked_on {
        Some(checked) => checked,
        None => gradatum_core::scope::AclCheckedVaultId::attest_read_checked(
            req.vault_id
                .clone()
                .unwrap_or_else(|| VaultId::new(tenant.as_str())),
        ),
    };
    let query = req.query.trim();
    if query.is_empty() {
        return Ok(VaultSearchResponse {
            items: vec![],
            corpus_match_count: None,
            corpus_count_capped: false,
        });
    }

    let limit = req.limit.unwrap_or(10).clamp(1, 50) as usize;
    let fts_query = build_fts_query(query);

    // F-246 : inventaire des sections exclues du périmètre de recherche PAR DÉFAUT.
    //
    // Sans filtre de section explicite (`req.section = None`), les sections
    // `Section::DEFAULT_SEARCH_EXCLUDED` (matière première brute, ex. `snapshot`) sont
    // écartées des résultats ET du `corpus_match_count`. Avec un filtre de section
    // explicite — Y COMPRIS `snapshot` lui-même — l'exclusion ne s'applique PAS :
    // exclusion par défaut ≠ inaccessibilité, une capture reste atteignable.
    let excluded_sections: &[Section] = if req.section.is_none() {
        Section::DEFAULT_SEARCH_EXCLUDED
    } else {
        &[]
    };

    // Signal BM25.
    let bm25_hits = state
        .search
        .search_fts_with_snippet(
            &read_vault,
            &fts_query,
            limit * 2,
            req.include_downgraded,
            req.section.as_deref(),
            req.locus.as_deref(),
            status_filter.as_deref(),
            req.from_ms, // F-65 temporal lower bound
            req.to_ms,   // F-65 temporal upper bound
        )
        .await?;

    // F-246 : exclusion par défaut sur le bras lexical. `search_fts_with_snippet` reste
    // section-agnostique à `None` (le filtre sémantique multi-section de vault_context
    // passe par le même appel puis filtre EN MÉMOIRE sur une section explicite) — on
    // filtre donc `SearchHitRaw.section` ici, parité avec `retrieve_candidates`.
    let bm25_hits = if excluded_sections.is_empty() {
        bm25_hits
    } else {
        bm25_hits
            .into_iter()
            .filter(|h| {
                !excluded_sections
                    .iter()
                    .any(|s| s.as_str() == h.section.as_str())
            })
            .collect()
    };

    // Signal sémantique (dégradation gracieuse si Noop ou erreur).
    let mut semantic_hits: Vec<(gradatum_core::identity::NoteId, f32)> =
        if state.embedder.backend_kind() != EmbedBackend::Noop {
            match state.embedder.embed(query).await {
                Ok(query_emb) => {
                    let hits = state
                        .search
                        .search_semantic(
                            &read_vault,
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
                    if hits.is_empty() && req.vault_id.is_some() && read_vault.as_str() != tenant {
                        tracing::info!(
                            vault_id = %read_vault,
                            query = %query,
                            "vault_search_impl: 0 semantic hits on cross-tenant vault"
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
            .get_titles_sections(&read_vault, &sem_ids)
            .await;
        semantic_hits = filter_semantic_by_section(semantic_hits, wanted_section, sec_result);
    }

    // F-246 : exclusion section par défaut sur le chemin sémantique (symétrique C2).
    // Sans filtre de section explicite, les sections `DEFAULT_SEARCH_EXCLUDED` sont
    // écartées AUSSI des hits sémantiques — même patron de batch `get_titles_sections`
    // que le filtre inclusif ci-dessus (les deux bras sont mutuellement exclusifs car
    // `excluded_sections` n'est non vide que si `req.section` est `None`).
    if !excluded_sections.is_empty() && !semantic_hits.is_empty() {
        let excluded_strs: Vec<&str> = excluded_sections.iter().map(Section::as_str).collect();
        let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
        let sec_result = state
            .search
            .get_titles_sections(&read_vault, &sem_ids)
            .await;
        semantic_hits =
            filter_semantic_excluding_sections(semantic_hits, &excluded_strs, sec_result);
    }

    // Filtre status sur chemin sémantique (symétrique C2).
    if let Some(wanted_status) = status_filter.as_deref()
        && !semantic_hits.is_empty()
    {
        let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
        let status_result = state.search.get_statuses(&read_vault, &sem_ids).await;
        semantic_hits = filter_semantic_by_status(semantic_hits, wanted_status, status_result);
    }

    // Filtre temporel + batch anchor_ms sur chemin sémantique (F-65).
    // Appel batch même sans bornes pour peupler sem_anchor_map (enrichit anchor_ms dans les hits).
    let sem_anchor_map: std::collections::HashMap<String, i64> = if semantic_hits.is_empty() {
        std::collections::HashMap::new()
    } else {
        let sem_ids: Vec<String> = semantic_hits.iter().map(|(id, _)| id.to_string()).collect();
        let bounds_active = req.from_ms.is_some() || req.to_ms.is_some();
        let anchor_result = state
            .search
            .get_anchor_ms_batch(&read_vault, &sem_ids)
            .await
            .unwrap_or_else(|e| {
                if bounds_active {
                    tracing::warn!(
                        err = %e,
                        count = sem_ids.len(),
                        from_ms = ?req.from_ms,
                        to_ms = ?req.to_ms,
                        "vault_search_impl: get_anchor_ms_batch failed with active bounds — \
                         ALL semantic hits dropped (temporal bound unverifiable)"
                    );
                } else {
                    tracing::warn!(
                        err = %e,
                        count = sem_ids.len(),
                        "vault_search_impl: get_anchor_ms_batch failed — \
                         anchor_ms absent from semantic hits (no active bounds)"
                    );
                }
                std::collections::HashMap::new()
            });
        // Filtrage temporel sur chemin sémantique si bornes actives.
        if bounds_active {
            semantic_hits.retain(|(id, _)| {
                match anchor_result.get(&id.to_string()) {
                    // Note sans entrée temporal_index → exclure si borne active (symétrique FTS).
                    None => false,
                    Some(&ms) => {
                        req.from_ms.is_none_or(|f| ms >= f) && req.to_ms.is_none_or(|t| ms <= t)
                    }
                }
            });
        }
        anchor_result
    };

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
    // F-162 critère 6 : court-circuit à bras unique. Quand le système est hybride
    // (embedder actif) et qu'un seul bras répond, la fusion par rang est court-circuitée —
    // le score normalisé du bras qui répond fait foi (la magnitude n'est plus jetée).
    // Le chemin BM25-only par configuration (embedder Noop) reste sur la fusion par rang
    // pure (rétrocompat bit-à-bit — tests snapshot `salience_off` inchangés).
    let mut fused = if state.embedder.backend_kind() != EmbedBackend::Noop {
        rrf_fuse_short_circuit(&bm25_for_rrf, &sem_for_rrf, 60.0, rrf_buffer)
    } else {
        rrf_fuse(&bm25_for_rrf, &sem_for_rrf, 60.0, rrf_buffer)
    };

    // Enrichir section + snippet + title + status + anchor_ms depuis la map BM25.
    // F-17 : pour les hits semantic-only (absents de bm25_map), enrichir anchor_ms depuis
    // sem_anchor_map ICI — avant la boucle composite — afin que `recency_factor` reçoive
    // l'ancre canonique (occurred_at/event-date/valid_from) et non `created_ms`.
    // Sans ce pré-enrichissement, anchor_ms resterait None au moment du scoring, et
    // `hit.anchor_ms.unwrap_or(created_ms)` tomberait systématiquement sur created_ms.
    for hit in &mut fused {
        if let Some(bh) = bm25_map.get(&hit.note_id) {
            hit.section = bh.section.clone();
            hit.snippet = Some(bh.snippet.clone());
            hit.title = bh.title.clone().filter(|s| !s.trim().is_empty());
            hit.status = bh.status.clone();
            hit.anchor_ms = bh.anchor_ms; // F-65 temporal
        } else if hit.anchor_ms.is_none() {
            // Hit semantic-only : peupler anchor_ms depuis sem_anchor_map avant le scoring. (F-17)
            hit.anchor_ms = sem_anchor_map.get(&hit.note_id).copied();
        }
    }

    // F-261 : la dérivation du facteur de confiance exige la section de CHAQUE hit au scoring.
    // Les hits semantic-only (absents de bm25_map) n'ont pas de section ici → lookup batch
    // (anti-N+1, même pattern que get_titles_sections, ci-dessous en fin de handler) AVANT la
    // boucle composite. Échec = section vide → trust neutre 0.5 (comportement actuel),
    // jamais bloquant.
    let missing_section_ids: Vec<String> = fused
        .iter()
        .filter(|h| h.section.is_empty())
        .map(|h| h.note_id.clone())
        .collect();
    if !missing_section_ids.is_empty() {
        let section_map = state
            .search
            .get_titles_sections(&read_vault, &missing_section_ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    err = %e,
                    count = missing_section_ids.len(),
                    "vault_search_impl: get_titles_sections (pre-scoring) failed — trust neutre"
                );
                HashMap::new()
            });
        for hit in &mut fused {
            if hit.section.is_empty()
                && let Some((_, section)) = section_map.get(&hit.note_id)
            {
                hit.section.clone_from(section);
            }
        }
    }

    // Scoring composite multi-facteur.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut composite_hits: Vec<(gradatum_search::RrfHit, f64)> = Vec::with_capacity(fused.len());
    let mut score_breakdowns: HashMap<String, ScoreBreakdown> = HashMap::new();

    // F-110 Phase 2 : lookup batch salience — UNE requête pour tout le buffer RRF (≤ 50).
    // Best-effort absolu : échec ⇒ map vide ⇒ facteur neutre partout + warn!.
    // Flag OFF (state.salience == None) ou store absent ⇒ aucun lookup, map vide.
    let salience_counts: HashMap<String, Vec<(String, u64)>> =
        match (&state.salience, &state.note_usage) {
            (Some(_), Some(store)) => {
                let ids: Vec<String> = fused.iter().map(|h| h.note_id.clone()).collect();
                // Dimension `note_usage` = NAMESPACE (vault cible), pas le principal :
                // la salience compte l'usage des notes DU vault lu (`read_vault`), cohérent
                // avec le read-side audit_job. À OFF `read_vault == tenant == "main"`
                // (byte-identical). Réf conflation Task 11 / `arch/01KXWMDDX1`.
                match store.counts_for_notes(read_vault.vault_id(), &ids).await {
                    Ok(map) => map,
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            "vault_search_impl: salience batch lookup failed — neutral factor"
                        );
                        HashMap::new()
                    }
                }
            }
            _ => HashMap::new(),
        };

    // L6 : params salience EFFECTIFS pour le vault lu — override per-vault A6 s'il existe,
    // sinon le global. Sélection de RÉFÉRENCE (aucune allocation), résolue UNE fois par requête
    // (`read_vault` constant sur toute la boucle). Gate `state.salience.as_ref()` : à OFF
    // (`state.salience == None`) la map per-vault n'est JAMAIS consultée ⇒ `effective_salience
    // == None`, le bras `None` du scoring ci-dessous reste byte-identical.
    let effective_salience: Option<&std::sync::Arc<gradatum_search::SalienceParams>> =
        state.salience.as_ref().and_then(|global| {
            match state.salience_per_vault.get(read_vault.vault_id().as_str()) {
                // Override présent : `Some(params)` = actif (raffine) ; `None` = désactivé
                // ⇒ salience neutralisée pour ce vault (fix footgun C1, symétrie
                // `review_promote_for`). Aucun override ⇒ global.
                Some(entry) => entry.as_ref(),
                None => Some(global),
            }
        });

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
                    "vault_search_impl: note absent, fallback (now_ms, 0)"
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

        // F-17 : recency sur l'ancre canonique (temporal_index.anchor_ms =
        // occurred_at / event-date / valid_from, via F-65) plutôt que sur created_ms.
        // Pour les notes statiques (anchor_src=Created, anchor_ms==created_ms), le résultat
        // est bit-identique à l'ancien comportement (backward-compat garantie).
        // Pour les notes Event (anchor_ms != created_ms), le recency reflète la date
        // d'événement réelle, pas la date d'ingestion.
        // Fallback defensif sur created_ms si anchor_ms absent (ne devrait pas arriver pour
        // les notes avec entrée temporal_index, mais protège contre les cas orphelins).
        let anchor_for_recency = hit.anchor_ms.unwrap_or(created_ms);
        let recency = recency_factor(anchor_for_recency, now_ms);
        let pagerank = pagerank_factor(in_degree);

        let trust_params = if state.scoring.enabled {
            let age_days = ((now_ms - created_ms).max(0) as f64) / 86_400_000.0;
            // F-261 : trust dérivé de la section (fonction pure) — plus de lecture de la colonne
            // `notes.trust`. Section inconnue/vide → neutre 0.5 + doc_kind Static (non périssable),
            // soit exactement le comportement actuel de la population. L'âge du decay reste sur
            // created_ms (invariant M-1, cf. context_select_recency).
            state.scoring.resolve_for_section(&hit.section, age_days)
        } else {
            None
        };

        let composite_base =
            composite_score_with_trust(hit.rrf_score, recency, pagerank, trust_params);
        // F-110 Phase 2 : 4ᵉ facteur salience. `effective_salience == None` (flag OFF, défaut)
        // ⇒ composite == composite_base bit-à-bit, aucun lookup n'a eu lieu. À ON, `params`
        // porte l'override per-vault L6 (ou le global si aucun override) — cf. `effective_salience`.
        let (composite, salience_ws) = match effective_salience {
            Some(params) => {
                let ws = salience_counts
                    .get(&hit.note_id)
                    .map(|counts| {
                        gradatum_search::salience_weighted_sum(counts, &params.kind_weights)
                    })
                    .unwrap_or(0.0);
                (
                    gradatum_search::apply_salience(composite_base, ws, params),
                    ws,
                )
            }
            None => (composite_base, 0.0),
        };

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
                    salience_weighted_sum: effective_salience.map(|_| salience_ws),
                    salience_factor: effective_salience
                        .map(|p| gradatum_search::salience_factor(salience_ws, p.k_norm)),
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
                // C2 : cible vérifiée (et plus `tenant`) — à OFF strictement identique
                // (cible == tenant), à ON évite de mélanger les vaults.
                .get_titles_sections(&read_vault, &semantic_only_ids)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        err = %e,
                        count = semantic_only_ids.len(),
                        "vault_search_impl: get_titles_sections failed, sem-only without title"
                    );
                    HashMap::new()
                })
        };
    let status_map: HashMap<String, String> = if semantic_only_ids.is_empty() {
        HashMap::new()
    } else {
        state
            .search
            .get_statuses(&read_vault, &semantic_only_ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    err = %e,
                    count = semantic_only_ids.len(),
                    "vault_search_impl: get_statuses failed, sem-only without status"
                );
                HashMap::new()
            })
    };

    // Guard P1-B FAIL-CLOSED (audit security-reviewer v0.7.3, durci 2026-06-28).
    //
    // Parité avec vault_read_impl (~L671) : la read-restriction identity existait déjà
    // pour la lecture directe, mais PAS pour le chemin search (fuite titre + snippet).
    //
    // Callers autorisés à voir les âmes :
    //   - TrustContext::Studio { .. } — admin UI (observabilité totale).
    //   - porteur du scope `identity_write` — privilège d'âme (v2.0.0, par scope, pas par nom).
    // Tout autre caller → exclusion des notes soul.
    //
    // FAIL-CLOSED : le filtre opère sur `hit.section` AVANT le fallback section→"main".
    //   - hit.section == "identity" → âme explicite → exclure.
    //   - hit.section.is_empty()   → section indéterminée (ex. soft-fail get_titles_sections
    //     qui retourne HashMap::new() pour un hit sémantique) → exclure par précaution
    //     (confidentialité > complétude search).
    // En fonctionnement nominal, hit.section est toujours peuplé → 0 impact sur recall.
    //
    // L'exclusion est simple (pas de matching par-agent) : un agent reçoit son âme
    // par injection MCP initialize, pas par search — le search est une surface RAG générique.
    let identity_privileged = is_identity_privileged(trust);

    // Construire la réponse.
    let items: Vec<SearchHit> = final_hits
        .into_iter()
        .filter_map(|(mut hit, score)| {
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
            // Guard FAIL-CLOSED : vérifier AVANT le fallback section→"main".
            // Si section=="identity" ou section=="" (indéterminée) et caller non-privilégié → exclure.
            if identity_section_hidden(identity_privileged, &hit.section) {
                return None;
            }
            if hit.status.is_empty()
                && let Some(fetched_status) = status_map.get(&hit.note_id)
            {
                hit.status = fetched_status.clone();
            }
            // F-65 : enrich anchor_ms for semantic-only hits from the batch map.
            if hit.anchor_ms.is_none() && hit.is_semantic_only {
                hit.anchor_ms = sem_anchor_map.get(&hit.note_id).copied();
            }
            let section = if hit.section.is_empty() {
                "main".to_string()
            } else {
                hit.section
            };
            let scores = score_breakdowns.remove(&hit.note_id);
            #[allow(deprecated)]
            Some(SearchHit {
                // Provenance : le vault EFFECTIVEMENT lu, pas celui demandé. Sur le
                // chemin cross-vault, `read_vault` est le témoin ACL de la CIBLE
                // (`effective_read_vault`) ; à `multi_tenant` OFF il vaut le vault du
                // JWT. Restituer `req.vault_id` à la place ré-affirmerait l'entrée
                // client au lieu d'attester la sortie serveur.
                vault_id: read_vault.vault_id().clone(),
                path: format!("{}/{}", section, hit.note_id),
                score,
                title: hit.title,
                snippet: hit.snippet,
                trust: 0.5,
                status: hit.status,
                scores,
                anchor_ms: hit.anchor_ms, // F-65 temporal
            })
        })
        .collect();

    // corpus_match_count (opt-in).
    let (corpus_match_count, corpus_count_capped) = if req.include_corpus_count {
        match state
            .search
            .count_fts_matches(
                &read_vault,
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

    // F-110 : télémétrie salience per-note — APRÈS construction des `items`, succès only,
    // best-effort (record() ne panique/propage jamais). +search-hit par note retournée,
    // +search-hit-top3 EN PLUS sur les rangs 1-3 (0-indexés 0..3). `note_id` extrait du
    // path `{section}/{note_id}` (ULID sans slash → `rsplit('/')` fiable). Aucune mutation
    // de la réponse — invariant byte-identique préservé.
    {
        let now_ms = chrono::Utc::now().timestamp_millis();
        for (rank, item) in items.iter().enumerate() {
            if let Some(note_id) = item.path.rsplit('/').next() {
                state
                    .note_usage_accumulators
                    // Télémétrie salience keyée par NAMESPACE (vault lu) — cf. note
                    // conflation Task 11. À OFF `read_vault == tenant == "main"`.
                    .record(read_vault.as_str(), note_id, KIND_SEARCH_HIT, now_ms);
                if rank < 3 {
                    state.note_usage_accumulators.record(
                        read_vault.as_str(),
                        note_id,
                        KIND_SEARCH_HIT_TOP3,
                        now_ms,
                    );
                }
            }
        }
    }

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
/// - `GradatumError::Forbidden` si ACL deny, ou si la note lue est en section
///   `identity` et que l'appelant n'est ni le propriétaire de l'âme, ni l'owner
///   privilégié `main-agent`, ni une session Studio (read-restrictive A2/C6,
///   (since v0.7.3). The guard applies after resolving the note's actual section —
///   addressing by bare ULID does not bypass it.
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
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
                tracing::debug!(title = %req.path, resolved_id = %found_id, "vault_read_impl: title resolved");
                found_id
            }
            Ok(None) => {
                let slug = title_to_slug(&req.path);
                match state.search.resolve_redirect(&tenant, &slug).await {
                    Ok(Some(ulid)) => ulid.to_string(),
                    Ok(None) => {
                        // req.path peut être un titre (pas un ULID) — Storage pour l'intro
                        return Err(GradatumError::Storage(format!("not found: {}", req.path)));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };

    // Task 23 (W3) : read public routé par le vault EFFECTIF (`tenant`) — à OFF singleton
    // `main` byte-identical, à ON handle du registre fail-closed (jamais un repli sur main).
    match read_back_reader(state, &tenant)?
        .read_note_by_id(&resolved_path)
        .await
    {
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
                match state
                    .search
                    .get_titles_sections(
                        &crate::api_v1::tenant_guard::own_vault_checked(&tenant),
                        ids,
                    )
                    .await
                {
                    Ok(map) => map
                        .get(&resolved_path)
                        .and_then(|(t, _)| t.clone())
                        .filter(|s| !s.trim().is_empty()),
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            note_id = %resolved_path,
                            "vault_read_impl: get_titles_sections failed — title=None (best-effort)"
                        );
                        None
                    }
                }
            };
            let title: Option<String> = title.or_else(|| extract_h1_title(&body));

            // Read-restrictive ACL on section `identity` (F-34 v0.7.3, A2/C6) — the
            // symmetric counterpart of the write-restrictive guard in `vault_write_impl`.
            // An agent's soul is private: only its owner (or the privileged owner
            // `main-agent`, SSI) may read it; Studio (admin UI) may read any soul for
            // observability. The check runs AFTER resolution, on the note's REAL section
            // (`note.frontmatter.section`), so addressing by raw ULID (where
            // `req.section` is `None`) cannot bypass it.
            if note.frontmatter.section == Section::Identity {
                // Studio sessions are the admin trust tier — full observability.
                let is_studio_admin = matches!(trust, TrustContext::Studio { .. });
                if !is_studio_admin {
                    // `subject()` returns the JWT `sub` — never derived from a client
                    // parameter. `target_agent` is extracted server-side from the note's
                    // own title (`identity/<agent>`), never from request input.
                    let caller_sub = trust.subject().map(AgentId::as_str).unwrap_or("");
                    let target_agent = title
                        .as_deref()
                        .and_then(|t| t.strip_prefix("identity/"))
                        .unwrap_or("");
                    let allowed = trust.has_scope(IDENTITY_WRITE_SCOPE)
                        || (!target_agent.is_empty() && caller_sub == target_agent);
                    if !allowed {
                        emit_read_rejection_audit(
                            state,
                            trust,
                            &tenant,
                            &locus,
                            "identity_read_denied_foreign_agent",
                            Some(resolved_path.clone()),
                        )
                        .await;
                        return Err(GradatumError::Forbidden(
                            "identity: read restricted to the soul's owner".into(),
                        ));
                    }
                }
            }

            // Statut autoritatif depuis la colonne DB (notes.status), pas le frontmatter :
            // vault_downgrade met à jour la colonne + replaced_by mais JAMAIS le frontmatter
            // YAML, qui reste donc périmé (ex. "live" sur une note réellement "downgraded").
            // Fallback sur frontmatter.status si get_statuses échoue (dégradation gracieuse).
            let note_id_str = note.id.to_string();
            let db_status = state
                .search
                .get_statuses(
                    &crate::api_v1::tenant_guard::own_vault_checked(&tenant),
                    std::slice::from_ref(&note_id_str),
                )
                .await
                .ok()
                .and_then(|mut m| m.remove(&note_id_str));
            let authoritative_status =
                db_status.unwrap_or_else(|| note.frontmatter.status.to_string());

            let resp = VaultReadResponse {
                path: note_id_str,
                title,
                content: body,
                metadata: Some(serde_json::json!({
                    "section": note.frontmatter.section.to_string(),
                    "status": authoritative_status,
                    "author": note.frontmatter.author.as_ref().map(|a| a.id.as_str()),
                    "tags": note.frontmatter.tags.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
                    "vault_id": note.frontmatter.vault_id.as_str(),
                    "created": note.frontmatter.created.timestamp_millis(),
                    "updated": note.frontmatter.updated.map(|d| d.timestamp_millis()),
                })),
                size_bytes,
                sha256,
            };
            // F-110 : télémétrie salience per-note — note effectivement lue, succès only,
            // best-effort. `resp.path` = ULID de la note servie. Aucune mutation de la réponse.
            // Télémétrie salience keyée par NAMESPACE : le vault où vit la note lue
            // (`note.frontmatter.vault_id`), pas le principal. À OFF == "main"
            // (byte-identical). Cf. résolution conflation Task 11 / `arch/01KXWMDDX1`.
            state.note_usage_accumulators.record(
                note.frontmatter.vault_id.as_str(),
                &resp.path,
                KIND_READ,
                chrono::Utc::now().timestamp_millis(),
            );
            Ok(resp)
        }
        Err(GradatumError::NoteNotFound(_)) => {
            let note_id = ulid::Ulid::from_string(&resolved_path)
                .map(NoteId)
                .unwrap_or_else(|_| NoteId::new());
            Err(GradatumError::NoteNotFound(note_id))
        }
        Err(GradatumError::Storage(ref msg)) if msg.contains("invalid ULID") => {
            let note_id = ulid::Ulid::from_string(&resolved_path)
                .map(NoteId)
                .unwrap_or_else(|_| NoteId::new());
            Err(GradatumError::NoteNotFound(note_id))
        }
        // Note « fantôme » (présente dans l'index SQLite mais `.md` absent du disque) :
        // depuis D2, `lifecycle::read_note` remonte un `NoteNotFound` TYPÉ — capté par
        // le bras `Err(GradatumError::NoteNotFound(_))` ci-dessus (→ 404). Plus aucun
        // string-match fragile sur le message d'erreur n'est nécessaire ici.
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let _ = req.pattern; // ignoré (T12 pattern filter différé)
    let limit = req.limit.unwrap_or(20).clamp(1, 200) as usize;

    // F-171 : route vers la variante filtrée UNIQUEMENT si un filtre de rôle est présent —
    // sinon `list_notes` inchangé (rétro-compat octet pour octet, aucun filtre neutre injecté).
    let (records, total) = if req.role_kind.is_some() || req.role_status.is_some() {
        state
            .search
            .list_notes_filtered(
                &tenant,
                req.section.as_deref(),
                req.role_kind.as_deref(),
                req.role_status.as_deref(),
                limit,
                req.cursor.as_deref(),
            )
            .await?
    } else {
        state
            .search
            .list_notes(
                &tenant,
                req.section.as_deref(),
                limit,
                req.cursor.as_deref(),
            )
            .await?
    };

    // Guard identity (parité `vault_search`/`vault_context`) : un appelant non
    // privilégié ne doit pas découvrir l'existence/ULID/mtime des âmes d'agents via le
    // listing (vecteur d'énumération). Exclusion simple sur la section RÉELLE de chaque
    // record (fan-out RAG, pas de matching par-agent). No-op pour Studio / main-agent.
    let identity_privileged = is_identity_privileged(trust);

    // `next_cursor` est calculé sur le nombre BRUT de records ramenés (avant filtre) pour
    // que la pagination continue d'avancer même si une page entière est masquée.
    let next_cursor = if records.len() == limit {
        records.last().map(|r| r.id.clone())
    } else {
        None
    };

    let entries: Vec<VaultEntry> = records
        .into_iter()
        .filter(|r| !identity_section_hidden(identity_privileged, &r.section))
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

/// Logique métier de `POST /api/v1/vault_archives_list` — LECTURE SEULE (F-100 1.6).
///
/// ACL Read. Liste le registre d'archives (filtres section/temps/gc/restored +
/// pagination). **Aucune mutation** : delete/restore/purge ne sont PAS exposés ici
/// (namespace interne loopback uniquement — invariant fondateur F-100). Partagée par le
/// handler HTTP public et l'outil MCP `vault_archives_list`.
///
/// ## Isolation multi-vault (C3a, clôture gate flag-ON C2 — P1-1)
///
/// `vault_filter` est le **vault CIBLE** du listing — intégré au modèle d'isolation,
/// parité `vault_search`/`vault_timeline` :
/// - `multi_tenant.enabled = true` → `effective_read_vault` (ACL recalculée sur la
///   CIBLE + grant read + cible active, fail-closed) ;
/// - OFF → ACL appelant historique + garde mono-vault (`vault_filter ≠ "main"` → 403).
///
/// Sur les DEUX chemins, le filtre registre est ÉPINGLÉ au vault vérifié : `None` ne
/// signifie plus « tous vaults » (le scan global reste réservé à l'endpoint admin
/// interne via [`list_archives_core`]) mais « le vault propre du tenant ».
///
/// # Errors
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si le tenant du body diverge, l'ACL Read refuse,
///   ou la cible n'est pas accessible (grant absent / cible non active / mono-vault).
/// - `GradatumError::InvalidInput` si `vault_filter` est mal formé (400).
/// - `GradatumError::Storage` sur échec de requête registre.
pub async fn vault_archives_list_impl(
    state: &AppState,
    trust: &TrustContext,
    mut req: gradatum_dto::VaultArchivesListRequest,
) -> Result<gradatum_dto::VaultArchivesListResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let checked: gradatum_core::scope::AclCheckedVaultId =
        if state.server_config.multi_tenant.enabled {
            // EX-C2-1 : ACL + grant + statut évalués sur la CIBLE (`vault_filter`),
            // plus jamais sur le seul locus de l'appelant.
            // `vault_filter: Option<String>` (non typé par Task 7 — filtre de listing) :
            // matérialisé en `VaultId` local pour la frontière typée `effective_read_vault`.
            // Le parse validant reste interne au choke-point → 400 byte-identical.
            let vault_filter_id = req.vault_filter.as_ref().map(|v| VaultId::new(v.as_str()));
            effective_read_vault(
                state,
                trust,
                &tenant,
                vault_filter_id.as_ref(),
                None,
                "acl deny",
            )
            .await?
        } else {
            let locus = locus_for_tenant(&tenant);
            if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
                return Err(GradatumError::Forbidden("acl deny".into()));
            }
            // Garde mono-vault (parité search/timeline) : cross-listing interdit à OFF.
            if let Some(vf) = req.vault_filter.as_deref() {
                if vf.is_empty() || vf.len() > 128 {
                    return Err(GradatumError::InvalidInput("invalid vault_filter".into()));
                }
                if vf != "main" {
                    return Err(GradatumError::Forbidden(
                        "cross-read vault_filter ≠ main not supported (mono-vault)".into(),
                    ));
                }
            }
            crate::api_v1::tenant_guard::own_vault_checked(&tenant)
        };
    // Épingle le filtre au vault vérifié — fail-closed : aucun chemin public ne peut
    // atteindre le registre sans cible contrôlée (le scan tous-vaults n'existe que
    // sur le chemin admin interne, qui appelle `list_archives_core` directement).
    req.vault_filter = Some(checked.as_str().to_owned());
    list_archives_core(state, req).await
}

/// Cœur du listing d'archives (filtres → registre → DTO), SANS auth ni ACL.
///
/// Partagé par [`vault_archives_list_impl`] (public/MCP, après auth+ACL Read) et par
/// l'endpoint admin interne (loopback + token admin, ACL bypassée). Zéro duplication du
/// mapping registre → DTO.
///
/// # Errors
///
/// - `GradatumError::Storage` sur échec de requête registre.
pub async fn list_archives_core(
    state: &AppState,
    req: gradatum_dto::VaultArchivesListRequest,
) -> Result<gradatum_dto::VaultArchivesListResponse, GradatumError> {
    let limit = req.limit.min(gradatum_index::ARCHIVE_LIST_MAX);
    let filter = gradatum_index::ArchiveListFilter {
        vault_id: req.vault_filter.clone(),
        section: req.section.clone(),
        from_ms: req.since_ms,
        until_ms: req.until_ms,
        include_gc: req.include_gc,
        include_restored: req.include_restored,
        limit,
        offset: req.offset,
    };

    let entries = state.vault.list_archives(&filter).await?;
    let dtos: Vec<gradatum_dto::ArchiveEntryDto> =
        entries.into_iter().map(archive_entry_to_dto).collect();
    let count = dtos.len();
    Ok(gradatum_dto::VaultArchivesListResponse {
        entries: dtos,
        limit,
        offset: req.offset,
        count,
    })
}

/// Mappe une [`gradatum_index::ArchiveEntry`] vers son DTO filaire.
///
/// SSOT du mapping registre → DTO (consommé par le listing ET la purge admin).
pub(crate) fn archive_entry_to_dto(
    e: gradatum_index::ArchiveEntry,
) -> gradatum_dto::ArchiveEntryDto {
    gradatum_dto::ArchiveEntryDto {
        note_id: e.note_id,
        // Frontière index→DTO : `ArchiveEntry.vault_id: String` → `VaultId` (newtype
        // transparent — même chaîne filaire, byte-identical).
        vault_id: e.vault_id.into(),
        section: e.section,
        title: e.title,
        original_locus: e.original_locus,
        archive_path: e.archive_path,
        archived_at: e.archived_at,
        archived_by: e.archived_by,
        gc_due: e.gc_due,
        gc_at: e.gc_at,
        restored_at: e.restored_at,
    }
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
    // A3-vault_status (T13) : les 4 hardcodes `"main"` (locus ACL, live_note_count,
    // total_body_size_bytes, champ tenant_id) sont routés sur le tenant appelant, GATÉS sur
    // le flag. `effective_tenant` (byte-identical à OFF : retourne le principal, AUCUN grant)
    // refuse les contextes sans tenant (Studio/Mtls) et n'a PAS de court-circuit OFF ;
    // l'appliquer inconditionnellement casserait le byte-identical OFF — `vault_status` est
    // atteignable par TOUT contexte authentifié. On gate donc comme T9/T12 :
    // - OFF (défaut LIVE) : hardcode `"main"` INCHANGÉ (mono-vault, principal == main).
    // - ON : métadonnées scopées au principal JWT via `effective_tenant`.
    let tenant: String = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(GradatumError::Forbidden("acl deny".into()));
        };
        effective_tenant(trust, Some(principal))
            .map_err(|_| GradatumError::Forbidden("acl deny".into()))?
            .to_owned()
    } else {
        "main".to_owned()
    };
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let note_count = state.search.live_note_count(&tenant).await.unwrap_or(0);
    let total_size_bytes = state
        .search
        .total_body_size_bytes(&tenant)
        .await
        .unwrap_or(0);
    // Fraîcheur de l'index (F-169) : horodatage de la note live la plus récemment
    // indexée (`MAX(COALESCE(updated, created))`). L'erreur est PROPAGÉE plutôt que
    // dégradée en `null` — un `null` sur échec rejouerait exactement le faux « jamais
    // indexé » que ce correctif supprime. `None` n'apparaît que si le corpus live est
    // vide. Format ISO 8601 UTC aligné sur `vault_list.modified_at`.
    let last_indexed_at = state.search.last_indexed_at(&tenant).await?.and_then(|ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    });
    Ok(VaultStatusResponse {
        tenant_id: tenant,
        note_count,
        total_size_bytes,
        index_version: "v1".to_string(),
        last_indexed_at,
        health: "healthy".to_string(),
    })
}

/// Filtre les nœuds/arêtes de graphe pointant vers une âme (`section = identity`) d'un
/// **autre** agent, pour un appelant **non privilégié** — parité [`identity_section_hidden`]
/// avec les surfaces RAG (`vault_search`, `vault_list`, `vault_context`).
///
/// Sans ce filtre, `vault_graph`/`vault_links` révélaient l'existence + l'ULID d'une âme
/// cross-agent via un nœud ou une arête wikilink, alors que `vault_read`/`vault_search`
/// la masquent déjà (F-1, énumération résiduelle).
///
/// **No-op strict** pour les appelants privilégiés (Studio / `main-agent` / owner) : la
/// résolution des sections est court-circuitée (zéro appel index). Pour les non-privilégiés,
/// une **seule** requête batch `get_titles_sections` résout la section de tous les nœuds —
/// volume borné (`neighbors` cape à `depth ≤ 3`, `backlinks` = liens entrants directs).
/// Seuls les nœuds de section **exactement** `identity` (ou indéterminée — fail-closed, cf.
/// [`identity_section_hidden`]) sont retirés ; tout nœud d'une autre section, ou absent de
/// l'index (lien pendant), est **préservé** (le graphe normal n'est pas cassé).
///
/// # Erreurs
///
/// Propage [`GradatumError::Storage`] si la résolution des sections échoue — fail-closed :
/// on refuse la requête plutôt que de renvoyer un graphe non filtré (risque de fuite).
async fn filter_identity_nodes(
    state: &AppState,
    trust: &TrustContext,
    tenant: &str,
    nodes: &mut Vec<String>,
    edges: &mut Vec<GraphEdge>,
) -> Result<(), GradatumError> {
    let identity_privileged = is_identity_privileged(trust);
    // Privilégié → observabilité totale : aucun filtre, aucun appel index.
    if identity_privileged || nodes.is_empty() {
        return Ok(());
    }

    // Section RÉELLE de chaque nœud résolue server-side (jamais depuis l'input).
    let sections = state
        .search
        .get_titles_sections(
            &crate::api_v1::tenant_guard::own_vault_checked(tenant),
            nodes.as_slice(),
        )
        .await?;
    let hidden: std::collections::HashSet<String> = nodes
        .iter()
        .filter(|id| {
            sections
                .get(*id)
                .is_some_and(|(_, section)| identity_section_hidden(identity_privileged, section))
        })
        .cloned()
        .collect();
    if hidden.is_empty() {
        return Ok(());
    }
    edges.retain(|e| !hidden.contains(&e.from) && !hidden.contains(&e.to));
    nodes.retain(|id| !hidden.contains(id));
    Ok(())
}

/// Normalise une référence de note vers son ULID **nu**.
///
/// `note_links` (et donc `neighbors`/`backlinks`) porte des identifiants ULID **nus**,
/// mais le reste de l'API expose la forme préfixée `section/ULID` : `vault_read`
/// l'accepte, `vault_search` la renvoie dans son champ `path`. Un appelant qui enchaîne
/// `vault_search` → `vault_links`/`vault_graph` passe donc naturellement `section/ULID` ;
/// sans résolution, la comparaison `=` en base échoue silencieusement et rend un graphe
/// **vide** (F-215 — un `edges: []` muet pris pour « pas de liens »).
///
/// La résolution reprend **intégralement** celle de [`vault_read_impl`] : ULID si la
/// queue en est un (dernier segment `/`-séparé — ni un ULID ni un nom de section ne
/// contiennent de `/`), sinon repli par titre (`title_lookup`), puis par slug de
/// redirection (`resolve_redirect`).
///
/// **Contrat v1 préservé** : une référence qui ne résout vers **aucune** note n'est PAS
/// une erreur. Le graphe d'un nœud inconnu est un graphe vide (200), pas un 400 — on
/// renvoie alors le dernier segment tel quel, qui ne matchera aucun lien. C'est la seule
/// moitié de `vault_read` qu'on n'imite pas : là où lui rend `NoteNotFound`, la lecture
/// d'un graphe reste un 200 vide (décision d'API v1 codifiée par les tests de parité).
///
/// # Errors
///
/// Propage [`GradatumError::Storage`] si l'index échoue (`title_lookup`/`resolve_redirect`).
async fn resolve_note_ref(
    state: &AppState,
    tenant: &str,
    reference: &str,
) -> Result<String, GradatumError> {
    let candidate = reference.rsplit('/').next().unwrap_or(reference);
    // Référence inconnue → graphe vide (contrat v1), jamais une erreur.
    Ok(resolve_note_ref_opt(state, tenant, reference)
        .await?
        .unwrap_or_else(|| candidate.to_string()))
}

/// Cœur de résolution partagé par [`resolve_note_ref`] (contrat v1, tolérant) et
/// [`resolve_note_ref_strict`] (contrat strict, refus nommé).
///
/// Rend `Some(ULID nu)` si la référence désigne une note atteignable — ULID nu, forme
/// préfixée `section/ULID`, titre exact, ou slug de redirection — et `None` si aucune
/// des trois voies n'aboutit. **Ne décide pas** ce qu'il faut faire d'un `None` : c'est
/// précisément le point où les deux contrats divergent.
///
/// # Errors
///
/// Propage [`GradatumError::Storage`] si l'index échoue (`title_lookup`/`resolve_redirect`).
async fn resolve_note_ref_opt(
    state: &AppState,
    tenant: &str,
    reference: &str,
) -> Result<Option<String>, GradatumError> {
    let candidate = reference.rsplit('/').next().unwrap_or(reference);
    if ulid::Ulid::from_string(candidate).is_ok() {
        return Ok(Some(candidate.to_string()));
    }
    if let Some(found_id) = state.search.title_lookup(tenant, reference).await? {
        return Ok(Some(found_id));
    }
    let slug = title_to_slug(reference);
    if let Some(ulid) = state.search.resolve_redirect(tenant, &slug).await? {
        return Ok(Some(ulid.to_string()));
    }
    Ok(None)
}

/// Variante **stricte** de [`resolve_note_ref`] : une référence irrésoluble est une
/// erreur d'entrée **nommée**, pas un repli silencieux.
///
/// Destinée aux surfaces où le contrat v1 « référence inconnue → 200 vide » ne
/// s'applique PAS — le sous-système d'historique CoW (`vault_history`,
/// `vault_history_get`, `vault_restore`, `vault_diff`). Avant F-215 critère 4, ces
/// quatre outils passaient la référence brute à la couche Vault, dont `parse_note_id`
/// rendait un [`GradatumError::Storage`] (« invalid ULID … invalid length ») → **500
/// opaque** côté appelant : un refus déguisé en panne interne, classé erreur de
/// *stockage* alors que la cause est une *entrée* invalide.
///
/// Accepte donc exactement ce qu'accepte `vault_read` (parité), et refuse le reste avec
/// un [`GradatumError::InvalidInput`] (→ 400) citant la valeur reçue et les formes
/// attendues.
///
/// # Errors
///
/// - [`GradatumError::InvalidInput`] si la référence ne résout vers aucune note.
/// - Propage [`GradatumError::Storage`] si l'index échoue.
async fn resolve_note_ref_strict(
    state: &AppState,
    tenant: &str,
    reference: &str,
) -> Result<String, GradatumError> {
    resolve_note_ref_opt(state, tenant, reference)
        .await?
        .ok_or_else(|| {
            GradatumError::InvalidInput(format!(
                "unresolvable note reference {} — expected a bare ULID, \
                 a prefixed \"section/ULID\", an exact note title, or a redirect slug",
                echo_ref(reference)
            ))
        })
}

/// Longueur maximale (en caractères) d'une référence réfléchie dans un message d'erreur.
///
/// Les champs `note_id` ne sont bornés par aucun schéma : sans troncature, une référence
/// de plusieurs kilo-octets serait recopiée telle quelle dans la réponse **et** dans les
/// journaux. Safety cap (ADN 5), pas un paramètre utilisateur.
const MAX_REF_ECHO_CHARS: usize = 96;

/// Rend une référence **fournie par l'appelant** sûre à citer dans un message d'erreur.
///
/// Tronque à [`MAX_REF_ECHO_CHARS`] sur une frontière de caractère (jamais d'octet, pour
/// ne pas produire d'UTF-8 invalide) et cite la valeur entre guillemets via `{:?}`, ce qui
/// neutralise aussi les caractères de contrôle dans les journaux.
///
/// Portée volontairement étroite : on ne réfléchit **que** l'entrée de l'appelant, jamais
/// une chaîne d'origine serveur — c'est la distinction que fait déjà
/// `jobs::sanitize_job_error`, qui opacifie les erreurs remontées d'un *worker* (chemins
/// FS absolus, état interne). Rendre à un client la valeur qu'il vient d'envoyer ne lui
/// apprend rien qu'il ne sache déjà.
fn echo_ref(reference: &str) -> String {
    let mut chars = reference.chars();
    let head: String = chars.by_ref().take(MAX_REF_ECHO_CHARS).collect();
    if chars.next().is_some() {
        format!("{head:?}(truncated)")
    } else {
        format!("{head:?}")
    }
}

/// Logique métier de `POST /api/v1/vault_graph`.
///
/// ACL Read + neighbors + backlinks optionnels.
///
/// `root` accepte l'ULID nu, la forme préfixée `section/ULID`, un titre ou un slug
/// (parité `vault_read` — cf. `resolve_note_ref`) ; un nœud inconnu rend un graphe
/// vide (200), pas une erreur (F-215).
pub async fn vault_graph_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultGraphRequest,
) -> Result<VaultGraphResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // F-215 : résout ULID / `section/ULID` / titre / slug (parité vault_read).
    // Nœud inconnu → graphe vide (contrat v1), jamais un rejet.
    let root = resolve_note_ref(state, &tenant, &req.root).await?;

    let raw_depth = req.depth.unwrap_or(2);
    if raw_depth > 5 {
        return Err(GradatumError::InvalidInput(
            "depth > 5 rejected (effective max = 3)".into(),
        ));
    }
    let depth = raw_depth.min(3) as u8;

    let neighbors = state.search.neighbors(&tenant, &root, depth).await?;

    let mut edges: Vec<GraphEdge> = neighbors
        .iter()
        .map(|n| GraphEdge {
            from: root.clone(),
            to: n.clone(),
            kind: "wikilink".to_string(),
        })
        .collect();

    let mut nodes: Vec<String> = neighbors;
    nodes.push(root.clone());
    nodes.sort();
    nodes.dedup();

    if req.include_backlinks.unwrap_or(false) {
        let backlinks = state.search.backlinks(&tenant, &root).await?;
        for bl in &backlinks {
            edges.push(GraphEdge {
                from: bl.clone(),
                to: root.clone(),
                kind: "wikilink".to_string(),
            });
            if !nodes.contains(bl) {
                nodes.push(bl.clone());
            }
        }
        nodes.sort();
        nodes.dedup();
    }

    // F-1 : masquer les âmes cross-agent aux non-privilégiés (parité vault_search/list).
    filter_identity_nodes(state, trust, &tenant, &mut nodes, &mut edges).await?;

    Ok(VaultGraphResponse { nodes, edges })
}

/// Logique métier de `POST /api/v1/vault_links`.
///
/// ACL Read + liens sortants (depth=1) + backlinks.
///
/// `path` accepte l'ULID nu, la forme préfixée `section/ULID`, un titre ou un slug
/// (parité `vault_read` — cf. `resolve_note_ref`) ; un nœud inconnu rend un graphe
/// vide (200), pas une erreur (F-215).
pub async fn vault_links_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultLinksRequest,
) -> Result<VaultLinksResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // F-215 : résout ULID / `section/ULID` / titre / slug (parité vault_read).
    // Nœud inconnu → graphe vide (contrat v1), jamais un rejet.
    let path = resolve_note_ref(state, &tenant, &req.path).await?;

    let outbound = state.search.neighbors(&tenant, &path, 1).await?;
    let mut edges: Vec<GraphEdge> = outbound
        .iter()
        .map(|n| GraphEdge {
            from: path.clone(),
            to: n.clone(),
            kind: "wikilink".to_string(),
        })
        .collect();

    let mut nodes: Vec<String> = outbound;
    nodes.push(path.clone());

    if req.include_backlinks.unwrap_or(true) {
        let backlinks = state.search.backlinks(&tenant, &path).await?;
        for bl in &backlinks {
            edges.push(GraphEdge {
                from: bl.clone(),
                to: path.clone(),
                kind: "wikilink".to_string(),
            });
            if !nodes.contains(bl) {
                nodes.push(bl.clone());
            }
        }
    }

    nodes.sort();
    nodes.dedup();

    // F-1 : masquer les âmes cross-agent aux non-privilégiés (parité vault_search/list).
    filter_identity_nodes(state, trust, &tenant, &mut nodes, &mut edges).await?;

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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
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
                tracing::debug!(title = %req.query, id = %note_id, "vault_trace_impl: title resolved");
                vec![note_id]
            }
            Ok(None) => {
                let fts_q = build_fts_query(&req.query);
                if fts_q.trim_matches(['"', ' ']).is_empty() {
                    return Ok(VaultTraceResponse { entries: vec![] });
                }
                let vault_id = crate::api_v1::tenant_guard::own_vault_checked(&tenant);
                let fts_limit = limit.min(5);
                match state
                    .search
                    .search_fts_with_snippet(
                        &vault_id, &fts_q, fts_limit, false, None, None, None, None, None,
                    )
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

    // F-1 : guard identité par-agent sur le/les seed(s) — parité `vault_read_impl` /
    // `vault_history_impl`. Un seed peut être une âme cross-agent (résolue par
    // `title_lookup` sur `query="identity/<agent>"`, par ULID nu, ou par FTS). Sans ce
    // guard, un non-privilégié confirmait l'existence + le lignage complet d'une âme
    // étrangère. Section RÉELLE résolue server-side ; refus 403 fail-closed (parité stricte,
    // sentinelle `identity` sur erreur d'index). No-op pour seed non-`identity` et appelant
    // privilégié. Volume borné (≤ 5 seeds : 1 ULID/titre, ou `fts_limit = limit.min(5)`).
    for seed_id in &resolved_seeds {
        let (title, section) = resolve_title_section_failclosed(state, &tenant, seed_id).await;
        enforce_identity_read_guard(state, trust, &tenant, &section, title.as_deref(), seed_id)
            .await?;
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
/// ACL Read + dispatch vers le module [`crate::context`] qui orchestre l'assemblage.
///
/// # Sécurité
///
/// - Authentification vérifiée avant tout accès aux données.
/// - Tenant validé via `effective_tenant` (cross-tenant interdit).
/// - ACL Read sur le locus section évalué avant dispatch.
///
/// # Délégation
///
/// La logique d'assemblage (résolution candidats, accumulation, rendu) vit dans
/// [`crate::context::assemble_context`] — `vault_context_impl` ne conserve que
/// les gardes de sécurité / ACL.
pub async fn vault_context_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultContextRequest,
) -> Result<VaultContextResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_section(&tenant, req.section.as_deref());
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // Guard identity F-34 (parité `vault_search_impl`, audit security-reviewer v0.7.3) :
    // le contexte assemblé est une surface RAG générique — les âmes d'agents ne doivent
    // pas devenir candidates (fuite de corps dans `assembled_text`) sauf caller privilégié.
    // Le privilège est résolu ICI (accès à `trust`) puis propagé aux assembleurs.
    let identity_privileged = is_identity_privileged(trust);
    crate::context::assemble_context(state, &tenant, &req, identity_privileged).await
}

/// Logique métier de `GET /api/v1/vault_authors`.
///
/// ACL Read (vault effectif) + distinct_authors.
///
/// # Sécurité (P0-8, famille cross-tenant)
///
/// Le vault est résolu depuis le JWT (`effective_tenant`), GATÉ sur
/// `multi_tenant.enabled` — sibling de lecture own-vault (parité stricte avec
/// [`vault_forgotten_list`](crate::api_v1::forget::vault_forgotten_list) et
/// `vault_status_impl`). À OFF le chemin `"main"` est INCHANGÉ (byte-identical). Sans ce
/// gate le vault était figé à `"main"` : un tenant ≠ main porteur d'un grant ACL couvrant
/// `main/*` listait les auteurs (identité, PII-adjacent) des notes de `main` → fuite
/// cross-tenant. À ON, un contexte sans tenant (Studio/Mtls) est refusé 403.
pub async fn vault_authors_impl(
    state: &AppState,
    trust: &TrustContext,
) -> Result<VaultAuthorsResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let vault_id: String = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(GradatumError::Forbidden("no tenant in context".into()));
        };
        effective_tenant(trust, Some(principal))
            .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
            .to_owned()
    } else {
        "main".to_owned()
    };
    let locus = locus_for_tenant(&vault_id);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }
    let rows = state.search.distinct_authors(&vault_id).await?;
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
/// ACL Read (vault effectif) + distinct_tags.
///
/// # Sécurité (P0-8, famille cross-tenant)
///
/// Même patron que [`vault_authors_impl`] : vault résolu depuis le JWT
/// (`effective_tenant`), GATÉ sur `multi_tenant.enabled`. À OFF `"main"` INCHANGÉ. Sans
/// ce gate un tenant ≠ main porteur d'un grant ACL couvrant `main/*` listait les tags
/// (topologie/PII-adjacent) des notes de `main` → fuite cross-tenant. À ON, un contexte
/// sans tenant est refusé 403.
/// Borne par défaut du nombre de tags renvoyés par `vault_tags` quand l'appelant
/// ne fournit pas de `limit` explicite.
///
/// Safety cap anti-DoS de contexte (F-216) : sans borne, `vault_tags` renvoyait la
/// liste complète (~135 Ko observés), saturant le budget de contexte des appelants
/// agents. Les tags sont triés fréquence décroissante (`distinct_tags`), donc les
/// `DEFAULT_TAGS_LIMIT` premiers sont les plus utiles ; l'appelant lève la borne
/// via `limit` et détecte la troncature via le champ `total` de la réponse.
const DEFAULT_TAGS_LIMIT: usize = 50;

pub async fn vault_tags_impl(
    state: &AppState,
    trust: &TrustContext,
    req: VaultTagsRequest,
) -> Result<VaultTagsResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }
    let vault_id: String = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(GradatumError::Forbidden("no tenant in context".into()));
        };
        effective_tenant(trust, Some(principal))
            .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
            .to_owned()
    } else {
        "main".to_owned()
    };
    let locus = locus_for_tenant(&vault_id);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }
    // `distinct_tags` renvoie déjà les tags triés fréquence décroissante (puis
    // alpha) — `total` = cardinal complet AVANT bornage (détection de troncature).
    let rows = state.search.distinct_tags(&vault_id).await?;
    let total = rows.len() as u64;
    let limit = req.limit.unwrap_or(DEFAULT_TAGS_LIMIT);
    let tags = rows
        .into_iter()
        .take(limit)
        .map(|(tag, count)| TagEntry {
            tag,
            note_count: count,
        })
        .collect();
    Ok(VaultTagsResponse { tags, total })
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();

    // EX-C2-1 : à ON, ACL recalculée sur la CIBLE (locus `{cible}/timeline`) + grant read.
    // À OFF, chemin legacy byte-identical (ACL appelant ici, garde mono-vault plus bas).
    let checked_on: Option<gradatum_core::scope::AclCheckedVaultId> =
        if state.server_config.multi_tenant.enabled {
            Some(
                effective_read_vault(
                    state,
                    trust,
                    &tenant,
                    req.vault_id.as_ref(),
                    Some("timeline"),
                    "acl deny timeline",
                )
                .await?,
            )
        } else {
            let acl_locus = format!("{}/timeline", tenant);
            if state.acl.evaluate(trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
                return Err(GradatumError::Forbidden("acl deny timeline".into()));
            }
            None
        };

    // Validation.
    if let (Some(f), Some(t)) = (req.from_ms, req.to_ms)
        && f > t
    {
        return Err(GradatumError::InvalidInput("from_ms > to_ms".into()));
    }
    if let Some(kinds) = req.doc_kind.as_ref() {
        if kinds.len() > KNOWN_DOC_KINDS.len() {
            return Err(GradatumError::InvalidInput("too many doc_kind".into()));
        }
        if kinds.iter().any(|k| !KNOWN_DOC_KINDS.contains(&k.as_str())) {
            return Err(GradatumError::InvalidInput(
                "doc_kind hors allowlist".into(),
            ));
        }
    }
    // Garde mono-vault legacy (OFF uniquement — à ON la cible est déjà vérifiée).
    if checked_on.is_none()
        && let Some(v) = req.vault_id.as_ref()
    {
        if v.as_str().is_empty() || v.as_str().len() > 128 {
            return Err(GradatumError::InvalidInput("invalid vault_id".into()));
        }
        if v.as_str() != "main" {
            return Err(GradatumError::Forbidden(
                "cross-read vault_id ≠ main not supported".into(),
            ));
        }
    }
    let cursor = match req.cursor.as_deref() {
        Some(s) => Some(
            TimelineCursor::decode(s)
                .map_err(|_| GradatumError::InvalidInput("malformed cursor".into()))?,
        ),
        None => None,
    };

    let limit = req.limit.unwrap_or(50).clamp(1, 200) as usize;
    // Témoin EX-C2-2 : à OFF la cible est "main" == tenant (ACL évaluée ci-dessus).
    let vault = match checked_on {
        Some(checked) => checked,
        None => gradatum_core::scope::AclCheckedVaultId::attest_read_checked(
            req.vault_id.unwrap_or_else(|| VaultId::new(tenant.clone())),
        ),
    };
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

    // ── Guard identité — DÉJÀ ASSURÉ AU NIVEAU SQL (pas de filtre serveur redondant) ──
    // La requête `SqliteIndex::timeline` (sqlite.rs) exclut `n.section NOT IN
    // (Section::PROTECTED_FORGET)`. `Section::PROTECTED_FORGET` inclut `Identity` → aucune
    // note `identity/<agent>` n'entre jamais dans `rows`, pour AUCUN appelant (garde plus
    // forte que le filtre par-privilège de `vault_list` : blackout total, source unique
    // `Section::PROTECTED_FORGET`, testée par `timeline_excludes_protected_sections`).
    // Un filtre serveur `identity_section_hidden` ici serait une 2ᵉ source de vérité + une
    // requête `get_titles_sections` superflue par appel → non ajouté (dev-code-economy).
    // Régression verrouillée côté serveur par `vault_timeline_excludes_identity_*`.

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

/// Nom canonique de la section réservée aux âmes d'agents (`identity/*`).
///
/// Source de vérité partagée par toutes les surfaces RAG génériques (search,
/// context, proactive-recall) for the identity exclusion guard.
pub(crate) const IDENTITY_SECTION: &str = "identity";

/// Vrai si l'appelant est autorisé à voir les notes de section `identity`
/// (âmes d'agents) sur les **surfaces RAG génériques** (search, context, proactive).
///
/// Parité avec le guard `vault_search_impl` (audit security-reviewer v0.7.3) :
/// l'exclusion est *simple*, sans matching par-agent — une surface RAG générique
/// ne cible pas un agent unique, contrairement à `vault_read_impl` (adressage
/// nominal `identity/<agent>`). Callers privilégiés :
///   - session Studio (admin UI, observabilité totale) ;
///   - porteur du scope [`IDENTITY_WRITE_SCOPE`] (`identity_write`).
///
/// Ce prédicat NE remplace PAS le guard read-restrictive par-agent de
/// `vault_read_impl` — il couvre uniquement les surfaces où aucun agent-cible
/// n'est adressable (fan-out RRF).
#[must_use]
pub(crate) fn is_identity_privileged(trust: &TrustContext) -> bool {
    matches!(trust, TrustContext::Studio { .. }) || trust.has_scope(IDENTITY_WRITE_SCOPE)
}

/// Vrai si une note de section `section` doit être **masquée** à cet appelant sur
/// a generic RAG surface (FAIL-CLOSED identity guard).
///
/// FAIL-CLOSED : la section vide (indéterminée — ex. soft-fail `get_titles_sections`)
/// est masquée par précaution (confidentialité > complétude), strictement aligné
/// sur le guard `vault_search_impl`. En fonctionnement nominal la section est
/// toujours peuplée → 0 impact sur les sections non-`identity`.
#[must_use]
pub(crate) fn identity_section_hidden(identity_privileged: bool, section: &str) -> bool {
    !identity_privileged && (section == IDENTITY_SECTION || section.is_empty())
}

/// Applique le guard read-restrictive **par-agent** de la section `identity` sur une
/// note déjà résolue — frère jumeau du guard inline de [`vault_read_impl`].
///
/// Utilisé par les surfaces qui exposent le corps/section/timeline d'une note ciblée
/// par ULID **sans** passer par [`vault_read_impl`] : l'historique CoW
/// (`vault_history`, `vault_history_get`). L'âme d'un agent est privée : seuls son
/// propriétaire, un porteur du scope [`IDENTITY_WRITE_SCOPE`] (`identity_write`) ou une
/// session Studio (observabilité admin) peuvent la lire.
///
/// `section` est la section **RÉELLE** de la note, résolue server-side (jamais depuis
/// l'input). `note_title` sert à dériver le `target_agent` du titre `identity/<agent>`
/// (server-side également). En cas de refus, un audit `vault_read_rejected` est émis.
///
/// **Parité stricte avec [`vault_read_impl`]** : le guard ne se déclenche QUE lorsque la
/// section résolue vaut exactement `identity` — **no-op strict** pour toute autre section
/// (y compris vide/indéterminée). C'est une surface d'accès *ciblée par ULID* (comme
/// `vault_read`), pas un fan-out RAG : la doctrine FAIL-CLOSED-sur-section-vide de
/// [`identity_section_hidden`] ne s'y applique donc pas. Un appelant qui veut fail-closed
/// sur une résolution *en erreur* passe une sentinelle `section = "identity"`.
///
/// # Erreurs
///
/// - [`GradatumError::Forbidden`] si l'appelant non privilégié n'est pas le
///   propriétaire de l'âme ciblée.
async fn enforce_identity_read_guard(
    state: &AppState,
    trust: &TrustContext,
    tenant: &str,
    section: &str,
    note_title: Option<&str>,
    note_id: &str,
) -> Result<(), GradatumError> {
    // No-op strict hors identity (parité exacte vault_read_impl).
    if section != IDENTITY_SECTION {
        return Ok(());
    }
    // Session Studio = palier de confiance admin — observabilité totale.
    if matches!(trust, TrustContext::Studio { .. }) {
        return Ok(());
    }
    // `subject()` = `sub` du JWT (jamais dérivé d'un paramètre client). `target_agent`
    // est extrait server-side du titre `identity/<agent>`, jamais de l'input requête.
    let caller_sub = trust.subject().map(AgentId::as_str).unwrap_or("");
    let target_agent = note_title
        .and_then(|t| t.strip_prefix("identity/"))
        .unwrap_or("");
    let allowed = trust.has_scope(IDENTITY_WRITE_SCOPE)
        || (!target_agent.is_empty() && caller_sub == target_agent);
    if allowed {
        return Ok(());
    }
    let locus = format!("{tenant}/main");
    emit_read_rejection_audit(
        state,
        trust,
        tenant,
        &locus,
        "identity_read_denied_foreign_agent",
        Some(note_id.to_string()),
    )
    .await;
    Err(GradatumError::Forbidden(
        "identity: read restricted to the soul's owner".into(),
    ))
}

/// Résout `(title, section)` **réels** d'une note par ULID depuis l'index, avec la
/// **calibration fail-closed identity** partagée par tous les guards ciblés-par-ULID
/// (`vault_history`, `vault_diff`, `vault_restore`, `vault_downgrade`, `patch_note`,
/// `move_note_locus`).
///
/// `title`/`section` sont TOUJOURS résolus server-side (colonne `notes.title` = titre H1
/// `identity/<agent>`, colonne `notes.section`), jamais dérivés d'un paramètre client.
///
/// Calibration (identique à l'inline historique de `vault_history_impl`) :
/// - note présente          → `(title, section)` réels ;
/// - note absente (map vide) → `(None, "")` → **no-op** des guards (préserve le
///   comportement nominal aval : 404 / réponse vide) ;
/// - erreur d'index         → `(None, "identity")` **sentinelle** → FAIL-CLOSED
///   (le guard refuse au non-privilégié, laisse passer le privilégié).
async fn resolve_title_section_failclosed(
    state: &AppState,
    tenant: &str,
    note_id: &str,
) -> (Option<String>, String) {
    let ids = [note_id.to_owned()];
    match state
        .search
        .get_titles_sections(
            &crate::api_v1::tenant_guard::own_vault_checked(tenant),
            &ids,
        )
        .await
    {
        Ok(mut map) => map.remove(note_id).unwrap_or((None, String::new())),
        Err(e) => {
            tracing::warn!(
                err = %e,
                note_id = %note_id,
                "resolve_title_section_failclosed: get_titles_sections failed — FAIL-CLOSED identity guard"
            );
            (None, IDENTITY_SECTION.to_string())
        }
    }
}

/// Applique le guard **write-restrictive par-agent** de la section `identity` sur une
/// note déjà résolue — frère jumeau WRITE du guard inline de [`vault_write_impl`] (C6).
///
/// Utilisé par les surfaces de **mutation** qui ciblent une note par ULID **sans** passer
/// par [`vault_write_impl`] : restauration CoW (`vault_restore`), déclassement
/// (`vault_downgrade`), patch de statut (`patch_note`), relocalisation physique
/// (`move_note_locus`). Sans ce guard, un appelant non privilégié disposant de l'ACL Write
/// (`write_patterns=["**"]`) pourrait restaurer, déclasser, muter le statut ou déplacer le
/// `.md` de l'âme d'un **autre** agent — atteinte d'intégrité de l'identité souveraine.
///
/// **Privilège identique** au guard read [`enforce_identity_read_guard`] et au guard write
/// inline de [`vault_write_impl`] : Studio (admin) || scope [`IDENTITY_WRITE_SCOPE`]
/// || `caller == target_agent`. `section`/`note_title` sont résolus **server-side**
/// (jamais depuis l'input). En cas de refus, un audit `vault_write_rejected`
/// (`identity_write_denied_foreign_agent`) est émis, symétrique au guard write inline.
///
/// **No-op strict** hors `identity` (parité exacte `vault_write_impl`) — zéro impact sur
/// les mutations de sections non-`identity`. Un appelant qui veut fail-closed sur une
/// résolution *en erreur* passe une sentinelle `section = "identity"` (cf.
/// [`resolve_title_section_failclosed`]).
///
/// # Erreurs
///
/// - [`GradatumError::Forbidden`] si l'appelant non privilégié n'est pas le propriétaire
///   de l'âme ciblée.
async fn enforce_identity_write_guard(
    state: &AppState,
    trust: &TrustContext,
    tenant: &str,
    section: &str,
    note_title: Option<&str>,
    note_id: &str,
) -> Result<(), GradatumError> {
    // No-op strict hors identity (parité exacte vault_write_impl).
    if section != IDENTITY_SECTION {
        return Ok(());
    }
    // Session Studio = palier de confiance admin — mutation totale autorisée.
    if matches!(trust, TrustContext::Studio { .. }) {
        return Ok(());
    }
    // `subject()` = `sub` du JWT (jamais dérivé d'un paramètre client). `target_agent`
    // est extrait server-side du titre `identity/<agent>`, jamais de l'input requête.
    let caller_sub = trust.subject().map(AgentId::as_str).unwrap_or("");
    let target_agent = note_title
        .and_then(|t| t.strip_prefix("identity/"))
        .unwrap_or("");
    let allowed = trust.has_scope(IDENTITY_WRITE_SCOPE)
        || (!target_agent.is_empty() && caller_sub == target_agent);
    if allowed {
        return Ok(());
    }
    let locus = format!("{tenant}/main");
    // Pas de `X-Request-ID` sur ces surfaces de mutation ciblées-par-ULID : ULID frais
    // (parité `emit_read_rejection_audit`).
    emit_write_rejection_audit(
        state,
        trust,
        tenant,
        &locus,
        &Ulid::generate().to_string(),
        "identity_write_denied_foreign_agent",
        Some(note_id.to_string()),
    )
    .await;
    Err(GradatumError::Forbidden(
        "identity: write restricted to the soul's owner".into(),
    ))
}

/// Provenance d'une écriture `project-map` vis-à-vis du rôle d'identité feature.
///
/// Le rôle `feature:F-XX` EST l'identité d'une carte-feature : c'est lui qui porte la
/// garantie d'unicité du numéro. Cette garantie n'est réelle que si **seul** le serveur peut
/// poser ou modifier ce rôle. Or [`vault_write_impl`] est la voie d'écriture **partagée** par
/// les appels externes (handlers HTTP / MCP relayant l'input client) ET par l'allocation
/// interne [`create_feature_card_impl`] — l'inspection du corps seul ne peut donc pas
/// distinguer une identité serveur légitime d'une identité client illégitime. La distinction
/// se lit au **site d'appel** : elle est portée ici, explicitement, plutôt que devinée.
///
/// Défaut sûr : [`FeatureWriteAuthority::External`] (le contrat d'immuabilité s'applique).
/// Chaque nouvel appelant est forcé de déclarer sa provenance — fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureWriteAuthority {
    /// Écriture externe (relais d'input client). Le rôle `feature` est soumis au contrat
    /// d'immuabilité : interdit à la création, identique à l'existant en mise à jour.
    External,
    /// Écriture interne consécutive à une allocation serveur atomique
    /// ([`create_feature_card_impl`]). Le rôle `feature` injecté est légitime — le
    /// contrat d'immuabilité est court-circuité pour ce seul chemin.
    ServerAllocated,
}

/// Logique métier de `POST /api/v1/vault_write`.
///
/// ACL Write + audit + optimistic-lock (F-41) + enqueue job curate. Sur `project-map`, une
/// écriture `External` est en outre soumise au contrat d'immuabilité d'identité feature
/// (voir [`FeatureWriteAuthority`]).
///
/// # Erreurs
///
/// - `GradatumError::Unauthorized` si non authentifié.
/// - `GradatumError::Forbidden` si ACL Write denied.
/// - `GradatumError::InvalidInput` si note_id invalide, sha256 malformé sur overwrite, ou
///   violation du contrat d'immuabilité feature sur `project-map`.
/// - `GradatumError::Conflict` si overwrite sans `expected_sha256`.
/// - `GradatumError::Storage` sur erreur enqueue.
pub async fn vault_write_impl(
    state: &AppState,
    trust: &TrustContext,
    mut req: crate::api_v1::dto::VaultWriteRequest,
    request_id: &str,
    authority: FeatureWriteAuthority,
) -> Result<EnqueuedResponseUlid, GradatumError> {
    let start = Instant::now();

    if !trust.is_authenticated() {
        emit_auth_failure_audit(
            state,
            trust,
            req.tenant_id.as_ref().map_or("", |t| t.as_str()),
            request_id,
            "unauthenticated",
        )
        .await;
        return Err(GradatumError::Unauthorized);
    }
    // C1 (F-63, EX-C1-1/2) : résolution write-scope — tenant JWT + grant write à flag ON.
    let tenant = effective_write_vault(state, trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.into_forbidden("tenant cross mismatch"))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        // B6′b — pendant write d'`effective_read_vault`. L'audit `auth_failure` ci-dessous
        // n'est PAS un substitut : il part vers le sink JSONL, pas vers le journal du
        // service, et ne porte pas le locus évalué. Le diagnostic se fait sur le journal.
        log_acl_deny(trust, AclOp::Write, &locus, "vault_write");
        emit_auth_failure_audit(state, trust, &tenant, request_id, "acl_deny").await;
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // ── F-36 write_check drift (v0.7.3, warn-only ABSOLU) ──────────────────────
    // Détecte les incohérences catégorie-titre / section_hint déclarée.
    // JAMAIS bloquant : zéro chemin d'erreur ajouté, zéro impact write.
    // Cardinalité bornée : TABLE = 13 règles fixes → pas d'explosion label.
    if let Some(w) = gradatum_core::write_check::check_category_section(
        &req.title,
        req.section_hint.as_deref(),
        &req.tags,
    ) {
        tracing::warn!(
            rule = w.rule,
            category = %w.category,
            expected = w.expected_section,
            actual = ?w.actual_section,
            title = ?req.title,
            "write_check: title category inconsistent with declared section_hint/tags — check section_hint and tags (NOMENCLATURE §10a, warn-only F-36)"
        );
        state
            .metrics
            .write_check
            .get_or_create(&crate::metrics::DriftRuleLabel { rule: w.rule })
            .inc();
        emit_drift_audit(state, trust, &tenant, request_id, &w).await;
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

    // ── Validation section `identity` (F-34 v0.7.3, A1/C2/C6) ──────────────────
    // Forçage de schéma : sur section_hint="identity" UNIQUEMENT, le body doit
    // respecter le format soul INVARIANTS/GATES/NARRATIVE avec INV-CANARY (C4).
    // Bypass curator LLM (A1), comme project-map.
    // Write-restrictive ACL (C6) : un agent ne peut écrire que sa propre âme —
    // `target_agent` extrait du titre côté serveur (jamais paramètre client).
    // Toute déviation → 403 + audit `"identity_write_denied_foreign_agent"`.

    // (P1 sécu) Un titre "identity/..." sans section_hint="identity" est une anomalie :
    // tentative de contourner la write-restrictive check identity, ou erreur client. Rejeter.
    if req.title.starts_with("identity/") && req.section_hint.as_deref() != Some("identity") {
        emit_write_rejection_audit(
            state,
            trust,
            &tenant,
            &locus,
            request_id,
            "rejected_400_identity_title_without_hint",
            None,
        )
        .await;
        return Err(GradatumError::InvalidInput(
            "identity: section_hint=\"identity\" required when the title starts with identity/"
                .into(),
        ));
    }

    if req.section_hint.as_deref() == Some("identity") {
        // (a) ACL write-restrictive d'abord (fail-fast droits avant traitement du body).
        //     Le target agent-id est extrait du titre côté serveur (jamais paramètre
        //     client). Un porteur du scope `identity_write` (v2.0.0, par scope et non par
        //     nom) est autorisé à écrire n'importe quelle âme.
        let target_agent = match req.title.strip_prefix("identity/") {
            Some(a) if !a.is_empty() => a,
            _ => {
                emit_write_rejection_audit(
                    state,
                    trust,
                    &tenant,
                    &locus,
                    request_id,
                    "rejected_400_identity_bad_title",
                    None,
                )
                .await;
                return Err(GradatumError::InvalidInput(
                    "identity: title must be identity/<agent-id> (non-empty)".into(),
                ));
            }
        };
        // `trust.subject()` renvoie `sub` du JWT — jamais dérivé d'un paramètre client.
        let caller_sub = trust.subject().map(AgentId::as_str).unwrap_or("");
        let is_privileged = trust.has_scope(IDENTITY_WRITE_SCOPE);
        if !is_privileged && target_agent != caller_sub {
            emit_write_rejection_audit(
                state,
                trust,
                &tenant,
                &locus,
                request_id,
                "identity_write_denied_foreign_agent",
                Some(target_agent.to_string()),
            )
            .await;
            return Err(GradatumError::Forbidden(
                "identity: an agent may only write its own soul".into(),
            ));
        }
        // (b) Schema soul après ACL (body traité uniquement si droits OK).
        if let Err(e) = gradatum_core::soul::validate_soul(&req.body) {
            emit_write_rejection_audit(
                state,
                trust,
                &tenant,
                &locus,
                request_id,
                "rejected_400_identity_schema",
                None,
            )
            .await;
            return Err(GradatumError::InvalidInput(format!("identity schema: {e}")));
        }
    }

    // Résolution note_id (None → ULID frais, invalide → 400).
    //
    // F-215 critère 4 — issue retenue : REFUS EXPLICITE (pas de parité `vault_read`).
    // Ce `note_id` n'est PAS une référence à résoudre : c'est un ULID **pré-alloué**
    // par un write antérieur, que le serveur honore tel quel. Y résoudre un titre ou un
    // slug ferait écraser une note homonyme. Refus déjà nommé et typé 400 ; le message
    // cite désormais la valeur reçue et la forme attendue.
    let note_id_prealloc = match req.note_id.as_deref() {
        None => Ulid::generate(),
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
                return Err(GradatumError::InvalidInput(format!(
                    "invalid note_id {} — a bare pre-allocated ULID is expected \
                     (this endpoint does not resolve \"section/ULID\", titles or slugs)",
                    echo_ref(s)
                )));
            }
        },
    };

    // ── project-map : immuabilité de l'identité feature (durcissement) ─────────
    // Le rôle [[feature:F-XX]] EST l'identité d'une carte-feature ; lui seul porte la
    // garantie d'unicité du numéro. Une écriture EXTERNE ne peut ni introduire cette
    // identité à la création (seul create_feature_card alloue+injecte), ni la muter en
    // place. Les 5 verbes de mise à jour (gov-todo RMW : act/ship/defer/drop + statut)
    // réécrivent la carte en CONSERVANT le rôle ⇒ identité inchangée ⇒ accepté. Renommer
    // = nouvelle carte + [[supersedes:]] (jamais une mutation). ServerAllocated
    // (create_feature_card) est exempté : il vient d'allouer le numéro atomiquement.
    //
    // Complète — ne remplace pas — la validation de cardinalité de schéma (plus haut) :
    // la cardinalité garantit la BONNE FORME (exactement 1 feature sur une carte-feature),
    // cette garde garantit que ce F-XX précis est PRÉSERVÉ.
    if authority == FeatureWriteAuthority::External
        && req.section_hint.as_deref() == Some("project-map")
    {
        let new_ident = gradatum_core::project_map::feature_identity_from_targets(
            &gradatum_curator::wikilinks::extract_wikilinks(&req.body),
        );
        // Identité existante : vide à la création (note_id absent) OU si la cible n'existe
        // pas encore (note_id fourni mais note neuve/fantôme = création déguisée). Lecture
        // dédiée sur ce chemin froid (governance) — non fusionnée avec la garde overwrite
        // ci-dessous pour ne pas entangler sa machine à états vivante/fantôme/neuve.
        let existing_ident: Vec<String> = if req.note_id.is_some() {
            let reader = read_back_reader(state, &tenant)?;
            match reader.read_note_by_id(&note_id_prealloc.to_string()).await {
                Ok(existing) => gradatum_core::project_map::feature_identity_from_targets(
                    &gradatum_curator::wikilinks::extract_wikilinks(&existing.body.markdown),
                ),
                Err(GradatumError::NoteNotFound(_)) => Vec::new(),
                Err(e) => return Err(e),
            }
        } else {
            Vec::new()
        };

        if new_ident != existing_ident {
            emit_write_rejection_audit(
                state,
                trust,
                &tenant,
                &locus,
                request_id,
                "rejected_400_project_map_feature_identity",
                None,
            )
            .await;
            return Err(GradatumError::InvalidInput(format!(
                "project-map: the [[feature:…]] role is immutable (card identity) — \
                 current {existing_ident:?}, submitted {new_ident:?}. Create through \
                 create_feature_card; a rename is a new card plus [[supersedes:]] on the old one."
            )));
        }
    }

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
            "malformed expected_sha256".into(),
        ));
    }

    // Garde overwrite (note_id fourni) — distingue 3 états de la cible :
    //   • vivante : `.md` présent (index présent) ;
    //   • fantôme : index présent mais `.md` absent (ULID mort, ressuscitable) ;
    //   • neuve   : jamais indexée, `.md` absent.
    //
    // | état    | expected_sha256 = None            | expected_sha256 = Some                |
    // |---------|-----------------------------------|---------------------------------------|
    // | vivante | 409 (overwrite sans garde)        | passe → optimistic-lock worker        |
    // | fantôme | passe → self-heal (résurrection)  | 409 (sha invérifiable, `.md` absent)  |
    // | neuve   | passe                             | passe → écriture neuve                 |
    //
    // Le cas fantôme + `expected_sha256 = Some` ne peut PAS être honoré (aucun `.md`
    // à hasher contre `sha`) : on refuse en 409 plutôt que d'ignorer silencieusement
    // la garde optimiste côté worker — qui traiterait le fantôme comme une note neuve
    // et l'écraserait inconditionnellement (bypass de l'optimistic-lock).
    if req.note_id.is_some() {
        let note_id_str = note_id_prealloc.to_string();
        // C4-1b (P0 security review) : la garde overwrite lit `state.vault` (Vault figé = `main`).
        // À flag ON, un tenant tiers pouvait ainsi SONDER l'existence d'une note de `main` par ULID
        // (409 overwrite/phantom vs passage), et — pré-fix write path — la faire écraser. Pour un
        // tenant ≠ vault servi (`main`), l'existence est désormais jugée sur l'INDEX du tenant
        // (table partagée, colonne `vault_id`) : absente → note neuve dans le vault du tenant, aucune
        // fuite cross-vault. Le tenant `main` conserve la garde historique inchangée (byte-identical,
        // y compris les états orphelins `.md`/index).
        let scoped_out = tenant != "main"
            && state
                .search
                .get_note(&tenant, &note_id_str)
                .await?
                .is_none();
        if scoped_out {
            // Note absente du vault du tenant → traitée comme neuve (création), pas de sonde `main`.
        } else {
            // Task 14 (W3) : la garde overwrite sonde le vault EFFECTIF (`tenant`), plus le
            // singleton `main` — à OFF `state.vault` inchangé (byte-identical).
            let reader = read_back_reader(state, &tenant)?;
            match reader.read_note_by_id(&note_id_str).await {
                Ok(_) => {
                    // `.md` présent : note vivante. Sans expected_sha256 → overwrite refusé.
                    if req.expected_sha256.is_none() {
                        emit_write_rejection_audit(
                            state,
                            trust,
                            &tenant,
                            &locus,
                            request_id,
                            "rejected_409_overwrite_no_sha",
                            Some(note_id_str),
                        )
                        .await;
                        return Err(GradatumError::Conflict(
                            "overwrite without expected_sha256".into(),
                        ));
                    }
                    // expected_sha256 = Some → optimistic-lock délégué au worker (write_if_match).
                }
                Err(gradatum_core::error::GradatumError::NoteNotFound(_)) => {
                    // `.md` absent : fantôme (indexé) ou note neuve (non indexée).
                    // Seul fantôme + sha = Some est refusé ; sinon (None, ou note neuve) on
                    // laisse passer (self-heal / création). L'appel index-level n'est payé
                    // que dans ce cas étroit (`.md` absent ET sha fourni).
                    if req.expected_sha256.is_some() && reader.note_indexed(&note_id_str).await? {
                        emit_write_rejection_audit(
                            state,
                            trust,
                            &tenant,
                            &locus,
                            request_id,
                            "rejected_409_phantom_expected_sha",
                            Some(note_id_str),
                        )
                        .await;
                        return Err(GradatumError::Conflict(
                            "ghost note (.md missing): expected_sha256 unverifiable".into(),
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ── Validation occurred_at (F-74) ─────────────────────────────────────────
    // Fail-fast serveur : si occurred_at est présent et non parseable, on rejette
    // immédiatement avec 400 AVANT d'enqueue le job.
    // Parseur SSOT partagé avec le worker : parse_temporal_str_as_ms (gradatum-core).
    // Cohérence serveur↔worker garantie par la même fonction — un format accepté ici
    // sera forcément parsé correctement côté worker (aucun false-negative silencieux).
    if let Some(occ) = &req.occurred_at
        && gradatum_core::parse_temporal_str_as_ms(occ).is_none()
    {
        return Err(GradatumError::InvalidInput("invalid occurred_at".into()));
    }

    // ── Author = identité du credential (v2.0.0, Task 10) ──────────────────────
    // L'author dérive du sujet authentifié (`trust.subject()`). Fail-closed : un
    // `req.author` fourni est REFUSÉ (400) et une absence totale d'identité résolue
    // est REFUSÉE (401) — jamais de note sans auteur, jamais de déclaration cliente.
    // Locus unique du refus dans `effective_author` ; on propage.
    req.author = Some(effective_author(&req.author, trust.subject())?);

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
        tracing::warn!(error = %e, "vault_write_impl: audit emit failed — non fatal");
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?
        .to_owned();
    let locus = locus_for_tenant(&tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny read".into()));
    }

    // Validation ULID — erreur 400 si invalide (avant toute I/O vault).
    //
    // F-215 critère 4 — issue retenue : REFUS EXPLICITE (pas de parité `vault_read`).
    // `vault_classify` prend une poignée de maintenance (ULID alloué par le write /
    // remonté par la file du curateur), pas un `path` de résultat de recherche : lui
    // faire résoudre un titre ou un slug introduirait une cible ambiguë là où le contrat
    // est déjà univoque. Le refus était DÉJÀ nommé et typé 400 ; seul le message manquait
    // — il cite désormais la valeur reçue et la forme attendue.
    if ulid::Ulid::from_string(&req.note_id).is_err() {
        return Err(GradatumError::InvalidInput(format!(
            "invalid note_id {} — a bare ULID is expected \
             (this endpoint does not resolve \"section/ULID\", titles or slugs)",
            echo_ref(&req.note_id)
        )));
    }

    // Lecture de la note — 404 si absente, 500 sur erreur disque.
    // Task 14 (W3) : read-back routé par le vault effectif (`tenant`) — à OFF singleton `main`.
    let note = read_back_reader(state, &tenant)?
        .read_note_by_id(&req.note_id)
        .await?;
    let current_section = note.frontmatter.section.to_string();

    // Guard identité par-agent (parité `vault_read_impl`) : `vault_classify` lit le CORPS
    // complet de la note server-side et expose `current_section`. Un non-propriétaire ne doit
    // pas pouvoir sonder / reclassifier l'âme d'un AUTRE agent. Section = section RÉELLE de la
    // note ; `target_agent` dérivé du H1 `identity/<agent>`. No-op strict hors `identity`.
    let h1_title = gradatum_curator::extract_h1_title(&note.body.markdown);
    enforce_identity_read_guard(
        state,
        trust,
        &tenant,
        &current_section,
        h1_title.as_deref(),
        &req.note_id,
    )
    .await?;

    // Construction du CuratorNote — pattern identique au worker classify.
    let title_for_curator = h1_title.unwrap_or_else(|| current_section.clone());
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
        GradatumError::Storage(msg) if msg.contains("not found") || msg.contains("Not found") => {
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // Guard identité par-agent (parité `vault_read_impl`) : ne pas divulguer l'existence
    // ni la timeline des versions d'une âme cross-agent. Section RÉELLE + titre résolus
    // server-side depuis l'index (`get_titles_sections`), jamais depuis l'input.
    //   - note présente, section=identity → guard par-âme ;
    //   - note présente, autre section    → no-op (200 normal) ;
    //   - note absente de l'index (map vide) → section "" → no-op (préserve le 200 vide) ;
    //   - erreur d'index → sentinelle `identity` → FAIL-CLOSED (non-priv 403, priv OK).
    // F-215 critère 4 : parité `vault_read` — ULID nu, `section/ULID`, titre ou slug.
    // Résolu AVANT le guard identité : une forme préfixée non résolue rendait une section
    // vide, donc un guard no-op — le guard doit porter sur la note RÉELLE.
    let note_id = resolve_note_ref_strict(state, tenant, &req.note_id).await?;

    let (title, section) = resolve_title_section_failclosed(state, tenant, &note_id).await;
    enforce_identity_read_guard(state, trust, tenant, &section, title.as_deref(), &note_id).await?;

    // Task 23 (W3) : history routé par le vault EFFECTIF (`tenant`) — `history_versions`
    // est instance-bound (`{self.vault_id}/.history/…`). À OFF singleton `main`.
    let versions = read_back_reader(state, tenant)?
        .history_versions(&note_id)
        .await?;
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    // F-215 critère 4 : parité `vault_read` (cf. [`resolve_note_ref_strict`]).
    let note_id = resolve_note_ref_strict(state, tenant, &req.note_id).await?;

    // Task 23 (W3) : history routé par le vault EFFECTIF (`tenant`) — `history_get` est
    // instance-bound (`{self.vault_id}/.history/…`). À OFF singleton `main`.
    let snapshot = read_back_reader(state, tenant)?
        .history_get(&note_id, req.ts_ms)
        .await?;

    // Guard identité par-agent (parité `vault_read_impl`) : l'historique CoW exposait
    // le corps complet d'une âme cross-agent sans aucune restriction. Section RÉELLE et
    // titre (`identity/<agent>`) résolus depuis le snapshot lui-même — self-contained,
    // sans appel index supplémentaire ni chemin d'échec.
    let section = snapshot.frontmatter.section.to_string();
    let title = extract_h1_title(&snapshot.body.markdown);
    enforce_identity_read_guard(state, trust, tenant, &section, title.as_deref(), &note_id).await?;

    Ok(VaultHistoryGetResponse {
        // Écho de l'identifiant RÉSOLU (parité `vault_read`, dont `path` est résolu) :
        // l'appelant récupère la forme canonique, pas la référence qu'il a saisie.
        note_id,
        ts_ms: req.ts_ms,
        body: snapshot.body.markdown,
        section,
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
    // C1 (F-63, EX-C1-1/2) : résolution write-scope — tenant JWT + grant write à flag ON.
    let tenant_owned = effective_write_vault(state, trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.into_forbidden("tenant cross mismatch"))?;
    let tenant = tenant_owned.as_str();
    let locus = format!("{}/main", tenant);
    // Restauration = écriture → AclOp::Write.
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // Guard write-restrictive par-agent (parité `vault_write_impl` C6) : `history_restore`
    // écrase le corps courant d'une note par une version antérieure, court-circuitant la
    // garde write inline de `vault_write_impl`. Sans ce guard, un non-privilégié avec ACL
    // Write pourrait restaurer/écraser l'âme d'un AUTRE agent. Section + titre résolus
    // server-side ; FAIL-CLOSED sur erreur d'index ; no-op hors section `identity`.
    // F-215 critère 4 : parité `vault_read`, résolue AVANT le guard write (le guard doit
    // porter sur la note RÉELLE). Le sous-système CoW est traité comme une unité : les
    // quatre outils qui partagent le champ `note_id` acceptent les mêmes formes.
    let note_id = resolve_note_ref_strict(state, tenant, &req.note_id).await?;

    let (title, section) = resolve_title_section_failclosed(state, tenant, &note_id).await;
    enforce_identity_write_guard(state, trust, tenant, &section, title.as_deref(), &note_id)
        .await?;

    // C4 (caveat C1 HAUTE, council 01KXTRART) : témoin write épinglant la restauration
    // couche-Vault au vault vérifié — un tenant tiers ciblant l'historique d'une note de
    // `main` par ULID → `NoteNotFound` avant tout CoW (fail-closed, note-victime intacte).
    let checked =
        gradatum_core::scope::AclCheckedVaultId::attest_write_checked(VaultId::new(tenant));
    let content_hash = state
        .vault
        .history_restore(&checked, &note_id, req.ts_ms)
        .await?;
    Ok(VaultRestoreResponse {
        // Écho de l'identifiant RÉSOLU (parité `vault_history_get`).
        note_id,
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
    let tenant = effective_tenant(trust, req.tenant_id.as_ref())
        .map_err(|_| GradatumError::Forbidden("tenant cross mismatch".into()))?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny".into()));
    }

    let is_valid_selector = |s: &str| -> bool { s == "current" || s.parse::<i64>().is_ok() };
    if !is_valid_selector(&req.a) || !is_valid_selector(&req.b) {
        return Err(GradatumError::InvalidInput(
            "invalid selector (expected 'current' or timestamp ms)".into(),
        ));
    }

    // Guard identité par-agent (parité `vault_history_get_impl`) : `history_diff` renvoie
    // les lignes de diff du CORPS d'une note entre 2 versions → exfiltration du corps d'âme
    // cross-agent identique à `vault_history_get`. Section + titre (`identity/<agent>`)
    // résolus server-side depuis l'index, jamais depuis l'input. FAIL-CLOSED sur erreur
    // d'index (sentinelle `identity`) ; no-op si note absente ou section non-`identity`.
    // F-215 critère 4 : parité `vault_read`. Placé APRÈS la validation des sélecteurs
    // pour préserver la précédence du 400 « invalid selector » déjà couverte par les tests.
    let note_id = resolve_note_ref_strict(state, tenant, &req.note_id).await?;

    let (title, section) = resolve_title_section_failclosed(state, tenant, &note_id).await;
    enforce_identity_read_guard(state, trust, tenant, &section, title.as_deref(), &note_id).await?;

    let lines = state.vault.history_diff(&note_id, &req.a, &req.b).await?;
    let count = lines.len();
    Ok(VaultDiffResponse { lines, count })
}

// ── §5 LESSONS ────────────────────────────────────────────────────────────────

/// Fixed section for the lesson corpus (synchronisé avec `lessons.rs`).
const LESSONS_SECTION: &str = "lessons-learned";
/// Single-vault tenant (synchronisé avec `lessons.rs`).
pub(crate) const LESSONS_TENANT: &str = "main";

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
        log_acl_deny(trust, AclOp::Read, &acl_locus, "lessons_recall");
        return Err(GradatumError::Forbidden("acl deny lessons".into()));
    }

    let class = params.class.trim();
    if !is_valid_lesson_class(class) {
        return Err(GradatumError::InvalidInput(format!(
            "class outside controlled vocabulary: {class}"
        )));
    }

    let limit = params.limit.unwrap_or(5).clamp(1, 20) as usize;
    // A3-lessons (T12, P1) : `lessons` reste un vault GLOBAL partagé (décision du mainteneur),
    // mais sécurisé explicitement. RÈGLE READ-PATH OFF-GATING :
    // - OFF (défaut LIVE) : chemin legacy INCHANGÉ — l'ACL `main/lessons-learned` (évaluée
    //   ci-dessus) a déjà statué, AUCUN grant consulté. Le témoin porte `main` (== tenant à
    //   OFF) → chemin inchangé (hook lesson-recall.sh, F-60 JIT, MCP vault_lessons_recall).
    // - ON : la lecture cross-tenant du `main/lessons` partagé exige un grant read EXPLICITE
    //   du principal JWT sur le vault `main` (fail-closed). Ferme la forge silencieuse
    //   `own_vault_checked("main")` qui, sans grant, laissait tout tenant lire le lessons de
    //   main. Le témoin `main` est alors légitimé par le grant vérifié.
    //   L3 (F-121) : le grant exigé porte sur la SEULE section `lessons-learned`
    //   (`Some(LESSONS_SECTION)`) — un grant borné à cette section suffit et n'ouvre rien
    //   d'autre de `main` ; un grant vault-entier reste évidemment couvrant. La lecture
    //   aval est elle-même bornée à `LESSONS_SECTION` (filtre `section` des deux chemins
    //   BM25 et sémantique ci-dessous), donc le grant ne sur-autorise pas la requête.
    let vault_id = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(GradatumError::Forbidden("acl deny lessons".into()));
        };
        crate::api_v1::tenant_guard::require_read_grant(
            state,
            principal.as_str(),
            LESSONS_TENANT,
            Some(LESSONS_SECTION),
        )
        .await?;
        crate::api_v1::tenant_guard::own_vault_checked(LESSONS_TENANT)
    } else {
        crate::api_v1::tenant_guard::own_vault_checked(LESSONS_TENANT)
    };

    // ── F-68 semantic opt-in ──────────────────────────────────────────────────
    //
    // Rétro-compat BLOQUANTE : `semantic` absent / `false` → chemin BM25 ci-dessous
    // INCHANGÉ. Le hook LIVE `lesson-recall.sh`, les agents et le MCP qui n'envoient
    // pas ce champ continuent à recevoir le résultat BM25 sans modification.
    //
    // `semantic = true` :
    //  1. retrieve_candidates (RRF BM25 + sémantique, section filtrée).
    //  2. Hydrate les ULIDs retournés → LessonHitRaw (titre, tags, anchor_ms, snippet).
    //  3. Filtres Rust : `codified` absent + tag == class.
    //  4. Apply rank si RecencyBoosted.
    //  5. Return early.
    //
    // Fallback silencieux → chemin BM25 : `embed_fallback = true` (Noop, timeout, erreur).
    if params.semantic.unwrap_or(false) {
        let query = params
            .query
            .as_deref()
            .filter(|q| !q.trim().is_empty())
            .unwrap_or(class)
            .to_string();

        let embed_timeout_ms = state.context.embed_timeout_ms;
        let retrieval_result = retrieve_candidates(
            state,
            &vault_id,
            &query,
            Some(&[LESSONS_SECTION]),
            // Sur-fetch : retrieve_candidates renvoie les top_n candidats, certains
            // seront filtrés (codified + class), donc on demande le double pour avoir
            // au moins `limit` résultats nets.
            (limit * 2).max(limit),
            embed_timeout_ms,
        )
        .await;

        match retrieval_result {
            Ok(outcome) if !outcome.embed_fallback => {
                // Embed a réussi — les candidats sont pertinents sémantiquement.
                let ulid_refs: Vec<&str> = outcome
                    .candidates
                    .iter()
                    .map(|c| c.note_id.as_str())
                    .collect();
                let hydrated = state
                    .search
                    .hydrate_lessons_by_ulids(vault_id.vault_id(), &ulid_refs)
                    .await?;

                // Filtres post-hydratation : codified exclu + classe correcte.
                let filtered: Vec<_> = hydrated
                    .into_iter()
                    .filter(|h| !h.tags.iter().any(|t| t == "codified"))
                    .filter(|h| h.tags.iter().any(|t| t.as_str() == class))
                    .take(limit)
                    .collect();

                // Recency boost opt-in (même logique que le chemin BM25 ci-dessous).
                let raw_hits_sem = if matches!(params.rank, Some(RankMode::RecencyBoosted)) {
                    let now_ms = Utc::now().timestamp_millis();
                    const K: f64 = 60.0;
                    let mut scored: Vec<(_, f64)> = filtered
                        .into_iter()
                        .enumerate()
                        .map(|(i, h)| {
                            let rank_proxy = 1.0 / (K + i as f64);
                            let combined = rank_proxy * recency_factor(h.anchor_ms, now_ms);
                            (h, combined)
                        })
                        .collect();
                    scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    scored.into_iter().map(|(h, _)| h).collect::<Vec<_>>()
                } else {
                    filtered
                };

                let items: Vec<LessonHit> = raw_hits_sem
                    .into_iter()
                    .map(|h| LessonHit {
                        ulid: h.note_id.0.to_string(),
                        title: h.title.unwrap_or_default(),
                        snippet: h.snippet,
                        tags: h.tags,
                        anchor_ms: h.anchor_ms,
                    })
                    .collect();
                return Ok(LessonsRecallResponse { items });
            }
            // embed_fallback = true (Noop, timeout, erreur) OU Err SQL → fallback BM25.
            _ => {
                tracing::debug!(
                    class = class,
                    query = query.as_str(),
                    "lessons_recall: embed KO ou fallback — chemin BM25"
                );
            }
        }
    }

    // ── Chemin BM25 (défaut) ──────────────────────────────────────────────────
    let raw_hits = state
        .search
        .recall_lessons(vault_id.vault_id(), class, limit)
        .await?;

    // Recency boost (opt-in) — rétro-compat BLOQUANTE : None / Relevance = BM25 inchangé.
    //
    // Stratégie : RRF-style rank-proxy (`1 / (K + rank_index)`, K = 60 standard)
    // multiplié par `recency_factor(anchor_ms, now_ms)`.  Le score BM25 brut n'est pas
    // exposé dans `LessonHitRaw` — le proxy de rang encode l'ordre BM25 sans modification
    // de la couche index.  Pour des scores BM25 identiques (corpus égaux), la différence
    // de recency détermine seule l'ordre final.
    //
    // `sort_by` est stable (Rust 1.x) : les ex-aequo de score combiné conservent l'ordre
    // BM25 d'origine (invariant additionnel documenté).
    let raw_hits = if matches!(params.rank, Some(RankMode::RecencyBoosted)) {
        let now_ms = Utc::now().timestamp_millis();
        // K = 60 : constante RRF standard, évite un score infini pour le rang 0.
        const K: f64 = 60.0;
        let mut scored: Vec<(_, f64)> = raw_hits
            .into_iter()
            .enumerate()
            .map(|(i, h)| {
                let rank_proxy = 1.0 / (K + i as f64);
                let combined = rank_proxy * recency_factor(h.anchor_ms, now_ms);
                (h, combined)
            })
            .collect();
        // desc : b cmp a (partial_cmp — f64 ne peut pas être NaN ici : exp() > 0 + division > 0)
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(h, _)| h).collect()
    } else {
        // None (absent) ou Relevance → ordre BM25 de recall_lessons, inchangé.
        raw_hits
    };

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
    // C1 (F-63, EX-C1-1/2) : + grant write exigé à flag ON.
    let tenant_owned = effective_write_vault(state, trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.into_forbidden("tenant cross mismatch"))?;
    let tenant = tenant_owned.as_str();
    let locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // Guard write-restrictive par-agent (parité `vault_write_impl` C6) : `downgrade_note`
    // mute le statut d'une note ciblée par ULID sans passer par `vault_write_impl`. Sans ce
    // guard, un non-privilégié avec ACL Write pourrait déclasser l'âme d'un AUTRE agent.
    // Section + titre résolus server-side ; FAIL-CLOSED sur erreur ; no-op hors `identity`.
    let (title, section) = resolve_title_section_failclosed(state, tenant, &req.note_id).await;
    enforce_identity_write_guard(
        state,
        trust,
        tenant,
        &section,
        title.as_deref(),
        &req.note_id,
    )
    .await?;

    // Parse des ULID — erreurs déjà typées via GradatumError::Validation.
    //
    // F-215 critère 4 — issue retenue : REFUS EXPLICITE (pas de parité `vault_read`).
    // `vault_downgrade` déclasse une note : résoudre un titre ou un slug rendrait la cible
    // d'une mutation ambiguë. Refus déjà nommé et typé 400 ; le message cite désormais la
    // valeur reçue et la forme attendue.
    let note_id = ulid::Ulid::from_string(&req.note_id)
        .map(NoteId)
        .map_err(|_| {
            GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                format!(
                    "invalid note_id {} — a bare ULID is expected \
                 (this endpoint does not resolve \"section/ULID\", titles or slugs)",
                    echo_ref(&req.note_id)
                ),
            ))
        })?;
    let replaced_by = req
        .replaced_by
        .as_deref()
        .map(|s| {
            ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                    format!(
                        "invalid replaced_by {} — a bare ULID is expected \
                         (this endpoint does not resolve \"section/ULID\", titles or slugs)",
                        echo_ref(s)
                    ),
                ))
            })
        })
        .transpose()?;

    // C3a (EX-C3a P0) : la mutation par ULID est épinglée au vault dont l'ACL Write vient
    // d'être vérifiée (`tenant`) — cross-vault → `NoteNotFound` (fail-closed, pas d'oracle).
    let checked =
        gradatum_core::scope::AclCheckedVaultId::attest_write_checked(VaultId::new(tenant));
    state
        .search
        .downgrade_note(&checked, &note_id, &req.reason, replaced_by.as_ref())
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
    // Frontière : `tenant_id()` typé `Option<&TenantId>` (Groupe B Task 3) ; ce hotspot
    // (require_write_grant, resolve_title_section, VaultId::new, ACL locus) consomme `&str`
    // — typage complet réservé Task 11. `.as_str()` = byte-identical.
    let tenant = trust
        .tenant_id()
        .ok_or_else(|| {
            GradatumError::Forbidden("context without tenant — vault access denied".into())
        })?
        .as_str();
    let locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // C3a (F-45, EX-C3a-1) : à ON, parité avec `effective_write_vault` — le DTO n'a
    // pas de tenant_id, le vault cible EST le tenant JWT : scope write exigé, puis
    // grant write du tenant sur son vault propre (fail-closed).
    if state.server_config.multi_tenant.enabled {
        use crate::api_v1::tenant_guard::{
            TenantGuardRefusal, require_write_grant, write_scope_allowed,
        };
        if !write_scope_allowed(state, trust) {
            return Err(TenantGuardRefusal::MissingWriteScope.into_forbidden("acl deny write"));
        }
        require_write_grant(state, tenant, tenant)
            .await
            .map_err(|r| r.into_forbidden("acl deny write"))?;
    }

    // Guard write-restrictive par-agent (parité `vault_write_impl` C6) : `patch_note` mute le
    // statut / `replaced_by` d'une note ciblée par ULID sans passer par `vault_write_impl`.
    // Sans ce guard, un non-privilégié avec ACL Write pourrait altérer le statut de l'âme
    // d'un AUTRE agent. Section + titre résolus server-side ; FAIL-CLOSED ; no-op hors identity.
    let note_id_str = note_id.to_string();
    let (title, section) = resolve_title_section_failclosed(state, tenant, &note_id_str).await;
    enforce_identity_write_guard(
        state,
        trust,
        tenant,
        &section,
        title.as_deref(),
        &note_id_str,
    )
    .await?;

    // C3a (EX-C3a P0) + C4 (caveat C1 HAUTE, council 01KXTRART) : témoin write épinglant les
    // mutations par ULID au vault dont l'ACL Write vient d'être vérifiée — cross-vault →
    // `NoteNotFound`. Couvre le chemin index (`patch_note_status`) ET le chemin couche-Vault
    // (`vault.update_note_status`, ex-scopé sur le seul tenant du Vault = vecteur tiers→main).
    let checked =
        gradatum_core::scope::AclCheckedVaultId::attest_write_checked(VaultId::new(tenant));

    // Logique métier extraite de patch_note (inchangée — contrat métier identique).
    if let Some(ref status_str) = body.status {
        let target: gradatum_core::status::NoteStatus =
            serde_json::from_value(serde_json::Value::String(status_str.clone()))
                .map_err(|_| GradatumError::InvalidInput("status hors enum NoteStatus".into()))?;

        state
            .vault
            .update_note_status(
                &checked,
                &note_id.to_string(),
                target,
                body.status_reason.clone(),
            )
            .await?;

        // replaced_by fourni conjointement avec status → patcher via SQL direct
        // après la transition state machine (update_note_status ne le prend pas).
        if body.replaced_by.is_some() {
            let replaced_by = body
                .replaced_by
                .as_deref()
                .map(|s| {
                    ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                        GradatumError::InvalidInput("invalid replaced_by (ULID expected)".into())
                    })
                })
                .transpose()?;
            state
                .search
                .patch_note_status(&checked, note_id, None, None, replaced_by.as_ref())
                .await?;
        }
    } else {
        // Patch partiel sans changement de statut — SQL direct (raison / replaced_by).
        let replaced_by = body
            .replaced_by
            .as_deref()
            .map(|s| {
                ulid::Ulid::from_string(s).map(NoteId).map_err(|_| {
                    GradatumError::InvalidInput("invalid replaced_by (ULID expected)".into())
                })
            })
            .transpose()?;
        state
            .search
            .patch_note_status(
                &checked,
                note_id,
                None,
                body.status_reason.as_deref(),
                replaced_by.as_ref(),
            )
            .await?;
    }

    Ok(())
}

/// Logique métier de `POST /api/v1/project-map/create-feature`.
///
/// Crée une **carte-feature** project-map dont le numéro `F-XX` est choisi PAR LE SERVEUR.
/// Le client fournit le corps SANS rôle `[[feature:…]]` (mais avec les 5 autres rôles
/// obligatoires) ; le serveur alloue le numéro **atomiquement**, l'injecte, puis enqueue
/// l'écriture via le chemin `vault_write` existant. La réponse rend le numéro attribué + le
/// `job_id` — l'écriture est asynchrone (confirmer via `job_status`). Le client ne voit
/// jamais un numéro qu'il pourrait détourner : il ne le fournit pas et ne peut pas en
/// fournir un. Ce geste unique remplace l'ancienne allocation nue en deux temps (le client
/// ne peut plus écrire un numéro différent de celui reçu, écrire deux fois, ni ne jamais
/// écrire).
///
/// ## Invariant
///
/// « pas de carte sans numéro, pas deux cartes avec le même » : le numéro est injecté
/// server-side (jamais de carte sans), et l'allocation est atomique + planchée sur le max
/// dérivé des cartes (jamais de doublon, y compris avec une carte hors-allocateur). Un
/// numéro brûlé sans carte (job échoué) reste toléré — la séquence a déjà des trous.
///
/// ## Séquence (fail-closed ; allocation APRÈS validation)
///
/// 1. auth + résolution vault (ACL Write `{tenant}/main` + grants, miroir `vault_write`).
/// 2. rejet si le corps porte déjà un `[[feature:…]]` (le client ne choisit pas).
/// 3. pré-validation schéma avec un rôle feature **synthétique** — attrape un corps
///    incomplet (project/status/kind/release/version manquant) AVANT d'allouer, pour ne
///    pas brûler de numéro sur une erreur client.
/// 4. allocation atomique.
/// 5. injection de `[[feature:F-XX]]` dans le corps.
/// 6. délégation à [`vault_write_impl`] (re-valide + enqueue).
///
/// # Erreurs
///
/// - [`GradatumError::Unauthorized`] si non authentifié.
/// - [`GradatumError::Forbidden`] si ACL Write refusée / cross-tenant.
/// - [`GradatumError::InvalidInput`] si le corps porte déjà un feature, ou si la carte est
///   incomplète (400) — **aucun numéro n'est brûlé** dans ces cas.
/// - [`GradatumError::Storage`] / [`GradatumError::Conflict`] propagées de l'allocation ou
///   de l'enqueue.
#[must_use = "the created card handle (number + job_id) must be used"]
pub async fn create_feature_card_impl(
    state: &AppState,
    trust: &TrustContext,
    req: crate::api_v1::dto::CreateFeatureCardRequest,
    request_id: &str,
) -> Result<crate::api_v1::dto::CreateFeatureCardResponse, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    // (1) Résolution vault + ACL Write AVANT allocation : une allocation est une mutation —
    // ne jamais brûler un numéro pour un appelant non autorisé. Miroir `vault_write_impl`.
    let tenant =
        crate::api_v1::tenant_guard::effective_write_vault(state, trust, req.tenant_id.as_ref())
            .await
            .map_err(|r| r.into_forbidden("tenant cross mismatch"))?;
    let locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // (2) Le client ne doit PAS fournir de rôle feature — c'est le serveur qui l'attribue.
    let targets = gradatum_curator::wikilinks::extract_wikilinks(&req.body);
    if targets
        .iter()
        .any(|t| t.trim_start().starts_with("feature:"))
    {
        return Err(GradatumError::InvalidInput(
            "create_feature_card: the body must not carry a [[feature:…]] link — \
             the server assigns the number"
                .into(),
        ));
    }

    // (3) Pré-validation schéma avec un feature SYNTHÉTIQUE : attrape un corps incomplet
    // AVANT d'allouer (un numéro n'est brûlé que si la carte est réellement enqueue).
    let mut precheck = targets;
    precheck.push("feature:F-00".to_string());
    gradatum_core::project_map::validate_links_from_targets(&precheck)
        .map_err(|e| GradatumError::InvalidInput(format!("project-map schema: {e}")))?;

    // (4) Allocation atomique du numéro sur le vault résolu.
    let vault = VaultId::new(tenant.clone());
    let number = state.search.allocate_feature_number(&vault).await?;
    let feature = format!("F-{number:02}");

    // (5) Injection du rôle feature dans le corps (extractible par extract_wikilinks).
    let body = format!("{}\n\n[[feature:{feature}]]", req.body);

    // (6) Délégation au chemin d'écriture asynchrone existant (re-valide + enqueue).
    let mut write_req = crate::api_v1::dto::VaultWriteRequest::new(req.title, body);
    write_req.author = req.author;
    write_req.tags = req.tags;
    write_req.section_hint = Some("project-map".to_string());
    write_req.tenant_id = Some(gradatum_core::scope::TenantId::new(tenant));
    write_req.occurred_at = req.occurred_at;
    // ServerAllocated : le rôle [[feature:…]] vient d'être injecté après allocation atomique
    // — il est exempté du contrat d'immuabilité (qui, sinon, refuserait toute création portant
    // une identité feature). Voir [`FeatureWriteAuthority`].
    let enq = vault_write_impl(
        state,
        trust,
        write_req,
        request_id,
        FeatureWriteAuthority::ServerAllocated,
    )
    .await?;

    tracing::debug!(number, job_id = %enq.job_id, "create_feature_card: enqueued");
    Ok(crate::api_v1::dto::CreateFeatureCardResponse {
        feature,
        number,
        job_id: enq.job_id,
        note_id: enq.note_id,
        poll_url: enq.poll_url,
    })
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
    // Frontière : `tenant_id()` typé `Option<&TenantId>` (Groupe B Task 3) ; ce hotspot
    // (require_write_grant, resolve_title_section, VaultId::new, ACL locus) consomme `&str`
    // — typage complet réservé Task 11. `.as_str()` = byte-identical.
    let tenant = trust
        .tenant_id()
        .ok_or_else(|| {
            GradatumError::Forbidden("context without tenant — vault access denied".into())
        })?
        .as_str();
    let acl_locus = format!("{tenant}/main");
    if state.acl.evaluate(trust, AclOp::Write, &acl_locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("acl deny write".into()));
    }

    // C3a (F-45, EX-C3a-1) : à ON, parité avec `effective_write_vault` — le DTO n'a
    // pas de tenant_id, le vault cible EST le tenant JWT : scope write exigé, puis
    // grant write du tenant sur son vault propre (fail-closed).
    if state.server_config.multi_tenant.enabled {
        use crate::api_v1::tenant_guard::{
            TenantGuardRefusal, require_write_grant, write_scope_allowed,
        };
        if !write_scope_allowed(state, trust) {
            return Err(TenantGuardRefusal::MissingWriteScope.into_forbidden("acl deny write"));
        }
        require_write_grant(state, tenant, tenant)
            .await
            .map_err(|r| r.into_forbidden("acl deny write"))?;
    }

    // Guard write-restrictive par-agent (parité `vault_write_impl` C6) : `move_locus`
    // relocalise physiquement le `.md` d'une note ciblée par ULID sans passer par
    // `vault_write_impl`. Sans ce guard, un non-privilégié avec ACL Write pourrait déplacer
    // (et donc casser la résolution soul-inject de) l'âme d'un AUTRE agent. Section + titre
    // résolus server-side ; FAIL-CLOSED sur erreur d'index ; no-op hors section `identity`.
    let (title, section) = resolve_title_section_failclosed(state, tenant, id).await;
    enforce_identity_write_guard(state, trust, tenant, &section, title.as_deref(), id).await?;

    // C4 (caveat C1 HAUTE, council 01KXTRART) : témoin write épinglant la relocalisation
    // couche-Vault au vault dont l'ACL Write vient d'être vérifiée (`tenant`) — un tenant
    // tiers ciblant une note de `main` par ULID → `NoteNotFound` (fail-closed, pas d'oracle).
    let checked =
        gradatum_core::scope::AclCheckedVaultId::attest_write_checked(VaultId::new(tenant));
    state.vault.move_locus(&checked, id, &locus).await?;
    Ok(())
}

// ── Tests unitaires ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use gradatum_core::scope::AgentId;
    use gradatum_core::trust::TrustContext;

    use super::{
        GradatumError, KNOWN_DOC_KINDS, LESSONS_SECTION, LESSONS_TENANT, MAX_REF_ECHO_CHARS,
        echo_ref, effective_author, locus_for_section, locus_for_tenant,
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
            jti: None,
        };
        assert!(
            trust.is_authenticated(),
            "BearerToken doit être authentifié"
        );
    }

    // ── Test author par défaut = agent authentifié (Task 3 — identité par canal) ──

    /// Construit un `TrustContext::BearerToken` portant le `sub` donné.
    fn bearer(sub: &str) -> TrustContext {
        TrustContext::BearerToken {
            kid: "k".into(),
            aud: "gradatum".into(),
            sub: AgentId::new(sub),
            scopes: vec!["read".into(), "write".into()],
            tenant_id: "main".into(),
            jti: None,
        }
    }

    #[test]
    fn echo_ref_truncates_beyond_the_safety_cap_on_a_char_boundary() {
        // F-215 critère 4 — le safety cap est la raison d'être de `echo_ref` : le champ
        // `note_id` n'est borné par aucun schéma, une référence énorme serait recopiée
        // telle quelle dans la réponse ET les journaux. Multi-octets pour prouver que la
        // coupe est faite en CARACTÈRES (une coupe en octets produirait de l'UTF-8 invalide,
        // donc un panic à la construction de la String).
        let long: String = "é".repeat(MAX_REF_ECHO_CHARS + 10);
        let echoed = echo_ref(&long);
        assert!(
            echoed.ends_with("(truncated)"),
            "au-delà du cap, la troncature doit être signalée — obtenu : {echoed}"
        );
        assert_eq!(
            echoed.matches('é').count(),
            MAX_REF_ECHO_CHARS,
            "exactement {MAX_REF_ECHO_CHARS} caractères retenus"
        );
    }

    #[test]
    fn echo_ref_quotes_a_short_reference_without_truncation_marker() {
        // En-deçà du cap, la valeur est citée intégralement (`{:?}` neutralise aussi les
        // caractères de contrôle) : c'est ce qui rend le refus diagnosticable.
        let echoed = echo_ref("project-map/01M0BCH3JSMGBNYMC2P8AGDRNM");
        assert_eq!(echoed, "\"project-map/01M0BCH3JSMGBNYMC2P8AGDRNM\"");
    }

    #[test]
    fn effective_author_derives_from_subject_when_no_req_author() {
        // Chemin NOMINAL : aucun author fourni + sujet présent → author = subject du
        // credential (un nom NU, sans préfixe `kind:`).
        let derived = effective_author(&None, bearer("agent-buzz").subject())
            .expect("un sujet résolu doit produire un author");
        assert_eq!(derived, "agent-buzz");
    }

    #[test]
    fn effective_author_rejects_explicit_req_author() {
        // Task 10 : `req.author` fourni → REFUS. L'identité vient du credential, elle ne
        // se déclare pas. (Inverse la sémantique v1.0.2 où `custom` primait.)
        let err = effective_author(&Some("custom".into()), bearer("agent-buzz").subject())
            .expect_err("un author fourni par le client doit être refusé");
        assert!(
            matches!(err, GradatumError::InvalidInput(_)),
            "author fourni → InvalidInput (400), obtenu : {err:?}"
        );
    }

    #[test]
    fn effective_author_rejects_when_no_identity_resolved() {
        // Task 10 : aucun author + aucun sujet → REFUS, jamais une note sans auteur.
        // (Inverse la sémantique v1.0.2 où l'author restait None — mémoire partagée.)
        let err = effective_author(&None, None)
            .expect_err("aucune identité résolue → refus (R2, pas de défaut)");
        assert!(
            matches!(err, GradatumError::Unauthorized),
            "aucune identité résolue → Unauthorized (401), obtenu : {err:?}"
        );
    }

    #[test]
    fn effective_author_rejects_explicit_author_even_without_subject() {
        // Un author fourni est refusé quelle que soit la présence d'un sujet : le refus
        // porte sur la DÉCLARATION cliente, pas sur l'absence d'identité serveur.
        let err = effective_author(&Some("custom".into()), None)
            .expect_err("un author fourni doit être refusé même sans sujet");
        assert!(
            matches!(err, GradatumError::InvalidInput(_)),
            "author fourni → InvalidInput (400), obtenu : {err:?}"
        );
    }
}
