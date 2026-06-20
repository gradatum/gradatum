//! Apalis multi-worker monitor orchestration.
//!
//! Builds an Apalis `Monitor` that orchestrates
//! the Gradatum workers (curate, embed, reindex) via the custom `GradatumBackend`
//! and Tower layers:
//!
//! | Kind | Concurrency | Timeout | Retries | Layer |
//! |---|---|---|---|---|
//! | `curate` | 2 | 30s | 3 | Trace + Timeout + Retry + CatchPanic |
//! | `embed` | 4 | 60s | 3 | Trace + Timeout + Retry + CatchPanic |
//! | `reindex` | 4 | 120s | 2 | Trace + Timeout + Retry + CatchPanic |
//!
//! # Graceful shutdown
//!
//! `SIGTERM` or `SIGINT` → [`Monitor::with_terminator`] with a 30s timeout.
//! Replaces `shutdown_timeout()`, which requires the `"sleep"` feature absent from rc.9.
//!
//! # Cron schedule
//!
//! Registers `cleanup_dlq_daily` via `CronStream` (apalis_cron) if
//! the config contains at least one `[[apalis.schedules]]` entry.
//!
//! # Prometheus metrics
//!
//! Metrics are incremented via `on_event` hooks on each worker.
//! Exposed on port `:19091` via [`spawn_metrics_server`](super::metrics::spawn_metrics_server).

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use apalis::layers::WorkerBuilderExt;
use apalis::layers::retry::RetryPolicy;
use apalis::prelude::AcknowledgementExt;
use apalis::prelude::{Event, EventListenerExt, Monitor, WorkerBuilder};
use apalis_cron::CronStream;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use tracing::info;

use gradatum_core::QueueStore;
use gradatum_curator::CuratorProcess;
use gradatum_embed::Embedder;
use sqlx::SqlitePool;

use crate::internal_client::InternalClient;

use super::apalis_backend::build_gradatum_backend;
use super::apalis_handlers::{
    DistillSynthesizer, handle_curate, handle_distill, handle_embed, handle_forget, handle_purge,
    handle_reindex,
};
use super::metrics::WorkerMetrics;
use super::schedules::{ScheduleConfig, handle_cleanup_dlq};

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Per-kind worker configuration read from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Number of concurrent slots for this kind.
    #[serde(default = "WorkerConfig::default_concurrency")]
    pub concurrency: usize,
    /// Per-job timeout in seconds.
    #[serde(default = "WorkerConfig::default_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum number of retries before DLQ.
    #[serde(default = "WorkerConfig::default_max_retries")]
    pub max_retries: usize,
}

impl WorkerConfig {
    fn default_concurrency() -> usize {
        2
    }
    fn default_timeout_secs() -> u64 {
        30
    }
    fn default_max_retries() -> usize {
        3
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: Self::default_concurrency(),
            timeout_secs: Self::default_timeout_secs(),
            max_retries: Self::default_max_retries(),
        }
    }
}

/// Complete Apalis configuration: workers and schedules.
///
/// Read from `[apalis]` in the server TOML via figment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApalisConfig {
    /// Per-kind worker configuration.
    #[serde(default)]
    pub workers: WorkersConfig,
    /// Periodic cron schedules.
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
}

/// Configuration for the active workers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersConfig {
    /// Curator worker — LLM-bound; low concurrency recommended.
    #[serde(default = "WorkersConfig::default_curate")]
    pub curate: WorkerConfig,
    /// Embed worker — I/O-bound; high concurrency acceptable.
    #[serde(default = "WorkersConfig::default_embed")]
    pub embed: WorkerConfig,
    /// Reindex worker — I/O-bound; long timeout.
    #[serde(default = "WorkersConfig::default_reindex")]
    pub reindex: WorkerConfig,
    /// Purge worker — Garbage lifecycle.
    ///
    /// Concurrency 1: destructive operation — only one worker at a time to avoid
    /// double-deletions. Long timeout for large corpora.
    #[serde(default = "WorkersConfig::default_purge")]
    pub purge: WorkerConfig,
    /// Forget worker — semantic forgetting.
    ///
    /// Concurrency 1: frontmatter mutation on potentially many notes.
    /// Non-destructive (no physical deletion).
    #[serde(default = "WorkersConfig::default_forget")]
    pub forget: WorkerConfig,
    /// Distill worker — semantic distillation.
    ///
    /// Concurrency 1: LLM/synthesis writes to the vault plus source marking
    /// (CoW). O(n²) clustering bounded by `batch_limit`. Long timeout (corpus + synthesis).
    #[serde(default = "WorkersConfig::default_distill")]
    pub distill: WorkerConfig,
}

impl WorkersConfig {
    fn default_curate() -> WorkerConfig {
        WorkerConfig {
            concurrency: 2,
            timeout_secs: 30,
            max_retries: 3,
        }
    }
    fn default_embed() -> WorkerConfig {
        WorkerConfig {
            concurrency: 4,
            timeout_secs: 60,
            max_retries: 3,
        }
    }
    fn default_reindex() -> WorkerConfig {
        WorkerConfig {
            concurrency: 4,
            timeout_secs: 120,
            max_retries: 2,
        }
    }
    fn default_purge() -> WorkerConfig {
        WorkerConfig {
            // Concurrency 1 — destructive operation, no parallelism.
            concurrency: 1,
            // Long timeout: a large vault may have many Garbage notes.
            timeout_secs: 300,
            // No automatic retry on purge — destructive operation.
            // If the batch fails mid-way, already-deleted notes remain deleted.
            // A retry would only reprocess the survivors (idempotent).
            max_retries: 0,
        }
    }

    fn default_forget() -> WorkerConfig {
        WorkerConfig {
            // Concurrency 1 — serial frontmatter mutation to avoid CoW conflicts.
            concurrency: 1,
            // Long timeout: a Topic/Agent scope may cover many notes.
            timeout_secs: 300,
            // No automatic retry: idempotent but conservative (double-forget = re-marking).
            max_retries: 0,
        }
    }

    fn default_distill() -> WorkerConfig {
        WorkerConfig {
            // Concurrency 1 — serial synthesis write + source marking (CoW-safe, deterministic).
            concurrency: 1,
            // Long timeout: O(n²) clustering over batch_limit notes + synthesis per cluster.
            timeout_secs: 300,
            // No automatic retry: a synthesis failure (gateway down) must not loop;
            // already-written clusters are idempotent (sources marked as processed).
            max_retries: 0,
        }
    }
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            curate: Self::default_curate(),
            embed: Self::default_embed(),
            reindex: Self::default_reindex(),
            purge: Self::default_purge(),
            forget: Self::default_forget(),
            distill: Self::default_distill(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main builder
// ─────────────────────────────────────────────────────────────────────────────

/// Handler dependencies injected via `.data()` into each Apalis worker.
///
/// Grouped into a struct to stay within the clippy argument-count limit (max 7).
pub struct MonitorDeps {
    /// HTTP client to the server internal API — all vault+index mutations.
    pub client: Arc<dyn InternalClient>,
    /// Curator pipeline — inbox classification (curate).
    pub curator: Arc<dyn CuratorProcess + Send + Sync>,
    /// Embedding backend — vector generation (embed + reindex + distill).
    pub embedder: Arc<dyn Embedder + Send + Sync>,
    /// Cluster synthesiser — distillation (distill worker).
    ///
    /// Pluggable: `TemplateSynthesizer` (deterministic, default) or an LLM
    /// gateway backend swapped in without changing the handler.
    pub distill_synthesizer: Arc<dyn DistillSynthesizer + Send + Sync>,
}

/// Builds the Apalis [`Monitor`] with the configured workers and schedules.
///
/// # Parameters
///
/// - `store`: `QueueStore` implementation shared across workers
/// - `pool`: SQLite pool for cron schedules (`cleanup_dlq_daily`)
/// - `deps`: handler dependencies — vault, curator, embedder, index
/// - `config`: worker and schedule configuration read from TOML
/// - `metrics`: shared Prometheus registry for `on_event` hooks
/// - `shutdown_timeout_secs`: graceful drain duration after SIGTERM/SIGINT
///
/// # Errors
///
/// Returns an error if `GradatumBackend` construction fails or if
/// a cron expression in the config is invalid.
pub fn build_monitor(
    store: Arc<dyn QueueStore + Send + Sync>,
    pool: Arc<SqlitePool>,
    deps: MonitorDeps,
    config: &ApalisConfig,
    metrics: WorkerMetrics,
    shutdown_timeout_secs: u64,
) -> anyhow::Result<Monitor> {
    let MonitorDeps {
        client,
        curator,
        embedder,
        distill_synthesizer,
    } = deps;
    let curate_cfg = config.workers.curate.clone();
    let embed_cfg = config.workers.embed.clone();
    let reindex_cfg = config.workers.reindex.clone();
    let purge_cfg = config.workers.purge.clone();
    let forget_cfg = config.workers.forget.clone();
    let distill_cfg = config.workers.distill.clone();

    // Isolated backends per kind — each worker fetches ONLY its own jobs.
    // DLQ routing fix: build_gradatum_backend(store, kind) calls dequeue_by_kind(kind)
    // instead of dequeue() — eliminates the race condition where embed/reindex workers
    // could fetch a Curate job and return HandlerError::UnexpectedVariant (~80% DLQ).
    // The `kind` column is now set by enqueue() and back-filled by migration 010.
    // build_gradatum_backend returns (backend, acknowledger).
    // The acknowledger is wired via .ack_with() → store.complete/fail.
    let (curate_backend, curate_ack) = build_gradatum_backend(Arc::clone(&store), "Curate")?;
    let (embed_backend, embed_ack) = build_gradatum_backend(Arc::clone(&store), "Embed")?;
    let (reindex_backend, reindex_ack) = build_gradatum_backend(Arc::clone(&store), "ReIndex")?;
    let (purge_backend, purge_ack) = build_gradatum_backend(Arc::clone(&store), "Purge")?;
    let (forget_backend, forget_ack) = build_gradatum_backend(Arc::clone(&store), "Forget")?;
    let (distill_backend, distill_ack) = build_gradatum_backend(Arc::clone(&store), "Distill")?;

    let m_curate = metrics.clone();
    let m_embed = metrics.clone();
    let m_reindex = metrics.clone();
    let m_purge = metrics.clone();
    let m_forget = metrics.clone();
    let m_distill = metrics.clone();

    let mut monitor = Monitor::new()
        // ── Curate worker ─────────────────────────────────────────────────────
        .register({
            let cfg = curate_cfg.clone();
            let backend = curate_backend;
            let ack = curate_ack;
            let m = m_curate;
            let client_c = Arc::clone(&client);
            let curator_c = Arc::clone(&curator);
            // Queue injected into handle_curate for curate→embed chaining.
            let queue_c = Arc::clone(&store);
            move |idx| {
                let worker_name = format!("curate-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    // Handler dependency injection — cloned per worker instance.
                    // Order matches the parameter order of handle_curate.
                    .data(Arc::clone(&client_c) as Arc<dyn InternalClient>)
                    .data(Arc::clone(&curator_c) as Arc<dyn CuratorProcess + Send + Sync>)
                    .data(Arc::clone(&queue_c) as Arc<dyn QueueStore + Send + Sync>)
                    // ack_with wires store.complete/fail after each handler.
                    // enable_tracing() active — task_id injected via record_to_task.
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| {
                            // Events: Start|Stop|Success|Error|Idle|HeartBeat|Custom
                            match ev {
                                Event::Start => m.inc_workers_active("curate"),
                                Event::Stop => m.dec_workers_active("curate"),
                                Event::Success => m.inc_jobs_total("curate", "done"),
                                Event::Error(_) => m.inc_jobs_total("curate", "error"),
                                _ => {}
                            }
                        }
                    })
                    .build(handle_curate)
            }
        })
        // ── Embed worker ──────────────────────────────────────────────────────
        .register({
            let cfg = embed_cfg.clone();
            let backend = embed_backend;
            let ack = embed_ack;
            let m = m_embed;
            let client_e = Arc::clone(&client);
            let embedder_e = Arc::clone(&embedder);
            move |idx| {
                let worker_name = format!("embed-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&client_e) as Arc<dyn InternalClient>)
                    .data(Arc::clone(&embedder_e) as Arc<dyn Embedder + Send + Sync>)
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| match ev {
                            Event::Start => m.inc_workers_active("embed"),
                            Event::Stop => m.dec_workers_active("embed"),
                            Event::Success => m.inc_jobs_total("embed", "done"),
                            Event::Error(_) => m.inc_jobs_total("embed", "error"),
                            _ => {}
                        }
                    })
                    .build(handle_embed)
            }
        })
        // ── Reindex worker ────────────────────────────────────────────────────
        .register({
            let cfg = reindex_cfg.clone();
            let backend = reindex_backend;
            let ack = reindex_ack;
            let m = m_reindex;
            let client_r = Arc::clone(&client);
            let embedder_r = Arc::clone(&embedder);
            move |idx| {
                let worker_name = format!("reindex-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&client_r) as Arc<dyn InternalClient>)
                    .data(Arc::clone(&embedder_r) as Arc<dyn Embedder + Send + Sync>)
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| match ev {
                            Event::Start => m.inc_workers_active("reindex"),
                            Event::Stop => m.dec_workers_active("reindex"),
                            Event::Success => m.inc_jobs_total("reindex", "done"),
                            Event::Error(_) => m.inc_jobs_total("reindex", "error"),
                            _ => {}
                        }
                    })
                    .build(handle_reindex)
            }
        })
        // ── Purge worker ──────────────────────────────────────────────────────
        .register({
            let cfg = purge_cfg.clone();
            let backend = purge_backend;
            let ack = purge_ack;
            let m = m_purge;
            let client_p = Arc::clone(&client);
            move |idx| {
                let worker_name = format!("purge-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&client_p) as Arc<dyn InternalClient>)
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| match ev {
                            Event::Start => m.inc_workers_active("purge"),
                            Event::Stop => m.dec_workers_active("purge"),
                            Event::Success => m.inc_jobs_total("purge", "done"),
                            Event::Error(_) => m.inc_jobs_total("purge", "error"),
                            _ => {}
                        }
                    })
                    .build(handle_purge)
            }
        })
        // ── Forget worker ─────────────────────────────────────────────────────
        .register({
            let cfg = forget_cfg.clone();
            let backend = forget_backend;
            let ack = forget_ack;
            let m = m_forget;
            let client_f = Arc::clone(&client);
            move |idx| {
                let worker_name = format!("forget-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&client_f) as Arc<dyn InternalClient>)
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| match ev {
                            Event::Start => m.inc_workers_active("forget"),
                            Event::Stop => m.dec_workers_active("forget"),
                            Event::Success => m.inc_jobs_total("forget", "done"),
                            Event::Error(_) => m.inc_jobs_total("forget", "error"),
                            _ => {}
                        }
                    })
                    .build(handle_forget)
            }
        })
        // ── Distill worker ────────────────────────────────────────────────────
        .register({
            let cfg = distill_cfg.clone();
            let backend = distill_backend;
            let ack = distill_ack;
            let m = m_distill;
            let client_d = Arc::clone(&client);
            let embedder_d = Arc::clone(&embedder);
            let synth_d = Arc::clone(&distill_synthesizer);
            move |idx| {
                let worker_name = format!("distill-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    // Order matches the parameter order of handle_distill.
                    .data(Arc::clone(&client_d) as Arc<dyn InternalClient>)
                    .data(Arc::clone(&embedder_d) as Arc<dyn Embedder + Send + Sync>)
                    .data(Arc::clone(&synth_d) as Arc<dyn DistillSynthesizer + Send + Sync>)
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| match ev {
                            Event::Start => m.inc_workers_active("distill"),
                            Event::Stop => m.dec_workers_active("distill"),
                            Event::Success => m.inc_jobs_total("distill", "done"),
                            Event::Error(_) => m.inc_jobs_total("distill", "error"),
                            _ => {}
                        }
                    })
                    .build(handle_distill)
            }
        });

    // ── Cron schedules ────────────────────────────────────────────────────────
    for sched_cfg in &config.schedules {
        if sched_cfg.name == "cleanup_dlq_daily" {
            let retention = sched_cfg.retention_days;
            let cron_expr = sched_cfg.cron.clone();
            let dlq_pool = Arc::clone(&pool);
            // Validate the cron expression before registering.
            Schedule::from_str(&cron_expr).map_err(|e| {
                anyhow::anyhow!(
                    "expression cron invalide pour '{}' ({}): {e}",
                    sched_cfg.name,
                    cron_expr
                )
            })?;
            info!(
                name = %sched_cfg.name,
                cron = %cron_expr,
                retention_days = retention,
                "schedule cron enregistré"
            );
            monitor = monitor.register(move |_idx| {
                // CronStream is not Clone — rebuild from the expression on each factory call.
                let schedule = Schedule::from_str(&cron_expr).expect(
                    "expression cron validée avant enregistrement — ne peut pas échouer ici",
                );
                WorkerBuilder::new("cleanup-dlq-daily")
                    .backend(CronStream::new(schedule))
                    // Inject pool and retention via Data (apalis::prelude::Data).
                    .data(Arc::clone(&dlq_pool))
                    .data(retention)
                    .enable_tracing()
                    .catch_panic()
                    .build(handle_cleanup_dlq)
            });
        } else {
            tracing::warn!(
                name = %sched_cfg.name,
                "schedule cron inconnu — ignoré (v0.2.0 supporte cleanup_dlq_daily uniquement)"
            );
        }
    }

    // ── Graceful shutdown via with_terminator ─────────────────────────────────
    // The "sleep" feature is absent from apalis rc.9 → use with_terminator(tokio::time::sleep).
    let monitor = monitor.with_terminator(async move {
        tokio::time::sleep(Duration::from_secs(shutdown_timeout_secs)).await;
        info!(
            secs = shutdown_timeout_secs,
            "Monitor : timeout graceful shutdown atteint — arrêt forcé"
        );
    });

    Ok(monitor)
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apalis_config_defaults_coherent() {
        let cfg = ApalisConfig::default();
        assert_eq!(cfg.workers.curate.concurrency, 2);
        assert_eq!(cfg.workers.curate.timeout_secs, 30);
        assert_eq!(cfg.workers.curate.max_retries, 3);
        assert_eq!(cfg.workers.embed.concurrency, 4);
        assert_eq!(cfg.workers.embed.timeout_secs, 60);
        assert_eq!(cfg.workers.embed.max_retries, 3);
        assert_eq!(cfg.workers.reindex.concurrency, 4);
        assert_eq!(cfg.workers.reindex.timeout_secs, 120);
        assert_eq!(cfg.workers.reindex.max_retries, 2);
        // F-22 distill : concurrence 1 (écriture série CoW-safe), timeout long, pas de retry.
        assert_eq!(cfg.workers.distill.concurrency, 1);
        assert_eq!(cfg.workers.distill.timeout_secs, 300);
        assert_eq!(cfg.workers.distill.max_retries, 0);
        assert!(cfg.schedules.is_empty());
    }

    #[test]
    fn apalis_config_from_toml() {
        let toml_str = r#"
[workers.curate]
concurrency = 3
timeout_secs = 45
max_retries = 5

[workers.embed]
concurrency = 8
timeout_secs = 90
max_retries = 2

[workers.reindex]
concurrency = 2
timeout_secs = 180
max_retries = 1

[[schedules]]
name = "cleanup_dlq_daily"
cron = "0 3 * * *"
retention_days = 30
"#;
        let cfg: ApalisConfig = toml::from_str(toml_str).expect("parse TOML ApalisConfig");
        assert_eq!(cfg.workers.curate.concurrency, 3);
        assert_eq!(cfg.workers.embed.concurrency, 8);
        assert_eq!(cfg.workers.reindex.timeout_secs, 180);
        assert_eq!(cfg.schedules.len(), 1);
        assert_eq!(cfg.schedules[0].name, "cleanup_dlq_daily");
        assert_eq!(cfg.schedules[0].retention_days, 30);
    }

    #[test]
    fn worker_config_defaults() {
        let cfg: WorkerConfig = toml::from_str("").expect("parse WorkerConfig vide");
        assert_eq!(cfg.concurrency, 2);
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(cfg.max_retries, 3);
    }
}
