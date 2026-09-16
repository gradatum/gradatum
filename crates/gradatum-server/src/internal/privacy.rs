//! Handler du balayage différé de confidentialité (F-160 L-D.2) — API interne.
//!
//! `POST /internal/v1/privacy/rescan` — re-juge un lot borné des notes
//! `privacy-unscanned` d'un vault, et rend les trois compteurs du lot.
//!
//! ## Pourquoi cette route existe
//!
//! Le worker ne détient ni vault ni index (il parle au serveur par l'API interne) et
//! le vault ne détient aucun handle de file : la seule partie qui tient à la fois le
//! registre des vaults et la primitive d'écriture est le serveur. Le handler Apalis
//! `handle_privacy_rescan` n'a donc rien à re-juger lui-même — il transporte une
//! enveloppe jusqu'ici, et c'est [`rescan_selected_notes`] qui sélectionne et réécrit.
//!
//! ## Portée
//!
//! Loopback uniquement, sous le token worker (`X-Gradatum-Internal`) : la route est
//! montée sur le sous-routeur worker de [`build_internal_router`](super::build_internal_router)
//! et jamais fusionnée au routeur public. Aucun client authentifié ne peut donc
//! déclencher un balayage — il coûte du CPU, et la liste blanche de
//! `POST /api/v1/jobs` ne porte pas ce `kind`.
//!
//! ## Ce que la route ne décide pas
//!
//! Le périmètre (`code-*`), la Garde 0 sur `Section::Identity` et le verdict
//! « inchangé ⇒ pas d'écriture » appartiennent à la primitive
//! ([`gradatum_vault::Registry::rescan_privacy_note`]) et sont ré-appliqués par elle
//! à chaque note. Le handler ne filtre rien : il valide deux entrées et rend ce que
//! la primitive a répondu.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gradatum_dto::{PrivacyRescanRequest, PrivacyRescanResponse};

use super::reads::validate_param_len;
use crate::privacy_rescan::RESCAN_BATCH_LIMIT;
use crate::state::AppState;

/// Longueur maximale acceptée pour `vault_id` (même plafond que les autres lectures
/// internes : aucun `vault_id` légitime n'approche cette borne).
const MAX_VAULT_ID_LEN: usize = 256;

/// Re-juge un lot borné des notes non scannées d'un vault.
///
/// ## Réponses
///
/// - `200` + [`PrivacyRescanResponse`] — les compteurs du lot. Un lot vide est un
///   succès (`0/0/0`) : plus rien n'est `privacy-unscanned` est le but du balayage,
///   pas une erreur.
/// - `400` si `vault_id` est vide ou trop long, ou si `limit` vaut `0`.
/// - `500` si l'index est illisible ou si une re-jugement échoue. Une note qui n'est
///   plus là, ou que la garde exclut, n'est **pas** une erreur : elle est comptée
///   `skipped`.
pub(crate) async fn handle_privacy_rescan(
    State(state): State<AppState>,
    Json(req): Json<PrivacyRescanRequest>,
) -> Response {
    if req.vault_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "missing `vault_id` field".to_string(),
        )
            .into_response();
    }
    if let Err(r) = validate_param_len(&req.vault_id, MAX_VAULT_ID_LEN, "vault_id") {
        return r;
    }
    // Un lot de zéro note serait un succès qui ne fait rien : la borne que le job
    // transporte est un quota, et un quota nul est une erreur d'appelant, jamais un
    // balayage. Au-delà du plafond de l'index, la borne est ramenée à celle que la
    // requête honore réellement — une borne non honorée n'est pas une borne.
    if req.limit == 0 {
        return (
            StatusCode::BAD_REQUEST,
            "`limit` must be at least 1".to_string(),
        )
            .into_response();
    }
    let limit = req.limit.min(RESCAN_BATCH_LIMIT);

    match crate::privacy_rescan::rescan_selected_notes(&state, &req.vault_id, limit).await {
        Ok(summary) => Json(PrivacyRescanResponse::new(
            summary.rewritten,
            summary.unchanged,
            summary.skipped,
        ))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("privacy rescan failed: {e}"),
        )
            .into_response(),
    }
}
