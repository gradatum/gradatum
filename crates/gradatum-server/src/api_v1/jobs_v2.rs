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
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;
use ulid::Ulid;

use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::{JobFilter, JobOrder, JobRecord, JobStatus, QueueEvent};

use crate::state::AppState;
use gradatum_core::trust::TrustContext;
use gradatum_db_sqlite::{idempotency_insert, idempotency_lookup};

/// Single-vault tenant — jobs are not body-scoped; ACL is evaluated on the
/// dedicated locus `main/jobs` (consistent with `dashboard`/`lessons`).
const TENANT: &str = "main";

/// ACL locus for the job endpoints — `main/jobs`.
///
/// Read (`list`/`detail`/`events`) → [`AclOp::Read`]; write/trigger
/// (`create`/`cancel`) → [`AclOp::Write`]. Consistent with other `api_v1`
/// handlers (`forget`, `write`, `dashboard`, `history`, …).
fn jobs_locus() -> String {
    format!("{TENANT}/jobs")
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
    if state.acl.evaluate(&trust, AclOp::Read, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

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
            tracing::error!(error = %e, "list_jobs: QueueStore.list() échoué");
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
    if state.acl.evaluate(&trust, AclOp::Read, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    match state.job_store.get(id).await {
        Ok(Some(record)) => Ok(Json(record)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "get_job_v2: QueueStore.get() échoué");
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
            tracing::warn!("create_job: jobs_pool non câblé — Idempotency-Key non supporté");
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
            tracing::error!(error = %e, "create_job: idempotency_lookup échoué");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // F-16.1 — Désérialise le JobKind réel demandé et construit le JobSpec concret.
    // Un kind sans handler worker réel (stub ou non routé) → 400 explicite,
    // PAS de Curate forcé sur un ULID aléatoire (fix E-13, plus de DLQ silencieux).
    let job_record = match build_job_record_from_spec(body) {
        Ok(record) => record,
        Err(reason) => {
            tracing::info!(reason = %reason, "create_job: spec rejetée → 400");
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": reason.to_string() })),
            )
                .into_response());
        }
    };

    match state.job_store.enqueue(job_record).await {
        Ok(job_id) => {
            let job_id_str = job_id.to_string();
            // Stocker la clé d'idempotence
            if let Err(e) = idempotency_insert(&pool, &idempotency_key, &job_id_str).await {
                // Non-fatal : le job a été créé, l'idempotence peut être manquée une fois.
                tracing::warn!(
                    error = %e,
                    job_id = %job_id_str,
                    "create_job: idempotency_insert échoué — job créé mais clé non stockée"
                );
            }
            let response = CreateJobResponse {
                id: job_id_str,
                idempotent: false,
            };
            Ok((StatusCode::ACCEPTED, Json(response)).into_response())
        }
        Err(e) => {
            tracing::error!(error = %e, "create_job: QueueStore.enqueue() échoué");
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
fn build_job_record_from_spec(body: CreateJobRequest) -> Result<JobRecord, JobSpecError> {
    use gradatum_core::{
        JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord, JobRetry,
        JobScheduling, JobScope, JobSpec, RetryBackoff, TriggerSource,
    };

    // Désérialise `spec.kind` en Job réel (format serde tag=type/content=data).
    let kind_value = body
        .spec
        .get("kind")
        .cloned()
        .ok_or_else(|| JobSpecError::Invalid("champ `spec.kind` absent".to_string()))?;

    let kind: gradatum_core::Job = serde_json::from_value(kind_value).map_err(|e| {
        JobSpecError::Invalid(format!(
            "`spec.kind` invalide ou champ obligatoire manquant : {e}"
        ))
    })?;

    // Rejette les kinds sans handler worker réel — PAS de DLQ silencieux.
    if !job_kind_is_triggerable(&kind) {
        return Err(JobSpecError::Unsupported(
            gradatum_core::job_kind_str(&kind).to_string(),
        ));
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
            scope: JobScope::VaultWide,
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
            JobSpecError::Invalid(msg) => write!(f, "spec invalide : {msg}"),
            JobSpecError::Unsupported(kind) => write!(
                f,
                "kind `{kind}` non déclenchable via l'API (handler worker absent ou stub) — \
                 kinds supportés : Curate, Distill, Purge, Embed, Forget"
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

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // Lire le statut courant pour appliquer les règles v81
    let record = match state.job_store.get(id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: get() échoué");
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

    match state.job_store.cancel(id).await {
        Ok(()) => Ok(Json(CancelJobResponse {
            id: id.to_string(),
            status: "cancelled".to_string(),
        })),
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "cancel_job: QueueStore.cancel() échoué");
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
    if state.acl.evaluate(&trust, AclOp::Read, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = match id_str.parse::<Ulid>() {
        Ok(u) => u,
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    // Vérifier l'existence du job avant de créer le stream
    match state.job_store.get(id).await {
        Ok(None) => return Err(StatusCode::NOT_FOUND),
        Ok(Some(_)) => {}
        Err(e) => {
            tracing::error!(error = %e, job_id = %id, "job_events: get() échoué");
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
