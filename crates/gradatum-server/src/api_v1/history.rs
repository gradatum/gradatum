//! Copy-on-Write history endpoints.
//!
//! Four synchronous (200 OK) endpoints for note history:
//! - `vault_history`     — lists timestamps of CoW snapshots.
//! - `vault_history_get` — reads the content of a specific snapshot.
//! - `vault_restore`     — restores a note from a snapshot (triggers a CoW).
//! - `vault_diff`        — raw line-by-line diff between two versions.
//!
//! Each handler:
//! 1. Verifies authentication via [`TrustContext::is_authenticated`].
//! 2. Evaluates ACL via `AclEngine::evaluate` (Read for history/get/diff,
//!    Write for restore).
//! 3. Delegates to `state.vault` (`Arc<dyn Registry>`).
//!
//! # Endpoints
//!
//! | Method | Path | ACL | Codes |
//! |--------|------|-----|-------|
//! | POST | `/api/v1/vault_history`     | Read  | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_history_get` | Read  | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_restore`     | Write | 200 / 401 / 403 / 404 / 500 |
//! | POST | `/api/v1/vault_diff`        | Read  | 200 / 401 / 403 / 400 / 404 / 500 |

use axum::{extract::State, http::StatusCode, Extension, Json};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::error::GradatumError;
use gradatum_core::trust::TrustContext;

use crate::api_v1::tenant_guard::effective_tenant;
use crate::state::AppState;

// ── Re-exports DTOs (depuis gradatum-dto) ─────────────────────────────────────
pub use gradatum_dto::{
    VaultDiffRequest, VaultDiffResponse, VaultHistoryGetRequest, VaultHistoryGetResponse,
    VaultHistoryRequest, VaultHistoryResponse, VaultRestoreRequest, VaultRestoreResponse,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Builds the ACL locus: `{tenant_id}/main` (default section).
fn locus_for_tenant(tenant_id: &str) -> String {
    format!("{}/main", tenant_id)
}

/// Maps a `GradatumError` to an HTTP `StatusCode`.
///
/// - `NoteNotFound` → 404
/// - `Storage` containing "introuvable" or "Not found" → 404
/// - Anything else → 500
fn map_err_to_status(e: &GradatumError) -> StatusCode {
    match e {
        GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
        GradatumError::Storage(msg) if msg.contains("introuvable") || msg.contains("Not found") => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `POST /api/v1/vault_history`
///
/// Lists the Unix timestamps (ms) of CoW snapshots for a note.
///
/// ## Response
///
/// ```json
/// { "versions": [1700000000000, 1700000001000], "count": 2 }
/// ```
///
/// Returns `versions: []` if the note has no history (never modified
/// with a different body) or if the note is unknown.
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?;
    let locus = locus_for_tenant(tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let versions = state
        .vault
        .history_versions(&req.note_id)
        .await
        .map_err(|e| {
            tracing::error!(
                err = %e,
                note_id = %req.note_id,
                "vault_history: history_versions failed"
            );
            map_err_to_status(&e)
        })?;

    let count = versions.len();
    Ok(Json(VaultHistoryResponse { versions, count }))
}

/// `POST /api/v1/vault_history_get`
///
/// Reads the content of a specific historical snapshot.
///
/// ## Response
///
/// ```json
/// {
///   "note_id": "01JTEXAMPLE",
///   "ts_ms": 1700000000000,
///   "body": "# Titre\n\ncorps de la note...",
///   "section": "decisions"
/// }
/// ```
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?;
    let locus = locus_for_tenant(tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let snapshot = state
        .vault
        .history_get(&req.note_id, req.ts_ms)
        .await
        .map_err(|e| {
            if !matches!(e, GradatumError::NoteNotFound(_)) {
                tracing::error!(
                    err = %e,
                    note_id = %req.note_id,
                    ts_ms = req.ts_ms,
                    "vault_history_get: history_get failed"
                );
            }
            map_err_to_status(&e)
        })?;

    Ok(Json(VaultHistoryGetResponse {
        note_id: req.note_id,
        ts_ms: req.ts_ms,
        body: snapshot.body.markdown,
        section: snapshot.frontmatter.section.to_string(),
    }))
}

/// `POST /api/v1/vault_restore`
///
/// Restores a note from a historical snapshot.
///
/// Writes the snapshot as the new current version (triggers a CoW:
/// the old current version is saved to `.history/`).
///
/// ## Response
///
/// ```json
/// {
///   "note_id": "01JTEXAMPLE",
///   "ts_ms": 1700000000000,
///   "content_hash": "a3f1c2d4..."
/// }
/// ```
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Restauration = opération d'écriture → locus dérivé du JWT (P1 fix : tenant JWT, pas body).
    let tenant = effective_tenant(&trust, &req.tenant_id)?;
    let locus = locus_for_tenant(tenant);
    if state.acl.evaluate(&trust, AclOp::Write, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    let content_hash = state
        .vault
        .history_restore(&req.note_id, req.ts_ms)
        .await
        .map_err(|e| {
            if !matches!(e, GradatumError::NoteNotFound(_)) {
                tracing::error!(
                    err = %e,
                    note_id = %req.note_id,
                    ts_ms = req.ts_ms,
                    "vault_restore: history_restore failed"
                );
            }
            map_err_to_status(&e)
        })?;

    Ok(Json(VaultRestoreResponse {
        note_id: req.note_id,
        ts_ms: req.ts_ms,
        content_hash,
    }))
}

/// `POST /api/v1/vault_diff`
///
/// Raw line-by-line diff between two versions of a note.
///
/// `a` and `b` are Unix timestamps in ms (from `vault_history`) or the
/// literal string `"current"` for the current version.
///
/// ## Response
///
/// ```json
/// {
///   "lines": [" shared line", "-removed line", "+added line"],
///   "count": 3
/// }
/// ```
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
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?;
    let locus = locus_for_tenant(tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &locus) != AclDecision::Allow {
        return Err(StatusCode::FORBIDDEN);
    }

    // Validation préalable des sélecteurs (400 avant tout appel vault).
    let is_valid_selector = |s: &str| -> bool { s == "current" || s.parse::<i64>().is_ok() };
    if !is_valid_selector(&req.a) || !is_valid_selector(&req.b) {
        tracing::warn!(
            a = %req.a,
            b = %req.b,
            note_id = %req.note_id,
            "vault_diff: sélecteur invalide (attendu 'current' ou timestamp ms)"
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    let lines = state
        .vault
        .history_diff(&req.note_id, &req.a, &req.b)
        .await
        .map_err(|e| {
            if !matches!(e, GradatumError::NoteNotFound(_)) {
                let status = map_err_to_status(&e);
                if status == StatusCode::INTERNAL_SERVER_ERROR {
                    tracing::error!(
                        err = %e,
                        note_id = %req.note_id,
                        a = %req.a,
                        b = %req.b,
                        "vault_diff: history_diff failed"
                    );
                }
            }
            map_err_to_status(&e)
        })?;

    let count = lines.len();
    Ok(Json(VaultDiffResponse { lines, count }))
}
