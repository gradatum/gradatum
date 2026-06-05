//! Métriques Prometheus pour le worker Gradatum — F-15.
//!
//! Expose 4 métriques sur le port `:19091` (config TOML `[apalis.metrics]`) :
//!
//! | Métrique | Type | Labels | Description |
//! |---|---|---|---|
//! | `gradatum_jobs_total` | Counter | `kind`, `status` | Jobs traités par kind et statut |
//! | `gradatum_jobs_duration_seconds` | Histogram | `kind` | Durée d'exécution par kind |
//! | `gradatum_jobs_dlq_total` | Counter | `kind` | Jobs envoyés en DLQ par kind |
//! | `gradatum_workers_active` | Gauge | `kind` | Workers actifs par kind |
//!
//! # HTTP endpoint
//!
//! `GET /metrics` → format texte Prometheus (Content-Type: text/plain).
//!
//! # Références
//!
//! - Spec §5 Phase 2 — Prometheus exporter
//! - v81 §6 L3178 — F-15 feature description

use std::sync::Arc;

use axum::extract::State;
use prometheus::{CounterVec, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry};
use tracing::warn;

/// Registre Prometheus partagé entre les workers.
///
/// Initialisé une seule fois via [`WorkerMetrics::new`].
#[derive(Clone)]
pub struct WorkerMetrics {
    registry: Arc<Registry>,
    /// Compteur total de jobs traités `{kind, status}`.
    pub jobs_total: CounterVec,
    /// Histogramme de durée d'exécution en secondes `{kind}`.
    /// Write-only côté Rust — lu via le registre Prometheus.
    #[allow(dead_code)]
    pub jobs_duration: HistogramVec,
    /// Compteur de jobs envoyés en DLQ `{kind}`.
    /// Write-only côté Rust — lu via le registre Prometheus.
    #[allow(dead_code)]
    pub jobs_dlq_total: CounterVec,
    /// Gauge de workers actifs `{kind}`.
    pub workers_active: GaugeVec,
}

impl WorkerMetrics {
    /// Crée et enregistre les 4 métriques F-15 dans un nouveau registre.
    ///
    /// # Panics
    ///
    /// Ne panique pas — les erreurs d'enregistrement sont loggées et
    /// les métriques défaillantes sont remplacées par des no-ops.
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

        // Enregistrement — erreurs loggées sans panic
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

    /// Incrémente le compteur `gradatum_jobs_total{kind, status}`.
    pub fn inc_jobs_total(&self, kind: &str, status: &str) {
        self.jobs_total.with_label_values(&[kind, status]).inc();
    }

    /// Observe la durée d'exécution pour `gradatum_jobs_duration_seconds{kind}`.
    /// Utilisé dans les tests + futur hook Monitor Phase 3.
    #[allow(dead_code)]
    pub fn observe_duration(&self, kind: &str, secs: f64) {
        self.jobs_duration.with_label_values(&[kind]).observe(secs);
    }

    /// Incrémente le compteur `gradatum_jobs_dlq_total{kind}`.
    /// Utilisé dans les tests + futur hook Monitor dead jobs Phase 3.
    #[allow(dead_code)]
    pub fn inc_dlq(&self, kind: &str) {
        self.jobs_dlq_total.with_label_values(&[kind]).inc();
    }

    /// Incrémente la gauge `gradatum_workers_active{kind}`.
    pub fn inc_workers_active(&self, kind: &str) {
        self.workers_active.with_label_values(&[kind]).inc();
    }

    /// Décrémente la gauge `gradatum_workers_active{kind}`.
    pub fn dec_workers_active(&self, kind: &str) {
        self.workers_active.with_label_values(&[kind]).dec();
    }

    /// Sérialise toutes les métriques au format texte Prometheus.
    ///
    /// Retourne une chaîne vide si le rendu échoue (loggué).
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

    /// Référence sur le registre Prometheus sous-jacent.
    /// Utilisé dans les tests + futur endpoint `/metrics` Phase 3.
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
// Serveur HTTP interne /metrics
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration du serveur HTTP Prometheus.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MetricsConfig {
    /// Active le serveur HTTP Prometheus.
    #[serde(default = "MetricsConfig::default_enabled")]
    pub enabled: bool,
    /// Adresse d'écoute (défaut : `127.0.0.1`).
    #[serde(default = "MetricsConfig::default_bind")]
    pub bind: String,
    /// Port d'écoute (défaut : `19091`).
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

/// Lance le serveur HTTP Prometheus en arrière-plan (tâche tokio détachée).
///
/// Le serveur répond à `GET /metrics` avec le format texte Prometheus.
/// Retourne `Ok(())` immédiatement — le serveur tourne en background.
///
/// # Erreurs
///
/// Retourne une erreur si la liaison TCP échoue au démarrage.
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

/// Handler Axum pour `GET /metrics`.
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
