//! Prometheus metrics for the Gradatum worker.
//!
//! Exposes 4 metrics on port `:19091` (TOML config `[apalis.metrics]`):
//!
//! | Metric | Type | Labels | Description |
//! |---|---|---|---|
//! | `gradatum_jobs_total` | Counter | `kind`, `status` | Jobs processed by kind and status |
//! | `gradatum_jobs_duration_seconds` | Histogram | `kind` | Execution duration by kind |
//! | `gradatum_jobs_dlq_total` | Counter | `kind` | Jobs sent to DLQ by kind |
//! | `gradatum_workers_active` | Gauge | `kind` | Active workers by kind |
//!
//! # HTTP endpoint
//!
//! `GET /metrics` → Prometheus text format (Content-Type: text/plain).

use std::sync::Arc;

use axum::extract::State;
use prometheus::{CounterVec, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry};
use tracing::warn;

/// Prometheus registry shared across workers.
///
/// Initialised once via [`WorkerMetrics::new`].
#[derive(Clone)]
pub struct WorkerMetrics {
    registry: Arc<Registry>,
    /// Total jobs processed counter `{kind, status}`.
    pub jobs_total: CounterVec,
    /// Execution duration histogram in seconds `{kind}`.
    /// Write-only on the Rust side — read via the Prometheus registry.
    #[allow(dead_code)]
    pub jobs_duration: HistogramVec,
    /// Jobs sent to DLQ counter `{kind}`.
    /// Write-only on the Rust side — read via the Prometheus registry.
    #[allow(dead_code)]
    pub jobs_dlq_total: CounterVec,
    /// Active workers gauge `{kind}`.
    pub workers_active: GaugeVec,
}

impl WorkerMetrics {
    /// Creates and registers the 4 metrics in a new registry.
    ///
    /// # Panics
    ///
    /// Does not panic — registration errors are logged and
    /// failing metrics fall back to no-ops.
    #[must_use]
    pub fn new() -> Self {
        let registry = Arc::new(Registry::new());

        let jobs_total = CounterVec::new(
            Opts::new(
                "gradatum_jobs_total",
                "Nombre total de jobs traités par kind et statut",
            ),
            &["kind", "status"],
        )
        .expect("métrique gradatum_jobs_total invalide — bug statique");

        let jobs_duration = HistogramVec::new(
            HistogramOpts::new(
                "gradatum_jobs_duration_seconds",
                "Durée d'exécution des jobs en secondes par kind",
            )
            .buckets(vec![
                0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 600.0, 1800.0,
            ]),
            &["kind"],
        )
        .expect("métrique gradatum_jobs_duration_seconds invalide — bug statique");

        let jobs_dlq_total = CounterVec::new(
            Opts::new(
                "gradatum_jobs_dlq_total",
                "Nombre total de jobs envoyés en DLQ par kind",
            ),
            &["kind"],
        )
        .expect("métrique gradatum_jobs_dlq_total invalide — bug statique");

        let workers_active = GaugeVec::new(
            Opts::new(
                "gradatum_workers_active",
                "Nombre de workers actifs (slots occupés) par kind",
            ),
            &["kind"],
        )
        .expect("métrique gradatum_workers_active invalide — bug statique");

        // Registration — errors logged without panicking
        for (name, result) in [
            (
                "jobs_total",
                registry.register(Box::new(jobs_total.clone())),
            ),
            (
                "jobs_duration",
                registry.register(Box::new(jobs_duration.clone())),
            ),
            (
                "jobs_dlq_total",
                registry.register(Box::new(jobs_dlq_total.clone())),
            ),
            (
                "workers_active",
                registry.register(Box::new(workers_active.clone())),
            ),
        ] {
            if let Err(e) = result {
                warn!(metric = name, error = %e, "enregistrement métrique Prometheus échoué");
            }
        }

        Self {
            registry,
            jobs_total,
            jobs_duration,
            jobs_dlq_total,
            workers_active,
        }
    }

    /// Increments the `gradatum_jobs_total{kind, status}` counter.
    pub fn inc_jobs_total(&self, kind: &str, status: &str) {
        self.jobs_total.with_label_values(&[kind, status]).inc();
    }

    /// Records an execution duration sample for `gradatum_jobs_duration_seconds{kind}`.
    /// Used in tests and the monitor hook (not wired by default).
    #[allow(dead_code)]
    pub fn observe_duration(&self, kind: &str, secs: f64) {
        self.jobs_duration.with_label_values(&[kind]).observe(secs);
    }

    /// Increments the `gradatum_jobs_dlq_total{kind}` counter.
    /// Used in tests and the dead-job monitor hook (not wired by default).
    #[allow(dead_code)]
    pub fn inc_dlq(&self, kind: &str) {
        self.jobs_dlq_total.with_label_values(&[kind]).inc();
    }

    /// Increments the `gradatum_workers_active{kind}` gauge.
    pub fn inc_workers_active(&self, kind: &str) {
        self.workers_active.with_label_values(&[kind]).inc();
    }

    /// Decrements the `gradatum_workers_active{kind}` gauge.
    pub fn dec_workers_active(&self, kind: &str) {
        self.workers_active.with_label_values(&[kind]).dec();
    }

    /// Serialises all metrics to Prometheus text format.
    ///
    /// Returns an empty string if rendering fails (logged).
    #[must_use]
    pub fn render(&self) -> String {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let mut buf = Vec::new();
        match encoder.encode(&self.registry.gather(), &mut buf) {
            Ok(()) => String::from_utf8(buf).unwrap_or_default(),
            Err(e) => {
                warn!(error = %e, "rendu métriques Prometheus échoué");
                String::new()
            }
        }
    }

    /// Returns a reference to the underlying Prometheus registry.
    /// Used in tests and the `/metrics` endpoint.
    #[must_use]
    #[allow(dead_code)]
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }
}

impl Default for WorkerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal HTTP server /metrics
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the Prometheus HTTP server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricsConfig {
    /// Enables the Prometheus HTTP server.
    #[serde(default = "MetricsConfig::default_enabled")]
    pub enabled: bool,
    /// Bind address (default: `127.0.0.1`).
    #[serde(default = "MetricsConfig::default_bind")]
    pub bind: String,
    /// Listen port (default: `19091`).
    #[serde(default = "MetricsConfig::default_port")]
    pub port: u16,
}

impl MetricsConfig {
    fn default_enabled() -> bool {
        false
    }
    fn default_bind() -> String {
        "127.0.0.1".to_string()
    }
    fn default_port() -> u16 {
        19091
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: Self::default_bind(),
            port: Self::default_port(),
        }
    }
}

/// Spawns the Prometheus HTTP server as a detached Tokio task.
///
/// The server responds to `GET /metrics` with Prometheus text format.
/// Returns `Ok(())` immediately — the server runs in the background.
///
/// # Errors
///
/// Returns an error if the TCP bind fails at startup.
pub async fn spawn_metrics_server(
    config: &MetricsConfig,
    metrics: WorkerMetrics,
) -> anyhow::Result<()> {
    use axum::{routing::get, Router};
    use std::net::SocketAddr;

    if !config.enabled {
        return Ok(());
    }

    let addr: SocketAddr = format!("{}:{}", config.bind, config.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("adresse métriques invalide: {e}"))?;

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("bind métriques :{}  échec: {e}", config.port))?;

    tracing::info!(
        bind = %config.bind,
        port = config.port,
        "serveur métriques Prometheus démarré"
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "erreur serveur métriques Prometheus");
        }
    });

    Ok(())
}

/// Axum handler for `GET /metrics`.
async fn metrics_handler(State(metrics): axum::extract::State<WorkerMetrics>) -> String {
    metrics.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_new_initialise_registry() {
        let m = WorkerMetrics::new();
        // Vérification basique : les métriques sont enregistrées
        let _initial = m.render();
        // Doit contenir les noms de métriques (seulement si au moins 1 valeur)
        // Incrémente pour forcer l'émission
        m.inc_jobs_total("curate", "Done");
        m.observe_duration("embed", 1.5);
        m.inc_dlq("reindex");
        m.inc_workers_active("curate");

        let rendered = m.render();
        assert!(
            rendered.contains("gradatum_jobs_total"),
            "gradatum_jobs_total absent du rendu"
        );
        assert!(
            rendered.contains("gradatum_jobs_duration_seconds"),
            "gradatum_jobs_duration_seconds absent du rendu"
        );
        assert!(
            rendered.contains("gradatum_jobs_dlq_total"),
            "gradatum_jobs_dlq_total absent du rendu"
        );
        assert!(
            rendered.contains("gradatum_workers_active"),
            "gradatum_workers_active absent du rendu"
        );
    }

    #[test]
    fn metrics_inc_dec_workers_active() {
        let m = WorkerMetrics::new();
        m.inc_workers_active("embed");
        m.inc_workers_active("embed");
        m.dec_workers_active("embed");
        // Pas de panic = succès
    }

    #[test]
    fn metrics_config_defaults() {
        let cfg = MetricsConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, 19091);
    }
}
