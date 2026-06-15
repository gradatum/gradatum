//! Legacy job-poll handler — `GET /api/v1/jobs/<id>` (`i64` job id).
//!
//! Reads the live state of a job from the legacy `jobs_v2` queue via `Queue::get`.
//! Retained for backwards compatibility: this is the `poll_url` returned by
//! `vault_downgrade` (`/api/v1/jobs/{i64}`). Newer clients poll `/jobs/{ulid}/v2` instead.
//!
//! # Endpoint
//!
//! | Method | Path | Auth |
//! |--------|------|------|
//! | GET | `/api/v1/jobs/:id` | Bearer JWT required + ACL Read on `main/jobs` |
//!
//! # Auth
//!
//! The endpoint requires an authenticated bearer token (`401` otherwise) **and**
//! an ACL [`AclOp::Read`] grant on the locus `main/jobs` (`403` otherwise) —
//! consistent with the `jobs_v2` and `forget` handler pattern.
//!
//! Without this check, the route would expose job status to any caller that
//! guesses an `AUTOINCREMENT` `i64`. The loopback bind mitigates the exposure,
//! but application-level authorization is now explicit.
//! Fine-grained multi-user JWT authorization is planned for Gold.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};

use crate::api_v1::dto::JobStatusResponse;
use crate::state::AppState;
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::trust::TrustContext;
use gradatum_queue::JobStatus;

/// ACL locus for the legacy jobs endpoint — `main/jobs`.
///
/// Matches the locus used by `jobs_v2` (shared authorization scope for all job routes).
/// Read access → [`AclOp::Read`].
fn jobs_locus() -> String {
    "main/jobs".to_string()
}

/// Maps a raw worker error string (anyhow `to_string()`) to an opaque error code.
///
/// Prevents information disclosure (absolute FS paths, reflected invalid ULIDs,
/// internal state) to callers.
fn sanitize_job_error(raw: &str) -> &'static str {
    if raw.contains("ULID invalide") || raw.contains("invalid character") {
        "invalid_input"
    } else if raw.contains("Vault::") || raw.contains("vault non configuré") {
        "vault_error"
    } else if raw.contains("Storage") || raw.contains("sqlx") || raw.contains("SQLite") {
        "storage_error"
    } else {
        "processing_error"
    }
}

/// `GET /api/v1/jobs/:id`
///
/// Returns the current status of a legacy-queue job (reads from `jobs_v2`).
///
/// # Auth
///
/// Requires an authenticated bearer JWT (injected via `Extension<TrustContext>`
/// by the `trust_layer` middleware) **and** an ACL [`AclOp::Read`] grant on
/// the locus `main/jobs`. Authorization is evaluated before any queue read.
///
/// # Responses
///
/// - **200 OK** + JSON [`JobStatusResponse`] — job status; `last_error` mapped to an opaque code.
/// - **401 Unauthorized** — missing or invalid bearer token.
/// - **403 Forbidden** — ACL Read denied on `main/jobs`.
/// - **404 Not Found** — job does not exist.
/// - **500 Internal Server Error** — SQLite failure (logged server-side, not exposed).
pub async fn get_job(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Path(id): Path<i64>,
) -> Result<Json<JobStatusResponse>, StatusCode> {
    // Fix C1 F-16 : authz AVANT lecture queue. Un i64 AUTOINCREMENT est devinable —
    // sans ce check, le statut d'un job fuit à tout appelant non authentifié.
    // Pattern identique à jobs_v2 (get_job_v2) et forget : 401 puis 403.
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if state.acl.evaluate(&trust, AclOp::Read, &jobs_locus()) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    match state.queue.get(id).await {
        Ok(Some(info)) => {
            // Caveat C1 2026-05-08 : signal soft d'incohérence DB potentielle.
            // SqliteQueue::get fait `unwrap_or(Pending)` sur status DB inconnu (silent fallback).
            // Si on observe attempts > 0 et status Pending, c'est suspect (un job traité ne
            // revient normalement pas à pending sans fail explicite).
            if info.status == JobStatus::Pending && info.attempts > 0 {
                tracing::warn!(
                    job_id = id,
                    attempts = info.attempts,
                    "job pending with attempts>0 — possible DB status inconsistency or unknown variant fallback"
                );
            }
            Ok(Json(JobStatusResponse {
                job_id: info.id,
                status: info.status.as_str().to_string(),
                attempts: info.attempts,
                last_error: info
                    .last_error
                    .as_deref()
                    .map(|raw| sanitize_job_error(raw).to_string()),
            }))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(error = %e, job_id = id, "queue.get failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_job_error;

    #[test]
    fn ulid_error_maps_invalid_input() {
        assert_eq!(
            sanitize_job_error("ULID invalide abc123: invalid character"),
            "invalid_input"
        );
    }

    #[test]
    fn vault_error_maps_vault_error() {
        assert_eq!(
            sanitize_job_error("Vault::open(/var/lib/gradatum/vault) failed: permission denied"),
            "vault_error"
        );
    }

    #[test]
    fn storage_error_maps_storage_error() {
        assert_eq!(
            sanitize_job_error("Storage(\"sqlx: connection refused\")"),
            "storage_error"
        );
    }

    #[test]
    fn fallback_processing_error() {
        assert_eq!(
            sanitize_job_error("unknown failure mode"),
            "processing_error"
        );
    }
}
