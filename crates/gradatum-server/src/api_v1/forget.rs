//! Semantic-forget endpoints — forget / unforgot / forgotten list.
//!
//! # Endpoints
//!
//! | Method | Path | Response | Notes |
//! |--------|------|----------|-------|
//! | POST | `/vault_forget` | 200 `ForgetPreview` (dry) or 202 `EnqueuedResponseUlid` (real) | Two-step confirmation |
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
    error::GradatumError, scope::VaultId, section::Section, trust::TrustContext,
};
use gradatum_dto::{
    ExcludedNote, ForgetPreview, ForgetScopeDto, ForgottenListResponse, ForgottenNoteEntry,
    MAX_FORGOTTEN_BY_LEN, UnforgotResponse, VaultForgetRequest,
};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::api_v1::logic::locus_for_tenant;
use crate::api_v1::tenant_guard::{effective_tenant, effective_write_vault};
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
                .search_fts_for_forget(&VaultId::new(effective_vault), query, max_limit)
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
                reason: format!("protected section: {section}"),
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

/// Returns the vault count of an `Agent` scope that targets more than one vault.
///
/// A7-bis — même classe que la garde A7 de `jobs_v2::build_job_record_from_spec`, appliquée
/// à l'autre porte d'entrée. Sans elle, `Agent { vaults: ["alice", "alice"] }` posté par
/// `alice` traverse [`cross_vault_violation`] (qui ne cherche qu'un vault ≠ tenant, et n'en
/// trouve aucun), puis `dto_scope_to_core` produit un `Agent` de longueur 2 que le worker
/// refuse terminalement (`ensure_forget_scope_vault`, branche `many`) : 202 puis DLQ.
///
/// ## Pourquoi compter SANS dédoublonner
///
/// Le worker branche sur la longueur BRUTE du slice : dédoublonner ici sans réécrire le
/// spec enfilé ne changerait rien au 202 → DLQ, et le réécrire reviendrait à enfiler
/// silencieusement autre chose que ce que l'appelant a posté — un registre inacceptable
/// pour un acte que le protocole `confirm_ulids` rend justement explicite. Le
/// dédoublonnage a un sens là où il y a un fan-out à produire : le CLI admin
/// (`fan_out_by_vault`) émet N jobs et dédoublonne pour ne pas en émettre deux sur la même
/// cible. Cette route n'émet qu'UN job : elle n'a rien à dédoublonner, seulement à refuser.
/// Une règle unique, identique sur les trois sites.
///
/// ## Pourquoi 400 et non 403
///
/// Refus de FORME, comme en A7 : il ne consulte ni trust ni tenant, vaut à `multi_tenant`
/// OFF, et se déclenche même si le porteur couvre tous les vaults cités. Un 403 serait un
/// faux signal de sécurité. Il est évalué APRÈS [`cross_vault_violation`] : un scope citant
/// un vault ≠ tenant reste un 403 (contrat public existant, mono-tenant v0.4.x) — cette
/// garde ne ferme que le trou qu'il laisse, elle ne le requalifie pas.
pub(crate) fn multi_vault_agent_scope(scope: &ForgetScopeDto) -> Option<usize> {
    match scope {
        ForgetScopeDto::Agent { vaults, .. } if vaults.len() > 1 => Some(vaults.len()),
        _ => None,
    }
}

/// Message de refus commun aux deux portes d'entrée `vault_forget` (HTTP et MCP).
fn multi_vault_agent_error(count: usize) -> String {
    format!(
        "`ForgetScope::Agent` targets {count} vaults — a job targets exactly one vault. \
         Post one job per vault (fan-out belongs to the enqueue site). Repeated vaults count."
    )
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
/// - `vault_id`: vault EFFECTIF résolu (JWT) — estampillé dans le job pour que le worker
///   re-dérive le tenant (voir le bloc de commentaire ci-dessous, complétude cross-tenant).
fn build_forget_job_record(
    scope: ForgetScope,
    confirm_ulids: Vec<String>,
    forgotten_by: Option<String>,
    vault_id: &str,
) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;
    // P0 cross-tenant (complétude worker) : estampiller le vault EFFECTIF dans le job.
    // Sans cela le job ne portait PAS le tenant : `JobScope::VaultWide` faisait retomber
    // `resolve_job_vault` (worker) sur `"main"` — donc `persist_forget` (apalis_handlers)
    // mutait sous le namespace `"main"` (perte du tenant / mutation cross-vault), et un
    // `Forget::Topic{vault:None}` résolvait ses candidats dans `"main"`. Deux dérivations
    // distinctes, corrigées ensemble :
    //   1. `JobScope::Vault(vault_id)` → `resolve_job_vault` rend le tenant (vault de
    //      `persist_forget` + fallback de résolution Topic côté worker).
    //   2. `Topic{vault:None}` normalisé en `Some(vault_id)` → `forget_scope_tenant`
    //      (colonne `tenant_id` de la file = routing per-vault à ON) rend le tenant.
    // À OFF (mono-vault) `vault_id == "main"` : les deux estampilles restent `"main"` →
    // byte-identical (seul le payload sérialisé du job diffère, jamais observable via l'API).
    let scope = match scope {
        ForgetScope::Topic {
            query,
            vault: None,
            limit,
        } => ForgetScope::Topic {
            query,
            vault: Some(vault_id.to_owned()),
            limit,
        },
        other => other,
    };
    let spec = ForgetSpec {
        scope,
        dry_run: false, // Mode réel — la double-garde vérifie aussi ce flag
        forgotten_by,
        confirm_ulids,
    };
    JobRecord {
        id: Ulid::generate(),
        spec: JobSpec {
            kind: Job::Forget(spec),
            class,
            mode: JobMode::Batch,
            scope: JobScope::Vault(vault_id.to_owned()),
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
/// - **400 Bad Request** — `confirm_ulids` mismatch, bounds exceeded, or
///   `ForgetScope::Agent` targeting more than one vault (repeated vaults count)
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
    // C1 (F-63, EX-C1-1/2) : + grant write exigé à flag ON.
    let vault_id = effective_write_vault(&state, &trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.status())?;
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
                    "cross-vault forget not supported in v0.4.x: scope.vault='{}' ≠ tenant_id='{}'. Planned for v0.5.1.",
                    scope_vault,
                    vault_id
                )
            })),
        ));
    }

    // A7-bis — un `Agent` multi-vault (répétitions comprises) n'est pas exécutable par
    // cette route : refus 400 en amont, AVANT toute résolution de scope, plutôt qu'un 202
    // suivi d'une mort en DLQ. Cf. `multi_vault_agent_scope` pour l'arbitrage
    // « refuser » vs « dédoublonner » et pour le choix 400 vs 403.
    if let Some(count) = multi_vault_agent_scope(&req.scope) {
        tracing::warn!(
            tenant_id = %vault_id,
            vault_count = count,
            "vault_forget: ForgetScope::Agent multi-vault — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": multi_vault_agent_error(count) })),
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
            "vault_forget: forgotten_by exceeds bound — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "forgotten_by exceeds bound: maximum {} bytes, received {}",
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
            "vault_forget: confirm_ulids exceeds bound 200 — 400"
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "confirm_ulids exceeds bound: maximum 200, received {}",
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
        &vault_id,
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
///   `confirm_ulids` mismatch sont invalides, ou si le scope est un `Agent` visant
///   plus d'un vault (répétitions comprises).
/// - [`GradatumError::Storage`] si l'enqueue du job échoue.
pub(crate) async fn vault_forget_mcp_impl(
    state: AppState,
    trust: TrustContext,
    req: VaultForgetRequest,
) -> Result<serde_json::Value, GradatumError> {
    if !trust.is_authenticated() {
        return Err(GradatumError::Unauthorized);
    }

    // C1 (F-63, EX-C1-1/2) : résolution write-scope — tenant JWT + grant write à flag ON.
    let vault_id = effective_write_vault(&state, &trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.into_forbidden("JWT tenant diverges from body"))?;

    let locus = format!("{}/main", vault_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(GradatumError::Forbidden("ACL Write denied".to_owned()));
    }

    // D3 — Validation mono-tenant : scope cross-vault refusé.
    if let Some(scope_vault) = cross_vault_violation(&req.scope, &vault_id) {
        return Err(GradatumError::Forbidden(format!(
            "cross-vault forget not supported in v0.4.x: scope.vault='{}' ≠ tenant_id='{}'. Planned for v0.5.1.",
            scope_vault, vault_id
        )));
    }

    // A7-bis — `Agent` multi-vault refusé en amont (cf. `multi_vault_agent_scope`).
    // Même règle que la porte HTTP : les deux portes ne doivent pas diverger, c'est
    // exactement la classe de défaut que ce lot ferme.
    if let Some(count) = multi_vault_agent_scope(&req.scope) {
        return Err(GradatumError::InvalidInput(multi_vault_agent_error(count)));
    }

    // G10 — Borne forgotten_by (anti DoS).
    if let Some(by) = req.forgotten_by.as_deref()
        && by.len() > MAX_FORGOTTEN_BY_LEN
    {
        return Err(GradatumError::InvalidInput(format!(
            "forgotten_by exceeds bound: maximum {} bytes, received {}",
            MAX_FORGOTTEN_BY_LEN,
            by.len()
        )));
    }

    // C8 — Borne confirm_ulids.
    if req.confirm_ulids.len() > 200 {
        return Err(GradatumError::InvalidInput(format!(
            "confirm_ulids exceeds bound: maximum 200, received {}",
            req.confirm_ulids.len()
        )));
    }

    let raw_candidates = resolve_scope(&state, &req.scope, &vault_id)
        .await
        .map_err(|_| GradatumError::Storage("scope resolution failed".to_owned()))?;

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
            .map_err(|e| GradatumError::Storage(format!("preview serialization: {e}")));
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
        &vault_id,
    );
    let job_ulid = state.job_store.enqueue(record).await.map_err(|e| {
        tracing::warn!(error = %e, "vault_forget_mcp_impl: enqueue Job::Forget failed");
        GradatumError::Storage("job enqueue failed".to_owned())
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
        .map_err(|e| GradatumError::Storage(format!("response serialization: {e}")))
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Résolution du vault EFFECTIF depuis le JWT, GATÉE sur `multi_tenant.enabled` —
    // alignée sur `vault_status_impl` (logic.rs), sibling de lecture own-vault :
    // - OFF (défaut LIVE) : `"main"` INCHANGÉ (mono-vault, principal == main → byte-identical).
    // - ON : le vault est le tenant du principal JWT (`effective_tenant`) ; un contexte sans
    //   tenant (Studio/Mtls) est refusé 403.
    // Sans ce gate le vault était figé à `"main"` : un tenant ≠ main porteur d'un grant ACL
    // couvrant `main/*` listait les `forgotten_by` (PII) des notes de main → fuite cross-tenant.
    let vault_id: String = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(StatusCode::FORBIDDEN);
        };
        effective_tenant(&trust, Some(principal))
            .map_err(|_| StatusCode::FORBIDDEN)?
            .to_owned()
    } else {
        "main".to_owned()
    };

    let locus = locus_for_tenant(&vault_id);
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
            tracing::warn!(error = %e, "vault_forgotten_list: count_forgotten failed — fallback to page size");
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // P0 cross-tenant (famille P0-8) : vault dérivé du JWT, GATÉ sur `multi_tenant.enabled`,
    // avec la MÊME protection write-path que `vault_forget` (dette C1 comblée : `vault_unforgot`
    // était le SEUL write-path vault sans grant — il n'appliquait que `write_scope_allowed`,
    // pas `require_write_grant`).
    // - OFF (défaut LIVE mono-vault) : `"main"` INCHANGÉ, byte-identical. `write_scope_allowed`
    //   valant toujours vrai à OFF, la garde standalone était un no-op → retirée (aucun
    //   changement de comportement à OFF).
    // - ON : `effective_write_vault` = effective_tenant (JWT) + write-scope (EX-C3a-1) +
    //   `require_write_grant` (EX-C1-2). Le hardcode `"main"` laissait un tenant ≠ main porteur
    //   d'un grant ACL couvrant `main/*` RESTAURER une note oubliée de `main` (tampering +
    //   droit à l'oubli défait). Route paramétrique sans body tenant_id : le `body_tenant`
    //   passé EST le principal JWT (cohérence triviale — `effective_tenant` compare JWT à
    //   lui-même), la garde effective vient du write-scope + du grant.
    let vault_id: String = if state.server_config.multi_tenant.enabled {
        let Some(principal) = trust.tenant_id() else {
            return Err(StatusCode::FORBIDDEN);
        };
        effective_write_vault(&state, &trust, Some(principal))
            .await
            .map_err(|r| r.status())?
    } else {
        "main".to_owned()
    };

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

    /// Complétude worker (P0 cross-tenant) : `build_forget_job_record` estampille le vault
    /// EFFECTIF pour que le worker re-dérive le tenant depuis le job.
    ///
    /// RED avant fix : `JobScope::VaultWide` + `Topic{vault:None}` → `spec_tenant` == `"main"`
    /// (donc `persist_forget` mutait sous `"main"` et le routing file l'attribuait à `"main"`).
    #[test]
    fn build_forget_job_record_stamps_effective_vault() {
        let scope = ForgetScope::Topic {
            query: "secret".to_string(),
            vault: None,
            limit: None,
        };
        let record = build_forget_job_record(scope, vec![], None, "vault-b");

        // 1. JobScope porte le vault effectif → worker `resolve_job_vault` rend le tenant
        //    (namespace de `persist_forget`), plus jamais `"main"` via `VaultWide`.
        match &record.spec.scope {
            JobScope::Vault(v) => assert_eq!(v, "vault-b", "JobScope doit porter le tenant"),
            other => panic!("attendu JobScope::Vault, obtenu {other:?}"),
        }
        // 2. Tenant estampillé dans la file (routing per-vault ON) = vault effectif, pas 'main'.
        assert_eq!(
            gradatum_core::spec_tenant(&record.spec),
            "vault-b",
            "un Forget::Topic{{vault:None}} d'un tenant ≠ main ne doit PAS retomber sur 'main'"
        );
        // 3. Le scope Topic a été normalisé (vault renseigné) → worker résout ses candidats
        //    dans le bon vault.
        match &record.spec.kind {
            Job::Forget(f) => match &f.scope {
                ForgetScope::Topic { vault, .. } => {
                    assert_eq!(vault.as_deref(), Some("vault-b"));
                }
                other => panic!("scope Forget inattendu : {other:?}"),
            },
            other => panic!("kind inattendu : {other:?}"),
        }
    }

    /// Parité OFF (mono-vault) : un job `main` reste estampillé `"main"` (byte-identical —
    /// `resolve_job_vault(Vault("main")) == "main"`, `spec_tenant == "main"`).
    #[test]
    fn build_forget_job_record_main_is_byte_identical_off() {
        let scope = ForgetScope::Topic {
            query: "x".to_string(),
            vault: None,
            limit: None,
        };
        let record = build_forget_job_record(scope, vec![], None, "main");
        match &record.spec.scope {
            JobScope::Vault(v) => assert_eq!(v, "main"),
            other => panic!("attendu JobScope::Vault(main), obtenu {other:?}"),
        }
        assert_eq!(gradatum_core::spec_tenant(&record.spec), "main");
    }
}
