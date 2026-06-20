//! Handler for `GET /metrics`.
//!
//! Exposes Prometheus metrics in text format 0.0.4.
//! Content-Type: `text/plain; version=0.0.4; charset=utf-8`

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tracing::instrument;

use crate::AppState;

/// Handler for `GET /metrics`.
///
/// Renders the full Prometheus export. Always returns 200.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();

    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )
        .body(axum::body::Body::from(body))
        // SAFETY: headers are static values — this error cannot occur.
        .expect("construction réponse /metrics impossible avec headers statiques")
        .into_response()
}
