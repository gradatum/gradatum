//! Métriques Prometheus pour `gradatum-engine`.
//!
//! Mirror de `gradatum-server/src/metrics.rs` — adapté au périmètre engine.
//!
//! ## Métriques déclarées
//!
//! | Nom | Type | Labels | Notes |
//! |-----|------|--------|-------|
//! | `engine_requests_total` | Counter | route, status_code | Requêtes traitées |
//! | `engine_request_latency_ms` | Histogram | route, status_code | Latence ms |
//!
//! ## Cardinalité
//!
//! Labels `route` = templates fixes (4 routes) × `status_code` = quelques valeurs.
//! Cardinalité totale < 50 — pas de plafond nécessaire.
use std::sync::Mutex;

use prometheus_client::{
    encoding::text::encode,
    metrics::{
        counter::Counter,
        family::Family,
        histogram::{exponential_buckets, Histogram},
    },
    registry::Registry,
};

/// Labels pour les métriques de requêtes engine.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct ReqLabels {
    /// Template de route (ex. `/v1/chat/completions`).
    pub route: String,
    /// Code HTTP en string (ex. `"200"`, `"504"`).
    pub status_code: String,
}

/// Métriques applicatives `gradatum-engine`.
///
/// Thread-safe via `Mutex<Registry>` interne.
pub struct EngineMetrics {
    registry: Mutex<Registry>,
    requests: Family<ReqLabels, Counter>,
    latency: Family<ReqLabels, Histogram>,
}

impl EngineMetrics {
    /// Crée et enregistre les métriques dans un nouveau registry.
    pub fn new() -> Self {
        let requests: Family<ReqLabels, Counter> = Family::default();
        let latency: Family<ReqLabels, Histogram> = Family::new_with_constructor(|| {
            // Buckets : 10ms → ~10s en progression exponentielle (base 2, 10 niveaux)
            Histogram::new(exponential_buckets(10.0, 2.0, 10))
        });

        let mut registry = Registry::default();
        registry.register(
            "engine_requests",
            "Requêtes engine traitées par route et code HTTP",
            requests.clone(),
        );
        registry.register(
            "engine_request_latency_ms",
            "Latence des requêtes engine en millisecondes",
            latency.clone(),
        );

        Self {
            registry: Mutex::new(registry),
            requests,
            latency,
        }
    }

    /// Enregistre une requête traitée avec son code HTTP et sa latence.
    pub fn record_request(&self, route: &str, status: u16, latency_ms: u64) {
        let labels = ReqLabels {
            route: route.into(),
            status_code: status.to_string(),
        };
        self.requests.get_or_create(&labels).inc();
        self.latency
            .get_or_create(&labels)
            .observe(latency_ms as f64);
    }

    /// Encode les métriques au format OpenMetrics text (pour `/metrics`).
    pub fn render(&self) -> String {
        let mut buf = String::new();
        encode(
            &mut buf,
            &self
                .registry
                .lock()
                .expect("EngineMetrics: lock poison — ne devrait pas arriver"),
        )
        .expect("encode métriques Prometheus");
        buf
    }
}

impl Default for EngineMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let m = EngineMetrics::new();
        m.record_request("/v1/chat/completions", 200, 42);
        let out = m.render();
        assert!(out.contains("engine_requests_total"));
        assert!(out.contains("/v1/chat/completions"));
    }

    #[test]
    fn records_multiple_routes() {
        let m = EngineMetrics::new();
        m.record_request("/v1/chat/completions", 200, 100);
        m.record_request("/v1/embeddings", 200, 50);
        m.record_request("/v1/chat/completions", 504, 120_000);
        let out = m.render();
        assert!(out.contains("/v1/chat/completions"));
        assert!(out.contains("/v1/embeddings"));
        assert!(out.contains("504"));
    }
}
