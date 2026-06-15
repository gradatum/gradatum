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
mod event_log_store;
mod health;
mod jwt_key_boot;
mod metrics;
mod middleware;
mod session_trace_store;
mod state;
mod stubs;
mod studio;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use crate::config::ServerConfig;
use crate::state::AppState;
use gradatum_core::paths::{queue_db_path, vault_index_path as canon_vault_index_path};

// Import du boot_guard_check (caveat C2 — interdit memory store en bind non-loopback).
use gradatum_auth::revocation::boot_guard_check;
// P0-1 Phase 4.2bis : QueueStore v81 pour endpoints F-16.
use gradatum_db_sqlite::{run_migrations, SqliteQueueStore};
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
        .with_search_path(&search_path)
        .await
        .context("search index init failed")?
        // F-17 — decay-trust depuis [scoring] config (défaut : activé, distilled=90j).
        .with_scoring(gradatum_search::TrustDecayConfig {
            enabled: cfg.scoring.trust_decay_enabled,
            half_life_days: cfg.scoring.half_life_days.clone(),
        });

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

        tokio::spawn(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{interval, Duration, MissedTickBehavior};

            // P1 R1 : `interval(Duration::from_secs(0))` panique — plancher à 60s.
            // La config documente que `purge_interval_secs` doit être ≥ 60.
            let interval_secs = retention_cfg.purge_interval_secs.max(60);
            let mut ticker = interval(Duration::from_secs(interval_secs));

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

                match event_log_store
                    .purge(cutoff_ms, retention_cfg.max_rows)
                    .await
                {
                    Ok(purged) => {
                        tracing::info!(
                            purged = purged,
                            retention_days = retention_cfg.retention_days,
                            max_rows = retention_cfg.max_rows,
                            "event_log rétention : purge terminée"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event_log purge échouée — non fatal");
                    }
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

        tokio::spawn(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{interval, Duration, MissedTickBehavior};

            // Plancher à 60s : `interval(Duration::from_secs(0))` panique.
            let interval_secs = retention_cfg.purge_interval_secs.max(60);
            let mut ticker = interval(Duration::from_secs(interval_secs));
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

                match session_trace_store
                    .purge(cutoff_ms, retention_cfg.max_rows)
                    .await
                {
                    Ok(purged) => {
                        tracing::info!(
                            purged = purged,
                            retention_days = retention_cfg.retention_days,
                            max_rows = retention_cfg.max_rows,
                            "session_trace rétention : purge terminée"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "session_trace purge échouée — non fatal");
                    }
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
    let app = build_router(state.clone(), &cfg.ratelimit, &cfg.studio);

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

    // V3 rate limiting : into_make_service_with_connect_info injecte ConnectInfo<SocketAddr>
    // dans les extensions — requis par PeerIpKeyExtractor + loopback_bypass.
    let make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();

    match tls_config {
        // --- HTTPS path: axum-server terminates TLS via rustls ---
        Some(rustls_config) => {
            info!(addr = %cfg.server.bind, "serveur en écoute (TLS natif)");
            #[cfg(target_os = "linux")]
            notify_ready();

            // axum-server drives graceful shutdown via a Handle (not with_graceful_shutdown).
            // On SIGTERM/SIGINT, signal a 30 s drain timeout for in-flight connections.
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
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
            #[cfg(target_os = "linux")]
            notify_ready();

            axum::serve(listener, make_service)
                .with_graceful_shutdown(shutdown_signal())
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
) -> axum::Router {
    use axum::{middleware, routing::get, Router};

    // Routeur soumis à l'auth middleware (api_v1).
    let authed =
        Router::new()
            .nest("/api/v1", api_v1::router())
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

/// Waits for SIGTERM or SIGINT, then waits 50 ms for minimal drain.
///
/// Total drain budget: 30 s.
/// The 50 ms here is a minimal symbolic drain; full 30 s drain is implemented at the router level.
async fn shutdown_signal() {
    let mut sigterm =
        signal(SignalKind::terminate()).expect("installer le handler SIGTERM — OS UNIX requis");
    let mut sigint =
        signal(SignalKind::interrupt()).expect("installer le handler SIGINT — OS UNIX requis");
    tokio::select! {
        _ = sigterm.recv() => info!("SIGTERM reçu, drain en cours"),
        _ = sigint.recv() => info!("SIGINT reçu, drain en cours"),
    }
    // Drain minimal T1 : 50ms. Budget complet (30s) implémenté en T10.
    tokio::time::sleep(Duration::from_millis(50)).await;
    info!("signal d'arrêt traité");
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
