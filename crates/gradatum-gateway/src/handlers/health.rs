//! Handler for `GET /health`.
//!
//! Returns the gateway status and the list of configured providers.
//! Always responds with 200 when the service is running (no backend probing).
//!
//! Response:
//! ```json
//! { "status": "ok", "version": "0.3.0", "providers": ["my-provider"] }
//! ```

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Response body for the health endpoint.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub providers: Vec<String>,
}

/// Handler for `GET /health`.
///
/// Returns 200 with a JSON body containing status `"ok"` and the list of configured providers.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        providers: state.config.provider_names(),
    })
}
