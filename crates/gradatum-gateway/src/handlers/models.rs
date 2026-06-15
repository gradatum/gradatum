//! Handler for `GET /v1/models`.
//!
//! Returns the list of configured aliases in OpenAI list format.
//! Source: `config.aliases` — no backend probing.
//!
//! Response:
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

/// A model entry in the OpenAI-compat list.
#[derive(Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: String,
}

/// OpenAI list format response.
#[derive(Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelInfo>,
}

/// Handler for `GET /v1/models`.
///
/// Builds the list from `config.aliases` — no network call.
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

    // Alphabetical sort for a deterministic response.
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Json(ModelsResponse {
        object: "list",
        data,
    })
}
