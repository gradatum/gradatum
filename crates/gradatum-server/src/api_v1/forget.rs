//! Semantic-forget endpoints — forget / unforgot / forgotten list.
//!
//! # Endpoints
//!
//! | Method | Path | Response | Notes |
//! |--------|------|----------|-------|
//! | POST | `/vault_forget` | 200 `ForgetPreview` (dry) or 202 `EnqueuedResponse` (real) | Two-step confirmation |
//! | GET  | `/vault/forgotten` | 200 `ForgottenListResponse` | Cursor pagination |
//! | POST | `/vault/unforgot/{ulid}` | 200 `UnforgotResponse` | Synchronous restore (index only) |
//!
//! # Mandatory dry-run
//!
//! Real execution requires `dry_run=false` **and** `confirm_ulids` containing
//! exactly the ULIDs returned by a prior preview call.
//! A mismatch returns **400 Bad Request**.
//!
//! # Async pattern (real mode)
//!
//! Real mode enqueues a `Job::Forget(ForgetSpec)` and returns **202 Accepted**
//! with a `poll_url`. Consistent with `vault_write`.
//! The frontmatter mutation is performed by the `handle_forget` worker.
//!
//! # Two-step confirmation
//!
//! 1. Initial call with `dry_run=true` (default) → 200 `ForgetPreview { ulids, count, excluded }`
//! 2. Confirmation call with `dry_run=false` + `confirm_ulids=<ulids from preview>`
//!    → 202 `{ job_id, poll_url }` (worker executes the forget)
//!
//! # Auth
//!
//! Same JWT middleware as all other routes. ACL Write required on the vault.
//!
//! # Protected sections
//!
//! Notes in `agent-issues` and `council` are automatically excluded from the batch.
//! They appear in `excluded` of the `ForgetPreview` response.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::{
    ForgetScope, ForgetSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    error::GradatumError, section::Section, trust::TrustContext,
};
use gradatum_dto::{
    ExcludedNote, ForgetPreview, ForgetScopeDto, ForgottenListResponse, ForgottenNoteEntry,
    MAX_FORGOTTEN_BY_LEN, UnforgotResponse, VaultForgetRequest,
};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::api_v1::tenant_guard::effective_tenant;
use crate::state::AppState;

// ── Sections protégées ────────────────────────────────────────────────────────
//
// Source unique : `Section::PROTECTED_FORGET` dans `gradatum-core::section`.
// Ne pas redéfinir ici — C7 audit F-44 : source unique garantit cohérence
// API (handler) et worker (apalis_handlers).

fn is_protected(section_name: &str) -> bool {
    Section::PROTECTED_FORGET
        .iter()
        .any(|s| s.as_str() == section_name)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parses a ULID from a string — returns 400 Bad Request if invalid.
fn parse_ulid(s: &str) -> Result<Ulid, StatusCode> {
    Ulid::from_string(s).map_err(|_| StatusCode::BAD_REQUEST)
}

// ── Résolution scope → candidats bruts ───────────────────────────────────────

/// Resolves a `ForgetScopeDto` into a list of candidate ULIDs from the index.
///
/// Returns `Vec<(ulid, section)>` so that the protected-section guard can be applied.
async fn resolve_scope(
    state: &AppState,
    scope: &ForgetScopeDto,
    vault_id: &str,
) -> Result<Vec<(String, String)>, StatusCode> {
    match scope {
        ForgetScopeDto::Topic {
            query,
            vault,
            limit,
        } => {
            let effective_vault = vault.as_deref().unwrap_or(vault_id);
            let max_limit = limit.unwrap_or(50).min(200);
            state
                .search
                .search_fts_for_forget(effective_vault, query, max_limit)
                .await
                .map_err(|e| {
                    tracing::warn!(error = %e, "vault_forget: search_fts_for_forget failed");
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        }
        ForgetScopeDto::Locus {
            vault: scope_vault,
            locus,
        } => state
            .search
            .list_notes_by_locus_prefix(scope_vault, locus)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "vault_forget: list_notes_by_locus_prefix failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }),
        ForgetScopeDto::Agent { agent_id, vaults } => state
            .search
            .list_notes_by_agent(agent_id, vaults)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "vault_forget: list_notes_by_agent failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }),
    }
}

// ── Partition éligibles / exclus ──────────────────────────────────────────────

fn partition_candidates(candidates: Vec<(String, String)>) -> (Vec<String>, Vec<ExcludedNote>) {
    let mut eligible = Vec::with_capacity(candidates.len());
    let mut excluded = Vec::new();

    for (ulid, section) in candidates {
        if is_protected(&section) {
            excluded.push(ExcludedNote {
                ulid,
                section: section.clone(),
                reason: format!("section protégée : {section}"),
            });
        } else {
            eligible.push(ulid);
        }
    }
    (eligible, excluded)
}

// ── Validation mono-tenant ────────────────────────────────────────────────────

/// Returns the first vault in the scope that diverges from `tenant_id`, or `None` if consistent.
///
/// Used by `vault_forget` to enforce single-vault validation across all scope variants
/// (`Topic`, `Agent`, `Locus`). Exposed as `pub(crate)` for unit tests.
pub(crate) fn cross_vault_violation<'a>(
    scope: &'a ForgetScopeDto,
    tenant_id: &str,
) -> Option<&'a str> {
    match scope {
        ForgetScopeDto::Locus { vault, .. } => {
            if vault != tenant_id {
                Some(vault.as_str())
            } else {
                None
            }
        }
        ForgetScopeDto::Topic {
            vault: Some(vault), ..
        } => {
            if vault != tenant_id {
                Some(vault.as_str())
            } else {
                None
            }
        }
        ForgetScopeDto::Agent { vaults, .. } => vaults
            .iter()
            .find(|v| v.as_str() != tenant_id)
            .map(|s| s.as_str()),
        // Topic { vault: None } — None = tenant courant, autorisé.
        _ => None,
    }
}

// ── Conversion DTO scope → core ───────────────────────────────────────────────

/// Converts a `ForgetScopeDto` into a `ForgetScope` (core) for the job worker.
fn dto_scope_to_core(dto: &ForgetScopeDto) -> ForgetScope {
    match dto {
        ForgetScopeDto::Topic {
            query,
            vault,
            limit,
        } => ForgetScope::Topic {
            query: query.clone(),
            vault: vault.clone(),
            limit: *limit,
        },
        ForgetScopeDto::Locus { vault, locus } => ForgetScope::Locus {
            vault: vault.clone(),
            locus: locus.clone(),
        },
        ForgetScopeDto::Agent { agent_id, vaults } => ForgetScope::Agent {
            agent_id: agent_id.clone(),
            vaults: vaults.clone(),
        },
    }
}

// ── Construction JobRecord Forget ─────────────────────────────────────────────

/// Builds a `JobRecord` for a `Job::Forget(ForgetSpec)` job.
///
/// Called by `vault_forget` in real mode — `dry_run=false` with `confirm_ulids` provided.
///
/// # Parameters
///
/// - `scope`: resolution scope converted to a `ForgetScope` core value
/// - `confirm_ulids`: confirmed ULIDs (exact match against the preview)
/// - `forgotten_by`: optional actor name (logged in frontmatters)
fn build_forget_job_record(
    scope: ForgetScope,
    confirm_ulids: Vec<String>,
    forgotten_by: Option<String>,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    let spec = ForgetSpec {
        scope,
        dry_run: false, // Mode réel — la double-garde vérifie aussi ce flag
        forgotten_by,
        confirm_ulids,
    };
    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Forget(spec),
            class,
            mode: JobMode::Batch,
            scope: JobScope::VaultWide,
            priority: JobPriority::Normal,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        // Pas de retry automatique — opération sémantique irréversible.
        // max=0 via Default : JobRetry::default() positionne Exponential mais max=0 disable.
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

// ── Réponse job enqueued (mode réel) ─────────────────────────────────────────

/// 202 response for a real (non-dry-run) forget operation.
#[derive(Debug, Serialize)]
pub struct ForgetJobResponse {
    /// Job ULID.
    pub job_id: String,
    /// Immediate status.
    pub status: &'static str,
    /// Poll URL for the job result.
    pub poll_url: String,
    /// Preview of the batch to forget (eligible ULIDs, not yet processed).
    pub preview: ForgetPreview,
}

// ── Handler POST /vault_forget ────────────────────────────────────────────────

/// `POST /api/v1/vault_forget`
///
/// In dry-run mode (default): resolves the scope, excludes protected sections, returns
/// `ForgetPreview { ulids, count, excluded }` with no mutation (200 OK).
///
/// In real mode (`dry_run=false`): verifies `confirm_ulids` (exact match),
/// enqueues `Job::Forget(ForgetSpec)`, returns 202 Accepted with `job_id` + `poll_url`.
/// The frontmatter mutation is performed by the `handle_forget` worker.
///
/// # Auth
///
/// Bearer JWT required + ACL Write.
///
/// # Mandatory dry-run
///
/// Real execution requires `confirm_ulids` = ULIDs from the preview (sorted).
/// Mismatch → **400 Bad Request**.
///
/// # Protected sections
///
/// Notes in `agent-issues` and `council` are automatically excluded into `excluded`.
///
/// # Responses
///
/// - **200 OK** + JSON `ForgetPreview` — dry-run
/// - **202 Accepted** + JSON `ForgetJobResponse` — real mode, job enqueued
/// - **400 Bad Request** — `confirm_ulids` mismatch
/// - **401 Unauthorized** — missing or invalid bearer token
/// - **403 Forbidden** — ACL Write required
/// - **500 Internal Server Error** — index or enqueue failure
pub async fn vault_forget(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultForgetRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant/vault dérivé du JWT, refuse body divergent.
    // Le scope cross_vault_violation est désormais évalué contre le tenant du JWT.
    let vault_id = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = format!("{}/main", vault_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // D3 — Validation mono-tenant v0.4.x : tout scope qui cible explicitement
    // un vault différent du tenant authentifié est refusé avec 403 explicite.
    //
    // Rationale : l'ACL est évaluée sur `req.tenant_id`, mais la résolution des
    // scopes Locus, Topic (vault optionnel), et Agent (vaults[]) s'effectue sur
    // le vault du scope. Si l'un de ces vault diverge, le scope résoudrait des
    // notes hors du périmètre autorisé sans contrôle supplémentaire.
    //
    // v0.4.x = mono-tenant ; cross-vault forget est prévu pour v0.5.1 (multi-tenant).
    if let Some(scope_vault) = cross_vault_violation(&req.scope, &vault_id) {
        tracing::warn!(
            tenant_id = %vault_id,
            scope_vault = %scope_vault,
            "vault_forget: scope cible un vault ≠ tenant_id — 403 (mono-tenant v0.4.x)"
        );
        return Ok((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": format!(
                    "cross-vault forget non supporté en v0.4.x : scope.vault='{}' ≠ tenant_id='{}'. Prévu en v0.5.1.",
                    scope_vault,
                    vault_id
                )
            })),
        ));
    }

    // G10 — Borne forgotten_by (anti DoS par amplification de stockage). Le champ
    // est persisté une fois par note du batch (colonne SQLite + frontmatter YAML de
    // chaque note), donc une valeur non bornée est amplifiée sur tout le périmètre.
    // Rejet déterministe 400 à la frontière, AVANT toute résolution de scope.
    if let Some(by) = req.forgotten_by.as_deref()
        && by.len() > MAX_FORGOTTEN_BY_LEN
    {
        tracing::warn!(
            len = by.len(),
            max = MAX_FORGOTTEN_BY_LEN,
            "vault_forget: forgotten_by dépasse la borne — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "forgotten_by dépasse la borne : maximum {} octets, reçu {}",
                    MAX_FORGOTTEN_BY_LEN,
                    by.len()
                )
            })),
        ));
    }

    // C8 — Borne confirm_ulids cohérente avec le cap Topic limit=200.
    // Dépasser 200 ULIDs de confirmation signale une tentative d'oubli
    // hors-protocole (preview → confirm exact match).
    if req.confirm_ulids.len() > 200 {
        tracing::warn!(
            count = req.confirm_ulids.len(),
            "vault_forget: confirm_ulids dépasse la borne 200 — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "confirm_ulids dépasse la borne : maximum 200, reçu {}",
                    req.confirm_ulids.len()
                )
            })),
        ));
    }

    // Résoudre le scope → candidats bruts (ulid, section).
    let raw_candidates = resolve_scope(&state, &req.scope, &vault_id).await?;

    // Partitionner éligibles / exclues (sections protégées).
    let (eligible, excluded) = partition_candidates(raw_candidates);
    let eligible_count = eligible.len();

    // ── Dry-run : réponse preview sans mutation ───────────────────────────────
    if req.dry_run {
        let preview = ForgetPreview {
            ulids: eligible,
            count: eligible_count,
            excluded,
            dry_run: true,
        };
        let v = serde_json::to_value(&preview).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok((StatusCode::OK, Json(v)));
    }

    // ── Mode réel — vérification confirm_ulids ────────────────────────────────
    // confirm_ulids doit correspondre EXACTEMENT aux ULIDs éligibles.
    let mut expected_sorted = eligible.clone();
    expected_sorted.sort();
    let mut confirmed_sorted = req.confirm_ulids.clone();
    confirmed_sorted.sort();

    if expected_sorted != confirmed_sorted {
        tracing::warn!(
            expected = expected_sorted.len(),
            confirmed = confirmed_sorted.len(),
            "vault_forget: confirm_ulids mismatch — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "confirm_ulids mismatch: expected {}, got {}",
                    expected_sorted.len(),
                    confirmed_sorted.len()
                )
            })),
        ));
    }

    // ── Mode réel — enqueue Job::Forget ──────────────────────────────────────
    let core_scope = dto_scope_to_core(&req.scope);
    let record = build_forget_job_record(
        core_scope,
        req.confirm_ulids.clone(),
        req.forgotten_by.clone(),
    );
    let job_ulid = state.job_store.enqueue(record).await.map_err(|e| {
        tracing::warn!(error = %e, "vault_forget: enqueue Job::Forget failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // C5 — Log enqueue avec job_id. La query du scope et forgotten_by ne sont PAS
    // loggés en clair (peuvent contenir des données sensibles / PII identifiante) —
    // c'est au worker de tracer les notes effectivement traitées.
    tracing::info!(
        forgotten_by_set = req.forgotten_by.is_some(),
        job_id = %job_ulid,
        eligible_count = eligible_count,
        "vault_forget: job enqueued"
    );

    let preview = ForgetPreview {
        ulids: eligible,
        count: eligible_count,
        excluded,
        dry_run: false, // Preview attachée à la demande réelle
    };
    let response = ForgetJobResponse {
        job_id: job_ulid.to_string(),
        status: "queued",
        poll_url: format!("/api/v1/jobs/{job_ulid}/v2"),
        preview,
    };
    let v = serde_json::to_value(&response).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::ACCEPTED, Json(v)))
}

// ── Logique métier extraite pour le serveur MCP natif ────────────────────────

/// Implémentation métier de `vault_forget` réutilisable depuis le serveur MCP natif.
///
/// Expose la même logique que [`vault_forget`] mais retourne `Result<serde_json::Value,
/// GradatumError>` au lieu de `Result<(StatusCode, Json<Value>), StatusCode>`.
///
/// Utilisée par `api_v1::mcp` pour dispatcher l'outil `vault_forget` sans duplication.
///
/// # Errors
///
/// - [`GradatumError::Unauthorized`] si le trust n'est pas authentifié.
/// - [`GradatumError::Forbidden`] si l'ACL Write est refusée ou si la requête cible
///   un vault différent du tenant JWT.
/// - [`GradatumError::InvalidInput`] si `forgotten_by`, `confirm_ulids` ou le
///   `confirm_ulids` mismatch sont invalides.
/// - [`GradatumError::Storage`] si l'enqueue du job échoue.
pub(crate) async fn vault_forget_mcp_impl(
    state: AppState,
    trust: TrustContext,
    req: VaultForgetRequest,
) -> Result<serde_json::Value, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    let vault_id = effective_tenant(&trust, &req.tenant_id)
        .map_err(|_| GradatumError::Forbidden("tenant JWT diverge du body".to_owned()))?
        .to_owned();

    let locus = format!("{}/main", vault_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("ACL Write refusée".to_owned()));
    }

    // D3 — Validation mono-tenant : scope cross-vault refusé.
    if let Some(scope_vault) = cross_vault_violation(&req.scope, &vault_id) {
        return Err(GradatumError::Forbidden(format!(
            "cross-vault forget non supporté en v0.4.x : scope.vault='{}' ≠ tenant_id='{}'. Prévu en v0.5.1.",
            scope_vault, vault_id
        )));
    }

    // G10 — Borne forgotten_by (anti DoS).
    if let Some(by) = req.forgotten_by.as_deref()
        && by.len() > MAX_FORGOTTEN_BY_LEN
    {
        return Err(GradatumError::InvalidInput(format!(
            "forgotten_by dépasse la borne : maximum {} octets, reçu {}",
            MAX_FORGOTTEN_BY_LEN,
            by.len()
        )));
    }

    // C8 — Borne confirm_ulids.
    if req.confirm_ulids.len() > 200 {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids dépasse la borne : maximum 200, reçu {}",
            req.confirm_ulids.len()
        )));
    }

    let raw_candidates = resolve_scope(&state, &req.scope, &vault_id)
        .await
        .map_err(|_| GradatumError::Storage("résolution scope échouée".to_owned()))?;

    let (eligible, excluded) = partition_candidates(raw_candidates);
    let eligible_count = eligible.len();

    // Dry-run.
    if req.dry_run {
        let preview = ForgetPreview {
            ulids: eligible,
            count: eligible_count,
            excluded,
            dry_run: true,
        };
        return serde_json::to_value(&preview)
            .map_err(|e| GradatumError::Storage(format!("sérialisation preview : {e}")));
    }

    // Mode réel — vérification confirm_ulids.
    let mut expected_sorted = eligible.clone();
    expected_sorted.sort();
    let mut confirmed_sorted = req.confirm_ulids.clone();
    confirmed_sorted.sort();

    if expected_sorted != confirmed_sorted {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids mismatch: expected {}, got {}",
            expected_sorted.len(),
            confirmed_sorted.len()
        )));
    }

    // Enqueue Job::Forget.
    let core_scope = dto_scope_to_core(&req.scope);
    let record = build_forget_job_record(
        core_scope,
        req.confirm_ulids.clone(),
        req.forgotten_by.clone(),
    );
    let job_ulid = state.job_store.enqueue(record).await.map_err(|e| {
        tracing::warn!(error = %e, "vault_forget_mcp_impl: enqueue Job::Forget failed");
        GradatumError::Storage("enqueue job échoué".to_owned())
    })?;

    tracing::info!(
        forgotten_by_set = req.forgotten_by.is_some(),
        job_id = %job_ulid,
        eligible_count = eligible_count,
        "vault_forget_mcp_impl: job enqueued"
    );

    let preview = ForgetPreview {
        ulids: eligible,
        count: eligible_count,
        excluded,
        dry_run: false,
    };
    let response = ForgetJobResponse {
        job_id: job_ulid.to_string(),
        status: "queued",
        poll_url: format!("/api/v1/jobs/{job_ulid}/v2"),
        preview,
    };
    serde_json::to_value(&response)
        .map_err(|e| GradatumError::Storage(format!("sérialisation réponse : {e}")))
}

// ── Handler GET /vault/forgotten ──────────────────────────────────────────────

/// Query parameters for forgotten-list pagination.
#[derive(Debug, Deserialize)]
pub struct ForgottenListQuery {
    /// Results per page (default 50, max 500).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Exclusive cursor (last ULID received).
    pub cursor: Option<String>,
}

fn default_limit() -> usize {
    50
}

/// `GET /api/v1/vault/forgotten`
///
/// Lists notes with `forgotten=1`, ordered by `forgotten_at DESC`.
/// Cursor-based pagination: `?cursor=<ulid>&limit=<n>`.
///
/// # Responses
///
/// - **200 OK** + JSON `ForgottenListResponse { notes, total, next_cursor }`
/// - **401 Unauthorized** — missing or invalid bearer token
/// - **403 Forbidden** — ACL Read required
/// - **500 Internal Server Error** — SQLite failure
pub async fn vault_forgotten_list(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(params): Query<ForgottenListQuery>,
) -> Result<Json<ForgottenListResponse>, StatusCode> {
    // Mono-tenant v0.4.x : tenant fixé à "main" (hardcodé).
    // Multi-tenant prévu en v0.5.1 — ce hardcode sera remplacé par extraction
    // depuis le JWT (TrustContext::BearerToken::tenant_id).
    // Cohérent avec vault_status et vault_write (même pattern v0.4.x).
    let vault_id = "main".to_string();

    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = format!("{}/main", vault_id);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let limit = params.limit.clamp(1, 500);
    let cursor = params.cursor.as_deref();

    // list_forgotten retourne limit+1 entrées pour détecter next_cursor.
    let rows = state
        .search
        .list_forgotten(&vault_id, limit, cursor)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "vault_forgotten_list: list_forgotten failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Détection next_cursor : si on a reçu limit+1 résultats → il y a une page suivante.
    let has_more = rows.len() > limit;
    let rows_page: Vec<_> = if has_more {
        rows.into_iter().take(limit).collect()
    } else {
        rows
    };

    let next_cursor = if has_more {
        rows_page.last().map(|(id, _, _, _, _)| id.clone())
    } else {
        None
    };

    let notes: Vec<ForgottenNoteEntry> = rows_page
        .into_iter()
        .map(
            |(ulid, title, section, forgotten_at_ms, forgotten_by)| ForgottenNoteEntry {
                ulid,
                title,
                section,
                forgotten_at: forgotten_at_ms,
                forgotten_by,
            },
        )
        .collect();

    // C3 (audit P1) : `total` = count global oubliées, pas taille de la page courante.
    // `count_forgotten_notes` exécute `SELECT COUNT(*)` sur l'index.
    // Fallback dégradé sur erreur : on retourne notes.len() pour rester non-bloquant
    // (la pagination reste fonctionnelle, seul le champ total est approximatif).
    let total = match state.search.count_forgotten(&vault_id).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "vault_forgotten_list: count_forgotten échoué — fallback taille page");
            notes.len()
        }
    };

    Ok(Json(ForgottenListResponse {
        notes,
        total,
        next_cursor,
    }))
}

// ── Handler POST /vault/unforgot/{ulid} ───────────────────────────────────────

/// `POST /api/v1/vault/unforgot/{ulid}`
///
/// Restores a forgotten note by clearing its forgotten marker in the SQLite index.
///
/// # Index / frontmatter consistency window
///
/// The SQLite index update (`forgotten=0`, `forgotten_at=NULL`,
/// `forgotten_by=NULL`) is **synchronous and immediate**: the note becomes
/// visible in search results as soon as this endpoint returns.
///
/// The YAML frontmatter in the `.md` file on disk (`forgotten`,
/// `forgotten_at`, `forgotten_by` fields) is **deferred**: it will be
/// re-synchronised on the next vault access triggered by a write or a cache miss.
/// When reading the YAML file directly (outside the API), a residual `forgotten`
/// field may be present until that point.
///
/// This asymmetry is intentional (performance: no disk I/O on unforgot).
/// API scoring and listing always reflect the index state (source of truth),
/// not the YAML file.
///
/// # Auth
///
/// Bearer JWT required + ACL Write.
///
/// # Idempotence
///
/// A second call on a note that is no longer forgotten returns 404
/// (`unmark_forgotten` requires `affected > 0`).
///
/// # Responses
///
/// - **200 OK** + JSON `UnforgotResponse { ulid, status }`
/// - **400 Bad Request** — invalid ULID
/// - **401 Unauthorized** — missing or invalid bearer token
/// - **403 Forbidden** — ACL Write required
/// - **404 Not Found** — note absent or not forgotten
/// - **500 Internal Server Error** — SQLite failure
pub async fn vault_unforgot(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(ulid_str): Path<String>,
) -> Result<Json<UnforgotResponse>, StatusCode> {
    // Mono-tenant v0.4.x : tenant fixé à "main" (hardcodé).
    // Multi-tenant prévu en v0.5.1 — même raisonnement que vault_forgotten_list.
    let vault_id = "main".to_string();

    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let locus = format!("{}/main", vault_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // Valider l'ULID.
    let _ulid = parse_ulid(&ulid_str)?;

    // Annuler le marquage oubli dans l'index SQLite.
    state
        .search
        .unmark_forgotten(&vault_id, &ulid_str)
        .await
        .map_err(|e| match e {
            gradatum_core::error::GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
            _ => {
                tracing::warn!(note_id = %ulid_str, error = %e, "vault_unforgot: unmark_forgotten failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })?;

    Ok(Json(UnforgotResponse {
        ulid: ulid_str,
        status: "restored".to_string(),
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard D3 — scope Topic avec vault explicite ≠ tenant_id → violation détectée.
    #[test]
    fn cross_vault_violation_topic_explicit_vault_mismatch() {
        let scope = ForgetScopeDto::Topic {
            query: "test".to_string(),
            vault: Some("other-vault".to_string()),
            limit: None,
        };
        let violation = cross_vault_violation(&scope, "main");
        assert_eq!(
            violation,
            Some("other-vault"),
            "scope Topic avec vault ≠ tenant_id doit déclencher 403"
        );
    }

    /// Guard D3 — scope Topic avec vault = None → pas de violation (tenant courant).
    #[test]
    fn cross_vault_violation_topic_no_vault_is_ok() {
        let scope = ForgetScopeDto::Topic {
            query: "test".to_string(),
            vault: None,
            limit: None,
        };
        let violation = cross_vault_violation(&scope, "main");
        assert!(
            violation.is_none(),
            "scope Topic sans vault doit être autorisé (défaut = tenant courant)"
        );
    }

    /// Guard D3 — scope Topic avec vault = tenant_id → pas de violation.
    #[test]
    fn cross_vault_violation_topic_same_vault_is_ok() {
        let scope = ForgetScopeDto::Topic {
            query: "test".to_string(),
            vault: Some("main".to_string()),
            limit: None,
        };
        let violation = cross_vault_violation(&scope, "main");
        assert!(violation.is_none());
    }

    /// Guard D3 — scope Agent avec vault ≠ tenant_id → violation détectée.
    #[test]
    fn cross_vault_violation_agent_vault_mismatch() {
        let scope = ForgetScopeDto::Agent {
            agent_id: "claude-agent".to_string(),
            vaults: vec!["main".to_string(), "restricted-vault".to_string()],
        };
        let violation = cross_vault_violation(&scope, "main");
        assert_eq!(
            violation,
            Some("restricted-vault"),
            "scope Agent avec un vault ≠ tenant_id doit déclencher 403"
        );
    }

    /// Guard D3 — scope Agent avec vaults = [] → pas de violation (notes du tenant courant).
    #[test]
    fn cross_vault_violation_agent_empty_vaults_is_ok() {
        let scope = ForgetScopeDto::Agent {
            agent_id: "claude-agent".to_string(),
            vaults: vec![],
        };
        let violation = cross_vault_violation(&scope, "main");
        assert!(
            violation.is_none(),
            "scope Agent sans vaults doit être autorisé (défaut = tenant courant)"
        );
    }

    /// Guard D3 — scope Locus avec vault ≠ tenant_id → violation (régression test).
    #[test]
    fn cross_vault_violation_locus_mismatch() {
        let scope = ForgetScopeDto::Locus {
            vault: "other".to_string(),
            locus: "inbox/old/".to_string(),
        };
        let violation = cross_vault_violation(&scope, "main");
        assert_eq!(violation, Some("other"));
    }
}
