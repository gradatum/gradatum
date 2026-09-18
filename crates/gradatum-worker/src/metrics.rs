//! Prometheus metrics for the Gradatum worker.
//!
//! Served on the port configured by `[apalis.metrics]` (default `19091`; the deployed
//! configuration uses `19093`, since `19091` is held by `gradatum-server`).
//!
//! | Metric | Type | Labels | Description |
//! |---|---|---|---|
//! | `gradatum_jobs_total` | Counter | `kind`, `status` | Jobs processed by kind and status |
//! | `gradatum_jobs_duration_seconds` | Histogram | `kind` | Execution duration by kind |
//! | `gradatum_jobs_dlq_total` | Counter | `kind` | Jobs sent to DLQ by kind |
//! | `gradatum_workers_active` | Gauge | `kind` | Active workers by kind |
//! | `gradatum_distill_cron_enqueued_total` | Counter | — | Distill jobs enqueued by the F-112 cron |
//! | `gradatum_queue_payloads_purged_total` | Counter | — | Terminal job payloads erased by the periodic queue purge |
//! | `gradatum_config_degraded` | Gauge | `section`, `cause` | Config section on defaults (`0`/`1`) |
//!
//! The table is the reference; no count is stated in prose, so adding a metric cannot
//! turn this documentation into a false statement.
//!
//! # HTTP endpoint
//!
//! `GET /metrics` → Prometheus text format (Content-Type: text/plain).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use prometheus::{
    CounterVec, Gauge, GaugeVec, HistogramOpts, HistogramVec, IntCounter, Opts, Registry,
};
use tracing::warn;

/// Every job kind the Monitor consumes, in a fixed, code-defined set.
///
/// Used to bound the label cardinality of the per-kind failure read
/// ([`WorkerMetrics::failure_counts_by_kind`]): the set is closed and never widened by
/// operator input. Kept in sync with the workers registered in
/// [`build_monitor`](crate::monitor::build_monitor).
pub const JOB_KINDS: [&str; 7] = [
    "curate", "embed", "reindex", "purge", "forget", "distill", "validate",
];

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
    /// Distill jobs enqueued by the conditional distill cron (total).
    pub distill_cron_enqueued: IntCounter,
    /// Configuration fallback state per section `{section, cause}` — `0` healthy, `1`
    /// running on defaults.
    ///
    /// Populated once at boot by
    /// [`ConfigHealth::publish`](crate::config_health::ConfigHealth::publish). Every
    /// consulted section yields exactly one series, so that a healthy configuration is
    /// distinguishable from a worker that never started.
    pub config_degraded: GaugeVec,
    /// Unix-epoch timestamp (seconds) of the **last job actually consumed** — the
    /// consumption signal.
    ///
    /// Distinct from the leader-election lease: the lease's `expires_at` only proves the
    /// *renew* task is alive, so a frozen consumer with a live renew loop reads as
    /// "reachable" though it processes nothing. This gauge advances **only** when a job
    /// reaches a terminal outcome (via [`WorkerMetrics::inc_jobs_total`]), so a stalled
    /// consumer is visible even while its lease keeps renewing.
    pub last_consumed_ts: Gauge,
    /// Terminal job payloads erased by the periodic queue purge (total).
    ///
    /// Counts **rewrites**, as `purge_terminal_payloads` reports them: a row already
    /// carrying the purge tombstone is not counted, so the counter is monotone and the
    /// purge's idempotence is visible in it. A value of `0` is not by itself evidence that
    /// the purge runs — this counter is registered at boot and never increments before the
    /// first tick — which is why every tick also logs its count at `info`.
    pub queue_payloads_purged: IntCounter,
    /// Unix-epoch milliseconds of the last consumed job, or `0` if none has been consumed
    /// since boot. Read by the `/health` readiness probe with no Prometheus round-trip.
    last_consumed_ms: Arc<AtomicI64>,
}

impl WorkerMetrics {
    /// Creates and registers every metric of the module table in a new registry.
    ///
    /// # Panics
    ///
    /// Panics if a metric **definition** is invalid (one `expect` per metric
    /// construction). Those are static, compile-time-constant descriptors, so a panic
    /// here means a bug in this function, never a runtime condition.
    ///
    /// **Registration**, by contrast, never panics: a `registry.register` error is logged
    /// as a warning and that metric silently stays unregistered.
    #[must_use]
    pub fn new() -> Self {
        let registry = Arc::new(Registry::new());

        let jobs_total = CounterVec::new(
            Opts::new(
                "gradatum_jobs_total",
                "Total number of jobs processed by kind and status",
            ),
            &["kind", "status"],
        )
        .expect("gradatum_jobs_total metric invalid — static bug");

        let jobs_duration = HistogramVec::new(
            HistogramOpts::new(
                "gradatum_jobs_duration_seconds",
                "Job execution duration in seconds by kind",
            )
            .buckets(vec![
                0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 300.0, 600.0, 1800.0,
            ]),
            &["kind"],
        )
        .expect("gradatum_jobs_duration_seconds metric invalid — static bug");

        let jobs_dlq_total = CounterVec::new(
            Opts::new(
                "gradatum_jobs_dlq_total",
                "Total number of jobs sent to DLQ by kind",
            ),
            &["kind"],
        )
        .expect("gradatum_jobs_dlq_total metric invalid — static bug");

        let workers_active = GaugeVec::new(
            Opts::new(
                "gradatum_workers_active",
                "Number of active workers (occupied slots) by kind",
            ),
            &["kind"],
        )
        .expect("gradatum_workers_active metric invalid — static bug");

        let distill_cron_enqueued = IntCounter::new(
            "gradatum_distill_cron_enqueued_total",
            "Total Distill jobs enqueued by the F-112 conditional distill cron",
        )
        .expect("gradatum_distill_cron_enqueued_total metric invalid — static bug");

        let queue_payloads_purged = IntCounter::new(
            "gradatum_queue_payloads_purged_total",
            "Total terminal job payloads erased by the periodic queue purge",
        )
        .expect("gradatum_queue_payloads_purged_total metric invalid — static bug");

        let config_degraded = GaugeVec::new(
            Opts::new(
                "gradatum_config_degraded",
                "Configuration section running on default values (0 healthy, 1 fallback) \
                 by section and cause",
            ),
            &["section", "cause"],
        )
        .expect("gradatum_config_degraded metric invalid — static bug");

        let last_consumed_ts = Gauge::new(
            "gradatum_worker_last_job_consumed_timestamp_seconds",
            "Unix timestamp (seconds) of the last job consumed by the Monitor — real \
             consumption signal, distinct from the leader lease renewal",
        )
        .expect("gradatum_worker_last_job_consumed_timestamp_seconds metric invalid — static bug");

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
            (
                "distill_cron_enqueued",
                registry.register(Box::new(distill_cron_enqueued.clone())),
            ),
            (
                "queue_payloads_purged",
                registry.register(Box::new(queue_payloads_purged.clone())),
            ),
            (
                "config_degraded",
                registry.register(Box::new(config_degraded.clone())),
            ),
            (
                "last_consumed_ts",
                registry.register(Box::new(last_consumed_ts.clone())),
            ),
        ] {
            if let Err(e) = result {
                warn!(metric = name, error = %e, "Prometheus metric registration failed");
            }
        }

        Self {
            registry,
            jobs_total,
            jobs_duration,
            jobs_dlq_total,
            workers_active,
            distill_cron_enqueued,
            queue_payloads_purged,
            config_degraded,
            last_consumed_ts,
            last_consumed_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Increments the `gradatum_jobs_total{kind, status}` counter **and** advances the
    /// consumption signal.
    ///
    /// Every terminal job outcome (`done` or `error`) flows through here, so this is the
    /// single choke point where "a job was actually consumed" is observable — it updates
    /// [`last_consumed_ms`](Self::last_consumed_ms), [`consumed_total`](Self::consumed_total)
    /// and the `gradatum_worker_last_job_consumed_timestamp_seconds` gauge. No extra call
    /// site is needed in the Monitor `on_event` hooks (they already call this).
    pub fn inc_jobs_total(&self, kind: &str, status: &str) {
        self.jobs_total.with_label_values(&[kind, status]).inc();
        self.record_consumption();
    }

    /// Records that a job was just consumed: stamps the current time into the atomic
    /// millis clock (read by the readiness probe) and the Prometheus gauge (scraped).
    fn record_consumption(&self) {
        let (ms, secs) = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => (
                i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
                d.as_secs_f64(),
            ),
            // Pre-epoch clock: impossible on a sane host. Fall back to "unknown" (0)
            // rather than panic — the readiness probe treats 0 as "never consumed".
            Err(_) => (0, 0.0),
        };
        self.last_consumed_ms.store(ms, Ordering::Relaxed);
        self.last_consumed_ts.set(secs);
    }

    /// Unix-epoch milliseconds of the last consumed job, or `0` if none since boot.
    #[must_use]
    pub fn last_consumed_ms(&self) -> i64 {
        self.last_consumed_ms.load(Ordering::Relaxed)
    }

    /// Reads, per job kind, `(failed, total)` from the in-process
    /// `gradatum_jobs_total{kind,status}` counter — the source of the per-kind failure
    /// rate exposed by `/health/detailed`.
    ///
    /// `failed` = observations at `status="error"`, `total` = `error + done`. Only kinds
    /// with `total > 0` are returned (a kind that never ran carries no signal). The read is
    /// **non-creating** (`get_metric_with_label_values`): querying an unobserved
    /// `{kind,status}` pair does not materialise a spurious zero series in `/metrics`.
    #[must_use]
    pub fn failure_counts_by_kind(&self) -> BTreeMap<String, (u64, u64)> {
        let mut out = BTreeMap::new();
        for kind in JOB_KINDS {
            // A never-observed label pair yields Err → counted as 0 (no series created).
            let read = |status: &str| -> u64 {
                self.jobs_total
                    .get_metric_with_label_values(&[kind, status])
                    .map(|c| c.get() as u64)
                    .unwrap_or(0)
            };
            let failed = read("error");
            let total = failed + read("done");
            if total > 0 {
                out.insert(kind.to_string(), (failed, total));
            }
        }
        out
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

    /// Increments `gradatum_distill_cron_enqueued_total` (one per Distill job the
    /// distill cron successfully enqueues).
    pub fn inc_distill_cron_enqueued(&self) {
        self.distill_cron_enqueued.inc();
    }

    /// Sets `gradatum_config_degraded{section, cause}`.
    ///
    /// `value` is `0.0` for a section loaded as written and `1.0` for one running on
    /// default values. Both `section` and `cause` come from a finite, code-defined set,
    /// so the label cardinality stays bounded by the number of configuration sections —
    /// never by operator input.
    pub fn set_config_degraded(&self, section: &str, cause: &str, value: f64) {
        self.config_degraded
            .with_label_values(&[section, cause])
            .set(value);
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
                warn!(error = %e, "Prometheus metrics rendering failed");
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
    use axum::{Router, routing::get};
    use std::net::SocketAddr;

    if !config.enabled {
        return Ok(());
    }

    let addr: SocketAddr = format!("{}:{}", config.bind, config.port)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid metrics address: {e}"))?;

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("metrics bind :{}  failed: {e}", config.port))?;

    tracing::info!(
        bind = %config.bind,
        port = config.port,
        "Prometheus metrics server started"
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "Prometheus metrics server error");
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

    #[test]
    fn consumption_signal_starts_at_zero_and_advances_on_consumption() {
        // The consumption signal is the P2 Caveat-1 fix: it must differ from a mere lease
        // renewal — it only advances when a job is actually consumed.
        let m = WorkerMetrics::new();
        assert_eq!(m.last_consumed_ms(), 0, "nothing consumed yet");

        m.inc_jobs_total("embed", "done");
        assert!(
            m.last_consumed_ms() > 0,
            "consuming a job must advance the consumption clock"
        );
        // The Prometheus gauge carries the same signal for scraping.
        assert!(
            m.render()
                .contains("gradatum_worker_last_job_consumed_timestamp_seconds"),
            "the consumption timestamp gauge must be exposed on /metrics"
        );
    }

    #[test]
    fn failure_counts_by_kind_reports_failed_over_total() {
        let m = WorkerMetrics::new();
        // 3 embed done + 2 embed error → (failed=2, total=5).
        for _ in 0..3 {
            m.inc_jobs_total("embed", "done");
        }
        for _ in 0..2 {
            m.inc_jobs_total("embed", "error");
        }
        // 1 curate done only → (failed=0, total=1).
        m.inc_jobs_total("curate", "done");

        let by_kind = m.failure_counts_by_kind();
        assert_eq!(by_kind.get("embed"), Some(&(2, 5)));
        assert_eq!(by_kind.get("curate"), Some(&(0, 1)));
        // Kinds that never ran carry no signal (and no spurious /metrics series).
        assert!(
            !by_kind.contains_key("purge"),
            "an unran kind must not appear"
        );
    }
}
