//! Handler GET /health
//!
//! Retourne le statut du gateway et la liste des providers configurés.
//! Répond toujours 200 si le service tourne (pas de probing backend).
//!
//! Réponse :
//! ```json
//! { "status": "ok", "version": "0.3.0", "providers": ["my-provider"] }
//! ```

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Réponse du health endpoint.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub providers: Vec<String>,
}

/// Handler GET /health
///
/// Retourne 200 + JSON avec statut "ok" et liste des providers configurés.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        providers: state.config.provider_names(),
    })
}
