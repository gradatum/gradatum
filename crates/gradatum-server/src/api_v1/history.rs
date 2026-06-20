//! Copy-on-Write history endpoints — thin wrappers sur `logic::*_impl`.
//!
//! Four synchronous (200 OK) endpoints for note history:
//! - `vault_history`     — lists timestamps of CoW snapshots.
//! - `vault_history_get` — reads the content of a specific snapshot.
//! - `vault_restore`     — restores a note from a snapshot (triggers a CoW).
//! - `vault_diff`        — raw line-by-line diff between two versions.
//!
//! # Endpoints
//!
//! | Method | Path | ACL | Codes |
//! |--------|------|-----|-------|
//! | POST | `/api/v1/vault_history`     | Read  | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_history_get` | Read  | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_restore`     | Write | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_diff`        | Read  | 200 / 401 / 403 / 400 / 404 / 500 |

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_core::trust::TrustContext;

use crate::state::AppState;

// ── Re-exports DTOs (depuis gradatum-dto) ─────────────────────────────────────
pub use gradatum_dto::{
    VaultDiffRequest, VaultDiffResponse, VaultHistoryGetRequest, VaultHistoryGetResponse,
    VaultHistoryRequest, VaultHistoryResponse, VaultRestoreRequest, VaultRestoreResponse,
};

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_history`
///
/// Lists the Unix timestamps (ms) of CoW snapshots for a note.
///
/// ## Error codes
///
/// - `401`: missing or invalid bearer token.
/// - `403`: ACL Read denied.
/// - `500`: unexpected error (log emitted).
pub async fn vault_history(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultHistoryRequest>,
) -> Result<Json<VaultHistoryResponse>, StatusCode> {
    crate::api_v1::logic::vault_history_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| crate::api_v1::logic::map_err_to_status_history(&e))
}

/// `POST /api/v1/vault_history_get`
///
/// Reads the content of a specific historical snapshot.
///
/// ## Error codes
///
/// - `401`: missing or invalid bearer token.
/// - `403`: ACL Read denied.
/// - `404`: snapshot or note not found.
/// - `500`: unexpected error (log emitted).
pub async fn vault_history_get(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultHistoryGetRequest>,
) -> Result<Json<VaultHistoryGetResponse>, StatusCode> {
    crate::api_v1::logic::vault_history_get_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if !matches!(e, gradatum_core::error::GradatumError::NoteNotFound(_)) {
                tracing::error!(err = %e, "vault_history_get: failed");
            }
            crate::api_v1::logic::map_err_to_status_history(&e)
        })
}

/// `POST /api/v1/vault_restore`
///
/// Restores a note from a historical snapshot.
///
/// ## Error codes
///
/// - `401`: missing or invalid bearer token.
/// - `403`: ACL Write denied.
/// - `404`: snapshot or note not found.
/// - `500`: unexpected error (log emitted).
pub async fn vault_restore(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultRestoreRequest>,
) -> Result<Json<VaultRestoreResponse>, StatusCode> {
    crate::api_v1::logic::vault_restore_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if !matches!(e, gradatum_core::error::GradatumError::NoteNotFound(_)) {
                tracing::error!(err = %e, "vault_restore: failed");
            }
            crate::api_v1::logic::map_err_to_status_history(&e)
        })
}

/// `POST /api/v1/vault_diff`
///
/// Raw line-by-line diff between two versions of a note.
///
/// ## Error codes
///
/// - `400`: selector `a` or `b` is invalid (neither a timestamp nor `"current"`).
/// - `401`: missing or invalid bearer token.
/// - `403`: ACL Read denied.
/// - `404`: note or snapshot not found.
/// - `500`: unexpected error (log emitted).
pub async fn vault_diff(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultDiffRequest>,
) -> Result<Json<VaultDiffResponse>, StatusCode> {
    crate::api_v1::logic::vault_diff_impl(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if !matches!(e, gradatum_core::error::GradatumError::NoteNotFound(_)) {
                let status = crate::api_v1::logic::map_err_to_status_history(&e);
                if status == StatusCode::INTERNAL_SERVER_ERROR {
                    tracing::error!(err = %e, "vault_diff: failed");
                }
            }
            crate::api_v1::logic::map_err_to_status_history(&e)
        })
}
