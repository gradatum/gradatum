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
//! `vault_classify` delegates to [`crate::api_v1::logic::vault_classify_impl`] —
//! synchronous offline heuristic (zero LLM, zero egress).
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
//! | POST | `/api/v1/vault_classify`  | bearer + ACL Read required  |
//! | POST | `/api/v1/vault_downgrade` | bearer + ACL Write required |
//!

use std::time::Instant;

use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, StatusCode},
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
use crate::api_v1::tenant_guard::effective_write_vault;
use crate::state::AppState;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parses a 64-char lowercase hex SHA-256 string into `[u8; 32]`.
///
/// Returns `None` if the string is not exactly 64 valid hex characters.
/// Used to parse `VaultWriteRequest.expected_sha256` into `CurateSpec.expected_sha256`.
pub(crate) fn parse_sha256_hex(hex: &str) -> Option<[u8; 32]> {
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
pub(crate) fn build_curate_job_record(
    req: &VaultWriteRequest,
    note_id: Ulid,
    tenant: &str,
) -> JobRecord {
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
                // F-74 — ancre temporelle événementielle (None = created, comportement historique).
                // Déjà validée par vault_write_impl avant cet appel (400 si invalide).
                occurred_at: req.occurred_at.clone(),
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
pub(crate) fn actor_from_trust(trust: &TrustContext) -> HttpAuditActor {
    match trust {
        TrustContext::BearerToken {
            kid, sub, aud, jti, ..
        } => HttpAuditActor {
            kid: kid.clone(),
            // Frontière DTO (`HttpAuditActor.sub: String`) : `as_str` est
            // byte-identical, la ligne d'audit émise est inchangée.
            sub: sub.as_str().to_owned(),
            aud: aud.clone(),
            jti: jti.clone(),
        },
        TrustContext::Mtls { cn, .. } => HttpAuditActor {
            kid: format!("mtls:{cn}"),
            sub: cn.clone(),
            aud: "gradatum".into(),
            jti: None,
        },
        TrustContext::Studio { user, .. } => HttpAuditActor {
            kid: "studio".into(),
            sub: user.clone(),
            aud: "gradatum-studio".into(),
            jti: None,
        },
        // Unauthenticated — et, TrustContext étant #[non_exhaustive] (A3), toute
        // variante future : tracée comme identité vide (jamais une identité forgée).
        _ => HttpAuditActor {
            kid: String::new(),
            sub: String::new(),
            aud: String::new(),
            jti: None,
        },
    }
}

/// Emits an `auth_failure` audit event with outcome `denied`.
///
/// Called on the 401/403 paths in the write handlers.
/// I/O errors from the sink are logged at WARN level and not propagated — the handler
/// always returns the HTTP error code even when the audit fails.
pub(crate) async fn emit_auth_failure_audit(
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
        tracing::warn!(error = %e, error_msg = error_msg, "audit emit auth_failure failed");
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
pub(crate) async fn emit_write_rejection_audit(
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
        tracing::warn!(error = %e, outcome = outcome, "audit emit vault_write_rejected failed");
    }
}

/// Emits a `write_check_category_section` audit event when a section-category drift is detected.
///
/// **Non-bloquant par construction** (WARN-ONLY ABSOLU) — cette fonction ne peut jamais
/// faire échouer un `vault_write`. Sink I/O errors sont non-fatales (logged WARN).
///
/// Fournit une trace SIEM distincte de `vault_write_rejected` (les dérives sont des
/// avertissements, pas des rejets) — outcome = `"drift:<CAT>→expected:<section>"`.
pub(crate) async fn emit_drift_audit(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    request_id: &str,
    warning: &gradatum_core::write_check::DriftWarning,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "write_check_category_section".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: format!("{}/main", tenant_id),
        note_id: None,
        content_hash: None,
        outcome: format!(
            "drift:{}→expected:{}",
            warning.category, warning.expected_section
        ),
        curator: Some(serde_json::json!({
            "rule": warning.rule,
            "category": warning.category,
            "expected_section": warning.expected_section,
            "actual_section": warning.actual_section,
        })),
        request_id: request_id.into(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(
            error = %e,
            rule = warning.rule,
            "audit emit write_check_drift failed — non fatal"
        );
    }
}

/// Emits a `vault_read_rejected` audit event when a read-restrictive guard denies a read.
///
/// Symmetric counterpart to [`emit_write_rejection_audit`]: traces denied reads of
/// access-restricted sections (currently `identity`, since v0.7.3 — the soul of an agent
/// is private) so that cross-agent soul read attempts leave a SIEM trail.
///
/// - `outcome`: distinct reason code (e.g. `identity_read_denied_foreign_agent`) for
///   SIEM correlation.
/// - `note_id`: the resolved note id whose read was denied, `None` if unavailable.
///
/// A fresh request id is generated because the read path carries no `X-Request-ID`
/// correlation header. Sink I/O errors are non-fatal — logged at WARN and not
/// propagated (the handler still returns the 403 even if the audit write fails).
pub(crate) async fn emit_read_rejection_audit(
    state: &AppState,
    trust: &TrustContext,
    tenant_id: &str,
    locus: &str,
    outcome: &str,
    note_id: Option<String>,
) {
    let evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_read_rejected".into(),
        actor: actor_from_trust(trust),
        tenant_id: tenant_id.into(),
        locus: locus.into(),
        note_id,
        content_hash: None,
        outcome: outcome.into(),
        curator: None,
        request_id: Ulid::new().to_string(),
    };
    if let Err(e) = state.audit.record(evt).await {
        tracing::warn!(error = %e, outcome = outcome, "audit emit vault_read_rejected failed");
    }
}

// ── vault_write ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_write`
///
/// Thin wrapper — délègue la logique métier à [`crate::api_v1::logic::vault_write_impl`].
///
/// Enqueues note creation through the curator pipeline (`gradatum_jobs`).
/// The `request_id` is extracted from the `X-Request-ID` header (or generated
/// as a fresh ULID) and forwarded to `vault_write_impl` for audit correlation.
///
/// # Returns
///
/// - **202 Accepted** + JSON [`EnqueuedResponseUlid`] — job enqueued; `poll_url` and
///   `note_id` are provided. `note_id` is the pre-allocated ULID, usable immediately
///   via `vault_read`.
/// - **401 Unauthorized** — missing or invalid bearer. Audit `auth_failure` emitted.
/// - **403 Forbidden** — ACL default-deny (consumer not configured). Audit `auth_failure` emitted.
/// - **409 Conflict** — overwrite attempted without `expected_sha256`.
/// - **500 Internal Server Error** — job construction or DB enqueue failure.
pub async fn vault_write(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(req): Json<VaultWriteRequest>,
) -> Result<(StatusCode, Json<EnqueuedResponseUlid>), StatusCode> {
    let request_id = extract_request_id(&headers);

    crate::api_v1::logic::vault_write_impl(
        &state,
        &trust,
        req,
        &request_id,
        crate::api_v1::logic::FeatureWriteAuthority::External,
    )
    .await
    .map(|resp| (StatusCode::ACCEPTED, Json(resp)))
    .map_err(|e| crate::api_v1::logic::err_to_status(&e))
}

// ── vault_classify ────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_classify`
///
/// Classifie une note existante via l'heuristique offline (synchrone, zéro LLM).
///
/// Thin wrapper — délègue la logique métier à
/// [`crate::api_v1::logic::vault_classify_impl`].
///
/// # Returns
///
/// - **200 OK** + JSON [`crate::api_v1::dto::VaultClassifyResponse`] — classification
///   réussie. `confidence` vaut `0.9` (admitted), `0.5` (pending), ou `0.0` (rejected).
/// - **400 Bad Request** — `note_id` n'est pas un ULID valide.
/// - **401 Unauthorized** — bearer absent ou invalide.
/// - **403 Forbidden** — ACL Read refusée.
/// - **404 Not Found** — note absente du vault.
/// - **500 Internal Server Error** — erreur I/O vault.
pub async fn vault_classify(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    _headers: HeaderMap,
    Json(req): Json<VaultClassifyRequest>,
) -> Result<Json<crate::api_v1::dto::VaultClassifyResponse>, StatusCode> {
    crate::api_v1::logic::vault_classify_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| crate::api_v1::logic::err_to_status(&e))
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
// Non câblée dans le routeur depuis Phase 2.1.2 — remplacée par notes::vault_downgrade sync.
// Conservée pour usage futur (async downgrade via queue) — non supprimée intentionnellement.
// Le lint dead_code se déclenche sur le bin (pas sur la lib car `pub`) — #[allow] justifié.
#[allow(dead_code)]
pub async fn vault_downgrade(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(mut req): Json<VaultDowngradeRequest>,
) -> Result<(StatusCode, Json<EnqueuedResponse>), StatusCode> {
    let start = Instant::now();
    let request_id = extract_request_id(&headers);

    if !trust.is_authenticated() {
        emit_auth_failure_audit(
            &state,
            &trust,
            req.tenant_id.as_ref().map_or("", |t| t.as_str()),
            &request_id,
            "unauthenticated",
        )
        .await;
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    // C1 (F-63, EX-C1-1/2) : + grant write exigé à flag ON (l'enqueue est une écriture).
    let tenant = effective_write_vault(&state, &trust, req.tenant_id.as_ref())
        .await
        .map_err(|r| r.status())?;
    let locus = format!("{}/main", tenant);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &tenant, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    // Lot A1 : le payload enqueué doit porter le tenant EFFECTIF (résolu du JWT), jamais le
    // `tenant_id` optionnel du client. On l'injecte avant l'encode bincode — le champ est
    // ainsi toujours `Some(_)` sur le fil (pas de `skip_serializing_if` déclenché → décodage
    // positionnel bincode aligné avec le miroir `dispatch.rs`).
    req.tenant_id = Some(gradatum_core::scope::TenantId::new(&tenant));
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
        tracing::warn!(error = %e, "audit emit vault_downgrade failed — non fatal");
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
            tenant_id: Some("main".into()),
            expected_sha256: None,
            note_id,
            occurred_at: None,
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
