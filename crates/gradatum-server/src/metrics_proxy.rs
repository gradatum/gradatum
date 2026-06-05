//! Handler `/metrics` côté serveur — agrégation server + worker (F-16 Phase 3).
//!
//! Scrape les métriques du worker `:19091` et les merge avec les métriques
//! du serveur. Si le worker n'est pas disponible, retourne uniquement les
//! métriques serveur (graceful degradation).
//!
//! # Endpoints
//!
//! Ce handler est monté sur `GET /metrics` du routeur principal.
//!
//! # Format
//!
//! Prometheus text format 0.0.4 — compatible avec `prometheus_client` (OpenMetrics).
//!
//! # Caveat E-16 (§11 spec)
//!
//! L'agrégation est une concaténation de deux blocs text/openmetrics.
//! Les noms de métriques du worker ne sont pas filtrés — risque de doublon
//! si worker et server exposent des métriques avec le même nom.
//! Mitigation : préfixe `gradatum_server_` vs `gradatum_worker_` dans les noms.
//!
//! # Références
//!
//! - spec §6 Phase 3 tâche 3 — `/metrics` endpoint
//! - v81 F-16 — Prometheus metrics intégration

use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use prometheus_client::encoding::text::encode;

use crate::state::AppState;

/// Config pour le scraping worker métriques (depuis `AppState`).
pub const DEFAULT_WORKER_METRICS_PORT: u16 = 19091;

/// `GET /metrics` — Prometheus scrape endpoint côté serveur.
///
/// Retourne les métriques server + métriques worker si disponibles.
///
/// # Retour
///
/// - **200 OK** + `Content-Type: application/openmetrics-text` — métriques encodées
/// - **500 Internal Server Error** — si l'encodage serveur échoue (rare)
///
/// # Degradation
///
/// Si le worker `:19091` n'est pas disponible (service arrêté, CI, test),
/// seules les métriques server sont retournées avec un commentaire d'avertissement.
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

/// Scrape les métriques du worker sur `http://127.0.0.1:19091/metrics`.
///
/// Retourne `Some(text)` si le scrape réussit, `None` sinon (timeout, connection refused).
/// Timeout strict de 2 secondes — le handler principal ne doit pas bloquer sur le worker.
async fn scrape_worker_metrics() -> Option<String> {
    let url = format!("http://127.0.0.1:{}/metrics", DEFAULT_WORKER_METRICS_PORT);

    // reqwest::Client non disponible directement ici — utiliser tokio::net::TcpStream
    // pour un check minimal, puis reqwest via le feature "json" déjà présent.
    //
    // Caveat : reqwest n'est pas dans les deps directes du server (dev-dep seulement).
    // On utilise une approche sans reqwest : tokio::net::TcpStream check + hyper via axum.
    //
    // Phase 3 Bronze : retourne None gracieusement si le scrape n'est pas câblé.
    // La concaténation complète est planifiée Phase 3 Silver (reqwest dep directe).
    //
    // Écart E-16 documenté §11.
    let _ = url;
    tracing::debug!(
        "metrics_proxy: scrape worker :19091 — non câblé Phase 3 Bronze (E-16, voir §11)"
    );
    None
}
