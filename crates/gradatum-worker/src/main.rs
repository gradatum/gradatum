//! gradatum-worker — consommateur de queue async avec Monitor Apalis multi-worker.
//!
//! ## Démarrage v0.2.0 (Phase 2 — F-15 Monitor)
//!
//! 1. Parse les arguments CLI (chemins DB, vault, config).
//! 2. Ouvre le pool SQLite WAL et applique le schéma.
//! 3. Ouvre le vault Gradatum (crée si absent via `Vault::create`).
//! 4. Charge la config Apalis depuis `--config` (section `[apalis]`).
//!    - Fichier absent ou section absente → défauts (2/4/4 workers, pas de schedules).
//! 5. Tente l'élection leader via `LeaderElection::try_acquire`.
//!    - Si non-leader : exit propre (le processus peut être relancé par systemd).
//!    - Si leader : démarre la renewal loop en arrière-plan.
//! 6. Démarre le serveur HTTP Prometheus (`:19091` si `[apalis.metrics].enabled = true`).
//! 7. Lance la boucle sweep périodique (recover_stale_leases + cancel_expired_deadlines + promote_retries).
//! 8. Lance le Monitor Apalis multi-worker.
//! 9. Shutdown propre sur SIGTERM / SIGINT avec drain 30s.
//!
//! ## Écarts par rapport au plan spec v0.2.0
//!
//! - E-11 : `shutdown_timeout()` requiert feature `"sleep"` absente de apalis rc.9.
//!   Remplacement : `with_terminator(tokio::time::sleep(30s))`.
//!   Comportement identique — drain 30s puis arrêt forcé.
//!
//! ## References
//!
//! - Spec §5 Phase 2 — F-15 Monitor multi-worker
//! - v81 §6 L9612-9780 — WorkerSupervisor
//! - ARCH-D15 — `docs/decisions/ARCH-D15-apalis-embedded.md`

mod apalis_backend;
mod apalis_handlers;
// curator_loader et dispatch conservés pour compatibilité tests d'intégration.
// Non utilisés par le binaire v0.2.0 — Monitor Apalis remplace le Dispatcher.
#[allow(dead_code)]
mod curator_loader;
#[allow(dead_code)]
mod dispatch;
mod leader;
mod metrics;
mod monitor;
mod schedules;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use figment::{
    providers::{Format, Toml},
    Figment,
};
use gradatum_core::scope::VaultId;
use gradatum_core::QueueStore;
use gradatum_db_sqlite::SqliteQueueStore;
use gradatum_embed::{Embedder, HttpEmbedder, Noop as NoopEmbedder};
use gradatum_index::SqliteIndex;
use gradatum_vault::Vault;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use curator_loader::build_curator_pipeline;
use leader::{LeaderConfig, LeaderElection};
use metrics::{spawn_metrics_server, MetricsConfig, WorkerMetrics};
use monitor::{build_monitor, ApalisConfig, MonitorDeps};
use schedules::run_sweep_once;

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

/// Arguments CLI de gradatum-worker.
#[derive(Parser, Debug)]
#[command(
    version,
    about = "gradatum-worker — async queue consumer (Monitor Apalis v0.2.0)"
)]
struct Cli {
    /// Chemin vers la base SQLite de la queue.
    #[arg(long, default_value = "/var/lib/gradatum/db/queue.sqlite")]
    db: PathBuf,

    /// Chemin racine du vault Gradatum.
    ///
    /// Si absent (nouveau déploiement), le vault est créé avec le tenant par défaut `"main"`.
    #[arg(long, default_value = "/var/lib/gradatum/vault")]
    vault: PathBuf,

    /// Chemin vers le fichier de configuration serveur
    /// (sections `[apalis]` et `[apalis.metrics]` lues).
    ///
    /// Si absent, les défauts s'appliquent : 2 curate / 4 embed / 4 reindex workers,
    /// métriques désactivées, pas de schedules cron.
    #[arg(long, default_value = "/var/lib/gradatum/config/server.toml")]
    config: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────────
// Config loading
// ─────────────────────────────────────────────────────────────────────────────

/// Charge la section `[apalis]` du TOML serveur.
///
/// - Fichier absent → `ApalisConfig::default()` (pas une erreur).
/// - Section absente → `ApalisConfig::default()`.
/// - Erreur parse → log warn + défauts.
fn load_apalis_config(config_path: &std::path::Path) -> ApalisConfig {
    if !config_path.exists() {
        return ApalisConfig::default();
    }
    let fig = Figment::new().merge(Toml::file(config_path));
    match fig.extract_inner::<ApalisConfig>("apalis") {
        Ok(cfg) => cfg,
        Err(e) if e.clone().into_iter().all(|inner| inner.missing()) => ApalisConfig::default(),
        Err(e) => {
            warn!(
                config = %config_path.display(),
                error = %e,
                "Échec parse config [apalis] — défauts appliqués"
            );
            ApalisConfig::default()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Config embed locale — miroir de gradatum_server::config::EmbedConfig
// Dupliquée pour éviter la dépendance gradatum-worker → gradatum-server.
// Synchroniser manuellement si les défauts changent dans gradatum-server/src/config.rs.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WorkerEmbedConfig {
    #[serde(default = "WorkerEmbedConfig::default_enabled")]
    enabled: bool,
    #[serde(default = "WorkerEmbedConfig::default_endpoint")]
    endpoint: String,
    #[serde(default = "WorkerEmbedConfig::default_model")]
    model: String,
    #[serde(default = "WorkerEmbedConfig::default_dim")]
    dim: u16,
    #[serde(default = "WorkerEmbedConfig::default_timeout_ms")]
    timeout_ms: u64,
}

impl WorkerEmbedConfig {
    fn default_enabled() -> bool {
        true
    }
    fn default_endpoint() -> String {
        "http://localhost:8431/v1/embeddings".to_string()
    }
    fn default_model() -> String {
        "bge-m3-Q8_0".to_string()
    }
    fn default_dim() -> u16 {
        1024
    }
    fn default_timeout_ms() -> u64 {
        5000
    }
}

impl Default for WorkerEmbedConfig {
    fn default() -> Self {
        Self {
            enabled: Self::default_enabled(),
            endpoint: Self::default_endpoint(),
            model: Self::default_model(),
            dim: Self::default_dim(),
            timeout_ms: Self::default_timeout_ms(),
        }
    }
}

/// Charge la section `[embed]` du TOML serveur.
fn load_embed_config(config_path: &std::path::Path) -> WorkerEmbedConfig {
    if !config_path.exists() {
        return WorkerEmbedConfig::default();
    }
    let fig = Figment::new().merge(Toml::file(config_path));
    match fig.extract_inner::<WorkerEmbedConfig>("embed") {
        Ok(cfg) => cfg,
        Err(e) if e.clone().into_iter().all(|inner| inner.missing()) => {
            WorkerEmbedConfig::default()
        }
        Err(e) => {
            warn!(
                config = %config_path.display(),
                error = %e,
                "Échec parse config [embed] — défauts appliqués (Noop embedder)"
            );
            WorkerEmbedConfig::default()
        }
    }
}

/// Charge la section `[apalis.metrics]` du TOML serveur.
fn load_metrics_config(config_path: &std::path::Path) -> MetricsConfig {
    if !config_path.exists() {
        return MetricsConfig::default();
    }
    let fig = Figment::new().merge(Toml::file(config_path));
    match fig.extract_inner::<MetricsConfig>("apalis.metrics") {
        Ok(cfg) => cfg,
        Err(e) if e.clone().into_iter().all(|inner| inner.missing()) => MetricsConfig::default(),
        Err(e) => {
            warn!(
                config = %config_path.display(),
                error = %e,
                "Échec parse config [apalis.metrics] — métriques désactivées"
            );
            MetricsConfig::default()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();
    info!(
        db = %cli.db.display(),
        vault = %cli.vault.display(),
        config = %cli.config.display(),
        "gradatum-worker v0.2.0 démarrage (Monitor Apalis multi-worker)"
    );

    // ── Ouverture du pool SQLite WAL ─────────────────────────────────────────
    let opts = SqliteConnectOptions::new()
        .filename(&cli.db)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // 5s busy_timeout : sans ce réglage, SQLite renvoie SQLITE_BUSY
        // immédiatement si le serveur tient le verrou WAL lors d'un dequeue ou
        // d'un ack. Avec busy_timeout, SQLite réessaie jusqu'à 5s avant
        // d'échouer — l'ack fail → job retry au lieu de rester Running.
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = Arc::new(
        SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .context("ouverture pool SQLite queue")?,
    );

    // Application du schéma (idempotent via IF NOT EXISTS).
    sqlx::query(gradatum_queue::schema::SCHEMA_V1)
        .execute(pool.as_ref())
        .await
        .context("application schéma queue")?;

    // ── Ouverture du vault ────────────────────────────────────────────────────
    let vault = open_or_create_vault(&cli.vault).await?;
    let vault = Arc::new(vault);
    info!(vault_root = %cli.vault.display(), "vault Gradatum prêt");

    // ── Ouverture de l'index SQLite FTS5 ─────────────────────────────────────
    // Index partagé vault/.gradatum/index.db — même base que gradatum-server (T10).
    let index_path = cli.vault.join(".gradatum").join("index.db");
    let index = Arc::new(
        SqliteIndex::open(&index_path)
            .await
            .context("ouverture SqliteIndex")?,
    );
    info!(index_path = %index_path.display(), "SqliteIndex (FTS5) prêt");

    // ── Curator pipeline ──────────────────────────────────────────────────────
    // build_curator_pipeline lit la section [curator] du TOML serveur.
    // Fichier absent ou section absente → mode heuristique offline (défaut sûr).
    let curator = Arc::new(build_curator_pipeline(&cli.config));
    info!(backend = curator.backend_name(), "CuratorPipeline prêt");

    // ── Embedder HTTP ou Noop selon config [embed] ────────────────────────────
    let embed_cfg = load_embed_config(&cli.config);
    let embedder: Arc<dyn Embedder + Send + Sync> = if embed_cfg.enabled {
        info!(
            endpoint = %embed_cfg.endpoint,
            model = %embed_cfg.model,
            dim = embed_cfg.dim,
            "embedder HTTP wired"
        );
        Arc::new(
            HttpEmbedder::new(&embed_cfg.endpoint, &embed_cfg.model, embed_cfg.dim)
                .with_timeout(std::time::Duration::from_millis(embed_cfg.timeout_ms)),
        )
    } else {
        warn!("embed.enabled=false — Noop embedder actif (aucun embedding généré)");
        Arc::new(NoopEmbedder::new(embed_cfg.dim))
    };

    // ── Élection leader ───────────────────────────────────────────────────────
    let el = LeaderElection::new(pool.clone(), LeaderConfig::default())
        .await
        .context("init élection leader")?;
    if !el.try_acquire().await.context("try_acquire leader")? {
        info!("pas leader — exit propre (systemd relancera si nécessaire)");
        return Ok(());
    }
    info!("leadership acquis");
    let renewal = el.clone().spawn_renewal();

    // ── Store QueueStore ──────────────────────────────────────────────────────
    // SqlitePool est lui-même un Arc interne (pool.clone() est cheap).
    // SqliteQueueStore::new prend SqlitePool (pas Arc<SqlitePool>).
    let store: Arc<dyn QueueStore + Send + Sync> = Arc::new(SqliteQueueStore::new((*pool).clone()));

    // ── Config Apalis ─────────────────────────────────────────────────────────
    let apalis_cfg = load_apalis_config(&cli.config);
    info!(
        curate_concurrency = apalis_cfg.workers.curate.concurrency,
        embed_concurrency = apalis_cfg.workers.embed.concurrency,
        reindex_concurrency = apalis_cfg.workers.reindex.concurrency,
        schedules = apalis_cfg.schedules.len(),
        "config Apalis chargée"
    );

    // ── Métriques Prometheus ──────────────────────────────────────────────────
    let metrics = WorkerMetrics::new();
    let metrics_cfg = load_metrics_config(&cli.config);
    spawn_metrics_server(&metrics_cfg, metrics.clone())
        .await
        .context("démarrage serveur métriques")?;

    // ── Sweep périodique (30s) ────────────────────────────────────────────────
    // Tâche tokio détachée — se termine naturellement à l'arrêt du runtime.
    let sweep_store = Arc::clone(&store);
    let sweep_pool = Arc::clone(&pool);
    let sweep_handle = tokio::spawn(async move {
        let lease_ttl = Duration::from_secs(300); // 5 minutes par défaut
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            // P1-2 : pool passé pour idempotency_cleanup TTL 24h.
            run_sweep_once(sweep_store.as_ref(), lease_ttl, Some(sweep_pool.as_ref())).await;
        }
    });

    // ── Construction du Monitor ───────────────────────────────────────────────
    let monitor = build_monitor(
        Arc::clone(&store),
        Arc::clone(&pool),
        MonitorDeps {
            vault: Arc::clone(&vault),
            curator: Arc::clone(&curator)
                as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>,
            embedder: Arc::clone(&embedder),
            index: Arc::clone(&index),
        },
        &apalis_cfg,
        metrics,
        30, // shutdown_timeout_secs
    )
    .context("construction Monitor Apalis")?;

    // ── Signal handling + run ─────────────────────────────────────────────────
    // SIGTERM + SIGINT → run_with_signal.
    // Le terminator 30s est déjà enregistré dans build_monitor via with_terminator.
    let mut sigterm = signal(SignalKind::terminate()).expect("installation SIGTERM impossible");
    let mut sigint = signal(SignalKind::interrupt()).expect("installation SIGINT impossible");

    // Crée une future combinée SIGTERM | SIGINT pour run_with_signal.
    let shutdown_signal = async move {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM reçu — arrêt graceful Monitor (30s drain)");
            }
            _ = sigint.recv() => {
                info!("SIGINT reçu — arrêt graceful Monitor (30s drain)");
            }
        }
        Ok::<(), std::io::Error>(())
    };

    match monitor.run_with_signal(shutdown_signal).await {
        Ok(()) => info!("Monitor arrêté proprement"),
        Err(e) => error!(error = %e, "Monitor erreur à l'arrêt"),
    }

    // ── Nettoyage ─────────────────────────────────────────────────────────────
    sweep_handle.abort();

    // Libération best-effort de la lease leadership.
    renewal.abort();
    match el.release().await {
        Ok(()) => info!("lease leadership libérée"),
        Err(e) => {
            error!(error = %e, "impossible de libérer la lease leadership (TTL fallback actif)")
        }
    }

    info!("gradatum-worker arrêté proprement");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Ouvre un vault existant ou en crée un nouveau si l'index est absent.
///
/// ## Comportement
///
/// - Si `<root>/.gradatum/index.db` existe → `Vault::open(root)`.
/// - Sinon → `Vault::create(root, VaultId::new("main"))`.
async fn open_or_create_vault(root: &std::path::Path) -> anyhow::Result<Vault> {
    let index_marker = root.join(".gradatum").join("index.db");
    if index_marker.exists() {
        Vault::open(root)
            .await
            .map_err(|e| anyhow::anyhow!("Vault::open({}) failed: {e}", root.display()))
    } else {
        Vault::create(root, VaultId::new("main"))
            .await
            .map_err(|e| anyhow::anyhow!("Vault::create({}) failed: {e}", root.display()))
    }
}

/// Initialise le subscriber tracing (JSON + env-filter).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .init();
}
