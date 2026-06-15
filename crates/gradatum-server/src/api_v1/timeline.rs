//! `POST /api/v1/vault_timeline` — paginated temporal read.
//!
//! Reads the `temporal_index` table via `IndexStore::timeline`.
//! Sort order: `anchor_ms DESC, note_id DESC`. Filters: `doc_kind`/`from_ms`/`to_ms`.
//! Opaque cursor pagination. ACL `Read` on locus `{tenant}/timeline`.

use axum::{extract::State, http::StatusCode, Extension, Json};
use gradatum_acl_policy::{AclDecision, AclOp};
use gradatum_core::scope::VaultId;
use gradatum_core::temporal_query::{TimelineCursor, TimelineFilter};
use gradatum_core::trust::TrustContext;
use serde::Serialize;

use crate::api_v1::dto::VaultTimelineRequest;
use crate::api_v1::tenant_guard::effective_tenant;
use crate::state::AppState;

/// Accepted `doc_kind` values (strict allowlist).
///
/// `Versioned` is reserved (never produced at this time) but accepted as a filter value.
const KNOWN_DOC_KINDS: [&str; 3] = ["Static", "Event", "Versioned"];

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
) -> Result<Json<VaultTimelineResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // P0 cross-tenant (Lot 3) : tenant dérivé du JWT, refuse body divergent.
    let tenant = effective_tenant(&trust, &req.tenant_id)?.to_owned();
    let acl_locus = format!("{}/timeline", tenant);
    if state.acl.evaluate(&trust, AclOp::Read, &acl_locus) != AclDecision::Allow {
        tracing::warn!(locus = %acl_locus, "vault_timeline: ACL Read deny");
        return Err(StatusCode::FORBIDDEN);
    }

    // ── Validation ───────────────────────────────────────────────────────────
    if let (Some(f), Some(t)) = (req.from_ms, req.to_ms) {
        if f > t {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if let Some(kinds) = req.doc_kind.as_ref() {
        // V3 — borne le Vec AVANT la validation par-élément : un client ne peut
        // jamais demander plus de variants qu'il n'en existe (≤ KNOWN_DOC_KINDS).
        if kinds.len() > KNOWN_DOC_KINDS.len() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if kinds.iter().any(|k| !KNOWN_DOC_KINDS.contains(&k.as_str())) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if let Some(v) = req.vault_id.as_ref() {
        if v.is_empty() || v.len() > 128 {
            return Err(StatusCode::BAD_REQUEST);
        }
        // P0 cross-tenant (Lot 4) : cross-read vault_id ≠ main non supporté (mono-vault).
        if v != "main" {
            tracing::warn!(vault_id = %v, "vault_timeline: cross-read vault_id ≠ main — 403");
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let cursor = match req.cursor.as_deref() {
        Some(s) => Some(TimelineCursor::decode(s).map_err(|_| StatusCode::BAD_REQUEST)?),
        None => None,
    };

    let limit = req.limit.unwrap_or(50).clamp(1, 200) as usize;
    // vault_id (si présent) == "main" == tenant après le clamp Lot 4 → tenant dérivé.
    let vault = VaultId(req.vault_id.unwrap_or_else(|| tenant.clone()));
    let filter = TimelineFilter {
        doc_kind: req.doc_kind,
        from_ms: req.from_ms,
        to_ms: req.to_ms,
        limit,
        cursor,
        as_of_ms: req.as_of_ms,
        include_expired: req.include_expired,
    };

    let rows = state.search.timeline(&vault, &filter).await.map_err(|e| {
        tracing::error!(err = %e, vault = %vault.0, "vault_timeline: timeline storage failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // next_cursor émis ssi la page est pleine (heuristique standard keyset).
    let next_cursor = if rows.len() == limit {
        rows.last().map(|r| {
            TimelineCursor {
                anchor_ms: r.anchor_ms,
                note_id: r.note_id.0.to_string(),
            }
            .encode()
        })
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(|r| TimelineItem {
            note_id: r.note_id.0.to_string(),
            anchor_ms: r.anchor_ms,
            anchor_src: r.anchor_src,
            doc_kind: r.doc_kind,
            title: r.title,
        })
        .collect();

    Ok(Json(VaultTimelineResponse { items, next_cursor }))
}
