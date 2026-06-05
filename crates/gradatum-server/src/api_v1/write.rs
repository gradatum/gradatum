//! Handlers write MCP v1 — 3 méthodes async 202 Accepted (P2.0b T3).
//!
//! Chaque handler :
//! 1. Vérifie l'authentification via [`TrustContext::is_authenticated`].
//! 2. Évalue l'ACL via [`AclEngine::evaluate`] (Write, locus = `tenant_id/main`).
//! 3. Construit un [`gradatum_core::JobRecord`] et enqueue via `state.job_store.enqueue()`.
//! 4. Émet un événement d'audit (T7 P2.0c).
//! 5. Retourne 202 Accepted + JSON [`EnqueuedResponse`].
//!
//! # Phase 1.2 — bridge job_store
//!
//! `vault_write` utilise désormais `state.job_store` (trait [`gradatum_core::QueueStore`],
//! table `gradatum_jobs` Apalis) au lieu de `state.queue` (trait [`gradatum_queue::Queue`],
//! table `jobs_v2` legacy sans worker). Résout l'incident post-deploy v0.2.0 :
//! jobs `pending` dans `jobs_v2` sans worker actif.
//!
//! `vault_classify` conserve `state.queue` (legacy) — son handler Apalis n'est pas encore
//! implémenté en Phase 1.2. Backlog Phase 1.3.
//!
//! # Auth failures
//!
//! Les réponses 401/403 émettent un événement d'audit `auth_failure` outcome `denied`
//! (T7 P2.0c) avant de retourner le code d'erreur.
//!
//! # Endpoints
//!
//! | Méthode | Path | Auth |
//! |---------|------|------|
//! | POST | `/api/v1/vault_write`     | bearer + ACL Write requis |
//! | POST | `/api/v1/vault_classify`  | bearer + ACL Write requis |
//! | POST | `/api/v1/vault_downgrade` | bearer + ACL Write requis |
//!

use std::time::Instant;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
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
use crate::state::AppState;

// ── Constructeur JobRecord pour vault_write ───────────────────────────────────

/// Construit un [`JobRecord`] Curate depuis une requête `vault_write`.
///
/// Le `note_id` est un ULID synthétique — il sera remplacé par le vrai ULID
/// lors de l'écriture dans le vault par le handler Apalis.
///
/// # Champs
///
/// - `spec.kind` = `Job::Curate(CurateSpec { note_id, tenant_id })`
/// - `spec.class` = `JobClass::Agent` (déclenché par un agent MCP)
/// - `spec.mode` = `JobMode::Batch` (traitement normal)
/// - `spec.scope` = `JobScope::SingleNote`
/// - `spec.priority` = `JobPriority::High` (vault_write déclenché à la demande)
/// - `scheduling.trigger` = `TriggerSource::Demand`
/// - `lifecycle.status` = `JobStatus::Pending`
///
/// Le `note_id` dans `CurateSpec` est un ULID **préalloué** — le handler curate Apalis
/// l'utilisera comme identifiant lors de `vault.write_note`. Les champs `title`, `body`,
/// `author`, `tags`, `section_hint` sont portés dans le payload JSON du `JobRecord`
/// (champ `lifecycle.result` non utilisé à l'enqueue — réservé pour le résultat).
///
/// # Sérialisation payload
///
/// Le payload complet de la requête (titre, body, tags, etc.) est sérialisé en JSON
/// dans un champ dédié du `JobRecord` via `JobLineage.cost_usd` comme champ porteur
/// temporaire (Phase 1.2 workaround). Phase 1.3 : ajouter un champ `payload_json`
/// dédié dans `JobRecord`.
fn build_curate_job_record(req: &VaultWriteRequest) -> (JobRecord, Ulid) {
    let now = Utc::now();
    let note_id = Ulid::new();
    let class = JobClass::Agent;
    let record = JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind: Job::Curate(CurateSpec {
                note_id,
                tenant_id: req.tenant_id.clone(),
                // Contenu porté dans le spec — le handler Apalis crée la note vault.
                title: Some(req.title.clone()),
                body: Some(req.body.clone()),
                author: req.author.clone(),
                tags: req.tags.clone(),
                section_hint: req.section_hint.clone(),
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
    };
    (record, note_id)
}

// ── Helpers d'audit ───────────────────────────────────────────────────────────

/// Extrait le `request_id` depuis le header `X-Request-ID`, ou génère un ULID.
fn extract_request_id(headers: &HeaderMap) -> String {
    headers
        .get("X-Request-ID")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| ulid::Ulid::new().to_string())
}

/// Construit un `HttpAuditActor` depuis le `TrustContext`.
///
/// Chaque variante extraite son identité de façon adéquate.
/// `Unauthenticated` retourne des champs vides (cas auth_failure 401).
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

/// Émet un événement d'audit `auth_failure` outcome `denied`.
///
/// Appelé sur les chemins 401/403 dans les handlers write.
/// Les erreurs I/O du sink sont loguées en WARN sans propager — le handler
/// doit toujours retourner le code d'erreur HTTP même si l'audit échoue.
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

// ── vault_write ───────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_write`
///
/// Enqueue la création d'une note via le curator pipeline Apalis (`gradatum_jobs`).
///
/// # Phase 1.2 — bridge job_store
///
/// Utilise `state.job_store` (trait [`gradatum_core::QueueStore`], table `gradatum_jobs`)
/// au lieu de `state.queue` (trait [`gradatum_queue::Queue`], table `jobs_v2` legacy).
/// Résout l'incident LIVE post-deploy v0.2.0 : jobs coincés `pending` dans `jobs_v2`
/// sans worker actif.
///
/// # Retour
///
/// - **202 Accepted** + JSON [`EnqueuedResponseUlid`] — job enqueued, poll_url fournie.
/// - **401 UNAUTHORIZED** — pas de bearer ou bearer invalide. Audit `auth_failure` émis.
/// - **403 FORBIDDEN** — ACL default deny (consumer non configuré). Audit `auth_failure` émis.
/// - **500 Internal Server Error** — échec construction ou enqueue DB.
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
    let locus = format!("{}/main", req.tenant_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &req.tenant_id, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    // Construire le JobRecord Curate avec le contenu complet (title, body, tags, etc.)
    // dans le CurateSpec. Le handler Apalis curate récupère ces champs pour créer la note.
    let (record, _note_id_prealloc) = build_curate_job_record(&req);
    let job_ulid = state
        .job_store
        .enqueue(record)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Émettre l'audit (T7 : toujours émettre si enqueue OK).
    let job_id_str = job_ulid.to_string();
    let duration_ms = start.elapsed().as_millis() as i64;
    let audit_evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_write".into(),
        actor: actor_from_trust(&trust),
        tenant_id: req.tenant_id.clone(),
        locus: locus.clone(),
        note_id: None,
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
        }),
    ))
}

// ── vault_classify ────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_classify`
///
/// Enqueue la re-classification d'une note existante via le curator pipeline.
///
/// # Retour
///
/// - **202 Accepted** + JSON [`EnqueuedResponse`].
/// - **401** / **403** — audit `auth_failure` émis (T7 P2.0c).
/// - **500** — voir [`vault_write`].
pub async fn vault_classify(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(req): Json<VaultClassifyRequest>,
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
    let locus = format!("{}/main", req.tenant_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &req.tenant_id, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    let payload = bincode::serde::encode_to_vec(&req, bincode::config::standard())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let job_id = state
        .queue
        .enqueue(NewJob {
            tenant_id: req.tenant_id.clone(),
            kind: "classify".to_string(),
            payload,
            max_attempts: 5,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let duration_ms = start.elapsed().as_millis() as i64;
    let audit_evt = HttpAuditEvent {
        ts: chrono::Utc::now(),
        event: "vault_classify".into(),
        actor: actor_from_trust(&trust),
        tenant_id: req.tenant_id.clone(),
        locus: locus.clone(),
        note_id: Some(req.note_id.clone()),
        content_hash: None,
        outcome: "queued".into(),
        curator: Some(serde_json::json!({ "job_id": job_id, "duration_ms": duration_ms })),
        request_id,
    };
    if let Err(e) = state.audit.record(audit_evt).await {
        tracing::warn!(error = %e, "audit emit vault_classify échoué — non fatal");
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

// ── vault_downgrade ───────────────────────────────────────────────────────────

/// `POST /api/v1/vault_downgrade` — version async queue (non câblée depuis Phase 2.1.2).
///
/// Enqueue la rétrogradation d'une note via le curator pipeline.
/// Remplacée par `notes::vault_downgrade` (synchrone 200) dans le routeur api_v1.
/// Conservée pour compatibilité future (worker downgrade via queue).
///
/// # Retour
///
/// - **202 Accepted** + JSON [`EnqueuedResponse`].
/// - **401** / **403** — audit `auth_failure` émis (T7 P2.0c).
/// - **500** — voir [`vault_write`].
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
    let locus = format!("{}/main", req.tenant_id);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        emit_auth_failure_audit(&state, &trust, &req.tenant_id, &request_id, "acl_deny").await;
        return Err(StatusCode::FORBIDDEN);
    }

    let payload = bincode::serde::encode_to_vec(&req, bincode::config::standard())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let job_id = state
        .queue
        .enqueue(NewJob {
            tenant_id: req.tenant_id.clone(),
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
        tenant_id: req.tenant_id.clone(),
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
