//! Handlers HTTP pour les endpoints proactive recall (F-46).
//!
//! | Méthode | Path | Handler |
//! |---|---|---|
//! | POST | `/api/v1/proactive_recall` | [`proactive_recall`] |
//! | POST | `/api/v1/proactive_recall/feedback` | [`proactive_recall_feedback`] |
//!
//! Both handlers delegate directly to the orchestrators in
//! [`crate::proactive_recall`] (in-process, direct access to `AppState`).

use axum::{Extension, Json, extract::State, http::StatusCode};
use gradatum_core::trust::TrustContext;
use gradatum_dto::{
    ProactiveRecallFeedbackRequest, ProactiveRecallRequest, ProactiveRecallResponse,
};

use crate::state::AppState;

/// `POST /api/v1/proactive_recall`
///
/// Orchestre un rappel proactif (pull) — lecture de surface ou retrieval contextuel.
///
/// ## Modes
///
/// - **`context` absent** → mode `"proactive"` : lit la surface pré-calculée
///   ([`crate::proactive_recall::proactive_recall`], `mode = "proactive"`).
///   Surface absente (service démarré depuis < 1 refresh) → `items: []`, pas d'erreur.
/// - **`context` présent** → mode `"contextual"` : retrieval RRF à la demande
///   (`mode = "contextual"`).
///
/// ## Re-filtrage ACL
///
/// Les deux modes appliquent un re-filtrage ACL par section (C3, BLOQUANT) :
/// les notes dont la section n'est pas lisible par l'appelant sont exclues du résultat.
/// Voir la doc de [`crate::proactive_recall::proactive_recall`].
///
/// ## Codes HTTP
///
/// | Code | Raison |
/// |------|--------|
/// | 200 | Succès — `ProactiveRecallResponse` JSON (`recall_id`, `mode`, `items`). |
/// | 400 | Corps JSON invalide ou champ inconnu (`deny_unknown_fields`). |
/// | 401 | Appelant non authentifié. |
/// | 403 | ACL Read refusée ou tenant divergent du JWT. |
/// | 500 | Erreur SQL irrécupérable (`get_surface`, retrieval, hydratation). |
///
/// # Errors
///
/// Retourne [`StatusCode`] si l'orchestrateur propage une [`gradatum_core::error::GradatumError`].
pub async fn proactive_recall(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<ProactiveRecallRequest>,
) -> Result<Json<ProactiveRecallResponse>, StatusCode> {
    crate::proactive_recall::proactive_recall(&state, &trust, req)
        .await
        .map(Json)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "proactive_recall: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}

/// `POST /api/v1/proactive_recall/feedback`
///
/// Enregistre le feedback d'acceptation pour une session de rappel proactif.
///
/// `recall_id` identifie la session de rappel (retourné par [`proactive_recall`]).
/// `accepted_ulids` est la liste des ULIDs effectivement acceptés (⊆ surfaced).
///
/// ## Validation (ordre strict)
///
/// 1. ACL/tenant identique à [`proactive_recall`].
/// 2. `recall_id` existe → 400 si inconnu.
/// 3. Chaque `accepted_ulid` parse en ULID → 400 si malformé.
/// 4. `accepted_ulids ⊆ surfaced` → 400 si sur-ensemble.
/// 5. `record_feedback` (UPSERT idempotent — 2× le même feedback = 1 ligne).
///
/// ## Codes HTTP
///
/// | Code | Raison |
/// |------|--------|
/// | 200 | Feedback enregistré. |
/// | 400 | `recall_id` inconnu, ULID mal formé, ou `accepted ⊄ surfaced`. |
/// | 401 | Appelant non authentifié. |
/// | 403 | ACL Read refusée ou tenant divergent du JWT. |
/// | 500 | Erreur SQL irrécupérable (`get_surfaced`, `record_feedback`). |
///
/// # Errors
///
/// Retourne [`StatusCode`] si l'orchestrateur propage une [`gradatum_core::error::GradatumError`].
pub async fn proactive_recall_feedback(
    State(state): State<AppState>,
    Extension(trust): Extension<TrustContext>,
    Json(req): Json<ProactiveRecallFeedbackRequest>,
) -> Result<StatusCode, StatusCode> {
    crate::proactive_recall::proactive_recall_feedback(&state, &trust, req)
        .await
        .map(|()| StatusCode::OK)
        .map_err(|e| {
            if matches!(e, gradatum_core::error::GradatumError::Storage(_)) {
                tracing::error!(err = %e, "proactive_recall_feedback: backend failed");
            }
            crate::api_v1::logic::err_to_status(&e)
        })
}
