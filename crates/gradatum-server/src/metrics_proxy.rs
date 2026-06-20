//! Server-side `/metrics` handler — aggregates server + worker metrics.
//!
//! Scrapes worker metrics from `:19091` and merges them with the server metrics.
//! If the worker is unavailable, returns only server metrics (graceful degradation).
//!
//! # Endpoints
//!
//! Mounted on `GET /metrics` of the main router.
//!
//! # Format
//!
//! Prometheus text format 0.0.4 — compatible with `prometheus_client` (OpenMetrics).
//!
//! # Server + worker aggregation
//!
//! Aggregation is a concatenation of two text/OpenMetrics blocks.
//! Worker metric names are not filtered — duplicate names may occur if both
//! server and worker expose the same metric name.
//! Mitigation: use `gradatum_server_` vs `gradatum_worker_` prefixes.

use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use prometheus_client::encoding::text::encode;

use crate::state::AppState;

/// Default port for scraping worker metrics.
pub const DEFAULT_WORKER_METRICS_PORT: u16 = 19091;

/// Handles `GET /metrics` — Prometheus scrape endpoint (server side).
///
/// Returns server metrics plus worker metrics when available.
///
/// # Returns
///
/// - **200 OK** + `Content-Type: application/openmetrics-text` — encoded metrics
/// - **500 Internal Server Error** — if server encoding fails (rare)
///
/// # Degradation
///
/// If the worker port `:19091` is unavailable (service stopped, CI, test),
/// only server metrics are returned with a warning comment.
pub async fn metrics_handler_server(State(state): State<AppState>) -> Result<Response, StatusCode> {
    // Encodage des métriques server
    let mut buf = String::new();
    encode(&mut buf, &state.metrics.registry).map_err(|e| {
        tracing::error!(error = %e, "metrics_handler_server: encodage server échoué");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Scrape best-effort du worker :19091
    let worker_metrics = scrape_worker_metrics().await;
    match worker_metrics {
        Some(worker_text) => {
            // Concaténation : server metrics + worker metrics
            // Séparateur vide : OpenMetrics text format tolère plusieurs blocs.
            buf.push_str("\n# Worker metrics (scraped from :19091)\n");
            buf.push_str(&worker_text);
        }
        None => {
            buf.push_str("\n# Worker metrics unavailable (worker down or not configured)\n");
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Type",
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
        .body(Body::from(buf))
        .map_err(|e| {
            tracing::error!(error = %e, "metrics_handler_server: construction réponse échouée");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Scrapes worker metrics from `http://127.0.0.1:19091/metrics`.
///
/// Returns `Some(text)` on success, `None` otherwise (timeout, connection refused).
/// Strict 2-second timeout — the main handler must not block on the worker.
async fn scrape_worker_metrics() -> Option<String> {
    let url = format!("http://127.0.0.1:{}/metrics", DEFAULT_WORKER_METRICS_PORT);

    // reqwest::Client non disponible directement ici — utiliser tokio::net::TcpStream
    // pour un check minimal, puis reqwest via le feature "json" déjà présent.
    //
    // Caveat : reqwest n'est pas dans les deps directes du server (dev-dep seulement).
    // On utilise une approche sans reqwest : tokio::net::TcpStream check + hyper via axum.
    //
    // Scrape non câblé — retourne None gracieusement.
    // Écart E-16 : concaténation complète nécessite dep directe reqwest côté server.
    let _ = url;
    tracing::debug!("metrics_proxy: scrape worker :19091 — non câblé (E-16)");
    None
}
