//! MCP write handlers for API v1 — async 202 Accepted pattern.
//!
//! Each handler:
//! 1. Verifies authentication via [`TrustContext::is_authenticated`].
//! 2. Evaluates the ACL via `AclEngine::evaluate` (Write, locus = `tenant_id/main`).
//! 3. Builds a [`gradatum_core::JobRecord`] and enqueues it via `state.job_store.enqueue()`.
//! 4. Emits an audit event.
//! 5. Returns 202 Accepted + JSON [`EnqueuedResponse`].
//!
//! `vault_write` uses `state.job_store` (trait [`gradatum_core::QueueStore`],
//! `gradatum_jobs` table).
//!
//! `vault_classify` returns 501 Not Implemented — classification is performed
//! automatically inside the `vault_write` pipeline.
//!
//! # Auth failures
//!
//! 401/403 responses emit an `auth_failure` audit event (outcome `denied`)
//! before returning the error code.
//!
//! # Endpoints
//!
//! | Method | Path | Auth |
//! |--------|------|------|
//! | POST | `/api/v1/vault_write`     | bearer + ACL Write required |
//! | POST | `/api/v1/vault_classify`  | bearer + ACL Write required |
//! | POST | `/api/v1/vault_downgrade` | bearer + ACL Write required |
//!

use std::time::Instant;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension, Json,
};
use chrono::Utc;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::audit::http::{HttpAuditActor, HttpAuditEvent};
use gradatum_core::trust::TrustContext;
use gradatum_core::{
    CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
    JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
};
use gradatum_queue::NewJob;
use ulid::Ulid;

use crate::api_v1::dto::{
    EnqueuedResponse, EnqueuedResponseUlid, VaultClassifyRequest, VaultDowngradeRequest,
    VaultWriteRequest,
};
use crate::api_v1::tenant_guard::effective_tenant;
use crate::state::AppState;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parses a 64-char lowercase hex SHA-256 string into `[u8; 32]`.
///
/// Returns `None` if the string is not exactly 64 valid hex characters.
/// Used to parse `VaultWriteRequest.expected_sha256` into `CurateSpec.expected_sha256`.
fn parse_sha256_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        bytes[i] = (hi * 16 + lo) as u8;
    }
    Some(bytes)
}

// ── Constructeur JobRecord pour vault_write ───────────────────────────────────

/// Builds a Curate [`JobRecord`] from a `vault_write` request.
///
/// `note_id` is a ULID **pre-allocated** at enqueue time and **honoured** by the
/// curate worker via `Vault::write_note_with_id` — the write-time ID matches the
/// stored ID. This guarantees that wikilinks `[[section:ulid]]` built from this
/// `note_id` resolve correctly.
///
/// # Fields
///
/// - `spec.kind` = `Job::Curate(CurateSpec { note_id, tenant_id })`
/// - `spec.class` = `JobClass::Agent` (triggered by an MCP agent)
/// - `spec.mode` = `JobMode::Batch` (normal processing)
/// - `spec.scope` = `JobScope::SingleNote`
/// - `spec.priority` = `JobPriority::High` (`vault_write` is triggered on demand)
/// - `scheduling.trigger` = `TriggerSource::Demand`
/// - `lifecycle.status` = `JobStatus::Pending`
///
/// The `title`, `body`, `author`, `tags`, and `section_hint` fields are carried in
/// the `CurateSpec`. The curate worker reads them to create the note via
/// `write_note_with_id(fm, body, NoteId(spec.note_id))`.
///
/// `tenant` is the **effective** tenant derived from the JWT — never `req.tenant_id`.
/// The spec consumed by the worker must not carry a tenant injected by the request body.
fn build_curate_job_record(req: &VaultWriteRequest, note_id: Ulid, tenant: &str) -> JobRecord {
    let now = Utc::now();
    let class = JobClass::Agent;

    // F-41 — Parser l'expected_sha256 hex → [u8; 32].
    // Un hex invalide (longueur ≠ 64 ou caractères non-hex) est ignoré silencieusement
    // pour maintenir la rétrocompat : le job sera traité comme inconditionnel plutôt
    // que refusé à l'enqueue (fail-open, documenté). Le client verra Written au lieu
    // de Conflict si le hash était périmé — acceptable sur des requêtes malformées.
    //
    // INVARIANT C1 (ne PAS supprimer en refactorant) : ce fail-open n'est désormais
    // atteint avec un sha invalide QUE pour `note_id = None` (ULID frais → création
    // pure, aucun risque de clobber). Le chemin overwrite (note_id présent + sha
    // malformé) est rejeté en amont par la garde C1 du handler `vault_write`
    // (return 400 AVANT cet appel). Si C1 disparaît, ce fail-open redevient un
    // clobber aveugle sur overwrite — re-rejeter le sha invalide ici dans ce cas.
    let expected_sha256: Option<[u8; 32]> =
        req.expected_sha256.as_deref().and_then(parse_sha256_hex);

    JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id,
                tenant_id: tenant.to_owned(),
                // Contenu porté dans le spec — le handler Apalis crée la note vault.
                title: Some(req.title.clone()),
                body: Some(req.body.clone()),
                author: req.author.clone(),
                tags: req.tags.clone(),
                section_hint: req.section_hint.clone(),
                // F-41 — hash attendu pour l'optimistic-lock (None = inconditionnel).
                expected_sha256,
            }),
            class,
            mode: JobMode::Batch,
            // VaultWide : Phase 1.2 — Notes(vec![note_id]) disponible Phase 1.3
            scope: JobScope::VaultWide,
            priority: JobPriority::High,
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

// ── Helpers d'audit ───────────────────────────────────────────────────────────

/// Extracts the `request_id` from the `X-Request-ID` header, or generates a fresh ULID.
fn extract_request_id(headers: &HeaderMap) -> String {
    headers
        .get("X-Request-ID")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ulid::Ulid::new().to_string())
}

/// Builds an `HttpAuditActor` from the `TrustContext`.
///
/// Each variant extracts its identity appropriately.
/// `Unauthenticated` returns empty fields (401 `auth_failure` case).
fn actor_from_trust(trust: &TrustContext) -> HttpAuditActor {
    match trust {
        TrustContext::BearerToken { kid, sub, aud, .. } => HttpAuditActor {
            kid: kid.clone(),
            sub: sub.clone(),
            aud: aud.clone(),
        },
        TrustContext::Mtls { cn, .. } => HttpAuditActor {
            kid: format!("mtls:{cn}"),
            sub: cn.clone(),
            aud: "gradatum".into(),
        },
        TrustContext::Studio { user, .. } => HttpAuditActor {
            kid: "studio".into(),
            sub: user.clone(),
            aud: "gradatum-studio".into(),
        },
        TrustContext::Unauthenticated => HttpAuditActor {
            kid: String::new(),
            sub: String::new(),
            aud: String::new(),
        },
    }
}

/// Emits an `auth_failure` audit event with outcome `denied`.
///
/// Called on the 401/403 paths in the write handlers.
/// I/O errors from the sink are logged at WARN level and not propagated — the handler
/// always returns the HTTP error code even when the audit fails.
async fn emit_auth_failure_audit(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    request_id: &str,
    error_msg: &str,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "auth_failure".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: format!("{}/main", tenant_id),
        note_id: None,
        content_hash: None,
        outcome: "denied".into(),
        curator: None,
        request_id: request_id.into(),
    };
    // Erreur I/O audit non fatale — loguée sans propager.
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, error_msg = error_msg, "audit emit auth_failure échoué");
    }
}

/// Emits a `vault_write_rejected` audit event on an early-return 400/409.
///
/// Traces `vault_write` validation/guard rejections (malformed `note_id`, malformed
/// SHA on overwrite, overwrite without `expected_sha256`) so that forgery or clobber
/// attempts leave a trace — same pattern as `emit_auth_failure_audit`.
///
/// - `outcome`: distinct reason code (`rejected_400_bad_note_id`, `rejected_400_bad_sha`,
///   `rejected_409_overwrite_no_sha`) for SIEM correlation.
/// - `note_id`: the provided/resolved value if available, `None` otherwise.
///
/// Sink I/O errors are non-fatal — logged at WARN and not propagated (the handler
/// always returns the HTTP error code even when the audit fails).
async fn emit_write_rejection_audit(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    locus: &str,
    request_id: &str,
    outcome: &str,
    note_id: Option<String>,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_write_rejected".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: locus.into(),
        note_id,
        content_hash: None,
        outcome: outcome.into(),
        curator: None,
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, outcome = outcome, "audit emit vault_write_rejected échoué");
    }
}

// ── vault_write ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_write`
///
/// Enqueues note creation through the curator pipeline (`gradatum_jobs`).
///
/// Uses `state.job_store` (trait [`gradatum_core::QueueStore`], `gradatum_jobs` table).
///
/// # Returns
///
/// - **202 Accepted** + JSON [`EnqueuedResponseUlid`] — job enqueued; `poll_url` and
///   `note_id` are provided. `note_id` is the pre-allocated ULID, usable immediately
///   via `vault_read`.
/// - **401 Unauthorized** — missing or invalid bearer. Audit `auth_failure` emitted.
/// - **403 Forbidden** — ACL default-deny (consumer not configured). Audit `auth_failure` emitted.
/// - **500 Internal Server Error** — job construction or DB enqueue failure.
pub async fn vault_write(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(req): Json<VaultWriteRequest>,
) -> Result<(StatusCode, Json<EnqueuedResponseUlid>), StatusCode> {
    let start = Instant::now();
    let request_id = extract_request_id(&headers);

    if !trust.is_authenticated() {
        emit_auth_failure_audit(
            &state,
            &trust,
            &req.tenant_id,
            &request_id,
            "unauthenticated",
        )
        .await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant effectif DÉRIVÉ du JWT, jamais du body.
    // Refuse 403 tout body tenant_id divergent ou contexte sans tenant. Après cette
    // garde, le tenant effectif (== JWT, garanti "main" par Lots 1+2) est la seule
    // source de vérité pour le locus et l'index.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &tenant, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    // Construire le JobRecord Curate avec le contenu complet (title, body, tags, etc.)
    // dans le CurateSpec. Le handler Apalis curate récupère ces champs pour créer la note.
    // note_id_prealloc : ULID préalloué, honoré par le worker via write_note_with_id (fix C v0.3.7).
    // Il est exposé directement dans la réponse pour éviter au client un poll inutile.
    // Fix B — résolution du note_id préalloué.
    // Absent → ULID frais (rétrocompat). Invalide → 400. Valide → honoré.
    let note_id_prealloc = match req.note_id.as_deref() {
        None => Ulid::new(),
        Some(s) => match Ulid::from_string(s) {
            Ok(id) => id,
            Err(_) => {
                // C2 (A09) — tracer la tentative (note_id fourni = string brute non-ULID).
                emit_write_rejection_audit(
                    &state,
                    &trust,
                    &tenant,
                    &locus,
                    &request_id,
                    "rejected_400_bad_note_id",
                    Some(s.to_string()),
                )
                .await;
                return Err(StatusCode::BAD_REQUEST);
            }
        },
    };

    // C1 — anti fail-open sha, scopé au chemin overwrite.
    // build_curate_job_record parse expected_sha256 avec parse_sha256_hex qui fail-open
    // silencieusement (None) sur format invalide → sur un overwrite, le worker écrirait
    // INCONDITIONNELLEMENT (clobber aveugle malgré un sha "fourni"). On rejette tout
    // expected_sha256 invalide quand un note_id est présent (overwrite potentiel).
    // Note : un write SANS note_id ne peut pas clobber (ULID frais) → non concerné,
    // chemin création 100% inchangé.
    // ⚠️ Cet anti-fail-open DOIT précéder la garde 409 ci-dessous (ordre non négociable).
    if req.note_id.is_some() {
        if let Some(sha) = req.expected_sha256.as_deref() {
            if parse_sha256_hex(sha).is_none() {
                // C2 (A09) — tracer la tentative d'overwrite avec sha malformé.
                emit_write_rejection_audit(
                    &state,
                    &trust,
                    &tenant,
                    &locus,
                    &request_id,
                    "rejected_400_bad_sha",
                    Some(note_id_prealloc.to_string()),
                )
                .await;
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    // Fix B — garde overwrite. note_id valide visant une note EXISTANTE sans
    // expected_sha256 → 409 (read-modify-write obligatoire ; le worker write_if_match
    // re-valide le hash, race-safe).
    // C3 (INFO) : le lookup est borné à self.vault.tenant_id (VaultId "main" interne,
    // NON injecté par le client) — pas d'oracle ni de clobber cross-tenant en mono-vault.
    // À préserver explicitement si multi-tenant est implémenté.
    // une erreur I/O ≠ "absente" → 500, jamais de clobber sous erreur.
    if req.note_id.is_some() && req.expected_sha256.is_none() {
        match state
            .vault
            .read_note_by_id(&note_id_prealloc.to_string())
            .await
        {
            Ok(_) => {
                // C2 (A09) — tracer la tentative de clobber (overwrite sans expected_sha256).
                emit_write_rejection_audit(
                    &state,
                    &trust,
                    &tenant,
                    &locus,
                    &request_id,
                    "rejected_409_overwrite_no_sha",
                    Some(note_id_prealloc.to_string()),
                )
                .await;
                return Err(StatusCode::CONFLICT);
            }
            Err(gradatum_core::error::GradatumError::NoteNotFound(_)) => { /* absent → prealloc-create */
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    let record = build_curate_job_record(&req, note_id_prealloc, &tenant);
    let job_ulid = state
        .job_store
        .enqueue(record)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Émettre l'audit (T7 : toujours émettre si enqueue OK).
    let job_id_str = job_ulid.to_string();
    let note_id_str = note_id_prealloc.to_string();
    let duration_ms = start.elapsed().as_millis() as i64;
    let audit_evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_write".into(),
        actor: actor_from_trust(&trust),
        tenant_id: tenant.clone(),
        locus: locus.clone(),
        note_id: Some(note_id_str.clone()),
        content_hash: None,
        outcome: "queued".into(),
        curator: Some(serde_json::json!({ "job_id": job_id_str, "duration_ms": duration_ms })),
        request_id: request_id.clone(),
    };
    if let Err(e) = state.audit.record(audit_evt).await {
        tracing::warn!(error = %e, "audit emit vault_write échoué — non fatal");
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(EnqueuedResponseUlid {
            job_id: job_id_str,
            status: "queued",
            poll_url: format!("/api/v1/jobs/{job_ulid}/v2"),
            note_id: note_id_str,
        }),
    ))
}

// ── vault_classify ────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_classify`
///
/// Explicitly re-classifies a note through the curator pipeline.
///
/// # Status
///
/// Not implemented in v0.4.x. Classification is performed automatically during
/// `vault_write` (Curate pipeline). A dedicated endpoint is planned for a future version.
///
/// # Returns
///
/// - **501 Not Implemented** — always, regardless of a valid request.
/// - **401** / **403** — auth/ACL check performed first (audit `auth_failure` emitted).
pub async fn vault_classify(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(req): Json<VaultClassifyRequest>,
) -> impl IntoResponse {
    let request_id = extract_request_id(&headers);

    if !trust.is_authenticated() {
        emit_auth_failure_audit(
            &state,
            &trust,
            &req.tenant_id,
            &request_id,
            "unauthenticated",
        )
        .await;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = match effective_tenant(&trust, &req.tenant_id) {
        Ok(t) => t.to_owned(),
        Err(code) => return code.into_response(),
    };
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &tenant, &request_id, "acl_deny").await;
        return StatusCode::FORBIDDEN.into_response();
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "vault_classify not implemented in v0.4.x",
            "hint": "classification is performed automatically during vault_write"
        })),
    )
        .into_response()
}

// ── vault_downgrade ───────────────────────────────────────────────────────────

/// `POST /api/v1/vault_downgrade` — async queue variant (not routed; superseded by `notes::vault_downgrade`).
///
/// Enqueues a note downgrade through the curator pipeline.
/// Superseded by `notes::vault_downgrade` (synchronous 200) in the `api_v1` router.
/// Kept for future use (worker-based async downgrade via queue).
///
/// # Returns
///
/// - **202 Accepted** + JSON [`EnqueuedResponse`].
/// - **401** / **403** — audit `auth_failure` emitted.
/// - **500** — see [`vault_write`].
#[allow(dead_code)] // non câblée dans le routeur depuis Phase 2.1.2 — remplacée par notes::vault_downgrade sync
pub async fn vault_downgrade(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(req): Json<VaultDowngradeRequest>,
) -> Result<(StatusCode, Json<EnqueuedResponse>), StatusCode> {
    let start = Instant::now();
    let request_id = extract_request_id(&headers);

    if !trust.is_authenticated() {
        emit_auth_failure_audit(
            &state,
            &trust,
            &req.tenant_id,
            &request_id,
            "unauthenticated",
        )
        .await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &tenant, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    let payload = bincode::serde::encode_to_vec(&req, bincode::config::standard())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let job_id = state
        .queue
        .enqueue(NewJob {
            tenant_id: tenant.clone(),
            kind: "downgrade".to_string(),
            payload,
            max_attempts: 5,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let duration_ms = start.elapsed().as_millis() as i64;
    let audit_evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_downgrade".into(),
        actor: actor_from_trust(&trust),
        tenant_id: tenant.clone(),
        locus: locus.clone(),
        note_id: Some(req.note_id.clone()),
        content_hash: None,
        outcome: "queued".into(),
        curator: Some(serde_json::json!({ "job_id": job_id, "duration_ms": duration_ms })),
        request_id,
    };
    if let Err(e) = state.audit.record(audit_evt).await {
        tracing::warn!(error = %e, "audit emit vault_downgrade échoué — non fatal");
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(EnqueuedResponse {
            job_id,
            status: "queued",
            poll_url: format!("/api/v1/jobs/{job_id}"),
        }),
    ))
}

// ── Tests unitaires ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod build_record_tests {
    use super::*;
    use gradatum_dto::VaultWriteRequest;

    fn minimal_req(note_id: Option<String>) -> VaultWriteRequest {
        VaultWriteRequest {
            title: "t".into(),
            body: "b".into(),
            author: None,
            tags: vec![],
            section_hint: Some("decisions".into()),
            tenant_id: "main".into(),
            expected_sha256: None,
            note_id,
        }
    }

    #[test]
    fn build_curate_job_record_honors_provided_note_id() {
        let fixed = Ulid::from_string("01KTW02W2YQH5XT71ZABFXTQX8").unwrap();
        let record = build_curate_job_record(&minimal_req(None), fixed, "main");
        match record.spec.kind {
            Job::Curate(spec) => {
                assert_eq!(spec.note_id, fixed);
                // P0 cross-tenant : le spec porte le tenant dérivé, pas le body.
                assert_eq!(spec.tenant_id, "main");
            }
            other => panic!("expected Curate, got {other:?}"),
        }
    }
}
