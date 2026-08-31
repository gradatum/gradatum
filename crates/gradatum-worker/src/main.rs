//! gradatum-worker — async queue consumer with Apalis multi-worker Monitor.
//!
//! ## Startup sequence
//!
//! 1. Parses CLI arguments (DB path, config path).
//! 2. Opens the SQLite WAL queue database, applies `SCHEMA_V1` (queue + leadership slot) then the
//!    `gradatum-db-sqlite` migrations (`gradatum_jobs`, `gradatum_idempotency`). Les deux
//!    sont nécessaires : sans les secondes, un worker démarré avant le serveur sur une base
//!    vierge s'arrête silencieusement dès la première tâche.
//! 3. Reads `GRADATUM_INTERNAL_URL` + `GRADATUM_INTERNAL_TOKEN` env vars.
//! 4. Builds `InternalPersistClient` — all vault+index mutations go through the server.
//! 5. Loads the Apalis config from `--config` (section `[apalis]`).
//!    - Missing file or missing section → defaults (2/4/4 workers, no schedules).
//! 6. Attempts leader election via `LeaderElection::try_acquire`.
//!    - Non-leader: clean exit (systemd will restart as needed).
//!    - Leader: starts the renewal loop in the background.
//! 7. Starts the Prometheus HTTP server if `[apalis.metrics].enabled = true`, on the
//!    configured port (default `19091`; the deployed configuration uses `19093`).
//!    Also publishes `gradatum_config_degraded`, which reports every section that fell
//!    back to its defaults during steps 5-6 — and why.
//! 8. Starts the periodic sweep loop (`recover_stale_leases` + `cancel_expired_deadlines` + `promote_retries`).
//! 9. Starts the Apalis multi-worker Monitor.
//! 10. Graceful shutdown on SIGTERM / SIGINT with a 30 s drain.
//!
//! ## Implementation note
//!
//! `shutdown_timeout()` requires the `"sleep"` feature absent from apalis rc.9.
//! Replacement: `with_terminator(tokio::time::sleep(30s))`.
//! Behavior is identical — 30 s drain followed by forced stop.
//!
//! ## Worker-flip (v0.5.3)
//!
//! The worker no longer opens `SqliteIndex` nor `Vault` directly.
//! All mutations go via `InternalPersistClient` → server `/internal/v1/` API.
//! The `--vault` CLI arg has been removed. Single-owner DB: server owns `index.db`.
//!
//! ## References
//!
//! - `docs/decisions/ARCH-D15-apalis-embedded.md`

mod apalis_backend;
mod apalis_handlers;
mod config_health;
// Required by apalis_handlers::handle_validate (F-43 quality gate) — wired via monitor.rs.
mod internal_client;
mod quality_score;
// curator_loader retained for integration test compatibility (also used by the binary).
#[allow(dead_code)]
mod curator_loader;
mod leader;
mod metrics;
mod monitor;
mod queue_path;
mod schedules;
mod wikilinks;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use gradatum_core::QueueStore;
use gradatum_db_sqlite::{SqliteQueueStore, open_queue_db, run_migrations};
use gradatum_embed::{Embedder, HttpEmbedder, Noop as NoopEmbedder};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use config_health::{ConfigHealth, load_section};
use curator_loader::build_curator_pipeline;
use internal_client::InternalPersistClient;
use leader::{LeaderConfig, LeaderElection};
use metrics::{MetricsConfig, WorkerMetrics, spawn_metrics_server};
use monitor::{ApalisConfig, MonitorDeps, build_monitor};
use schedules::{DistillCronConfig, run_sweep_once};

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

/// String rendered by `--version`: the semantic version **followed by the build commit
/// SHA**.
///
/// The format is stable and guaranteed to stay script-extractable:
/// `gradatum-worker <semver> (build_sha <sha>)`
///
/// `<sha>` is injected at compile time by `build.rs` (`cargo:rustc-env=BUILD_SHA`) and
/// matches the `build_sha` field of the server's `GET /health`. It reads `unknown` when
/// the SHA could not be resolved at build time — no `.git` directory, or a build from a
/// tarball — a fallback carried by `build.rs`, which never fails. `env!` is therefore
/// always resolvable here, since `build.rs` emits the variable unconditionally.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (build_sha ",
    env!("BUILD_SHA"),
    ")"
);

/// CLI arguments for gradatum-worker.
#[derive(Parser, Debug)]
#[command(
    version = VERSION,
    about = "gradatum-worker — async queue consumer (Monitor Apalis v0.2.0)"
)]
struct Cli {
    /// Path to the queue SQLite database.
    ///
    /// Optional. When omitted the path is derived from `[storage] root` of
    /// `--config` through `gradatum_core::paths::queue_db_path` — the same
    /// helper `gradatum-server` uses (SSOT).
    ///
    /// When supplied, the value is **validated** against that canonical path,
    /// not trusted: a divergent `--db` aborts the boot instead of silently
    /// creating a second, empty queue database.
    #[arg(long)]
    db: Option<PathBuf>,

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

// ─────────────────────────────────────────────────────────────────────────────
// Note — the five per-section loaders that used to live here (`[apalis]`,
// `[distill_cron]`, `[multi_tenant]`, `[embed]`, `[apalis.metrics]`) were five copies of
// the same figment extraction. They are now a single call to
// `config_health::load_section`, which additionally records WHY a section fell back to
// its defaults instead of collapsing "absent" and "malformed" into the same silence.
// Each call site names the section, and the effect its fallback produces.
// ─────────────────────────────────────────────────────────────────────────────

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

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // SSOT — the queue path comes from [storage] root via the canonical helper,
    // exactly like `gradatum-server`. A divergent `--db` is a hard error here
    // rather than a silently-created empty database (fail-fast symmetrical with
    // the `vault_index_path` divergence check on the server side).
    let db_path = queue_path::resolve_queue_db_path(cli.db.as_deref(), &cli.config)?;

    info!(
        db = %db_path.display(),
        config = %cli.config.display(),
        "gradatum-worker v0.2.0 starting (Apalis Monitor multi-worker, worker-flip)"
    );

    // ── Open the SQLite WAL queue database ────────────────────────────────────
    // create_if_missing stays true: the worker unit only `Wants=` the server,
    // so it may legitimately reach this point on a first boot before the
    // server created the file. What used to make this dangerous was the
    // hard-coded path — a wrong path could be created silently. That door is
    // now closed by resolve_queue_db_path: the path is either derived from
    // [storage] root, or validated against it.
    // `open_queue_db` applique WAL + busy_timeout 5 s (réglages sqlx d'origine) :
    // sans busy_timeout, SQLite renvoie SQLITE_BUSY immédiatement si le serveur tient
    // le verrou WAL lors d'un dequeue/ack — avec, il retente jusqu'à 5 s avant d'échouer.
    let db = open_queue_db(&db_path)
        .await
        .context("opening SQLite queue database")?;

    // Apply the schema (idempotent via IF NOT EXISTS).
    //
    // `execute_batch` exécute l'unique instruction de SCHEMA_V1 (`worker_leadership`).
    // La table legacy `jobs_v2` et ses deux index ne sont plus créés depuis 2.1.0 (F-177)
    // — le worker ne lit que `gradatum_jobs`.
    db.with_conn(|conn| conn.execute_batch(gradatum_queue::schema::SCHEMA_V1))
        .await
        .context("applying queue schema")?;

    // Migrations Apalis — `gradatum_jobs`, `gradatum_idempotency` et leurs index.
    //
    // SCHEMA_V1 ne couvre QUE le slot de leadership (la file legacy `jobs_v2` est
    // supprimée depuis 2.1.0, F-177) ; les tables que consomment le Monitor et les
    // balayages périodiques viennent des migrations de `gradatum-db-sqlite`. Sans cet appel, un worker démarré sur une base
    // vierge journalise un boot nominal puis s'arrête ~200 ms plus tard : chaque tâche
    // Apalis échoue sur `no such table: gradatum_jobs`, le Monitor se vide et rend la main
    // avec le code 0 — une mort silencieuse que rien ne distingue d'un arrêt propre.
    //
    // Invisible en production tant que `gradatum-server` ouvre la base en premier, mais
    // l'unité du worker ne fait que `Wants=` le serveur : l'ordre inverse est légal.
    //
    // Le runner honore la table de suivi `_sqlx_migrations` : aucune migration déjà
    // appliquée n'est rejouée (les migrations 007/011 non-idempotentes ne peuvent donc
    // pas corrompre la base LIVE). Sur une base vierge où les deux processus démarrent
    // simultanément, le perdant de la course voit sa transaction rejetée et sort en
    // erreur ; `Restart=always` + `RestartSec=15s` le ramènent sur une base déjà migrée.
    run_migrations(&db)
        .await
        .context("applying queue migrations (gradatum_jobs, gradatum_idempotency)")?;

    // ── Internal client (worker-flip) ─────────────────────────────────────────
    // All vault+index mutations go through the server's /internal/v1/ API.
    // GRADATUM_INTERNAL_URL  : e.g. "http://127.0.0.1:19092"
    // GRADATUM_INTERNAL_TOKEN: Bearer token matching server GRADATUM_INTERNAL_TOKEN.
    let internal_url = std::env::var("GRADATUM_INTERNAL_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:19092".to_string());
    let internal_token =
        std::env::var("GRADATUM_INTERNAL_TOKEN").context("GRADATUM_INTERNAL_TOKEN must be set")?;
    let internal_client = Arc::new(
        InternalPersistClient::new(&internal_url, &internal_token)
            .context("building InternalPersistClient")?,
    );
    info!(url = %internal_url, "InternalPersistClient ready → server /internal/v1/");

    // ── Santé de la configuration ─────────────────────────────────────────────
    // Chaque section lue ci-dessous peut retomber sur ses valeurs par défaut sans
    // interrompre le boot. `config_health` accumule ces replis avec LEUR CAUSE, puis les
    // publie dans `gradatum_config_degraded` une fois le registre Prometheus construit
    // — le journal seul ne suffit pas, il faut que la machine puisse interroger l'état.
    let mut config_health = ConfigHealth::new();

    // ── Curator pipeline ──────────────────────────────────────────────────────
    // build_curator_pipeline lit la section [curator] du TOML serveur.
    // Fichier absent ou section absente → mode heuristique offline (défaut sûr).
    let curator = Arc::new(build_curator_pipeline(&cli.config, &mut config_health));
    info!(backend = curator.backend_name(), "CuratorPipeline ready");

    // ── HTTP or Noop embedder depending on [embed] config ────────────────────
    let embed_cfg: WorkerEmbedConfig = load_section(
        &cli.config,
        "embed",
        "default HTTP embedder (http://localhost:8436, bge-m3-Q8_0, dim 1024)",
        &mut config_health,
    );
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
        warn!("embed.enabled=false — Noop embedder active (no embedding generated)");
        Arc::new(NoopEmbedder::new(embed_cfg.dim))
    };

    // ── Leader election ───────────────────────────────────────────────────────
    let el = LeaderElection::new(db.clone(), LeaderConfig::default())
        .await
        .context("leader election init")?;
    if !el.try_acquire().await.context("try_acquire leader")? {
        info!("not leader — clean exit (systemd will restart if needed)");
        return Ok(());
    }
    info!("leadership acquired");
    let renewal = el.clone().spawn_renewal();

    // ── QueueStore ────────────────────────────────────────────────────────────
    // QueueDb is an Arc<Mutex<Connection>> — `clone()` shares the same connection.
    // SqliteQueueStore::new takes QueueDb (not Arc<QueueDb>).
    let store: Arc<dyn QueueStore + Send + Sync> = Arc::new(SqliteQueueStore::new(db.clone()));

    // ── Apalis config ─────────────────────────────────────────────────────────
    let apalis_cfg: ApalisConfig = load_section(
        &cli.config,
        "apalis",
        "default concurrencies (2 curate / 4 embed / 4 reindex), no schedule",
        &mut config_health,
    );
    info!(
        curate_concurrency = apalis_cfg.workers.curate.concurrency,
        embed_concurrency = apalis_cfg.workers.embed.concurrency,
        reindex_concurrency = apalis_cfg.workers.reindex.concurrency,
        schedules = apalis_cfg.schedules.len(),
        "Apalis config loaded"
    );

    // Top-level [distill_cron] (F-112) — fail-soft load; validated in build_monitor.
    let distill_cron_cfg: DistillCronConfig = load_section(
        &cli.config,
        "distill_cron",
        "distill cron disabled, no job emitted",
        &mut config_health,
    );
    let multi_tenant_cfg: apalis_handlers::MultiTenantCfg = load_section(
        &cli.config,
        "multi_tenant",
        "strict single-vault path, vaults other than \"main\" rejected",
        &mut config_health,
    );
    info!(
        distill_cron_enabled = distill_cron_cfg.enabled,
        "distill_cron config loaded"
    );

    // ── Prometheus metrics ────────────────────────────────────────────────────
    // [apalis.metrics] est lue en dernier mais publiée avec les autres : c'est la seule
    // section dont le repli coupe le canal d'exposition lui-même, d'où le récapitulatif
    // ci-dessous qui distingue « dégradé et observable » de « dégradé et invisible ».
    let metrics_cfg: MetricsConfig = load_section(
        &cli.config,
        "apalis.metrics",
        "Prometheus server disabled, no metric exposed",
        &mut config_health,
    );
    let metrics = WorkerMetrics::new();
    // Publier AVANT le démarrage du serveur : la première collecte voit déjà l'état réel.
    config_health.publish(&metrics);
    spawn_metrics_server(&metrics_cfg, metrics.clone())
        .await
        .context("starting metrics server")?;

    // ── Récapitulatif des replis de configuration ─────────────────────────────
    // Un avertissement sans destinataire équivaut à un silence (leçon F-120). Quand le
    // serveur de métriques est actif, l'état dégradé est interrogeable et le
    // récapitulatif renvoie vers lui. Quand il ne l'est pas, l'absence de destinataire
    // est elle-même le fait à signaler — et elle sort en ERROR.
    if config_health.is_degraded() {
        let sections = config_health.degraded_summary();
        if metrics_cfg.enabled {
            warn!(
                sections = %sections,
                scrape = %format!("http://{}:{}/metrics", metrics_cfg.bind, metrics_cfg.port),
                "degraded configuration — sections on defaults, state queryable \
                 through the gradatum_config_degraded gauge"
            );
        } else {
            error!(
                sections = %sections,
                "degraded configuration AND metrics server disabled — the degraded state \
                 exists only in this log, no machine can query it. Enable [apalis.metrics] \
                 to make it observable"
            );
        }
    }

    // ── Periodic sweep (30 s) ─────────────────────────────────────────────────
    // Detached tokio task — terminates naturally when the runtime stops.
    let sweep_store = Arc::clone(&store);
    let sweep_db = db.clone();
    let sweep_handle = tokio::spawn(async move {
        let lease_ttl = Duration::from_secs(300); // 5 minutes par défaut
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            // QueueDb passed for idempotency_cleanup with 24 h TTL.
            run_sweep_once(sweep_store.as_ref(), lease_ttl, Some(&sweep_db)).await;
        }
    });

    // ── Build the Monitor ─────────────────────────────────────────────────────
    let monitor = build_monitor(
        Arc::clone(&store),
        db.clone(),
        MonitorDeps {
            client: Arc::clone(&internal_client) as Arc<dyn internal_client::InternalClient>,
            curator: Arc::clone(&curator)
                as Arc<dyn gradatum_curator::CuratorProcess + Send + Sync>,
            embedder: Arc::clone(&embedder),
            // Distillation: deterministic synthesizer by default (MVP).
            // Replaceable by a `distill-semantic` LLM gateway backend without handler changes.
            distill_synthesizer: Arc::new(gradatum_distill::TemplateSynthesizer)
                as Arc<dyn gradatum_distill::DistillSynthesizer + Send + Sync>,
        },
        &apalis_cfg,
        distill_cron_cfg,
        multi_tenant_cfg,
        metrics,
        30, // shutdown_timeout_secs
    )
    .context("building Apalis Monitor")?;

    // ── Signal handling + run ─────────────────────────────────────────────────
    // SIGTERM + SIGINT → run_with_signal.
    // The 30 s terminator is already registered in build_monitor via with_terminator.
    let mut sigterm = signal(SignalKind::terminate()).expect("cannot install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("cannot install SIGINT handler");

    // Combined SIGTERM | SIGINT future for run_with_signal.
    let shutdown_signal = async move {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM received — graceful Monitor shutdown (30s drain)");
            }
            _ = sigint.recv() => {
                info!("SIGINT received — graceful Monitor shutdown (30s drain)");
            }
        }
        Ok::<(), std::io::Error>(())
    };

    match monitor.run_with_signal(shutdown_signal).await {
        Ok(()) => info!("Monitor shut down cleanly"),
        Err(e) => error!(error = %e, "Monitor error on shutdown"),
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    sweep_handle.abort();

    // Best-effort release of the leadership lease.
    renewal.abort();
    match el.release().await {
        Ok(()) => info!("leadership lease released"),
        Err(e) => {
            error!(error = %e, "cannot release leadership lease (TTL fallback active)")
        }
    }

    info!("gradatum-worker shut down cleanly");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

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
                "semantic search disabled — invalid embed endpoint URL (unexpected format)"
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
