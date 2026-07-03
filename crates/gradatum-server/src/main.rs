//! gradatum-server — HTTP/MCP facade on port 19090.
//!
//! Design notes:
//! - bind/TLS fail-closed (see `config.rs`); native TLS termination via axum-server + rustls
//! - JWT TTL scoped per audience (`auth_middleware`)
//! - metrics side-channel restricted to loopback (`metrics.rs`)
//! - real Ed25519 JWT verification via `middleware::auth_middleware`
//! - graceful SIGTERM shutdown with 30 s drain
//! - `with_job_store()` wired (`SqliteQueueStore` on `queue.sqlite`)

mod api_v1;
mod audit_jsonl;
mod auth_routes;
mod config;
mod context;
mod curated_metrics;
mod event_log_store;
mod health;
mod internal;
mod jwt_key_boot;
mod mcp_usage;
mod metrics;
mod middleware;
mod proactive_recall;
mod proactive_recall_store;
mod proactive_surface_store;
mod read_usage_store;
mod review_promote;
mod scheduled_tasks;
mod session_trace_store;
mod state;
mod stubs;
mod studio;
mod telemetry_flush;
/// Enregistrement de l'extension sqlite-vec (unsafe confiné ici, hors `gradatum-index`).
mod vec_ext;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use crate::config::ServerConfig;
use crate::state::AppState;
use gradatum_core::paths::{queue_db_path, vault_index_path as canon_vault_index_path};
// Import du boot_guard_check (caveat C2 — interdit memory store en bind non-loopback).
use gradatum_auth::revocation::boot_guard_check;
// P0-1 Phase 4.2bis : QueueStore v81 pour endpoints F-16.
use gradatum_db_sqlite::{SqliteQueueStore, run_migrations};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

#[derive(Parser, Debug)]
#[command(version, about = "gradatum-server façade HTTP/MCP")]
struct Cli {
    /// Path to the TOML configuration file (optional — defaults apply otherwise).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = ServerConfig::load(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("échec chargement config : {e}"))?;
    init_tracing(&cfg.log.format);

    info!(
        bind = %cfg.server.bind,
        metrics_bind = %cfg.server.metrics_bind,
        version = env!("CARGO_PKG_VERSION"),
        "gradatum-server démarrage"
    );

    // C7 strict : le listener métriques doit être loopback — pas de TLS escape (contrairement à C3).
    if !cfg.server.metrics_bind.ip().is_loopback() {
        anyhow::bail!(
            "metrics_bind doit être loopback (caveat C7) : adresse refusée = {}",
            cfg.server.metrics_bind
        );
    }

    // AUTH-T6 : boot_guard_check caveat C2 — refuse memory store si bind est non-loopback.
    let bind_is_loopback = cfg.server.bind.ip().is_loopback();
    boot_guard_check(bind_is_loopback, &cfg.auth.revocation_store)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // F-13 + fix P0 persistance clé JWT :
    // Load-or-generate la seed Ed25519 depuis le répertoire de config.
    // - Si jwt-signing-key.secret existe (perms ≤ 600) → charger.
    // - Si absent → générer + écrire atomiquement (tmp+chmod600+rename) + log INFO.
    // La clé n'est JAMAIS loggée — seul le chemin est tracé.
    //
    // Répertoire de la clé :
    // - Si jwt_private_key_path est sous storage.root → utiliser son parent.
    // - Sinon (config par défaut hors storage.root, ou path non dérivable)
    //   → utiliser storage.root/config/ (accessible dans l'env de test et prod).
    let jwt_key_dir = {
        let default_dir = cfg.storage.root.join("config");
        let derived = cfg
            .auth
            .jwt_private_key_path
            .parent()
            .map(|p| p.to_path_buf());
        // N'utiliser le parent dérivé que s'il est sous storage.root ou s'il s'agit
        // d'un chemin configuré explicitement (≠ défaut /var/lib/gradatum/config).
        // Heuristique : si le parent contient storage.root comme préfixe → sous contrôle.
        match derived {
            Some(ref parent) if parent.starts_with(&cfg.storage.root) => parent.clone(),
            _ => default_dir,
        }
    };

    // Créer le répertoire si absent (idempotent).
    tokio::fs::create_dir_all(&jwt_key_dir)
        .await
        .with_context(|| format!("création du répertoire clé JWT: {}", jwt_key_dir.display()))?;
    // V2 : restreindre le répertoire à 0o700 (owner only) APRÈS create_dir_all.
    // Nécessaire même si write_atomic le refait : le répertoire existant depuis un boot
    // précédent pourrait avoir des permissions trop ouvertes (ex. 0o755 umask).
    tokio::fs::set_permissions(&jwt_key_dir, std::fs::Permissions::from_mode(0o700))
        .await
        .with_context(|| {
            format!(
                "chmod 0o700 du répertoire clé JWT: {}",
                jwt_key_dir.display()
            )
        })?;

    // kid dérivé du nom du fichier (sans extension) pour la traçabilité.
    // Exemple : "jwt.private.pem" → kid = "gradatum-v0".
    // Défaut fixe pour garantir la stabilité du kid entre boots.
    let jwt_kid = "gradatum-v0".to_string();

    let jwt_service = crate::jwt_key_boot::load_or_generate_jwt_key(
        &jwt_key_dir,
        jwt_kid,
        "gradatum".to_string(),
        cfg.auth.jwt_ttl_human_secs,
        cfg.auth.jwt_ttl_service_secs,
    )
    .context("chargement ou génération de la clé de signature JWT")?;

    // T1 P2.0c : SqliteQueue câblée sur storage.root/db/queue.sqlite.
    // T2 P2.0c : Vault câblé sur storage.root/vault/ (créé si absent).
    // AUTH-T6 : SqliteRevocationStore câblé si revocation_store == "sqlite".
    // T10 : SqliteIndex câblé sur cfg.storage.vault_index_path (SSOT — config.toml respectée).
    //       Le chemin est soit lu depuis [storage].vault_index_path dans server.toml,
    //       soit dérivé par StorageConfigRaw::from() via gradatum_core::paths::vault_index_path.
    //       JAMAIS inventé ici : toute dérivation manuelle root.join(...) est interdite.
    if cfg.storage.legacy_alias_used() {
        tracing::warn!(
            "[storage].db_path is deprecated, use vault_index_path. \
             Retrait prévu en alpha.7+1. Voir CHANGELOG v0.1.0-alpha.7."
        );
    }

    // P0 — Fail-fast : détection de divergence vault_index_path (Slice A1 round 2).
    //
    // `vault_index_path` peut être fourni explicitement dans le TOML.
    // Si un override diverge du chemin canonique, le server (search) lirait un fichier
    // différent du vault (écriture via registry.rs).
    // Le split-index pluggable n'est pas disponible avant v0.5.3 — refuser de démarrer.
    //
    // Invariant nominal : TOML sans override (ou override == canon) → pas de divergence,
    // pas de fail-fast. `vault_index_path_override_diverges = false` dans ce cas.
    if cfg.storage.vault_index_path_override_diverges() {
        let canonical = canon_vault_index_path(&cfg.storage.root);
        anyhow::bail!(
            "vault_index_path divergent du chemin canonique — split index non supporté avant v0.5.3.\n\
             \tConfiguré : {}\n\
             \tCanonique  : {}\n\
             Alignez [storage].vault_index_path avec le chemin canonique ou supprimez l'override.",
            cfg.storage.vault_index_path.display(),
            canonical.display()
        );
    }

    // SSOT : chemin queue via helper canonique (interdit root.join("db/queue.sqlite") direct).
    let queue_path = queue_db_path(&cfg.storage.root);
    let vault_path = cfg.storage.root.join("vault");
    // SSOT : vault_index_path LU depuis la config — jamais inventé ici.
    // StorageConfig::from(StorageConfigRaw) applique canon_vault_index_path() en défaut.
    let search_path = cfg.storage.vault_index_path.clone();

    // ANN-5 : enregistrement de l'extension sqlite-vec AVANT toute ouverture de connexion.
    //
    // sqlite3_auto_extension est globale (processus) : doit être appelé en amont de
    // SqliteIndex::open (qui ouvre la connexion et déclenche l'init vec0).
    // Si ann_backend != SqliteVec → skip pour économiser le linkage runtime vec0.
    // Si l'enregistrement échoue → WARN + basculement brute-force (pas de panique).
    let ann_ext_registered = if cfg.search.ann_backend == crate::config::AnnBackend::SqliteVec {
        match crate::vec_ext::register_sqlite_vec() {
            Ok(()) => {
                tracing::info!("sqlite-vec extension enregistrée (vec0 disponible)");
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "sqlite-vec extension non enregistrée — ann_backend forcé brute-force"
                );
                false
            }
        }
    } else {
        false
    };

    // AUTH-T6 : sélection du chemin SQLite de révocation selon config.
    // Fallback sur un chemin dérivé de storage.root si revocation_db_path absent.
    let revocation_db_path = cfg
        .auth
        .revocation_db_path
        .clone()
        .unwrap_or_else(|| cfg.storage.root.join("db/revocation.sqlite"));

    // Construire AppState avec la clé JWT persistante (fix P0).
    // AppState::new() (éphémère) n'est plus utilisé en prod — uniquement via with_jwt().
    let state = AppState::with_jwt(jwt_service)
        .with_queue_path(&queue_path)
        .await
        .context("queue init failed")?
        .with_vault_path(&vault_path)
        .await
        .context("vault init failed")?
        .with_search_path_ann(
            &search_path,
            if ann_ext_registered {
                cfg.search.ann_backend
            } else {
                crate::config::AnnBackend::BruteForce
            },
            cfg.search.ann_ef_search,
        )
        .await
        .context("search index init failed")?
        // F-17 — decay-trust depuis [scoring] config (défaut : activé, distilled=90j).
        .with_scoring(gradatum_search::TrustDecayConfig {
            enabled: cfg.scoring.trust_decay_enabled,
            half_life_days: cfg.scoring.half_life_days.clone(),
        })
        // F-35 Task 11 — câblage context config (budget, top_n, skills, embed_timeout).
        .with_context(cfg.context.clone())
        // v0.7.5 F-85 T5 — intervalles des tâches récurrentes disponibles dans
        // `GET /api/v1/system/scheduled` via task_interval_secs SSOT.
        .with_server_config(cfg.clone());

    // ANN-5 backfill au boot : remplir la table vec0 depuis les embeddings existants.
    //
    // Exécuté UNIQUEMENT si sqlite-vec a été enregistré avec succès (ann_ext_registered).
    // Non-fatal : une erreur dégrade vers brute-force sans interrompre le démarrage.
    // Chemin BruteForce (ann_ext_registered=false) → skippé byte-identique.
    if ann_ext_registered {
        match state.search.backfill_ann_index().await {
            Ok(n) => {
                tracing::info!(backfilled = n, "ANN vec0 backfill au boot");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ANN vec0 backfill au boot échoué — service continue"
                );
            }
        }
    }

    // AUTH-T6 : câbler le revocation store (sqlite ou memory selon config).
    let state = if cfg.auth.revocation_store == "sqlite" {
        // Créer le répertoire db/ si absent (le store crée le fichier, pas le dossier).
        if let Some(parent) = revocation_db_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("création du répertoire revocation db: {}", parent.display())
            })?;
        }
        let state = state
            .with_revocation_path(&revocation_db_path)
            .await
            .context("revocation store init failed")?;
        tracing::info!(
            path = %revocation_db_path.display(),
            "SqliteRevocationStore ready"
        );
        state
    } else {
        // revocation_store != "sqlite" → InMemoryRevocationStore déjà initialisé par AppState::new()
        // Le WARN DEV ONLY est émis par InMemoryRevocationStore::new() dans with_jwt().
        tracing::warn!(
            store = %cfg.auth.revocation_store,
            "revocation_store non-sqlite — InMemoryRevocationStore actif (DEV ONLY)"
        );
        state
    };

    // AUTH-T5 : câbler le store d'API keys (SqliteApiKeyStore en production).
    let api_keys_db_path = cfg
        .auth
        .api_keys_db_path
        .clone()
        .unwrap_or_else(|| cfg.storage.root.join("db/api_keys.sqlite"));
    // Créer le répertoire db/ si absent.
    if let Some(parent) = api_keys_db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("création du répertoire api_keys db: {}", parent.display()))?;
    }
    let state = state
        .with_api_keys_path(&api_keys_db_path)
        .await
        .context("api_keys store init failed")?;
    tracing::info!(
        path = %api_keys_db_path.display(),
        "SqliteApiKeyStore ready"
    );

    // E1 fix P2.0c-bis : charger le preset ACL depuis cfg.acl.preset_path.
    // Fail-closed : si le fichier est absent, DENY-ALL (warn loggé dans with_acl_preset_path).
    let state = state.with_acl_preset_path(&cfg.acl.preset_path);

    // P0-1 Phase 4.2bis : câblage QueueStore v81 pour endpoints F-16 (/api/v1/jobs/*).
    //
    // Utilise le même fichier SQLite que le worker (Option A code audit — cohérence single
    // source of truth, pool multi-reader WAL). Les migrations sont idempotentes : safe à
    // exécuter même si le worker a déjà appliqué le schéma.
    //
    // v0.2.0 Bronze : endpoints jobs ouverts sans auth conditionnelle (invariant réseau privé).
    // Auth granulaire F-45 multi-user JWT planifiée v1.0.0 Gold.
    // SSOT : chemin queue via helper canonique (même path que queue_path ci-dessus).
    let jobs_db_path = queue_db_path(&cfg.storage.root);
    if let Some(parent) = jobs_db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("création répertoire jobs db: {}", parent.display()))?;
    }
    let jobs_opts = SqliteConnectOptions::new()
        .filename(&jobs_db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // 5s busy_timeout : sans ce réglage, SQLite renvoie SQLITE_BUSY
        // immédiatement si le worker tient le verrou WAL lors d'un ack
        // (store.complete/fail). Avec busy_timeout, SQLite réessaie jusqu'à 5s
        // avant d'échouer — évite les jobs coincés en Running sur contention.
        .busy_timeout(std::time::Duration::from_secs(5));
    let jobs_pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(jobs_opts)
        .await
        .context("jobs pool init failed")?;
    run_migrations(&jobs_pool)
        .await
        .context("jobs migrations failed")?;
    let job_store = Arc::new(SqliteQueueStore::new(jobs_pool.clone()));
    let state = state.with_job_store(job_store, jobs_pool);
    tracing::info!(
        path = %jobs_db_path.display(),
        "SqliteQueueStore (F-16) ready"
    );

    // T8 P2.1.1 — wire embedder HTTP ou Noop selon cfg.embed.enabled.
    // HttpEmbedder est construit ici mais n'émet aucune requête au boot
    // (pas d'appel embed() avant Task 9+10 worker pipeline).
    let state = if cfg.embed.enabled {
        let embedder =
            gradatum_embed::HttpEmbedder::new(&cfg.embed.endpoint, &cfg.embed.model, cfg.embed.dim)
                .with_timeout(std::time::Duration::from_millis(cfg.embed.timeout_ms));
        tracing::info!(
            endpoint = %cfg.embed.endpoint,
            model = %cfg.embed.model,
            dim = cfg.embed.dim,
            timeout_ms = cfg.embed.timeout_ms,
            "embedder HTTP wired (Phase 2.1.1)"
        );
        // Probe de santé non-bloquant : si l'endpoint est injoignable au boot,
        // le serveur démarre quand même mais émet un WARN (recherche sémantique dégradée).
        probe_embed_health(&cfg.embed.endpoint).await;
        state.with_embedder(Arc::new(embedder))
    } else {
        tracing::warn!("embed.enabled=false — Noop embedder actif (aucun embedding généré)");
        state
    };

    // B1 tranche v0.3.0 — câblage EventLogStore sur la même DB que SqliteIndex.
    //
    // La migration 0006 (event_log table) est exécutée par `with_search_path`
    // (SqliteIndex::open applique toutes les migrations).
    // L'EventLogStore ouvre sa propre connexion WAL — safe multi-connexion SQLite.
    let state = state
        .with_event_log_path(&search_path)
        .await
        .context("EventLogStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "EventLogStore (B1) câblé sur index.db"
    );

    // session-log Tier 1 (council Art.15bis 2026-06-12) — câblage SessionTraceStore
    // sur la même DB que SqliteIndex. La migration 0015 (table session_trace) est
    // exécutée par `with_search_path`. Connexion WAL dédiée (safe multi-connexion).
    let state = state
        .with_session_trace_path(&search_path)
        .await
        .context("SessionTraceStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "SessionTraceStore (session-log Tier 1) câblé sur index.db"
    );

    // Surface proactive pré-calculée (F-46, Active Recall v0.7.1) — câblage ProactiveSurfaceStore
    // sur la même DB que SqliteIndex. La migration 0022 (table proactive_surface) est
    // exécutée par `with_search_path`. Connexion WAL dédiée (safe multi-connexion).
    let state = state
        .with_proactive_surface_path(&search_path)
        .await
        .context("ProactiveSurfaceStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "ProactiveSurfaceStore (surface proactive F-46) câblé sur index.db"
    );

    // Sessions + feedback proactif (F-46, Active Recall v0.7.1) — câblage ProactiveRecallStore
    // sur la même DB que SqliteIndex. La migration 0023 (tables proactive_recall_sessions +
    // proactive_recall_feedback) est exécutée par `with_search_path`. Connexion WAL dédiée.
    let state = state
        .with_proactive_recall_path(&search_path)
        .await
        .context("ProactiveRecallStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "ProactiveRecallStore (sessions+feedback recall proactif F-46) câblé sur index.db"
    );

    // Télémétrie usage read-paths (v0.5.3 #4) — ReadUsageCounterStore sur index.db.
    // La migration 0019 (table read_usage_counters) est exécutée par `with_search_path`.
    let state = state
        .with_read_usage_path(&search_path)
        .await
        .context("ReadUsageCounterStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "ReadUsageCounterStore (télémétrie read-paths) câblé sur index.db"
    );

    // Télémétrie usage — seed Prometheus au boot depuis la DB (P1-3 reviewer).
    //
    // INVARIANT P1-3 : seed_metrics_from_db DOIT être complété (await) AVANT le
    // tokio::spawn de la boucle flush ci-dessous. Sinon un premier flush pourrait
    // écrire en DB une donnée que le seed relit → double-count.
    {
        let read_usage_store_seed = state
            .read_usage
            .as_ref()
            .expect("ReadUsageCounterStore câblé — invariant post with_read_usage_path");
        if let Err(e) =
            crate::telemetry_flush::seed_metrics_from_db(read_usage_store_seed, &state.metrics)
                .await
        {
            tracing::warn!(
                error = %e,
                "seed_metrics_from_db échoué au boot — familles Prometheus non seedées (non fatal)"
            );
        }
    }

    // Seed des 7 tâches récurrentes au boot (v0.7.5 F-85).
    //
    // INSERT OR IGNORE → idempotent : un redémarrage ne remet pas run_count à zéro.
    // Les tâches non encore tickées apparaissent avec last_run_ms=None dans l'endpoint.
    // Doit être effectué AVANT les spawns pour garantir les entrées visibles dès le démarrage.
    {
        use crate::scheduled_tasks::ALL_SCHEDULED_TASKS;

        for task_name in ALL_SCHEDULED_TASKS {
            if let Err(e) = state.search.seed_scheduled_task(task_name).await {
                tracing::warn!(
                    error = %e,
                    task = task_name,
                    "seed_scheduled_task échoué au boot (non fatal)"
                );
            }
        }
        tracing::info!(
            count = ALL_SCHEDULED_TASKS.len(),
            "tâches récurrentes seedées au boot"
        );
    }

    // Tâche flush 60s : swap+reset des AtomicU64 (read-path + MCP) → UPSERT dans
    // read_usage_counters + fan-out Prometheus via route_metric.
    //
    // Design : AtomicU64 Relaxed dans AppState (coût hot-path ~0, aucun I/O handler),
    // flush toutes les 60s (granularité horaire, perte max = 1 fenêtre de 60s si crash).
    // Erreur flush → log WARN + reset quand même (ne bloque pas le server).
    // Self-contained — ne touche QUE read_usage_counters + familles Prometheus.
    //
    // P1-3 reviewer : seed_metrics_from_db est déjà complété ci-dessus AVANT ce spawn.
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        use tokio::time::{Duration, MissedTickBehavior, interval};

        let accumulators = state.read_usage_accumulators.clone();
        let mcp_counters = state.mcp_tool_counters.clone();
        let read_usage_store = state
            .read_usage
            .clone()
            .expect("ReadUsageCounterStore câblé — invariant post with_read_usage_path");
        let metrics = state.metrics.clone();
        let search_flush = state.search.clone();
        let interval_secs_flush = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_TELEMETRY_FLUSH,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;

            // Plancher à 60s : `interval(0)` panique.
            let mut ticker = interval(Duration::from_secs(interval_secs_flush));
            // Skip : at-most-one-missed-tick après un freeze/resume (maintenance).
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick consommé immédiatement (comportement tokio::interval) —
            // la première purge réelle arrive à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                // window_h = heure courante (floor division epoch_ms / 3_600_000).
                let window_h = now_ms / 3_600_000;

                let start = std::time::Instant::now();
                let flush_result = crate::telemetry_flush::flush_once(
                    &accumulators,
                    &mcp_counters,
                    &read_usage_store,
                    &metrics,
                    window_h,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match &flush_result {
                    Ok(()) => (TaskOutcome::Ok, None),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "telemetry-flush : flush_batch échoué — hits perdus pour cette fenêtre"
                        );
                        (TaskOutcome::Error, Some(e.to_string()))
                    }
                };
                if let Err(e) = search_flush
                    .record_task_run(
                        crate::scheduled_tasks::TASK_TELEMETRY_FLUSH,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run télémétrie-flush échoué (non fatal)");
                }
            }
        });
    }

    // Tâche d'échantillonnage timeseries (v0.7.5 Slice 2a) : capture la photo curée
    // du registry Prometheus toutes les 60s → metric_sample + purge paresseuse 14j.
    // Instrumentée via record_task_run → visible dans /api/v1/system/scheduled.
    // Self-contained, infaillible (warn-only).
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        use tokio::time::{Duration, MissedTickBehavior, interval};

        let metrics = state.metrics.clone();
        let search_ms = state.search.clone();
        let interval_secs = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_METRIC_SAMPLE,
            &cfg,
        );
        const RETENTION_MS: i64 = 14 * 86_400_000; // 14 jours

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;

            let mut ticker = interval(Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await; // premier tick consommé immédiatement

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let start = std::time::Instant::now();
                let samples = crate::curated_metrics::collect_curated_samples(&metrics);
                let result: Result<(), String> = async {
                    search_ms
                        .insert_metric_samples(now_ms, &samples)
                        .await
                        .map_err(|e| e.to_string())?;
                    search_ms
                        .purge_metric_samples(now_ms - RETENTION_MS)
                        .await
                        .map_err(|e| e.to_string())?;
                    Ok(())
                }
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;

                let (outcome, err_msg) = match &result {
                    Ok(()) => (TaskOutcome::Ok, None),
                    Err(e) => {
                        tracing::warn!(error = %e, "metric-sample : échantillonnage échoué");
                        (TaskOutcome::Error, Some(e.clone()))
                    }
                };
                if let Err(e) = search_ms
                    .record_task_run(
                        crate::scheduled_tasks::TASK_METRIC_SAMPLE,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run metric-sample échoué (non fatal)");
                }
            }
        });
    }

    // B1 — tâche de rétention tokio interval (purge par âge + cap max_rows).
    //
    // Alligné v81 l.5938 : TTL 30j event-log, PurgeMode::Ttl précurseur F-32 v0.5.0.
    // Cette tâche est self-contained — ne touche QUE event_log.
    // DELETE par âge + cap borné : zéro interaction avec notes/jobs.
    {
        let retention_cfg = cfg.event_log.clone();
        let event_log_store = state
            .event_log
            .clone()
            .expect("EventLogStore câblé — invariant post with_event_log_path");
        let metrics = state.metrics.clone();
        let search_elog = state.search.clone();
        let interval_secs_elog = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_PURGE_EVENT_LOG,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // P1 R1 : SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_elog));

            // P2 R2 : Skip évite N purges en rafale après un freeze/resume (ex : SIGSTOP,
            // freeze VM, débogueur). La sémantique "at-most-one-missed-tick" est correcte
            // pour une tâche de maintenance — aucune purge manquée n'est critique.
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            // La première purge réelle arrive à t=interval_secs (pas immédiatement).
            // Le premier `tick()` est consommé immédiatement (comportement tokio::interval),
            // le second tick() est le premier tick "réel" à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let retention_ms = (retention_cfg.retention_days as i64) * 86_400_000;
                let cutoff_ms = now_ms - retention_ms;

                let start = std::time::Instant::now();
                let purge_result = event_log_store
                    .purge(cutoff_ms, retention_cfg.max_rows)
                    .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match purge_result {
                    Ok(purged) => {
                        tracing::info!(
                            purged = purged,
                            retention_days = retention_cfg.retention_days,
                            max_rows = retention_cfg.max_rows,
                            "event_log rétention : purge terminée"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event_log purge échouée — non fatal");
                        let msg = e.to_string();
                        (TaskOutcome::Error, Some(msg))
                    }
                };
                if let Err(e) = search_elog
                    .record_task_run(
                        crate::scheduled_tasks::TASK_PURGE_EVENT_LOG,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run purge-event-log échoué (non fatal)");
                }

                // Mise à jour gauge Prometheus.
                match event_log_store.count().await {
                    Ok(count) => {
                        metrics.event_log_rows.set(count as i64);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event_log count échoué — gauge non mise à jour");
                    }
                }
            }
        });
    }

    // session-log Tier 1 — tâche de rétention 90j (purge par âge + cap max_rows).
    //
    // Self-contained — ne touche QUE session_trace (DELETE par âge + cap borné).
    // Copie structurelle de la tâche event_log ci-dessus (C-SA3 TTL 90j).
    {
        let retention_cfg = cfg.session_trace.clone();
        let session_trace_store = state
            .session_trace
            .clone()
            .expect("SessionTraceStore câblé — invariant post with_session_trace_path");
        let search_strace = state.search.clone();
        let interval_secs_strace = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_PURGE_SESSION_TRACE,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_strace));
            // Skip : at-most-one-missed-tick après un freeze/resume (maintenance).
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick consommé immédiatement (comportement tokio::interval) — la
            // première purge réelle arrive à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let retention_ms = (retention_cfg.retention_days as i64) * 86_400_000;
                let cutoff_ms = now_ms - retention_ms;

                let start = std::time::Instant::now();
                let purge_result = session_trace_store
                    .purge(cutoff_ms, retention_cfg.max_rows)
                    .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match purge_result {
                    Ok(purged) => {
                        tracing::info!(
                            purged = purged,
                            retention_days = retention_cfg.retention_days,
                            max_rows = retention_cfg.max_rows,
                            "session_trace rétention : purge terminée"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "session_trace purge échouée — non fatal");
                        let msg = e.to_string();
                        (TaskOutcome::Error, Some(msg))
                    }
                };
                if let Err(e) = search_strace
                    .record_task_run(
                        crate::scheduled_tasks::TASK_PURGE_SESSION_TRACE,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run purge-session-trace échoué (non fatal)");
                }
            }
        });
    }

    // read_usage_counters — tâche de rétention 90j (purge par window_h).
    //
    // Self-contained — ne touche QUE read_usage_counters.
    // Granularité horaire : cutoff_window_h = (now_ms - 90j_ms) / 3_600_000.
    // Aligné sur session_trace (même TTL 90j, même intervalle de purge configurable).
    {
        let retention_cfg = cfg.session_trace.clone(); // réutilise même TTL 90j
        let read_usage_store = state
            .read_usage
            .clone()
            .expect("ReadUsageCounterStore câblé — invariant post with_read_usage_path");
        let search_rusage = state.search.clone();
        let interval_secs_rusage = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_PURGE_READ_USAGE,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_rusage));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                // cutoff_window_h : première fenêtre à CONSERVER = heure courante - 90j.
                // Toutes les window_h < cutoff_window_h sont supprimées.
                let retention_ms = (retention_cfg.retention_days as i64) * 86_400_000;
                let cutoff_window_h = (now_ms - retention_ms) / 3_600_000;

                let start = std::time::Instant::now();
                let purge_result = read_usage_store.purge_before(cutoff_window_h).await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match purge_result {
                    Ok(purged) => {
                        if purged > 0 {
                            tracing::info!(
                                purged = purged,
                                retention_days = retention_cfg.retention_days,
                                cutoff_window_h = cutoff_window_h,
                                "read_usage_counters rétention : purge terminée"
                            );
                        }
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "read_usage_counters purge échouée — non fatal");
                        let msg = e.to_string();
                        (TaskOutcome::Error, Some(msg))
                    }
                };
                if let Err(e) = search_rusage
                    .record_task_run(
                        crate::scheduled_tasks::TASK_PURGE_READ_USAGE,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run purge-read-usage échoué (non fatal)");
                }
            }
        });
    }

    // Review auto-promote — promeut staging/pending-review âgés > N jours.
    // Miroir structurel de la tâche event_log (non-fatal, self-contained, MissedTickBehavior::Skip).
    {
        let promote_cfg = cfg.review_promote.clone();
        let index = state.search.clone();
        let vault = state.vault.clone();
        let metrics = state.metrics.clone();
        let interval_secs_promote = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_REVIEW_PROMOTE,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_promote));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick consommé immédiatement — première vraie promotion à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let start = std::time::Instant::now();
                let stats = crate::review_promote::promote_once(
                    &index,
                    &vault,
                    &metrics,
                    &promote_cfg,
                    now_ms,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = if stats.errors > 0 {
                    (
                        TaskOutcome::Error,
                        Some(format!("{} erreur(s) de promotion", stats.errors)),
                    )
                } else {
                    (TaskOutcome::Ok, None)
                };
                if let Err(e) = index
                    .record_task_run(
                        crate::scheduled_tasks::TASK_REVIEW_PROMOTE,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run review-promote échoué (non fatal)");
                }
            }
        });
    }

    // Active Recall — tâche interval in-process (F-46, v0.7.1, B').
    //
    // Gabarit miroir de `review_promote` (non-fatal, self-contained, MissedTickBehavior::Skip).
    // Calcule la surface proactive du tenant "main" à chaque tick via `proactive_refresh_once`.
    // Erreur loggée + skip — n'interrompt jamais la boucle.
    {
        let pr_cfg = cfg.proactive_recall.clone();
        let pr_state = state.clone();
        let interval_secs_pr = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_PROACTIVE_REFRESH,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_pr));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick consommé immédiatement (comportement tokio::interval) —
            // le premier refresh réel arrive à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let start = std::time::Instant::now();
                let refresh_result =
                    crate::proactive_recall::refresh::proactive_refresh_once(&pr_state, &pr_cfg)
                        .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match &refresh_result {
                    Ok(_) => (TaskOutcome::Ok, None),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "proactive_recall: refresh échoué — skip tick (non fatal)"
                        );
                        (TaskOutcome::Error, Some(e.to_string()))
                    }
                };
                if let Err(e) = pr_state
                    .search
                    .record_task_run(
                        crate::scheduled_tasks::TASK_PROACTIVE_REFRESH,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run proactive-refresh échoué (non fatal)");
                }
            }
        });
    }

    // Active Recall — tâche de rétention (purge sessions + feedback par âge + cap).
    //
    // Rend VRAI le commentaire de la migration 0023 ("Rétention automatique via
    // ProactiveRecallStore::purge") : sans ce câblage, les tables proactive_recall_*
    // croissent sans borne sur index.db. Gabarit miroir de session_trace (non-fatal,
    // self-contained, MissedTickBehavior::Skip). Réutilise cfg.session_trace (même
    // TTL + cap max_rows + intervalle de purge) — comme read_usage_counters ci-dessus.
    // Store optionnel (None possible en config dégradée) → on ne spawne rien (skip propre).
    // La tâche est tout de même seedée au boot (entrée avec last_run_ms=None dans l'endpoint).
    if let Some(proactive_recall_store) = state.proactive_recall.clone() {
        let retention_cfg = cfg.session_trace.clone();
        let search_arcpurge = state.search.clone();
        let interval_secs_arcpurge = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_ACTIVE_RECALL_PURGE,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            // SSOT interval via task_interval_secs (plancher 60s garanti).
            let mut ticker = interval(Duration::from_secs(interval_secs_arcpurge));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick consommé immédiatement — première purge réelle à t=interval_secs.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let retention_ms = (retention_cfg.retention_days as i64) * 86_400_000;
                let cutoff_ms = now_ms - retention_ms;
                // `session_trace.max_rows` est i64 ; `purge` attend usize. Conversion
                // sûre : une valeur négative/hors-borne → usize::MAX (cap désactivé).
                let max_rows = usize::try_from(retention_cfg.max_rows).unwrap_or(usize::MAX);

                let start = std::time::Instant::now();
                let purge_result = proactive_recall_store.purge(cutoff_ms, max_rows).await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match purge_result {
                    Ok(purged) => {
                        tracing::info!(
                            purged = purged,
                            retention_days = retention_cfg.retention_days,
                            max_rows = retention_cfg.max_rows,
                            "proactive_recall rétention : purge terminée"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "proactive_recall purge échouée — non fatal");
                        let msg = e.to_string();
                        (TaskOutcome::Error, Some(msg))
                    }
                };
                if let Err(e) = search_arcpurge
                    .record_task_run(
                        crate::scheduled_tasks::TASK_ACTIVE_RECALL_PURGE,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run active-recall-purge échoué (non fatal)");
                }
            }
        });
    }

    tracing::info!(queue_path = %queue_path.display(), "SqliteQueue ready");
    tracing::info!(vault_path = %vault_path.display(), "Vault ready");
    tracing::info!(search_path = %search_path.display(), "SqliteIndex (FTS5) ready");
    tracing::info!(
        enabled = cfg.ratelimit.enabled,
        per_minute = cfg.ratelimit.per_minute,
        burst = cfg.ratelimit.burst,
        exempt_localhost = cfg.ratelimit.exempt_localhost,
        "rate limiting V3"
    );
    // API interne (v0.5.3 Wave 2) — câbler le token si configuré.
    // Validation longueur + conversion String → SecretString ici (seul endroit de la chaîne).
    // La longueur minimale est publique-par-design (documentée dans config) — cf. validate_internal_token.
    let state = if let Some(ref raw_token) = cfg.internal_api.token {
        crate::config::validate_internal_token(raw_token).map_err(|e| anyhow::anyhow!("{e}"))?;
        let secret = secrecy::SecretString::from(raw_token.clone());
        state.with_internal_api_token(secret)
    } else {
        state
    };

    // Construire le service MCP en amont pour récupérer le CancellationToken.
    // Le token est annulé lors du shutdown pour arrêter proprement les sessions rmcp
    // (évite les tâches tokio orphelines qui retarderaient la sortie du processus).
    let (mcp_service, mcp_cancel) = api_v1::mcp::build_mcp_service(state.clone());
    let app = build_router(state.clone(), &cfg.ratelimit, &cfg.studio, mcp_service);

    // Native TLS termination (B-2): if [server.tls] is configured, load the cert/key
    // BEFORE binding, so a bad certificate aborts the boot fail-closed — never a silent
    // cleartext fallback. The cleartext path (no [server.tls]) is unchanged (LIVE).
    let tls_config = match cfg.server.tls.as_ref() {
        Some(tls) => Some(load_tls_config(tls).await?),
        None => None,
    };

    // Notify systemd READY (Type=notify dans gradatum-server.service).
    // Sans cet appel,
    // systemd attend indéfiniment le signal READY et passe le service en
    // état "activating" jusqu'au timeout (TimeoutStartSec=90s par défaut).
    // L'erreur est loggée en DEBUG uniquement — hors systemd (dev local,
    // tests, docker) sd_notify échoue silencieusement, ce qui est attendu.
    #[cfg(target_os = "linux")]
    let notify_ready = || {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
            tracing::debug!(error = %e, "sd_notify ready ignoré (exécution hors systemd)");
        }
    };

    // Spawn du listener métriques en tâche tokio parallèle (C7).
    let metrics_bind = cfg.server.metrics_bind;
    let app_metrics = state.metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = metrics::spawn_metrics_listener(metrics_bind, app_metrics).await {
            error!(error = %e, "metrics listener arrêté avec erreur");
        }
    });

    // API interne (v0.5.3 Wave 2) — spawn listener loopback :19092 si token configuré.
    if state.internal_api_token.is_some() {
        let internal_bind = cfg.internal_api.bind;
        let internal_router = internal::build_internal_router(state.clone());
        tokio::spawn(async move {
            if let Err(e) = internal::spawn_internal_listener(internal_bind, internal_router).await
            {
                error!(error = %e, "listener API interne arrêté avec erreur");
            }
        });
    }

    // V3 rate limiting : into_make_service_with_connect_info injecte ConnectInfo<SocketAddr>
    // dans les extensions — requis par PeerIpKeyExtractor + loopback_bypass.
    let make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();

    // Installer les handlers de signal UNE SEULE FOIS, avant le match tls_config,
    // pour éliminer la race : si SIGTERM arrive pendant la construction du router serve,
    // le signal doit déjà être capturé par l'OS (le noyau met en file).
    // Applicable aux deux paths (TLS et cleartext) — les Signal sont move'd dans les closures.
    let mut shutdown_sigterm =
        signal(SignalKind::terminate()).expect("installer SIGTERM — OS UNIX requis");
    let mut shutdown_sigint =
        signal(SignalKind::interrupt()).expect("installer SIGINT — OS UNIX requis");

    match tls_config {
        // --- HTTPS path: axum-server terminates TLS via rustls ---
        Some(rustls_config) => {
            info!(addr = %cfg.server.bind, "serveur en écoute (TLS natif)");
            #[cfg(target_os = "linux")]
            notify_ready();

            // axum-server drives graceful shutdown via a Handle (not with_graceful_shutdown).
            // On SIGTERM/SIGINT, signal a 30 s drain timeout for in-flight connections.
            // mcp_cancel est annulé simultanément pour arrêter les sessions rmcp internes.
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown_sigterm.recv() => info!("SIGTERM reçu, drain en cours (TLS)"),
                    _ = shutdown_sigint.recv() => info!("SIGINT reçu, drain en cours (TLS)"),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                info!("signal d'arrêt traité (TLS)");
                mcp_cancel.cancel();
                shutdown_handle.graceful_shutdown(Some(Duration::from_secs(30)));
            });

            axum_server::bind_rustls(cfg.server.bind, rustls_config)
                .handle(handle)
                .serve(make_service)
                .await
                .map_err(|e| {
                    error!(error = %e, "serveur TLS arrêté avec erreur");
                    anyhow::anyhow!("erreur axum-server TLS serve : {e}")
                })?;
        }
        // --- Cleartext path (LIVE, unchanged): loopback behind reverse proxy ---
        None => {
            let listener = tokio::net::TcpListener::bind(cfg.server.bind).await?;
            let actual_addr = listener
                .local_addr()
                .expect("obtenir l'adresse locale après bind — le listener est actif");
            info!(addr = %actual_addr, "serveur en écoute");

            // Émettre l'adresse bound sur stdout pour permettre aux tests (et aux scripts)
            // de connaître le port alloué dynamiquement (bind sur :0) et de confirmer
            // la readiness sans sleep arbitraire.
            // Format stable : "listening on <SocketAddr>\n"
            // Cette ligne est émise APRÈS le bind (port connu) et AVANT serve (readiness
            // réelle confirmée par le poll /health côté test).
            println!("listening on {actual_addr}");

            #[cfg(target_os = "linux")]
            notify_ready();

            // Shutdown graceful : utiliser les handlers installés avant ce match (anti-race).
            axum::serve(listener, make_service)
                .with_graceful_shutdown(async move {
                    tokio::select! {
                        _ = shutdown_sigterm.recv() => info!("SIGTERM reçu, drain en cours"),
                        _ = shutdown_sigint.recv() => info!("SIGINT reçu, drain en cours"),
                    }
                    // Drain minimal T1 : 50ms. Budget complet (30s) implémenté au niveau router.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    info!("signal d'arrêt traité");
                    mcp_cancel.cancel();
                })
                .await
                .map_err(|e| {
                    error!(error = %e, "serveur arrêté avec erreur");
                    anyhow::anyhow!("erreur axum serve : {e}")
                })?;
        }
    }
    info!("gradatum-server arrêté proprement");
    Ok(())
}

/// Loads native TLS material (PEM cert + key) for `axum-server`, fail-closed.
///
/// Installs the process-default rustls crypto provider (`aws_lc_rs`) on first call —
/// the `tls-rustls-no-provider` feature keeps provider selection explicit and avoids a
/// second (`ring`) provider being pulled in. A subsequent install attempt (already set,
/// e.g. by another component) is tolerated.
///
/// # Errors
/// Returns an error if the certificate or key cannot be read or parsed. The caller MUST
/// propagate this so the server refuses to boot rather than serving cleartext.
async fn load_tls_config(
    tls: &crate::config::TlsConfig,
) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig> {
    // Idempotent: ignore "already installed" — only the first install wins, which is fine
    // since both candidate providers in the graph use aws_lc_rs primitives.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert_path, &tls.key_path)
        .await
        .with_context(|| {
            format!(
                "chargement TLS fail-closed : impossible de charger cert={} / key={}",
                tls.cert_path.display(),
                tls.key_path.display()
            )
        })
}

/// Initialises the tracing subscriber according to the requested format.
///
/// - `"json"`: structured JSON output (production)
/// - any other value: human-readable pretty output (development)
fn init_tracing(format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::fmt().with_env_filter(filter);
    if format == "json" {
        registry.json().init();
    } else {
        registry.pretty().init();
    }
}

/// Builds the Axum router.
///
/// Mounts:
/// - `/health` — unauthenticated health endpoint
/// - `/api/v1` — MCP handlers behind `auth_middleware` (Ed25519 JWT)
/// - `/auth/exchange` — mounted before JWT middleware (api-key → JWT bootstrap)
/// - `WardenLayer` applied to `/api/v1` and `/auth/exchange`
/// - `/ui/*` — static studio bundle (`ServeDir`, no auth, strict CSP headers)
///
/// # Mount order (critical)
///
/// Layers applied to rate-limited routes (`api_v1` + `auth_exchange`):
/// 1. `WardenLayer` (outer) — real loopback bypass + CIDR filters + per-IP rate limit.
/// 2. `auth_middleware` (on `api_v1` only) — Ed25519 JWT verification.
///
/// Routes exempt from rate limiting: `/health`, `/metrics`, `/ui/*`.
/// These are merged via `Router::merge` without the warden layer.
///
/// A `merge` on a router without a layer does not inherit layers from the other router.
///
/// # Loopback bypass
///
/// [`WardenLayer`] calls `inner.call(req)` directly for loopback IPs
/// (when `bypass_loopback=true`) — the handler returns its real body.
/// No synthetic `Body::empty()` response is produced.
///
/// # Studio `/ui/*`
///
/// Served without authentication (LAN — the JS bundle is public; API calls carry
/// the bearer JWT). If `ui_dir` does not exist → clean 404, never a panic
/// (`tower-http` `ServeDir` handles absence gracefully).
/// Strict CSP injected via `tower_http::set_header::SetResponseHeaderLayer`.
fn build_router(
    state: AppState,
    rl: &crate::config::RateLimitConfig,
    studio_cfg: &crate::config::StudioConfig,
    mcp_service: api_v1::mcp::GradatumMcpService,
) -> axum::Router {
    use axum::{Router, middleware, routing::get};

    // Routeur soumis à l'auth middleware (api_v1 + MCP natif).
    //
    // `/mcp` est monté DANS le routeur `authed` de sorte que l'`auth_middleware`
    // s'exécute EN PREMIER et injecte `TrustContext` dans les extensions HTTP.
    // Le `StreamableHttpService` rmcp injecte ensuite les `http::request::Parts`
    // dans `RequestContext.extensions`, ce qui permet à `mcp.rs` de traverser :
    // extensions rmcp → `http::request::Parts` → `TrustContext`.
    //
    // Le `CancellationToken` lié au service est géré dans `main` (annulé lors du SIGTERM
    // pour arrêt propre des sessions rmcp — évite les tâches orphelines post-shutdown).
    // F-02 — `/mcp` porte une limite de body anti-DoS (512 KiB).
    //
    // `axum::extract::DefaultBodyLimit` est INEFFECTIF ici : c'est une extension lue par
    // l'extracteur `Body`/`Bytes` d'Axum, or rmcp `StreamableHttpService` lit le corps
    // lui-même au niveau `tower::Service` (jamais via l'extracteur). Vérifié empiriquement :
    // un body > limite renvoyait 422 (rejet rmcp), pas 413.
    //
    // `tower_http::limit::RequestBodyLimitLayer` enveloppe le `Body` au niveau service et
    // court-circuite en 413 si `Content-Length` dépasse la limite ; en l'absence de
    // `Content-Length` (transfer chunked), il borne la lecture à la limite et coupe au-delà —
    // dans les deux cas AVANT que rmcp ne consomme le corps, indépendamment de tout extracteur.
    // Preuve : test d'intégration `f02_body_au_dessus_limite_rejete_413`.
    let mcp_router = Router::new().route_service("/mcp", mcp_service).layer(
        tower_http::limit::RequestBodyLimitLayer::new(api_v1::mcp::MCP_BODY_LIMIT),
    );

    let authed = Router::new()
        .nest("/api/v1", api_v1::router())
        // Route fixe /mcp — définie AVANT toute route paramétrique.
        .merge(mcp_router)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth_middleware,
        ));

    // Sous-router /auth/exchange (sans auth JWT — émetteur du token).
    let auth_exchange = auth_routes::router();

    // Appliquer le rate limiting via WardenLayer sur authed + auth_exchange si activé.
    let (authed, auth_exchange) = match crate::middleware::build_warden_layer(rl) {
        Some(warden) => (authed.layer(warden.clone()), auth_exchange.layer(warden)),
        None => (authed, auth_exchange),
    };

    // Studio /ui/* — ServeDir sans auth + CSP/sécu + fallback SPA.
    // Construit via `studio::build_studio_router` (factorisé pour testabilité —
    // V2 sécu + fallback SPA #6 couverts par tests d'intégration).
    let studio_router =
        crate::studio::build_studio_router(std::path::Path::new(&studio_cfg.ui_dir));

    // Routeur non soumis au rate limiting : /health, /ui/* (monitoring + studio).
    let unauthed = Router::new()
        .route("/health", get(health::handler))
        .merge(auth_exchange)
        .merge(studio_router);

    // Fusion : les routes unauthed ne voient pas le layer JWT.
    authed.merge(unauthed).with_state(state)
}

/// Non-blocking TCP health probe for the embedding endpoint at startup.
///
/// Extracts `host:port` from the embedding endpoint URL and attempts a TCP connection
/// with a 2 s timeout. On failure: emits `tracing::warn!` — semantic search will be degraded.
/// Never panics; never prevents the server from starting.
async fn probe_embed_health(endpoint: &str) {
    // Extraire "host:port" depuis "http://host:port/path".
    let host_port = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|s| s.split('/').next());

    let host_port = match host_port {
        Some(hp) => hp,
        None => {
            tracing::warn!(
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
            tracing::warn!(
                host_port = %host_port,
                error = %e,
                "semantic search disabled — embed endpoint unreachable"
            );
        }
        Err(_timeout) => {
            tracing::warn!(
                host_port = %host_port,
                "semantic search disabled — embed endpoint unreachable (timeout 2s)"
            );
        }
    }
}
