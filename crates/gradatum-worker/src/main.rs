//! gradatum-worker — async queue consumer with Apalis multi-worker Monitor.
//!
//! ## Startup sequence
//!
//! 1. Parses CLI arguments (DB path, vault path, config path).
//! 2. Opens the SQLite WAL pool and applies the schema.
//! 3. Opens the Gradatum vault (creates it via `Vault::create` if absent).
//! 4. Loads the Apalis config from `--config` (section `[apalis]`).
//!    - Missing file or missing section → defaults (2/4/4 workers, no schedules).
//! 5. Attempts leader election via `LeaderElection::try_acquire`.
//!    - Non-leader: clean exit (systemd will restart as needed).
//!    - Leader: starts the renewal loop in the background.
//! 6. Starts the Prometheus HTTP server (`:19091` if `[apalis.metrics].enabled = true`).
//! 7. Starts the periodic sweep loop (`recover_stale_leases` + `cancel_expired_deadlines` + `promote_retries`).
//! 8. Starts the Apalis multi-worker Monitor.
//! 9. Graceful shutdown on SIGTERM / SIGINT with a 30 s drain.
//!
//! ## Implementation note
//!
//! `shutdown_timeout()` requires the `"sleep"` feature absent from apalis rc.9.
//! Replacement: `with_terminator(tokio::time::sleep(30s))`.
//! Behavior is identical — 30 s drain followed by forced stop.
//!
//! ## References
//!
//! - `docs/decisions/ARCH-D15-apalis-embedded.md`

mod apalis_backend;
mod apalis_handlers;
mod distill_cluster;
// curator_loader and dispatch retained for integration test compatibility.
// Not used by the binary — the Apalis Monitor replaces the Dispatcher.
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
use gradatum_core::paths::{vault_dir_index_path, vault_index_path as canon_vault_index_path};
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

/// CLI arguments for gradatum-worker.
#[derive(Parser, Debug)]
#[command(
    version,
    about = "gradatum-worker — async queue consumer (Monitor Apalis v0.2.0)"
)]
struct Cli {
    /// Path to the queue SQLite database.
    #[arg(long, default_value = "/var/lib/gradatum/db/queue.sqlite")]
    db: PathBuf,

    /// Root path of the Gradatum vault.
    ///
    /// If absent (new deployment), the vault is created with the default tenant `"main"`.
    #[arg(long, default_value = "/var/lib/gradatum/vault")]
    vault: PathBuf,

    /// Path to the server configuration file
    /// (sections `[apalis]` and `[apalis.metrics]` are read).
    ///
    /// If absent, defaults apply: 2 curate / 4 embed / 4 reindex workers,
    /// metrics disabled, no cron schedules.
    #[arg(long, default_value = "/var/lib/gradatum/config/server.toml")]
    config: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────────
// Config loading
// ─────────────────────────────────────────────────────────────────────────────

/// Loads the `[apalis]` section from the server TOML.
///
/// - Missing file → `ApalisConfig::default()` (not an error).
/// - Missing section → `ApalisConfig::default()`.
/// - Parse error → logs a warning and returns defaults.
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
// Local embed config — mirrors gradatum_server::config::EmbedConfig.
// Duplicated to avoid a gradatum-worker → gradatum-server dependency.
// Synchronize manually if defaults change in gradatum-server/src/config.rs.
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
        "http://localhost:8436/v1/embeddings".to_string()
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

/// Loads the `[embed]` section from the server TOML.
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

// ─────────────────────────────────────────────────────────────────────────────
// Storage config — lecture de [storage].root depuis server.toml
// Utilisé pour dériver index_path via SSOT helper (gradatum-core::paths).
// ─────────────────────────────────────────────────────────────────────────────

/// Représentation minimale de `[storage]` pour extraire `root` et détecter un override divergent.
///
/// `vault_index_path` est optionnel — absent = valeur par défaut canonique, présent = override à valider.
#[derive(Debug, serde::Deserialize)]
struct WorkerStorageConfig {
    root: std::path::PathBuf,
    #[serde(default)]
    vault_index_path: Option<std::path::PathBuf>,
}

/// Charge la section `[storage]` depuis le TOML serveur.
///
/// - Fichier absent → fallback sur `/var/lib/gradatum` (prod default), `vault_index_path = None`.
/// - Section absente ou parse error → idem fallback + warn.
///
/// Le `root` retourné est utilisé pour dériver `index_path` via
/// `gradatum_core::paths::vault_index_path` (SSOT).
/// `vault_index_path` non-`None` déclenche le fail-fast de divergence dans `main`.
fn load_storage_config(config_path: &std::path::Path) -> WorkerStorageConfig {
    let default = WorkerStorageConfig {
        root: std::path::PathBuf::from("/var/lib/gradatum"),
        vault_index_path: None,
    };
    if !config_path.exists() {
        return default;
    }
    let fig = Figment::new().merge(Toml::file(config_path));
    match fig.extract_inner::<WorkerStorageConfig>("storage") {
        Ok(cfg) => cfg,
        Err(e) if e.clone().into_iter().all(|inner| inner.missing()) => default,
        Err(e) => {
            warn!(
                config = %config_path.display(),
                error = %e,
                "Échec parse config [storage] — root défaut /var/lib/gradatum appliqué"
            );
            default
        }
    }
}

/// Loads the `[apalis.metrics]` section from the server TOML.
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

    // ── Open the SQLite WAL pool ──────────────────────────────────────────────
    let opts = SqliteConnectOptions::new()
        .filename(&cli.db)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        // busy_timeout 5 s: without this, SQLite returns SQLITE_BUSY immediately
        // if the server holds the WAL lock during a dequeue or ack. With
        // busy_timeout, SQLite retries for up to 5 s before failing — ack failure
        // triggers job retry instead of leaving the job in Running state.
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = Arc::new(
        SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .context("ouverture pool SQLite queue")?,
    );

    // Apply the schema (idempotent via IF NOT EXISTS).
    sqlx::query(gradatum_queue::schema::SCHEMA_V1)
        .execute(pool.as_ref())
        .await
        .context("application schéma queue")?;

    // ── Open the vault ────────────────────────────────────────────────────────
    let vault = open_or_create_vault(&cli.vault).await?;
    let vault = Arc::new(vault);
    info!(vault_root = %cli.vault.display(), "vault Gradatum prêt");

    // ── Open the SQLite FTS5 index ────────────────────────────────────────────
    // SSOT : index_path dérivé depuis [storage].root de server.toml via le helper
    // canonique gradatum-core::paths::vault_index_path.
    // JAMAIS dérivé depuis --vault (triple source of truth supprimée ici).
    // Fallback : si server.toml absent → root = /var/lib/gradatum (prod default).
    //
    // P0 — Fail-fast : si vault_index_path override est présent et diverge du canonique,
    // refuser de démarrer (split-index non disponible avant v0.5.3, même traitement que server).
    let storage_cfg = load_storage_config(&cli.config);
    let storage_root = storage_cfg.root;
    let index_path = canon_vault_index_path(&storage_root);
    if let Some(ref override_path) = storage_cfg.vault_index_path {
        if override_path != &index_path {
            anyhow::bail!(
                "vault_index_path divergent du chemin canonique — split index non supporté avant v0.5.3.\n\
                 \tConfiguré : {}\n\
                 \tCanonique  : {}\n\
                 Alignez [storage].vault_index_path avec le chemin canonique ou supprimez l'override.",
                override_path.display(),
                index_path.display()
            );
        }
    }
    info!(
        storage_root = %storage_root.display(),
        index_path = %index_path.display(),
        "--vault arg conservé pour open_or_create_vault uniquement"
    );
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

    // ── HTTP or Noop embedder depending on [embed] config ────────────────────
    let embed_cfg = load_embed_config(&cli.config);
    let embedder: Arc<dyn Embedder + Send + Sync> = if embed_cfg.enabled {
        info!(
            endpoint = %embed_cfg.endpoint,
            model = %embed_cfg.model,
            dim = embed_cfg.dim,
            "embedder HTTP wired"
        );
        // Non-blocking health probe: if the endpoint is unreachable at boot,
        // the worker still starts but emits a WARN (degraded embedding).
        probe_embed_health_tcp(&embed_cfg.endpoint).await;
        Arc::new(
            HttpEmbedder::new(&embed_cfg.endpoint, &embed_cfg.model, embed_cfg.dim)
                .with_timeout(std::time::Duration::from_millis(embed_cfg.timeout_ms)),
        )
    } else {
        warn!("embed.enabled=false — Noop embedder actif (aucun embedding généré)");
        Arc::new(NoopEmbedder::new(embed_cfg.dim))
    };

    // ── Leader election ───────────────────────────────────────────────────────
    let el = LeaderElection::new(pool.clone(), LeaderConfig::default())
        .await
        .context("init élection leader")?;
    if !el.try_acquire().await.context("try_acquire leader")? {
        info!("pas leader — exit propre (systemd relancera si nécessaire)");
        return Ok(());
    }
    info!("leadership acquis");
    let renewal = el.clone().spawn_renewal();

    // ── QueueStore ────────────────────────────────────────────────────────────
    // SqlitePool internally owns an Arc (pool.clone() is cheap).
    // SqliteQueueStore::new takes SqlitePool (not Arc<SqlitePool>).
    let store: Arc<dyn QueueStore + Send + Sync> = Arc::new(SqliteQueueStore::new((*pool).clone()));

    // ── Apalis config ─────────────────────────────────────────────────────────
    let apalis_cfg = load_apalis_config(&cli.config);
    info!(
        curate_concurrency = apalis_cfg.workers.curate.concurrency,
        embed_concurrency = apalis_cfg.workers.embed.concurrency,
        reindex_concurrency = apalis_cfg.workers.reindex.concurrency,
        schedules = apalis_cfg.schedules.len(),
        "config Apalis chargée"
    );

    // ── Prometheus metrics ────────────────────────────────────────────────────
    let metrics = WorkerMetrics::new();
    let metrics_cfg = load_metrics_config(&cli.config);
    spawn_metrics_server(&metrics_cfg, metrics.clone())
        .await
        .context("démarrage serveur métriques")?;

    // ── Periodic sweep (30 s) ─────────────────────────────────────────────────
    // Detached tokio task — terminates naturally when the runtime stops.
    let sweep_store = Arc::clone(&store);
    let sweep_pool = Arc::clone(&pool);
    let sweep_handle = tokio::spawn(async move {
        let lease_ttl = Duration::from_secs(300); // 5 minutes par défaut
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            // Pool passed for idempotency_cleanup with 24 h TTL.
            run_sweep_once(sweep_store.as_ref(), lease_ttl, Some(sweep_pool.as_ref())).await;
        }
    });

    // ── Build the Monitor ─────────────────────────────────────────────────────
    let monitor = build_monitor(
        Arc::clone(&store),
        Arc::clone(&pool),
        MonitorDeps {
            vault: Arc::clone(&vault),
            curator: Arc::clone(&curator)
                as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>,
            embedder: Arc::clone(&embedder),
            // Coerce `Arc<SqliteIndex>` → `Arc<dyn Index>` (type-erased).
            index: Arc::clone(&index) as Arc<dyn gradatum_core::index::Index>,
            // Distillation: deterministic synthesizer by default (MVP).
            // Replaceable by a `distill-semantic` LLM gateway backend without handler changes.
            distill_synthesizer: Arc::new(apalis_handlers::TemplateSynthesizer)
                as Arc<dyn apalis_handlers::DistillSynthesizer + Send + Sync>,
        },
        &apalis_cfg,
        metrics,
        30, // shutdown_timeout_secs
    )
    .context("construction Monitor Apalis")?;

    // ── Signal handling + run ─────────────────────────────────────────────────
    // SIGTERM + SIGINT → run_with_signal.
    // The 30 s terminator is already registered in build_monitor via with_terminator.
    let mut sigterm = signal(SignalKind::terminate()).expect("installation SIGTERM impossible");
    let mut sigint = signal(SignalKind::interrupt()).expect("installation SIGINT impossible");

    // Combined SIGTERM | SIGINT future for run_with_signal.
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

    // ── Cleanup ───────────────────────────────────────────────────────────────
    sweep_handle.abort();

    // Best-effort release of the leadership lease.
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

/// Opens an existing vault or creates a new one if the index is absent.
///
/// ## Behavior
///
/// - If `<root>/.gradatum/index.db` exists → `Vault::open(root)`.
/// - Otherwise → `Vault::create(root, VaultId::new("main"))`.
async fn open_or_create_vault(root: &std::path::Path) -> anyhow::Result<Vault> {
    // SSOT : index_marker via helper canonique — jamais root.join(".gradatum").join("index.db").
    // `root` ici est le répertoire vault/ (cli.vault), donc vault_dir_index_path.
    let index_marker = vault_dir_index_path(root);
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

/// Initializes the tracing subscriber (JSON + env-filter).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .init();
}

/// Non-blocking TCP health probe for the embed endpoint at startup.
///
/// Extracts `host:port` from the embeddings endpoint URL and attempts a TCP
/// connection with a 2 s timeout. On failure, emits `warn!` — embedding will be
/// degraded (jobs will fail). Never panics, never prevents worker startup.
async fn probe_embed_health_tcp(endpoint: &str) {
    // Extract "host:port" from "http://host:port/path".
    let host_port = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|s| s.split('/').next());

    let host_port = match host_port {
        Some(hp) => hp,
        None => {
            warn!(
                endpoint = %endpoint,
                "semantic search disabled — embed endpoint URL invalide (format inattendu)"
            );
            return;
        }
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect(host_port),
    )
    .await
    {
        Ok(Ok(_)) => {
            tracing::debug!(host_port = %host_port, "embed endpoint TCP OK");
        }
        Ok(Err(e)) => {
            warn!(
                host_port = %host_port,
                error = %e,
                "semantic search disabled — embed endpoint unreachable"
            );
        }
        Err(_timeout) => {
            warn!(
                host_port = %host_port,
                "semantic search disabled — embed endpoint unreachable (timeout 2s)"
            );
        }
    }
}
