//! Synchronous note endpoints.
//!
//! Unlike the `write.rs` handlers (202 Accepted async queue), these handlers are
//! **synchronous**: they operate directly on the SQLite index and return the result
//! immediately.
//!
//! # Endpoints
//!
//! | Method | Path | Response | Notes |
//! |--------|------|----------|-------|
//! | POST | `/vault_downgrade` | 200 + JSON [`VaultDowngradeResponse`] | Replaces the async 202 downgrade |
//! | PATCH | `/notes/{id}` | 204 No Content | Partial patch: status / reason / replaced_by |
//!
//! # `PATCH /notes/{id}` routing — state machine harmonization
//!
//! When `body.status` is provided:
//! - Deserialized into `NoteStatus` (6-state enum: draft/staging/pending-review/live/deprecated/garbage).
//! - Validated by the state machine via `vault.update_note_status` (CoW traces each transition).
//! - `"downgraded"` and any out-of-enum value → **400 Bad Request** (outside the graph).
//! - Invalid transition → **409 Conflict** (state conflict).
//! - Missing note → **404 Not Found**.
//!
//! When both `body.status` and `replaced_by` are provided:
//! - The status transition is applied via the state machine (`vault.update_note_status`).
//! - The `replaced_by` field is then patched via `search.patch_note_status` (direct SQL).
//!
//! When `body.status` is `None` but `status_reason` or `replaced_by` is provided:
//! - Partial patch via `search.patch_note_status` (direct SQL, no state machine).
//!   Allows updating the reason or `replaced_by` without changing the status.
//!
//! # Auth
//!
//! These endpoints do not require a bearer JWT (private network assumed).
//! Access is expected to come from the loopback interface or a private network (VPN, firewall).
//!
//! # Idempotence
//!
//! `POST /vault_downgrade` is idempotent: a second call updates the reason and
//! timestamp without error (`downgrade_note` always executes an UPDATE if the note exists).
//!
//! `PATCH /notes/{id}` with the same status as the current one → **no-op 204** (state
//! machine idempotence — `update_note_status` returns `Ok(())` when target == current).
//!

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{patch as patch_route, post},
    Json, Router,
};
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::LocusId;
use gradatum_core::status::NoteStatus;
use gradatum_dto::{NoteStatusPatch, VaultDowngradeRequest, VaultDowngradeResponse};
use serde::Deserialize;
use ulid::Ulid;

use crate::state::AppState;

/// Builds the notes sub-router (fixed routes before parametric).
///
/// Merged into `api_v1::router()` via `.merge(notes::router())`.
/// Fixed routes are defined BEFORE parametric routes (fixed-before-parametric convention).
pub fn router() -> Router<AppState> {
    Router::new()
        // Route fixe POST vault_downgrade — avant la route paramétrique /notes/{id}
        .route("/vault_downgrade", post(vault_downgrade))
        // Route paramétrique PATCH /notes/{id} — axum 0.8 : syntaxe {param} (remplace :param de 0.7)
        .route("/notes/{id}", patch_route(patch_note))
        // F-37 S1.4 — POST /notes/{id}/move (move to locus). Segment supplémentaire
        // → ne conflicte pas avec /notes/{id} (cardinalité de segments distincte).
        .route("/notes/{id}/move", post(move_note_locus))
}

/// Parses a ULID from a string — returns 400 Bad Request if invalid.
fn parse_note_id(s: &str) -> Result<NoteId, StatusCode> {
    Ulid::from_string(s)
        .map(NoteId)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// Maximum number of tags that can be added in a single PATCH call (anti-DoS cap).
const MAX_ADD_TAGS_PER_CALL: usize = 20;

/// Validates and normalizes the `add_tags` list from a PATCH body.
///
/// Rules (→ `StatusCode::BAD_REQUEST` on violation):
/// - list must be non-empty (an explicit `add_tags: []` is rejected — nothing to add);
/// - at most [`MAX_ADD_TAGS_PER_CALL`] tags;
/// - each tag must be valid per `Tag::new` (non-empty, lowercase-with-dash, ≤ 64 chars);
/// - **case-insensitive** deduplication within the call (input duplicates are merged).
///
/// Returns the deduplicated list (original case preserved for the first occurrence).
/// Union with the note's **existing** tags is performed on the vault side (`add_tags`).
fn validate_add_tags(raw: &[String]) -> Result<Vec<String>, StatusCode> {
    if raw.is_empty() || raw.len() > MAX_ADD_TAGS_PER_CALL {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut seen_lower = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for t in raw {
        // Format strict — Tag::new rejette vide, casse haute, caractères interdits.
        gradatum_core::tag::Tag::new(t.clone()).map_err(|_| StatusCode::BAD_REQUEST)?;
        let lower = t.to_ascii_lowercase();
        if seen_lower.insert(lower) {
            out.push(t.clone());
        }
    }
    Ok(out)
}

/// `POST /api/v1/vault_downgrade` — synchronous note downgrade.
///
/// Sets `status = 'downgraded'` + `status_reason` + optional `replaced_by`
/// directly in the SQLite index. Returns 200 + JSON immediately.
///
/// # Responses
///
/// - **200 OK** + JSON [`VaultDowngradeResponse`] — note downgraded successfully.
/// - **400 Bad Request** — `note_id` or `replaced_by` is not a valid ULID,
///   or `replaced_by == note_id` (self-reference forbidden).
/// - **404 Not Found** — no note with that id in the index, or `replaced_by`
///   references a non-existent note.
/// - **500 Internal Server Error** — unexpected SQLite error.
///
/// # Idempotence
///
/// A second call on an already-downgraded note updates the reason and timestamp
/// without error — `downgrade_note` always executes an UPDATE when the note exists.
async fn vault_downgrade(
    State(state): State<AppState>,
    Json(req): Json<VaultDowngradeRequest>,
) -> Result<Json<VaultDowngradeResponse>, StatusCode> {
    let note_id = parse_note_id(&req.note_id)?;
    let replaced_by = req.replaced_by.as_deref().map(parse_note_id).transpose()?;

    state
        .search
        .downgrade_note(&note_id, &req.reason, replaced_by.as_ref())
        .await
        .map_err(|e| match e {
            GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
            GradatumError::Validation(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    let now = chrono::Utc::now().timestamp_millis();
    Ok(Json(VaultDowngradeResponse {
        note_id: req.note_id,
        status: "downgraded".to_string(),
        status_changed: now,
        reason: req.reason,
    }))
}

/// `PATCH /api/v1/notes/{id}` — partial note patch.
///
/// Updates only the fields provided in the JSON body (`None` = unchanged).
/// At least one field must be present — returns 400 otherwise.
///
/// # Responses
///
/// - **204 No Content** — patch applied successfully (or idempotent no-op).
/// - **400 Bad Request** — invalid ULID `id`, no field provided, or `status`
///   out of enum (e.g. `"downgraded"` belongs to the separate downgrade mechanism).
/// - **404 Not Found** — no note with that id in the index or vault.
/// - **409 Conflict** — invalid status transition per the state machine
///   (e.g. Live → Draft). The current status is included in the error message.
/// - **500 Internal Server Error** — unexpected error.
///
/// # Accepted `status` values
///
/// The 6 `NoteStatus` enum variants in kebab-case:
/// `"draft"` | `"staging"` | `"pending-review"` | `"live"` | `"deprecated"` | `"garbage"`.
///
/// `"downgraded"` (out-of-enum, legacy downgrade mechanism) → **400**.
///
/// # Internal routing
///
/// - `body.status` provided → `vault.update_note_status` (state machine + CoW).
/// - `body.status` absent, `status_reason`/`replaced_by` only → `search.patch_note_status`
///   (direct SQL, no state machine — allows updating the reason independently).
/// - `body.add_tags` provided → `vault.add_tags` (CoW + FTS reindex) — additive, case-insensitive
///   UNION. Validated first (400 if a tag is empty/malformed or count > 20). Applied after status
///   for a combined PATCH.
async fn patch_note(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NoteStatusPatch>,
) -> Result<StatusCode, StatusCode> {
    let note_id = parse_note_id(&id)?;

    // Au moins un champ requis — guard applicatif avant tout appel.
    if body.status.is_none()
        && body.status_reason.is_none()
        && body.replaced_by.is_none()
        && body.add_tags.is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Valider add_tags AVANT toute mutation (fail-fast 400 — n'applique pas le status
    // si les tags sont invalides : un PATCH combiné est atomique du point de vue validation).
    let validated_tags = match body.add_tags.as_deref() {
        Some(raw) => Some(validate_add_tags(raw)?),
        None => None,
    };

    if let Some(ref status_str) = body.status {
        // Désérialiser le status en NoteStatus typé.
        // `"downgraded"` et toute valeur hors enum → 400.
        let target: NoteStatus =
            serde_json::from_value(serde_json::Value::String(status_str.clone()))
                .map_err(|_| StatusCode::BAD_REQUEST)?;

        // Passer par la state machine via vault.
        state
            .vault
            .update_note_status(&note_id.to_string(), target, body.status_reason.clone())
            .await
            .map_err(|e| match e {
                GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
                GradatumError::InvalidStatusTransition { .. } => StatusCode::CONFLICT,
                GradatumError::Validation(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            })?;

        // B5 audit C2 — fix : replaced_by fourni conjointement avec status → patcher
        // via SQL direct après la transition state machine (update_note_status ne le prend pas).
        if body.replaced_by.is_some() {
            let replaced_by = body.replaced_by.as_deref().map(parse_note_id).transpose()?;
            state
                .search
                .patch_note_status(&note_id, None, None, replaced_by.as_ref())
                .await
                .map_err(|e| match e {
                    GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                })?;
        }
    } else {
        // Patch partiel sans changement de statut — SQL direct (raison / replaced_by).
        let replaced_by = body.replaced_by.as_deref().map(parse_note_id).transpose()?;

        state
            .search
            .patch_note_status(
                &note_id,
                None,
                body.status_reason.as_deref(),
                replaced_by.as_ref(),
            )
            .await
            .map_err(|e| match e {
                GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            })?;
    }

    // A4 unblock — ajout additif de tags (UNION case-insensitive, via le chemin vault CoW
    // qui réindexe le FTS). Appliqué après le status pour qu'un PATCH combiné status+add_tags
    // applique d'abord la transition d'état, puis fusionne les tags sur la note résultante.
    if let Some(tags) = validated_tags {
        state
            .vault
            .add_tags(&note_id.to_string(), &tags)
            .await
            .map_err(|e| match e {
                GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
                // dépassement du cap total (MAX_NOTE_TAGS) → 409
                // Conflict (l'état n'a pas été modifié, c'est un conflit de ressource).
                // Les autres Validation (tag mal formé) sont déjà filtrées en amont par
                // validate_add_tags (400) ; par sécurité elles restent en 400 ici.
                GradatumError::Validation(gradatum_core::error::ValidationError::InvalidInput(
                    _,
                )) => StatusCode::CONFLICT,
                GradatumError::Validation(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            })?;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Request body for `POST /api/v1/notes/{id}/move`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveLocusRequest {
    /// Target locus (strictly validated via `LocusId::parse` in the handler).
    pub locus: String,
}

/// `POST /api/v1/notes/{id}/move` — moves a note to a new locus.
///
/// **Physical relocation**: the `.md` file is rewritten to
/// `<tenant>/<new_locus>/<id>.md` via `vault.move_locus` (Vault path), the index is
/// updated, and the orphan old `.md` is deleted. The ULID is preserved —
/// **no redirect table** (the locus is not part of the identity path; wikilinks
/// resolve by title/ULID).
///
/// ## Consistency contract
///
/// Before this implementation, a move was an index-only mutation (`UPDATE notes.locus`):
/// the `.md` was not relocated and `vault_read` continued to expose the **old** locus.
/// The operation is now end-to-end consistent:
/// - the `.md` is rewritten at the new path (`frontmatter.locus` updated);
/// - a CoW snapshot is created under `.history/` (the `content_hash` changes because
///   the locus is part of the JCS hash);
/// - the index reflects the new locus;
/// - `vault_read` returns the **new** locus immediately after the call.
///
/// Since `locus` is not a `notes_fts` column, no FTS reindex is required.
///
/// # Locus validation
/// `LocusId::parse`: non-empty, charset `[a-z0-9-/]`, ≤ 128 bytes, anti-traversal.
///
/// # Responses
/// - **204 No Content** — move completed (or no-op if locus is unchanged).
/// - **400 Bad Request** — `id` is not a ULID, or `locus` is invalid (charset/traversal/length).
/// - **404 Not Found** — no note with that id.
/// - **500 Internal Server Error** — unexpected storage error.
async fn move_note_locus(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MoveLocusRequest>,
) -> Result<StatusCode, StatusCode> {
    // Validation de l'id (ULID) — 400 si invalide, sans toucher au vault.
    let _ = parse_note_id(&id)?;

    // Validation stricte du locus cible (parse-don't-validate à la frontière).
    let locus = LocusId::parse(body.locus.trim()).map_err(|e| {
        tracing::warn!(locus = %body.locus, err = %e, "move_note_locus: locus invalide");
        StatusCode::BAD_REQUEST
    })?;

    // D1.1 — relocalisation physique via le chemin Vault (réécriture .md + CoW +
    // index + suppression orphelin). Remplace l'ancienne mutation index-only.
    state
        .vault
        .move_locus(&id, &locus)
        .await
        .map_err(|e| match e {
            GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
            GradatumError::Validation(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    Ok(StatusCode::NO_CONTENT)
}
