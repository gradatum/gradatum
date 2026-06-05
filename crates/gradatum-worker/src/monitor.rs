//! Orchestration Monitor Apalis multi-worker — F-15.
//!
//! Construit un [`Monitor`](apalis_core::monitor::Monitor) Apalis qui orchestre
//! les trois workers Gradatum (curate, embed, reindex) via le [`GradatumBackend`]
//! custom et les layers Tower :
//!
//! | Kind | Concurrence | Timeout | Retries | Couche |
//! |---|---|---|---|---|
//! | `curate` | 2 | 30s | 3 | Trace + Timeout + Retry + CatchPanic |
//! | `embed` | 4 | 60s | 3 | Trace + Timeout + Retry + CatchPanic |
//! | `reindex` | 4 | 120s | 2 | Trace + Timeout + Retry + CatchPanic |
//!
//! # Graceful shutdown
//!
//! `SIGTERM` ou `SIGINT` → [`Monitor::with_terminator`] avec 30s timeout.
//! Remplace `shutdown_timeout()` qui requiert la feature `"sleep"` absente de rc.9.
//!
//! # Schedule cron
//!
//! Enregistre `cleanup_dlq_daily` via [`CronStream`](apalis_cron::CronStream) si
//! la config contient au moins une entrée `[[apalis.schedules]]`.
//!
//! # Métriques Prometheus
//!
//! Les métriques sont incrémentées via les hooks `on_event` sur les workers.
//! Exposées sur le port `:19091` via [`spawn_metrics_server`](super::metrics::spawn_metrics_server).
//!
//! # Références
//!
//! - Spec §5 Phase 2 — Monitor multi-worker
//! - v81 §6 L9612-9780 — WorkerSupervisor + Monitor pattern
//! - ARCH-D15 — `docs/decisions/ARCH-D15-apalis-embedded.md`

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use apalis::layers::retry::RetryPolicy;
use apalis::layers::WorkerBuilderExt;
use apalis::prelude::AcknowledgementExt;
use apalis::prelude::{Event, EventListenerExt, Monitor, WorkerBuilder};
use apalis_cron::CronStream;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use tracing::info;

use gradatum_core::QueueStore;
use gradatum_curator::CuratorProcess;
use gradatum_embed::Embedder;
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use sqlx::SqlitePool;

use super::apalis_backend::build_gradatum_backend;
use super::apalis_handlers::{handle_curate, handle_embed, handle_reindex};
use super::metrics::WorkerMetrics;
use super::schedules::{handle_cleanup_dlq, ScheduleConfig};

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration d'un worker par kind depuis le TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Nombre de slots concurrents pour ce kind.
    #[serde(default = "WorkerConfig::default_concurrency")]
    pub concurrency: usize,
    /// Timeout par job en secondes.
    #[serde(default = "WorkerConfig::default_timeout_secs")]
    pub timeout_secs: u64,
    /// Nombre maximum de retentatives avant DLQ.
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

/// Configuration Apalis complète : workers + schedules.
///
/// Lue depuis `[apalis]` du TOML serveur via figment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApalisConfig {
    /// Config par kind de worker.
    #[serde(default)]
    pub workers: WorkersConfig,
    /// Schedules cron périodiques.
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
}

/// Config des 3 workers actifs v0.2.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersConfig {
    /// Worker curator LLM — LLM-bound, concurrence faible recommandée.
    #[serde(default = "WorkersConfig::default_curate")]
    pub curate: WorkerConfig,
    /// Worker embed — I/O-bound, concurrence élevée OK.
    #[serde(default = "WorkersConfig::default_embed")]
    pub embed: WorkerConfig,
    /// Worker reindex — I/O-bound, timeout long.
    #[serde(default = "WorkersConfig::default_reindex")]
    pub reindex: WorkerConfig,
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
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            curate: Self::default_curate(),
            embed: Self::default_embed(),
            reindex: Self::default_reindex(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Builder principal
// ─────────────────────────────────────────────────────────────────────────────

/// Dépendances des handlers injectées via `.data()` dans chaque worker Apalis.
///
/// Regroupées dans un struct pour respecter la limite clippy (max 7 args par fonction).
pub struct MonitorDeps {
    /// Registry vault — écriture/lecture notes (curate + embed).
    pub vault: Arc<Vault>,
    /// Pipeline curator — classification inbox F-42 (curate).
    pub curator: Arc<dyn CuratorProcess + Send + Sync>,
    /// Backend embedding — génération vecteurs F-01 (embed + reindex).
    pub embedder: Arc<dyn Embedder + Send + Sync>,
    /// Index FTS5 SQLite — wikilinks B5 + reindex (tous workers).
    pub index: Arc<SqliteIndex>,
}

/// Construit le [`Monitor`] Apalis avec les workers et schedules configurés.
///
/// # Paramètres
///
/// - `store` : implémentation `QueueStore` partagée entre les workers
/// - `pool` : pool SQLite pour les schedules cron (cleanup_dlq_daily)
/// - `deps` : dépendances handlers — vault, curator, embedder, index
/// - `config` : configuration workers + schedules lue depuis le TOML
/// - `metrics` : registre Prometheus partagé pour les hooks `on_event`
/// - `shutdown_timeout_secs` : durée du drain graceful après signal SIGTERM/SIGINT
///
/// # Erreurs
///
/// Retourne une erreur si la construction du [`GradatumBackend`] échoue ou si
/// une expression cron de la config est invalide.
pub fn build_monitor(
    store: Arc<dyn QueueStore + Send + Sync>,
    pool: Arc<SqlitePool>,
    deps: MonitorDeps,
    config: &ApalisConfig,
    metrics: WorkerMetrics,
    shutdown_timeout_secs: u64,
) -> anyhow::Result<Monitor> {
    let MonitorDeps {
        vault,
        curator,
        embedder,
        index,
    } = deps;
    let curate_cfg = config.workers.curate.clone();
    let embed_cfg = config.workers.embed.clone();
    let reindex_cfg = config.workers.reindex.clone();

    // Backends isolés par kind — chaque worker ne fetch QUE ses propres jobs.
    // Fix routing DLQ : build_gradatum_backend(store, kind) appelle dequeue_by_kind(kind)
    // au lieu de dequeue() — élimine la race condition où embed/reindex-workers
    // pouvaient fetcher un Curate job et retourner HandlerError::UnexpectedVariant (~80% DLQ).
    // La colonne `kind` est désormais renseignée par enqueue() + backfillée par migration 010.
    // Phase 1.2 fix : build_gradatum_backend retourne (backend, acknowledger).
    // L'acknowledger est branché via .ack_with() → store.complete/fail.
    let (curate_backend, curate_ack) = build_gradatum_backend(Arc::clone(&store), "Curate")?;
    let (embed_backend, embed_ack) = build_gradatum_backend(Arc::clone(&store), "Embed")?;
    let (reindex_backend, reindex_ack) = build_gradatum_backend(Arc::clone(&store), "ReIndex")?;

    let m_curate = metrics.clone();
    let m_embed = metrics.clone();
    let m_reindex = metrics.clone();

    let mut monitor = Monitor::new()
        // ── Worker curate ─────────────────────────────────────────────────────
        .register({
            let cfg = curate_cfg.clone();
            let backend = curate_backend;
            let ack = curate_ack;
            let m = m_curate;
            let vault_c = Arc::clone(&vault);
            let curator_c = Arc::clone(&curator);
            let index_c = Arc::clone(&index);
            // Queue injectée dans handle_curate pour le chaînage curate→embed (Tranche A).
            let queue_c = Arc::clone(&store);
            move |idx| {
                let worker_name = format!("curate-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    // Injection des dépendances handler — clonées par worker instance.
                    // Ordre = ordre des params de handle_curate.
                    .data(Arc::clone(&vault_c))
                    .data(Arc::clone(&curator_c) as Arc<dyn CuratorProcess + Send + Sync>)
                    .data(Arc::clone(&index_c))
                    .data(Arc::clone(&queue_c) as Arc<dyn QueueStore + Send + Sync>)
                    // Phase 1.2 fix : ack_with câble store.complete/fail après chaque handler.
                    // enable_tracing() re-activé — task_id injecté dans record_to_task.
                    .ack_with(ack.clone())
                    .enable_tracing()
                    .timeout(Duration::from_secs(cfg.timeout_secs))
                    .retry(RetryPolicy::retries(cfg.max_retries))
                    .catch_panic()
                    .concurrency(cfg.concurrency)
                    .on_event({
                        let m = m.clone();
                        move |_ctx, ev| {
                            // Event rc.9 : Start|Stop|Success|Error|Idle|HeartBeat|Custom
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
        // ── Worker embed ──────────────────────────────────────────────────────
        .register({
            let cfg = embed_cfg.clone();
            let backend = embed_backend;
            let ack = embed_ack;
            let m = m_embed;
            let vault_e = Arc::clone(&vault);
            let embedder_e = Arc::clone(&embedder);
            let index_e = Arc::clone(&index);
            move |idx| {
                let worker_name = format!("embed-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&vault_e))
                    .data(Arc::clone(&embedder_e) as Arc<dyn Embedder + Send + Sync>)
                    .data(Arc::clone(&index_e))
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
        // ── Worker reindex ────────────────────────────────────────────────────
        .register({
            let cfg = reindex_cfg.clone();
            let backend = reindex_backend;
            let ack = reindex_ack;
            let m = m_reindex;
            let embedder_r = Arc::clone(&embedder);
            let index_r = Arc::clone(&index);
            move |idx| {
                let worker_name = format!("reindex-{idx}");
                WorkerBuilder::new(&worker_name)
                    .backend(backend.clone())
                    .data(Arc::clone(&index_r))
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
        });

    // ── Schedules cron ────────────────────────────────────────────────────────
    for sched_cfg in &config.schedules {
        if sched_cfg.name == "cleanup_dlq_daily" {
            let retention = sched_cfg.retention_days;
            let cron_expr = sched_cfg.cron.clone();
            let dlq_pool = Arc::clone(&pool);
            // Valider l'expression cron avant de l'enregistrer.
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
                // CronStream n'est pas Clone — reconstruire depuis l'expression à chaque appel factory.
                let schedule = Schedule::from_str(&cron_expr).expect(
                    "expression cron validée avant enregistrement — ne peut pas échouer ici",
                );
                WorkerBuilder::new("cleanup-dlq-daily")
                    .backend(CronStream::new(schedule))
                    // Injection du pool et de la rétention via Data (apalis::prelude::Data).
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

    // ── Graceful shutdown — with_terminator (shutdown_timeout nécessite feature "sleep") ──
    // E-11 : feature "sleep" absente de apalis rc.9 → utiliser with_terminator(tokio::time::sleep)
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
// Tests
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
