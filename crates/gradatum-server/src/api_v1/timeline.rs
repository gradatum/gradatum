//! `POST /api/v1/vault_timeline` — paginated temporal read.
//!
//! Reads the `temporal_index` table via `IndexStore::timeline`.
//! Sort order: `anchor_ms DESC, note_id DESC`. Filters: `doc_kind`/`from_ms`/`to_ms`.
//! Opaque cursor pagination. ACL `Read` on locus `{tenant}/timeline`.

use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_core::trust::TrustContext;
use serde::Serialize;

use crate::api_v1::compact::{self, CompactBody};
use crate::api_v1::dto::VaultTimelineRequest;
use crate::state::AppState;

/// A single timeline item (wire response).
#[derive(Debug, Serialize)]
pub struct TimelineItem {
    pub note_id: String,
    pub anchor_ms: i64,
    pub anchor_src: String,
    pub doc_kind: String,
    pub title: Option<String>,
}

/// Response for `vault_timeline`.
#[derive(Debug, Serialize)]
pub struct VaultTimelineResponse {
    pub items: Vec<TimelineItem>,
    /// Always serialized (no `skip_serializing_if`) — the contract guarantees the field
    /// is present as `null` when `items.len() < limit`.
    pub next_cursor: Option<String>,
}

/// `POST /api/v1/vault_timeline` — see module documentation.
///
/// 401 if unauthenticated · 403 if ACL `Read` denied on `{tenant}/timeline` ·
/// 400 if `from_ms > to_ms`, `doc_kind` outside the allowlist, invalid `vault_id`,
/// or malformed `cursor` · 500 on storage failure.
pub async fn vault_timeline(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<VaultTimelineRequest>,
) -> Result<Response, StatusCode> {
    // Read the opt-in flag before `req` is moved into the impl.
    let want_compact = req.compact;
    let resp = crate::api_v1::logic::vault_timeline_impl(&state, &trust, req)
        .await
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "vault_timeline: storage failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })?;
    // `compact=false` returns exactly `Json(resp)` as before → byte-for-byte identical.
    Ok(if want_compact {
        Json(CompactBody {
            compact: compact::render_timeline(&resp),
        })
        .into_response()
    } else {
        Json(resp).into_response()
    })
}
