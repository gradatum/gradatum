//! Handler GET /v1/models
//!
//! Retourne la liste des aliases configurés au format OpenAI list.
//! Source : `config.aliases` — pas de probing backend.
//!
//! Réponse :
//! ```json
//! {
//!   "object": "list",
//!   "data": [
//!     { "id": "qwen3-alias", "object": "model", "created": 0, "owned_by": "my-provider" }
//!   ]
//! }
//! ```

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Un modèle dans la liste OpenAI-compat.
#[derive(Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

/// Réponse format OpenAI list.
#[derive(Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelInfo>,
}

/// Handler GET /v1/models
///
/// Construit la liste depuis `config.aliases` — aucun appel réseau.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<ModelsResponse> {
    let mut data: Vec<ModelInfo> = state
        .config
        .aliases
        .iter()
        .map(|(alias_id, target)| ModelInfo {
            id: alias_id.clone(),
            object: "model",
            created: 0,
            owned_by: target.provider.clone(),
        })
        .collect();

    // Tri alphabétique pour une réponse déterministe.
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Json(ModelsResponse {
        object: "list",
        data,
    })
}
