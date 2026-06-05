//! Phase 2.1.2 alpha.9 — Endpoints synchrones notes.
//!
//! Contrairement aux handlers `write.rs` (202 Accepted async queue), ces handlers
//! sont **synchrones** : ils opèrent directement sur l'index SQLite et retournent
//! le résultat immédiatement.
//!
//! # Endpoints
//!
//! | Méthode | Path | Retour | Notes |
//! |---------|------|--------|-------|
//! | POST | `/vault_downgrade` | 200 + JSON [`VaultDowngradeResponse`] | Remplace write::vault_downgrade (202 async) |
//! | PATCH | `/notes/{id}` | 204 No Content | Patch partiel status/reason/replaced_by |
//!
//! # Auth
//!
//! Ces endpoints ne requièrent pas de bearer JWT (MVP Phase 2.1.2 — V4 default false).
//! Invariant réseau privé : accès présupposé loopback ou réseau privé (VPN, pare-feu).
//!
//! # Idempotence
//!
//! `POST /vault_downgrade` est idempotent : un second appel met à jour la raison
//! et le timestamp sans erreur (comportement `downgrade_note` SqliteIndex).
//!

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{patch as patch_route, post},
    Json, Router,
};
use gradatum_core::error::GradatumError;
use gradatum_core::identity::NoteId;
use gradatum_dto::{NoteStatusPatch, VaultDowngradeRequest, VaultDowngradeResponse};
use ulid::Ulid;

use crate::state::AppState;

/// Construit le sous-routeur notes (routes fixes avant paramétriques).
///
/// Intégré dans `api_v1::router()` via `.merge(notes::router())`.
/// Routes fixes définies AVANT les routes paramétriques (convention fixed-before-parametric).
pub fn router() -> Router<AppState> {
    Router::new()
        // Route fixe POST vault_downgrade — avant la route paramétrique /notes/{id}
        .route("/vault_downgrade", post(vault_downgrade))
        // Route paramétrique PATCH /notes/{id} — axum 0.8 : syntaxe {param} (remplace :param de 0.7)
        .route("/notes/{id}", patch_route(patch_note))
}

/// Parse un ULID depuis une string — retourne 400 Bad Request si invalide.
fn parse_note_id(s: &str) -> Result<NoteId, StatusCode> {
    Ulid::from_string(s)
        .map(NoteId)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

/// `POST /api/v1/vault_downgrade` — rétrogradation synchrone d'une note.
///
/// Positionne `status = 'downgraded'` + `status_reason` + `replaced_by` (optionnel)
/// directement dans l'index SQLite. Retourne 200 + JSON immédiatement.
///
/// Remplace l'ancien handler async `write::vault_downgrade` (202 + queue).
///
/// # Retour
///
/// - **200 OK** + JSON [`VaultDowngradeResponse`] — note rétrogradée avec succès.
/// - **400 Bad Request** — `note_id` ou `replaced_by` n'est pas un ULID valide.
/// - **404 Not Found** — aucune note avec cet id dans l'index.
/// - **500 Internal Server Error** — erreur SQLite inattendue.
///
/// # Idempotence
///
/// Un second appel sur une note déjà downgradée met à jour la raison et le timestamp
/// sans erreur — comportement `downgrade_note` (UPDATE toujours exécuté si la note existe).
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

/// `PATCH /api/v1/notes/{id}` — patch partiel du status d'une note.
///
/// Met à jour uniquement les champs fournis dans le body JSON (`None` = inchangé).
/// Au moins un champ doit être présent — retourne 400 sinon.
///
/// # Retour
///
/// - **204 No Content** — patch appliqué avec succès.
/// - **400 Bad Request** — `id` ULID invalide, aucun champ fourni, ou `status` invalide.
/// - **404 Not Found** — aucune note avec cet id dans l'index.
/// - **500 Internal Server Error** — erreur SQLite inattendue.
///
/// # Valeurs `status` acceptées
///
/// `"live"` | `"staging"` | `"downgraded"` — toute autre valeur retourne 400.
async fn patch_note(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<NoteStatusPatch>,
) -> Result<StatusCode, StatusCode> {
    let note_id = parse_note_id(&id)?;

    // Au moins un champ requis — guard applicatif avant appel SQL.
    if body.status.is_none() && body.status_reason.is_none() && body.replaced_by.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Valider le status si fourni.
    if let Some(s) = &body.status {
        if !matches!(s.as_str(), "live" | "staging" | "downgraded") {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let replaced_by = body.replaced_by.as_deref().map(parse_note_id).transpose()?;

    state
        .search
        .patch_note_status(
            &note_id,
            body.status.as_deref(),
            body.status_reason.as_deref(),
            replaced_by.as_ref(),
        )
        .await
        .map_err(|e| match e {
            GradatumError::NoteNotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })?;

    Ok(StatusCode::NO_CONTENT)
}
