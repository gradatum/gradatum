//! Prometheus metrics for `gradatum-engine`.
//!
//! Mirrors `gradatum-server/src/metrics.rs`, adapted to the engine scope.
//!
//! ## Declared metrics
//!
//! | Name | Type | Labels | Notes |
//! |------|------|--------|-------|
//! | `engine_requests_total` | Counter | route, status_code | Processed requests |
//! | `engine_request_latency_ms` | Histogram | route, status_code | Latency in ms |
//!
//! ## Cardinality
//!
//! `route` labels = fixed templates (4 routes) × `status_code` = a few values.
//! Total cardinality < 50 — no cap needed.
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

/// Labels for engine request metrics.
#[derive(Clone, Hash, Eq, PartialEq, Debug, prometheus_client::encoding::EncodeLabelSet)]
pub struct ReqLabels {
    /// Route template (e.g. `/v1/chat/completions`).
    pub route: String,
    /// HTTP status code as a string (e.g. `"200"`, `"504"`).
    pub status_code: String,
}

/// Application metrics for `gradatum-engine`.
///
/// Thread-safe via an internal `Mutex<Registry>`.
pub struct EngineMetrics {
    registry: Mutex<Registry>,
    requests: Family<ReqLabels, Counter>,
    latency: Family<ReqLabels, Histogram>,
}

impl EngineMetrics {
    /// Creates and registers metrics in a new registry.
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

    /// Records a processed request with its HTTP status code and latency.
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

    /// Encodes metrics in OpenMetrics text format (for `/metrics`).
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
