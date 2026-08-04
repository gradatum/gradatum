//! `GET /api/v1/review` — Curator / distillation review queue.
//!
//! Paginated list of notes awaiting human judgment:
//! `status ∈ {pending-review, staging}`. Fed by the curator
//! (`CurateOutcome::Pending → PendingReview`) and by the distillation pipeline
//! (native `PendingReview`).
//!
//! # Contract
//!
//! | Method | Path | Query params | Response | Codes |
//! |--------|------|--------------|----------|-------|
//! | GET | `/api/v1/review` | `limit` (opt, default 50, max 200), `cursor` (opt, ULID) | [`ReviewQueueResponse`] | 200 / 400 / 401 / 403 / 500 |
//!
//! - Auth: standard JWT (Read) — same middleware as all other `/api/v1` routes.
//! - `provenance` distinguishes `distilled` notes from curator/agent origin.
//! - Curator `confidence` is **not exposed**: not persisted in the current version.
//! - `staging` notes remain listed (distinct legacy badge on the UI side) pending a
//!   one-shot `staging → pending-review` migration (deferred to a future release).

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use gradatum_core::scope::VaultId;
use gradatum_core::trust::TrustContext;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Vault namespace ciblé par ce handler (dimension NAMESPACE, distincte du
/// principal `TenantId`).
///
/// Déploiement single-vault : toujours `main`, aligné sur `vault_status`. Point de
/// résolution **typé** remplaçant l'ancien `const TENANT: &str` — en multi-vault
/// (Groupe B) il deviendra un routage par registre plutôt qu'un littéral.
#[must_use]
pub fn target_vault() -> VaultId {
    VaultId::new("main")
}

/// Query parameters for `GET /api/v1/review`.
#[derive(Debug, Deserialize)]
pub struct ReviewQueueQuery {
    /// Maximum rows per page (default 50, clamped to `[1, 200]`).
    pub limit: Option<usize>,
    /// Pagination cursor = last ULID received (next page).
    pub cursor: Option<String>,
}

/// A single entry in the review queue (wire response).
#[derive(Debug, Serialize)]
pub struct ReviewItem {
    /// Note ULID.
    pub ulid: String,
    /// H1 title (`null` if absent).
    pub title: Option<String>,
    /// Canonical section (e.g. `"decisions"`).
    pub section: String,
    /// Physical locus (path), `null` if unassigned.
    pub locus: Option<String>,
    /// Status: `"pending-review"` or `"staging"`.
    pub status: String,
    /// Provenance (`"distilled"` for distillation output, otherwise curator/agent), `null` if absent.
    pub provenance: Option<String>,
    /// Creation timestamp (Unix epoch ms UTC).
    pub created_ms: i64,
}

/// Response for `GET /api/v1/review`.
#[derive(Debug, Serialize)]
pub struct ReviewQueueResponse {
    /// Rows on the current page (may be empty).
    pub items: Vec<ReviewItem>,
    /// Next-page cursor (ULID), `null` on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total notes in the review queue (across all pages), for counter display.
    pub total: u64,
}

/// `GET /api/v1/review?limit=<n>&cursor=<ulid>`
///
/// See the module documentation for the full contract.
///
/// # Errors
/// - `401 Unauthorized`: unauthenticated request.
/// - `403 Forbidden`: ACL Read denied on `main/*`.
/// - `400 Bad Request`: malformed `cursor` (not a ULID).
/// - `500 Internal Server Error`: index storage failure.
pub async fn list_review(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Query(params): Query<ReviewQueueQuery>,
) -> Result<Json<ReviewQueueResponse>, StatusCode> {
    if !trust.is_authenticated() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // T9 (A3-handlers) : OFF = ACL Read legacy sur `main/review` (byte-identical,
    // lecture transverse toutes sections) ; ON = vault effectif du principal JWT.
    let vault_id =
        crate::api_v1::tenant_guard::resolve_read_vault(&state, &trust, target_vault(), "review")
            .await?;

    let limit = params.limit.unwrap_or(50).clamp(1, 200);

    // Validation cursor : doit être un ULID si fourni (anti-injection + cohérence).
    let cursor: Option<&str> = match params.cursor.as_deref() {
        Some(c) => {
            if ulid::Ulid::from_string(c).is_err() {
                tracing::warn!(cursor = %c, "list_review: cursor non-ULID");
                return Err(StatusCode::BAD_REQUEST);
            }
            Some(c)
        }
        None => None,
    };

    // limit + 1 pour détecter la page suivante sans seconde requête.
    let mut rows = state
        .search
        .list_review_queue(&vault_id, cursor, limit + 1)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "list_review: list_review_queue failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let has_more = rows.len() > limit;
    if has_more {
        rows.truncate(limit);
    }
    let next_cursor = if has_more {
        rows.last().map(|r| r.note_id.0.to_string())
    } else {
        None
    };

    let total = state
        .search
        .count_review_queue(&vault_id)
        .await
        .map_err(|e| {
            tracing::error!(err = %e, "list_review: count_review_queue failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Guard identité (parité stricte `vault_list_impl` / `get_notes_by_status`) :
    // la file de revue est une surface de listing TRANSVERSE (toutes sections, filtre
    // `status IN ('pending-review','staging')` côté SQL — aucune exclusion `identity`).
    // Une âme d'agent qui atteindrait l'un de ces statuts verrait son titre
    // (`identity/<agent>`) + section exfiltrés vers tout appelant disposant de l'ACL
    // `main/review`. Fail-closed : on masque les entrées `identity` pour un appelant non
    // privilégié AVANT de construire les items. No-op pour Studio / main-agent / owner.
    // `next_cursor` (calculé plus haut) et `total` restent sur le compte BRUT — la
    // pagination continue d'avancer même si une page entière est masquée.
    let identity_privileged = crate::api_v1::logic::is_identity_privileged(&trust);

    let items: Vec<ReviewItem> = rows
        .into_iter()
        .filter(|r| !crate::api_v1::logic::identity_section_hidden(identity_privileged, &r.section))
        .map(|r| ReviewItem {
            ulid: r.note_id.0.to_string(),
            title: r.title,
            section: r.section,
            locus: r.locus,
            status: r.status,
            provenance: r.provenance,
            created_ms: r.created_ms,
        })
        .collect();

    Ok(Json(ReviewQueueResponse {
        items,
        next_cursor,
        total,
    }))
}
