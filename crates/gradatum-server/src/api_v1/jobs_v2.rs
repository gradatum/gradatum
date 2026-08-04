//! Job introspection endpoints.
//!
//! Implements the five job API endpoints:
//!
//! | Method | Path | Description |
//! |--------|------|-------------|
//! | GET  | `/api/v1/jobs`             | Paginated list (cursor-based) |
//! | GET  | `/api/v1/jobs/:id`         | Job detail |
//! | POST | `/api/v1/jobs`             | Job creation with `Idempotency-Key` |
//! | POST | `/api/v1/jobs/:id/cancel`  | Cancellation (409 if Running) |
//! | GET  | `/api/v1/jobs/:id/events`  | SSE event stream |
//!
//! # Auth
//!
//! All endpoints require an authenticated bearer JWT (`401` otherwise) **and**
//! an ACL grant on the locus `main/jobs`:
//! - read (`list`/`detail`/`events`) → [`AclOp::Read`]
//! - write / trigger (`create`/`cancel`) → [`AclOp::Write`] (`403` otherwise)
//!
//! `create_job` is a **mutating** endpoint (real Purge, Curate injection):
//! missing authorization would allow untrusted callers to trigger destructive jobs.
//! The loopback bind mitigates exposure, but application-level authorization is now
//! explicit and consistent with the rest of `api_v1`.
//! Fine-grained multi-user JWT authorization is planned for Gold.
//!
//! # Idempotency-Key
//!
//! The `Idempotency-Key` header is required on `POST /api/v1/jobs`.
//! Missing → 400 Bad Request.
//! Known key → 200 `{ id, idempotent: true }` without creating a new job.
//!
//! # SSE (Last-Event-ID)
//!
//! When `Last-Event-ID` is present, broadcast events are filtered from the
//! head of the channel (shared circular buffer of capacity 256).
//! Gap: if the buffer was recycled since the last `Last-Event-ID`, replay
//! restarts from 0.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Extension,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use ulid::Ulid;

use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::scope::VaultId;
use gradatum_core::{JobFilter, JobOrder, JobRecord, JobStatus, QueueEvent};

use crate::state::AppState;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{idempotency_insert, idempotency_lookup};

/// Vault namespace ciblé par ces handlers (dimension NAMESPACE, distincte du
/// principal `TenantId` porté par le JWT).
///
/// Jobs non body-scoped ; ACL évaluée sur le locus dédié `main/jobs` (cohérent avec
/// `dashboard`/`lessons`). Déploiement single-vault : toujours `main`. Point de
/// résolution **typé** remplaçant l'ancien `const TENANT: &str`.
#[must_use]
pub fn target_vault() -> VaultId {
    VaultId::new("main")
}

/// ACL locus for the job endpoints — `main/jobs`.
///
/// Read (`list`/`detail`/`events`) → [`AclOp::Read`]; write/trigger
/// (`create`/`cancel`) → [`AclOp::Write`]. Consistent with other `api_v1`
/// handlers (`forget`, `write`, `dashboard`, `history`, …).
fn jobs_locus() -> String {
    format!("{}/jobs", target_vault())
}

/// L1+L2 — tenant filter for the job queue endpoints (get/cancel/list/events).
///
/// - `multi_tenant.enabled = false` (défaut LIVE) → `None` : aucune clause tenant,
///   comportement **byte-identical** (le store voit toute la queue globale).
/// - `enabled = true` → `Some(tenant JWT)` : la queue est scopée au tenant du
///   Bearer, fermant la disclosure/DoS cross-tenant (get/cancel d'autrui → 404).
///   Un contexte sans tenant (non-Bearer : mTLS/Studio) n'a aucun tenant à ON →
///   `403` (pas d'accès jobs sans tenant identifié).
///
/// # Errors
/// `StatusCode::FORBIDDEN` si `multi_tenant` est ON et que le contexte ne porte
/// pas de tenant.
pub(crate) fn job_tenant_filter<'a>(
    state: &AppState,
    trust: &'a TrustContext,
) -> Result<Option<&'a str>, StatusCode> {
    if state.server_config.multi_tenant.enabled {
        match trust.tenant_id() {
            Some(t) => Ok(Some(t.as_str())),
            None => Err(StatusCode::FORBIDDEN),
        }
    } else {
        Ok(None)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DTOs
// ─────────────────────────────────────────────────────────────────────────────

/// Response for `GET /api/v1/jobs` — paginated list.
#[derive(Debug, Serialize)]
pub struct JobListResponse {
    /// List of `JobRecord` values.
    pub items: Vec<JobRecord>,
    /// Cursor for the next page — `None` on the last page.
    pub next_cursor: Option<String>,
}

/// Query parameters for `GET /api/v1/jobs`.
#[derive(Debug, Deserialize)]
pub struct JobListQuery {
    /// Filter by status (single value).
    pub status: Option<String>,
    /// Filter by kind (single value).
    pub kind: Option<String>,
    /// Date lower-bound (RFC 3339 UTC) — **backwards-compat alias** for `created_after`.
    ///
    /// When both `created_after` and `since` are supplied, `created_after` takes precedence.
    pub since: Option<String>,
    /// Exclusive lower-bound date (RFC 3339 UTC) — jobs created strictly after this timestamp.
    pub created_after: Option<String>,
    /// Exclusive upper-bound date (RFC 3339 UTC) — jobs created strictly before this timestamp.
    ///
    /// Combined with `created_after` (or `since`) to isolate a time range.
    pub created_before: Option<String>,
    /// Sort order: `asc` (default, oldest first) or `desc` (newest first).
    pub order: Option<String>,
    /// Number of results (default 50, max 200).
    pub limit: Option<usize>,
    /// Pagination cursor (ULID of the last returned job).
    pub cursor: Option<String>,
}

/// Request body for `POST /api/v1/jobs`.
///
/// `scheduling` and `lineage` are deserialized but partially consumed.
/// `lineage.triggered_by` is extracted in `build_job_record_from_spec`.
#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    /// Job specification — object `{ "kind": { "type": ..., "data": ... } }`.
    ///
    /// `spec.kind` is deserialized into [`gradatum_core::Job`] inside
    /// `build_job_record_from_spec`.
    pub spec: serde_json::Value,
    /// Optional scheduling (scheduled_at, deadline, etc.) — not consumed in the current version.
    #[allow(dead_code)]
    pub scheduling: Option<serde_json::Value>,
    /// Optional lineage (triggered_by, parent_job).
    pub lineage: Option<serde_json::Value>,
}

/// Response for `POST /api/v1/jobs`.
#[derive(Debug, Serialize)]
pub struct CreateJobResponse {
    /// ULID of the created (or existing, if idempotent) job.
    pub id: String,
    /// `true` when an existing job was returned (known `Idempotency-Key`).
    pub idempotent: bool,
}

/// Response for `POST /api/v1/jobs/:id/cancel`.
#[derive(Debug, Serialize)]
pub struct CancelJobResponse {
    /// ULID of the cancelled job.
    pub id: String,
    /// Status after the operation.
    pub status: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parses a sort order from a string (case-insensitive).
///
/// `asc` → [`JobOrder::CreatedAsc`] · `desc` → [`JobOrder::CreatedDesc`].
/// Returns `None` for unknown values (→ 400 at the handler level).
fn parse_order(s: &str) -> Option<JobOrder> {
    match s.to_lowercase().as_str() {
        "asc" => Some(JobOrder::CreatedAsc),
        "desc" => Some(JobOrder::CreatedDesc),
        _ => None,
    }
}

/// Parses an optional RFC 3339 UTC date from a query parameter.
///
/// `None` → `Ok(None)` (filter disabled). `Some(invalid)` → `Err(())` (→ 400).
fn parse_opt_rfc3339(opt: Option<&String>) -> Result<Option<chrono::DateTime<Utc>>, ()> {
    match opt {
        Some(s) => s.parse::<chrono::DateTime<Utc>>().map(Some).map_err(|_| ()),
        None => Ok(None),
    }
}

/// Parses a job status from a string (case-insensitive).
fn parse_status(s: &str) -> Option<JobStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Some(JobStatus::Pending),
        "running" => Some(JobStatus::Running),
        "waiting" => Some(JobStatus::Waiting),
        "done" => Some(JobStatus::Done),
        "failed" => Some(JobStatus::Failed),
        "dlq" => Some(JobStatus::DLQ),
        "cancelled" | "canceled" => Some(JobStatus::Cancelled),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────────

/// `GET /api/v1/jobs` — Paginated job list.
///
/// # Query parameters
///
/// - `status`: filter by status (e.g. `pending`, `running`, `dlq`)
/// - `kind`: filter by kind (e.g. `Curate`, `Embed`)
/// - `since`: **backwards-compat alias** for `created_after` (RFC 3339 UTC). `created_after` takes precedence.
/// - `created_after`: exclusive lower-bound date (RFC 3339 UTC)
/// - `created_before`: exclusive upper-bound date (RFC 3339 UTC) — range with `created_after`
/// - `order`: `asc` (default) or `desc` (newest first)
/// - `limit`: result count (default 50, max 200)
/// - `cursor`: ULID of the last returned job (valid for both sort orders)
///
/// # Responses
///
/// - **200 OK** + `{ items: [JobRecord], next_cursor: Option<String> }`.
///   `next_cursor` is the `id` of the last returned item; pass it as `cursor`
///   (with the same `order`) to retrieve the next page in the same direction.
/// - **400 Bad Request**: malformed query parameter (non-RFC 3339 date, unknown
///   status or order, non-ULID cursor)
/// - **401 Unauthorized**: missing or invalid bearer token
/// - **403 Forbidden**: ACL Read denied on `main/jobs`
/// - **500 Internal Server Error**: SQLite failure
pub async fn list_jobs(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(query): Query<JobListQuery>,
) -> Result<Json<JobListResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // T9 (A3-handlers) : OFF = ACL Read legacy sur `main/jobs` (byte-identical) ; ON = ACL
    // cible + grant read + statut actif du vault propre. Jobs non vault-scopés (store
    // global) : seul l'enforcement ACL/grant est câblé, le vault résolu est ignoré.
    crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), "jobs").await?;

    let limit = query.limit.unwrap_or(50).clamp(1, 200);

    let status_filter = match &query.status {
        Some(s) => match parse_status(s) {
            Some(st) => Some(st),
            None => return Err(StatusCode::BAD_REQUEST),
        },
        None => None,
    };

    let cursor_filter = match &query.cursor {
        Some(c) => match c.parse::<Ulid>() {
            Ok(u) => Some(u),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        },
        None => None,
    };

    let order = match &query.order {
        Some(o) => match parse_order(o) {
            Some(ord) => ord,
            None => return Err(StatusCode::BAD_REQUEST),
        },
        None => JobOrder::CreatedAsc,
    };

    // `created_after` explicite prime sur l'alias rétrocompat `since`.
    let created_after = parse_opt_rfc3339(query.created_after.as_ref().or(query.since.as_ref()))
        .map_err(|()| StatusCode::BAD_REQUEST)?;
    let created_before =
        parse_opt_rfc3339(query.created_before.as_ref()).map_err(|()| StatusCode::BAD_REQUEST)?;

    // L1 : filtre tenant (OFF = None byte-identical, ON = Some(tenant JWT)).
    let tenant_filter = job_tenant_filter(&state, &trust)?.map(str::to_owned);

    // Demande limit + 1 pour détecter s'il y a une page suivante.
    let filter = JobFilter {
        status: status_filter,
        kind: query.kind.clone(),
        created_after,
        created_before,
        order,
        cursor: cursor_filter,
        // +1 pour détecter has_more sans double query
        limit: limit + 1,
        tenant: tenant_filter,
        ..Default::default()
    };

    match state.job_store.list(filter).await {
        Ok(mut items) => {
            let has_more = items.len() > limit;
            if has_more {
                items.truncate(limit);
            }
            let next_cursor = if has_more {
                items.last().map(|r| r.id.to_string())
            } else {
                None
            };
            Ok(Json(JobListResponse { items, next_cursor }))
        }
        Err(e) => {
            tracing::error!(error = %e, "list_jobs: QueueStore.list() failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /api/v1/jobs/:id` — Job detail.
///
/// Returns the full `JobRecord` with the SQL-synchronized status.
///
/// # Responses
///
/// - **200 OK** + full `JobRecord` JSON
/// - **400 Bad Request**: ID is not a valid ULID
/// - **401 Unauthorized**: missing or invalid bearer token
/// - **403 Forbidden**: ACL Read denied on `main/jobs`
/// - **404 Not Found**: job does not exist
/// - **500 Internal Server Error**: SQLite failure
pub async fn get_job_v2(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
) -> Result<Json<JobRecord>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // T9 (A3-handlers) : OFF = ACL Read legacy sur `main/jobs` (byte-identical) ; ON = ACL
    // cible + grant read + statut actif du vault propre. Jobs non vault-scopés (store
    // global) : seul l'enforcement ACL/grant est câblé, le vault résolu est ignoré.
    crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), "jobs").await?;

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // L1 : à ON, un job d'un autre tenant lit `None` → 404 (anti-disclosure).
    let tf = job_tenant_filter(&state, &trust)?;
    match state.job_store.get(id, tf).await {
        Ok(Some(record)) => Ok(Json(record)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "get_job_v2: QueueStore.get() failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP `job_status` — vue compacte de l'état d'un job (F-63 « tout MCP natif »)
// ─────────────────────────────────────────────────────────────────────────────

/// Compact, consumer-oriented view of a job's state — payload of the `job_status` MCP tool.
///
/// Distilled from the full [`JobRecord`] returned by [`get_job_v2`] to answer a single
/// question: **keep polling, or conclude?** The `terminal` flag is the load-bearing field —
/// derived from [`JobStatus::is_terminal`] (the single source of truth), so a consumer never
/// hardcodes the terminal set (which would silently rot if the enum evolves).
///
/// # Status vocabulary
///
/// `status` is the raw [`JobStatus`] exactly as `/jobs/{id}/v2` serializes it (`"Pending"`,
/// `"Running"`, `"Waiting"`, `"Done"`, `"Failed"`, `"DLQ"`, `"Cancelled"`, `"Conflict"`).
/// It is read from **`lifecycle.status`** of the record — NOT the flat `.status` seen on the
/// `202` enqueue response (`"queued"`) nor on the legacy `/jobs/{i64}` endpoint (lowercase
/// `"pending"`/`"done"`/…), which are distinct vocabularies and are not authoritative here.
///
/// # Terminal vs transient
///
/// - Terminal (`terminal = true`): `Done`, `DLQ`, `Cancelled`, `Conflict` — no further
///   transition; the caller concludes.
/// - Transient (`terminal = false`): `Pending`, `Running`, `Waiting`, **`Failed`**. `Failed`
///   is transient on purpose: a retry is pending, so the job will still reach `Done` (retry
///   succeeded) or `DLQ` (retries exhausted). A caller must NOT conclude on `Failed`.
#[derive(Debug, Clone, Serialize)]
pub struct JobStatusView {
    /// ULID of the job (echoes the request).
    pub job_id: String,
    /// Raw status, identical to `lifecycle.status` of `/jobs/{id}/v2`.
    pub status: JobStatus,
    /// `true` iff the status is terminal ([`JobStatus::is_terminal`]) — THE decision field.
    pub terminal: bool,
    /// Attempts made so far (`retry.count`).
    pub attempts: u32,
    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,
    /// Completion timestamp (UTC) — `None` until the job reaches a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Last recorded error message (`retry.last_error`) — surfaced for `Failed`/`DLQ`.
    /// A silent `Failed`/`DLQ` is worthless: the reason travels with the state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optimistic-lock conflict payload (`lifecycle.result.conflict_payload`) — present
    /// only when `status == Conflict`. Carries `current_sha256` / `attempted_sha256` so a
    /// caller can resolve the conflict. Reachable on the RMW `vault_write` path (F-41): a stale
    /// `expected_sha256` fails the compare-and-swap and marks the job `Conflict`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<serde_json::Value>,
    /// Result note ULID (`lifecycle.result.result_note`) — single entry point to the note
    /// the job produced, present on a successful `Done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_note: Option<Ulid>,
}

impl JobStatusView {
    /// Distills a [`JobRecord`] into the compact `job_status` view.
    ///
    /// Conflict payload and result note both live under `lifecycle.result` (`None` until the
    /// job finishes); they are read before the record is consumed.
    fn from_record(record: JobRecord) -> Self {
        // `JobStatus` is `Copy` — reading `is_terminal()` after the move is sound.
        let status = record.lifecycle.status;
        let conflict = record
            .lifecycle
            .result
            .as_ref()
            .and_then(|r| r.conflict_payload.clone());
        let result_note = record.lifecycle.result.as_ref().and_then(|r| r.result_note);
        Self {
            job_id: record.id.to_string(),
            status,
            terminal: status.is_terminal(),
            attempts: record.retry.count,
            created_at: record.lifecycle.created_at,
            completed_at: record.lifecycle.completed_at,
            error: record.retry.last_error,
            conflict,
            result_note,
        }
    }
}

/// MCP-facing job introspection — business logic of the `job_status` tool.
///
/// Returns a compact [`JobStatusView`] for `job_id`, reusing the authorization of
/// [`get_job_v2`] **verbatim** (`resolve_read_vault` on locus `main/jobs`, then the same
/// tenant scoping) so the MCP surface can never read a job the HTTP endpoint would refuse —
/// zero auth drift.
///
/// **Read-only, instant-T**: no polling, no wait loop, no server-side timeout. It reports the
/// state now; the caller re-polls if `terminal == false`. An unbounded server-side wait is a
/// documented anti-pattern here.
///
/// # Errors
///
/// Mirrors [`get_job_v2`]:
/// - [`StatusCode::FORBIDDEN`] — ACL/grant denied on `main/jobs` (or no tenant, flag ON).
/// - [`StatusCode::BAD_REQUEST`] — `job_id` is not a valid ULID.
/// - [`StatusCode::NOT_FOUND`] — job absent (or another tenant's, flag ON).
/// - [`StatusCode::INTERNAL_SERVER_ERROR`] — store failure.
#[must_use = "the resolved job state must be surfaced to the MCP caller"]
pub async fn job_status_mcp(
    state: &AppState,
    trust: &TrustContext,
    job_id: &str,
) -> Result<JobStatusView, StatusCode> {
    // Même barreau d'autorisation que `get_job_v2` (ACL `main/jobs`, byte-identical OFF/ON).
    crate::api_v1::tenant_guard::resolve_read_vault(state, trust, target_vault(), "jobs").await?;

    let id = job_id
        .parse::<Ulid>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // L1 : à ON, un job d'un autre tenant lit `None` → 404 (anti-disclosure).
    let tf = job_tenant_filter(state, trust)?;
    match state.job_store.get(id, tf).await {
        Ok(Some(record)) => Ok(JobStatusView::from_record(record)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "job_status_mcp: QueueStore.get() failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /api/v1/jobs` — Creates a job with `Idempotency-Key`.
///
/// The `Idempotency-Key` header is **required**.
///
/// # Behavior
///
/// - Missing or invalid bearer → **401 Unauthorized** (before any body validation)
/// - ACL Write denied on `main/jobs` → **403 Forbidden**
/// - Missing key → **400 Bad Request**
/// - Known key → lookup → **200 OK** `{ id, idempotent: true }` (no new job created)
/// - Invalid `spec.kind` / missing required field / kind without a real worker handler
///   → **400 Bad Request** `{ error }`
/// - Unknown key + valid spec → enqueue → **202 Accepted** `{ id, idempotent: false }`
///
/// # Body
///
/// ```json
/// { "spec": { "kind": { "type": "Curate", "data": { "note_id": "01..." } } } }
/// ```
///
/// `spec.kind` is deserialized into a real [`gradatum_core::Job`]:
/// the requested `JobKind` is honored (`Curate` on the provided `note_id`,
/// `Distill`, `Purge`, `Embed`, `Forget`). Only kinds with a real worker
/// handler are accepted — see `job_kind_is_triggerable`.
pub async fn create_job(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    headers: HeaderMap,
    Json(body): Json<CreateJobRequest>,
) -> Result<Response, StatusCode> {
    // Authz AVANT toute validation de body : un non-authentifié ne doit pas
    // pouvoir distinguer un 400 (body invalide) d'un 401, ni déclencher un job
    // destructeur (Purge réelle, Curate) — fix authz F-16.
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if state.acl.evaluate(&trust, AclOp::Write, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // C1 (F-63, EX-C1-1/2) : à flag ON, l'enqueue est une écriture — le tenant JWT
    // doit détenir un grant write sur son vault. Les jobs ne sont pas body-scoped :
    // le tenant est dérivé du contexte, jamais du body. Flag OFF → aucun changement.
    //
    // A6' : le tenant est **hissé** hors du bloc car il n'est plus seulement un
    // prédicat d'autorisation — il scope le job créé (`JobScope::Vault(tenant)`,
    // cf. `build_job_record_from_spec`). À OFF il vaut `"main"`, le vault unique :
    // `resolve_job_vault(Vault("main"), false) == resolve_job_vault(VaultWide, false)`
    // → chemin OFF inchangé.
    let tenant: &str = if state.server_config.multi_tenant.enabled {
        // C3a (EX-C3a-1) : scope write exigé avant le grant — token lecture-seule refusé.
        if !crate::api_v1::tenant_guard::write_scope_allowed(&state, &trust) {
            return Err(StatusCode::FORBIDDEN);
        }
        // Frontière : `tenant_id()` typé `Option<&TenantId>` (Task 3). `.map(as_str)` →
        // `require_write_grant(&str, &str)` inchangé, byte-identical.
        let Some(tenant) = trust.tenant_id().map(|t| t.as_str()) else {
            // Contexte sans tenant (Mtls/Studio) : pas d'écriture vault par grant.
            return Err(StatusCode::FORBIDDEN);
        };
        crate::api_v1::tenant_guard::require_write_grant(&state, tenant, tenant)
            .await
            .map_err(|r| r.status())?;
        tenant
    } else {
        "main"
    };

    // Idempotency-Key obligatoire (v81 L5642)
    let idempotency_key = match headers.get("Idempotency-Key") {
        Some(v) => match v.to_str() {
            Ok(s) if !s.is_empty() && s.len() <= 256 => s.to_string(),
            _ => return Err(StatusCode::BAD_REQUEST),
        },
        None => return Err(StatusCode::BAD_REQUEST),
    };

    // Pool requis pour l'idempotence — 501 si non câblé
    let pool = match &state.jobs_pool {
        Some(p) => p.clone(),
        None => {
            tracing::warn!("create_job: jobs_pool not wired — Idempotency-Key not supported");
            return Err(StatusCode::NOT_IMPLEMENTED);
        }
    };

    // Lookup idempotent
    match idempotency_lookup(&pool, &idempotency_key).await {
        Ok(Some(existing_id)) => {
            // Key connue → retourner le job existant (idempotent = true)
            let response = CreateJobResponse {
                id: existing_id,
                idempotent: true,
            };
            return Ok((StatusCode::OK, Json(response)).into_response());
        }
        Ok(None) => {} // Continuer la création
        Err(e) => {
            tracing::error!(error = %e, "create_job: idempotency_lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // F-16.1 — Désérialise le JobKind réel demandé et construit le JobSpec concret.
    // Un kind sans handler worker réel (stub ou non routé) → 400 explicite,
    // PAS de Curate forcé sur un ULID aléatoire (fix E-13, plus de DLQ silencieux).
    let job_record = match build_job_record_from_spec(body, tenant) {
        Ok(record) => record,
        Err(reason) => {
            tracing::info!(reason = %reason, "create_job: spec rejected → 400");
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": reason.to_string() })),
            )
                .into_response());
        }
    };

    // P0 (SecAuditor #1) — ANTI-FORGE : à ON, le tenant SERVI par le spec
    // (`spec_tenant` : CurateSpec.tenant_id / vault / scope…) doit être celui du
    // Bearer. Sans ce cross-check, `alice` pourrait enqueuer un `Curate` servant
    // `bob` (écriture cross-tenant). `effective_tenant` compare spec-tenant vs JWT
    // → 403 si divergent. OFF → AUCUN check (byte-identical).
    //
    // A6' : le check reste **discriminant** pour tout kind portant un tenant propre
    // (`Curate`/`Ingest`/`Embed`/`Validate`, `Forget`/`Distill` à scope vaulté) — c'est
    // celui-là qui est comparé. Pour un kind qui n'en porte aucun (`Purge`, `Distill`
    // à scope `Locus`), `spec_tenant` replie désormais sur `JobSpec.scope`, que l'on
    // vient de fixer à `Vault(tenant JWT)` : le job ne peut donc servir que le vault
    // du porteur. Avant ce lot le repli valait `"main"` en dur, ce qui rendait ces
    // deux kinds **inatteignables** pour tout tenant ≠ main (403 systématique).
    if state.server_config.multi_tenant.enabled {
        let job_tenant =
            gradatum_core::scope::TenantId::new(gradatum_core::spec_tenant(&job_record.spec));
        let _ = crate::api_v1::tenant_guard::effective_tenant(&trust, Some(&job_tenant))?;
    }

    match state.job_store.enqueue(job_record).await {
        Ok(job_id) => {
            let job_id_str = job_id.to_string();
            // Stocker la clé d'idempotence
            if let Err(e) = idempotency_insert(&pool, &idempotency_key, &job_id_str).await {
                // Non-fatal : le job a été créé, l'idempotence peut être manquée une fois.
                tracing::warn!(
                    error = %e,
                    job_id = %job_id_str,
                    "create_job: idempotency_insert failed — job created but key not stored"
                );
            }
            let response = CreateJobResponse {
                id: job_id_str,
                idempotent: false,
            };
            Ok((StatusCode::ACCEPTED, Json(response)).into_response())
        }
        Err(e) => {
            tracing::error!(error = %e, "create_job: QueueStore.enqueue() failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Returns `true` when a `Job` kind can be triggered via `POST /api/v1/jobs`.
///
/// Only kinds with a real (non-stub) worker handler are accepted.
///
/// | Kind | Handler | Status |
/// |------|---------|--------|
/// | `Curate`  | `handle_curate`  | Operational — allowed |
/// | `Distill` | `handle_distill` | Operational — allowed |
/// | `Purge`   | `handle_purge`   | Operational — allowed |
/// | `Embed`   | `handle_embed`   | Operational — allowed |
/// | `Forget`  | `handle_forget`  | Operational — allowed |
/// | `ReIndex` | `handle_reindex` | **Stub** — rejected 400 |
///
/// All other variants (`Agent`, `Backup`, `Review`, `Migrate`, …) have no
/// routed worker handler and are rejected with 400.
/// Direct enqueues by internal agents remain possible via `QueueStore::enqueue`
/// outside this public API.
fn job_kind_is_triggerable(kind: &gradatum_core::Job) -> bool {
    use gradatum_core::Job;
    matches!(
        kind,
        Job::Curate(_) | Job::Distill(_) | Job::Purge(_) | Job::Embed(_) | Job::Forget(_)
    )
}

/// Builds a concrete `JobRecord` from a `CreateJobRequest` body.
///
/// Deserializes the requested `JobKind` and honors its data
/// (e.g. `CurateSpec.note_id` provided by the client), rather than coercing
/// to a fixed kind that would be guaranteed to enter the DLQ.
///
/// # Vault du job
///
/// `tenant` est le vault sur lequel le job est scopé — dérivé du **contexte
/// d'authentification** par l'appelant (`trust.tenant_id()` à flag ON, `"main"` à
/// OFF), **jamais** du corps de requête, qui reste hostile. Il est posé tel quel
/// dans `JobSpec.scope = JobScope::Vault(tenant)`, valeur que le worker consomme
/// via `resolve_job_vault` pour scoper tout accès index/vault.
///
/// Avant A6' ce champ valait `JobScope::VaultWide`, que `resolve_job_vault` refuse
/// terminalement dès `multi_tenant = ON` (A2) : tout job créé par cette route
/// partait en 202 puis mourait en DLQ. À OFF, `Vault("main")` et `VaultWide`
/// résolvent tous deux `"main"` — chemin inchangé.
///
/// # Contract
///
/// Expected body:
///
/// ```json
/// { "spec": { "kind": { "type": "Curate", "data": { "note_id": "01..." } } } }
/// ```
///
/// `kind` follows the serde representation of [`gradatum_core::Job`]
/// (`#[serde(tag = "type", content = "data")]`). Fields with `#[serde(default)]`
/// in the spec structs are respected (e.g. `PurgeSpec.dry_run = true` by default).
///
/// # Errors (→ 400 Bad Request at the handler level)
///
/// - `spec.kind` absent or not deserializable as `Job` (unknown kind, missing
///   required field such as `CurateSpec.note_id`) → [`JobSpecError::Invalid`]
/// - Valid kind but without a real worker handler (stub `ReIndex`, or unrouted
///   kind `Backup`/`Agent`/…) → [`JobSpecError::Unsupported`]
/// - `Job::Forget` carrying a `ForgetScope::Agent` over more than one vault →
///   [`JobSpecError::Invalid`] (A7). One job targets exactly one vault; the fan-out
///   belongs to the enqueue site, so the route refuses instead of returning 202 and
///   letting the worker send the job to the DLQ.
fn build_job_record_from_spec(
    body: CreateJobRequest,
    tenant: &str,
) -> Result<JobRecord, JobSpecError> {
    use gradatum_core::{
        JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
        JobScheduling, JobScope, JobSpec, RetryBackoff, TriggerSource,
    };

    // Désérialise `spec.kind` en Job réel (format serde tag=type/content=data).
    let kind_value = body
        .spec
        .get("kind")
        .cloned()
        .ok_or_else(|| JobSpecError::Invalid("`spec.kind` field absent".to_string()))?;

    let kind: gradatum_core::Job = serde_json::from_value(kind_value).map_err(|e| {
        JobSpecError::Invalid(format!(
            "`spec.kind` invalid or required field missing: {e}"
        ))
    })?;

    // Rejette les kinds sans handler worker réel — PAS de DLQ silencieux.
    if !job_kind_is_triggerable(&kind) {
        return Err(JobSpecError::Unsupported(
            gradatum_core::job_kind_str(&kind).to_string(),
        ));
    }

    // A7 — même principe, un cran plus fin : un `Forget::Agent` visant N > 1 vaults
    // n'est pas exécutable par cette route. Un job cible exactement un vault (A2-bis) ;
    // le worker refuse ce spec terminalement (`ensure_forget_scope_vault`, branche
    // `many`). Sans cette garde la requête partait en 202 puis mourait en DLQ — le
    // symptôme même qu'A6' a corrigé ailleurs. Le fan-out « un job par vault » relève
    // du site d'enqueue (le CLI admin le fait via `fan_out_by_vault`) ; cette route
    // publique dit non plutôt que d'accepter puis de mourir.
    //
    // 400 et non 403 : refus de FORME, pas d'autorisation. Il ne dépend pas de
    // l'identité du porteur (il vaut même si le Bearer couvre tous les vaults cités),
    // s'applique à `multi_tenant` OFF où aucune anti-forge n'existe, et rejoint les
    // deux refus voisins de cette fonction. Le 403 observé avant A6' était un artefact
    // du repli `"main"` de `spec_tenant`, pas une décision d'autorisation.
    if let gradatum_core::Job::Forget(forget) = &kind
        && let gradatum_core::ForgetScope::Agent { vaults, .. } = &forget.scope
        && vaults.len() > 1
    {
        return Err(JobSpecError::Invalid(format!(
            "`ForgetScope::Agent` targets {} vaults — a job targets exactly one vault. \
             Post one job per vault (fan-out belongs to the enqueue site).",
            vaults.len()
        )));
    }

    let now = Utc::now();

    // triggered_by : lineage explicite, sinon marqueur API.
    let triggered_by = body
        .lineage
        .as_ref()
        .and_then(|l| l.get("triggered_by"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| Some("api".to_string()));

    Ok(JobRecord {
        id: Ulid::new(),
        spec: JobSpec {
            kind,
            class: JobClass::Api,
            mode: JobMode::Batch,
            // A6' — un job = exactement un vault (A2-bis). Le vault vient du contexte
            // d'auth, jamais du body.
            scope: JobScope::Vault(tenant.to_owned()),
            priority: JobPriority::default_for(&JobClass::Api),
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
        retry: JobRetry {
            count: 0,
            max: 3,
            backoff: RetryBackoff::Exponential { base: 5, max: 120 },
            last_error: None,
            errors: vec![],
        },
        lineage: JobLineage {
            triggered_by,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    })
}

/// Rejection reason for a `POST /api/v1/jobs` body (→ 400 Bad Request).
#[derive(Debug)]
enum JobSpecError {
    /// `spec.kind` absent, malformed, or missing a required field (e.g. `note_id`).
    Invalid(String),
    /// Valid kind but without a real worker handler (stub or unrouted).
    Unsupported(String),
}

impl std::fmt::Display for JobSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobSpecError::Invalid(msg) => write!(f, "invalid spec: {msg}"),
            JobSpecError::Unsupported(kind) => write!(
                f,
                "kind `{kind}` not triggerable via the API (worker handler absent or stub) — \
                 supported kinds: Curate, Distill, Purge, Embed, Forget"
            ),
        }
    }
}

/// `POST /api/v1/jobs/:id/cancel` — Cancels a job.
///
/// # Behavior
///
/// - Job in `Running` → **409 Conflict** (let it finish, do not kill)
/// - Job in `Pending`/`Waiting` → cancelled → **200 OK** `{ id, status: "Cancelled" }`
/// - Job already terminal (`Done`/`Failed`/`DLQ`/`Cancelled`) → **200 OK** idempotent
/// - Job does not exist → **404 Not Found**
/// - Invalid ULID → **400 Bad Request**
/// - Missing or invalid bearer → **401 Unauthorized**
/// - ACL Write denied on `main/jobs` → **403 Forbidden**
pub async fn cancel_job(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
) -> Result<Json<CancelJobResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if state.acl.evaluate(&trust, AclOp::Write, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }
    // C3a (EX-C3a-1) : à ON, un token lecture-seule ne peut pas annuler un job.
    if !crate::api_v1::tenant_guard::write_scope_allowed(&state, &trust) {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // L1 : à ON, scoper get ET cancel au tenant du Bearer (job d'autrui → 404).
    let tf = job_tenant_filter(&state, &trust)?;

    // Lire le statut courant pour appliquer les règles v81
    let record = match state.job_store.get(id, tf).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: get() failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    match record.lifecycle.status {
        // Job Running → 409 Conflict (caveat L12411 v81)
        JobStatus::Running => {
            return Err(StatusCode::CONFLICT);
        }
        // Job déjà terminal → 200 idempotent
        JobStatus::Done | JobStatus::DLQ | JobStatus::Cancelled | JobStatus::Conflict => {
            return Ok(Json(CancelJobResponse {
                id: id.to_string(),
                status: format!("{:?}", record.lifecycle.status).to_lowercase(),
            }));
        }
        // Job Pending ou Waiting → annuler
        JobStatus::Pending | JobStatus::Waiting | JobStatus::Failed => {}
    }

    match state.job_store.cancel(id, tf).await {
        Ok(()) => Ok(Json(CancelJobResponse {
            id: id.to_string(),
            status: "cancelled".to_string(),
        })),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: QueueStore.cancel() failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET /api/v1/jobs/:id/events` — SSE event stream for a job.
///
/// Returns a `text/event-stream` carrying job events.
///
/// # Event types
///
/// - `status`: status change `{ event_id, type, status, attempts, timestamp }`
/// - `progress`: progress update `{ event_id, type, current, total, step, eta_secs }`
/// - `heartbeat`: keepalive every 30 s
///
/// # Stream closure
///
/// The stream closes automatically when the job reaches a terminal state
/// (`Done`, `DLQ`, `Cancelled`).
///
/// # Last-Event-ID
///
/// When `Last-Event-ID` is present, the client has already received events up
/// to that ID. Replay restarts from the head of the buffer (the shared
/// circular buffer of capacity 256 does not support exact replay from an
/// arbitrary ID).
///
/// # Responses
///
/// - **200 OK** + `Content-Type: text/event-stream`
/// - **400 Bad Request**: ID is not a valid ULID
/// - **401 Unauthorized**: missing or invalid bearer token
/// - **403 Forbidden**: ACL Read denied on `main/jobs`
/// - **404 Not Found**: job does not exist
pub async fn job_events(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(id_str): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // T9 (A3-handlers) : OFF = ACL Read legacy sur `main/jobs` (byte-identical) ; ON = ACL
    // cible + grant read + statut actif du vault propre. Jobs non vault-scopés (store
    // global) : seul l'enforcement ACL/grant est câblé, le vault résolu est ignoré.
    crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), "jobs").await?;

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // L1 : à ON, un stream sur le job d'un autre tenant → 404 (anti-disclosure).
    let tf = job_tenant_filter(&state, &trust)?;
    // Vérifier l'existence du job avant de créer le stream
    match state.job_store.get(id, tf).await {
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Ok(Some(_)) => {}
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "job_events: get() failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Last-Event-ID header pour reconnexion
    let _last_event_id = headers
        .get("Last-Event-ID")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // S'abonner au broadcast AVANT de lire l'état final (évite race condition)
    let rx = state.job_store.subscribe();
    let target_id = id;

    // Compteur d'events pour les IDs SSE — non utilisé Phase 3 Bronze (E-15 : IDs fixes).
    // Planifié F-16 Silver via scan() pour état mutable.
    let event_counter: u64 = 0;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Err(_) => {
                // Lagged (buffer overflow) — skip silencieusement
                // Le client peut se reconnecter via Last-Event-ID
                None
            }
            Ok(event) => {
                let matches = matches!(
                    &event,
                    QueueEvent::JobInserted(eid) |
                    QueueEvent::JobFailed(eid, _) |
                    QueueEvent::JobReady(eid) |
                    QueueEvent::JobCancelled(eid)
                    if *eid == target_id
                ) || matches!(
                    &event,
                    QueueEvent::JobCompleted(eid, _, _) if *eid == target_id
                );

                if !matches {
                    return None;
                }

                let event_data = match &event {
                    QueueEvent::JobCompleted(_, status, _) => {
                        let status_str = format!("{:?}", status).to_lowercase();
                        serde_json::json!({
                            "type": "status",
                            "status": status_str,
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobFailed(_, attempt) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "failed",
                            "attempts": attempt,
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobCancelled(_) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "cancelled",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    QueueEvent::JobReady(_) => {
                        serde_json::json!({
                            "type": "status",
                            "status": "pending",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                    _ => {
                        serde_json::json!({
                            "type": "status",
                            "status": "inserted",
                            "timestamp": Utc::now().timestamp_millis()
                        })
                    }
                };

                // Signal de fermeture pour les états terminaux
                let is_terminal = matches!(
                    &event,
                    QueueEvent::JobCompleted(eid, _, _) | QueueEvent::JobCancelled(eid)
                    if *eid == target_id
                );

                let data_str = serde_json::to_string(&event_data)
                    .unwrap_or_else(|_| r#"{"type":"error"}"#.to_string());

                if is_terminal {
                    // Retourner l'event + None pour fermer le stream
                    Some(Ok::<Event, Infallible>(Event::default().data(data_str)))
                } else {
                    Some(Ok::<Event, Infallible>(Event::default().data(data_str)))
                }
            }
        }
    });

    // Le compteur d'event_counter est capturé dans une closure séparée
    // pour l'event ID (limitation : pas d'état mutable dans filter_map sans RefCell)
    // Caveat E-15 : event IDs séquentiels non implémentés — IDs fixes à 0.
    // Planifié : refacto avec scan() pour state mutable → F-16 Silver.
    let _ = event_counter; // Supprime le warning unused

    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("heartbeat"),
    );

    Ok(sse)
}
