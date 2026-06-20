//! `GET /api/v1/dashboard` — vault observability aggregate.
//!
//! Read-only: composes existing counts into a single payload for the studio.
//! No new tables, no triggers here (job triggers reuse the existing jobs API
//! `POST /api/v1/jobs` — Curate; ReIndex deferred; Purge dry-run only).
//!
//! # Contract
//!
//! | Method | Path | Response | Codes |
//! |--------|------|----------|-------|
//! | GET | `/api/v1/dashboard` | [`DashboardResponse`] | 200 / 401 / 403 / 500 |
//!
//! - Auth: standard JWT (Read) — behind the auth middleware (unlike `/health`
//!   which remains unauthenticated). The dashboard aggregates potentially
//!   sensitive counts (volume, DLQ depth) → must not be exposed without auth.
//!
//! # Field accuracy
//!
//! - `notes_by_status`: tolerant of out-of-enum values (legacy statuses such as
//!   `"downgraded"` are preserved as-is — no silent loss).
//! - `wal_size_bytes`: `Option<u64>` — `null` ("n/a" on the UI side) when not
//!   measurable. **NEVER 0** (which would falsely imply a healthy WAL).
//! - `jobs_by_status`: native `GROUP BY status`, DLQ included.

use std::collections::HashMap;

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::trust::TrustContext;
use serde::Serialize;

use crate::state::AppState;

/// Default tenant for the single-vault deployment — aligned with `vault_status`.
const TENANT: &str = "main";

/// Minimal summary of the most recent job (for dashboard display).
#[derive(Debug, Serialize)]
pub struct LastJobSummary {
    /// Job ULID.
    pub id: String,
    /// Current status (serialised as the enum variant name).
    pub status: String,
    /// Creation timestamp (RFC 3339 UTC).
    pub created_at: String,
}

/// Response for `GET /api/v1/dashboard`.
#[derive(Debug, Serialize)]
pub struct DashboardResponse {
    /// Note count by status (key = raw status string, includes legacy out-of-enum values).
    pub notes_by_status: HashMap<String, u64>,
    /// Number of forgotten notes (soft-deleted), orthogonal to the status dimension.
    pub forgotten_count: u64,
    /// Job count by status (DLQ included). Empty when the store is not wired.
    pub jobs_by_status: HashMap<String, u64>,
    /// Queue depth = number of `Pending` jobs (derived from `jobs_by_status`).
    pub queue_depth: u64,
    /// SQLite WAL size in bytes. `null` ("n/a") when not measurable — NEVER `0` (misleading).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_size_bytes: Option<u64>,
    /// Most recent known job. `null` when the queue is empty or the store is not wired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_job: Option<LastJobSummary>,
}

/// `GET /api/v1/dashboard`
///
/// See the module documentation for the full contract.
///
/// # Errors
/// - `401 Unauthorized`: unauthenticated request.
/// - `403 Forbidden`: ACL Read denied on `main/dashboard`.
/// - `500 Internal Server Error`: storage failure on the primary notes count — blocking.
///
/// Secondary sources (jobs, WAL, last job) degrade gracefully (empty value / `None`)
/// rather than failing the entire dashboard response.
pub async fn dashboard(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
) -> Result<Json<DashboardResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let acl_locus = format!("{TENANT}/dashboard");
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Notes par statut (source primaire — échec = 500) ──────────────────────
    let notes_by_status = state
        .search
        .count_notes_by_status(TENANT)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "dashboard: count_notes_by_status failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let forgotten_count = state
        .search
        .count_forgotten(TENANT)
        .await
        .map(|n| n as u64)
        .unwrap_or_else(|e| {
            tracing::warn!(err = %e, "dashboard: count_forgotten failed, fallback 0");
            0
        });

    // ── Jobs par statut (source secondaire — dégrade en map vide) ─────────────
    let jobs_by_status_enum = state
        .job_store
        .count_jobs_by_status()
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(err = %e, "dashboard: count_jobs_by_status failed, fallback vide");
            HashMap::new()
        });

    // Profondeur de file = jobs Pending.
    let queue_depth = jobs_by_status_enum
        .get(&gradatum_core::job::JobStatus::Pending)
        .copied()
        .unwrap_or(0);

    // Sérialise les clés JobStatus en string stable (Debug = nom du variant).
    let jobs_by_status: HashMap<String, u64> = jobs_by_status_enum
        .into_iter()
        .map(|(k, v)| (format!("{k:?}"), v))
        .collect();

    // ── WAL size : Option<u64>, "n/a" honnête si non mesurable ────────────────
    let wal_size_bytes: Option<u64> = state.wal_path.as_ref().and_then(|p| {
        // `metadata` échoue si le fichier WAL est absent (checkpoint complet) ou
        // inaccessible → None ("n/a"), JAMAIS 0 (qui prétendrait "WAL sain").
        match std::fs::metadata(p) {
            Ok(m) => Some(m.len()),
            Err(e) => {
                tracing::debug!(err = %e, "dashboard: WAL non mesurable → n/a");
                None
            }
        }
    });

    // ── Dernier job (source secondaire — dégrade en None) ─────────────────────
    // `latest_job` renvoie le job le plus RÉCENT (`ORDER BY id DESC`). On n'utilise
    // PAS `list()` ici : `list()` ordonne `id ASC` (pagination cursor) et renverrait
    // le job le plus *ancien*, faisant croire que le worker est mort.
    let last_job = match state.job_store.latest_job(TENANT).await {
        Ok(job) => job.map(|j| LastJobSummary {
            id: j.id.to_string(),
            status: format!("{:?}", j.lifecycle.status),
            created_at: j.lifecycle.created_at.to_rfc3339(),
        }),
        Err(e) => {
            tracing::warn!(err = %e, "dashboard: latest_job failed, fallback None");
            None
        }
    };

    Ok(Json(DashboardResponse {
        notes_by_status,
        forgotten_count,
        jobs_by_status,
        queue_depth,
        wal_size_bytes,
        last_job,
    }))
}
