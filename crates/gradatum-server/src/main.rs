//! gradatum-server — façade HTTP/MCP :19090.
//!
//! Décisions :
//! - C3 bind/TLS fail-closed (config.rs)
//! - R-A1 JWT TTL par scope (tâches auth T3-T5)
//! - C7 métriques canal latéral loopback (metrics.rs T11)
//! - T9 : vraie vérification JWT Ed25519 via middleware::auth_middleware.
//! - Arrêt gracieux SIGTERM 30s drain.
//! - P0-1 Phase 4.2bis : with_job_store() câblé (SqliteQueueStore sur queue.sqlite).
//! - P1-4 Phase 4.2bis : require_jwt_jobs_endpoint retiré (flag fantôme inopérant).

mod api_v1;
mod audit_jsonl;
mod auth_routes;
mod config;
mod event_log_store;
mod health;
mod jwt_key_boot;
mod metrics;
mod middleware;
mod state;
mod stubs;

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

// Import du boot_guard_check (caveat C2 — interdit memory store en bind non-loopback).
use gradatum_auth::revocation::boot_guard_check;
// P0-1 Phase 4.2bis : QueueStore v81 pour endpoints F-16.
use gradatum_db_sqlite::{run_migrations, SqliteQueueStore};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

#[derive(Parser, Debug)]
#[command(version, about = "gradatum-server façade HTTP/MCP")]
struct Cli {
    /// Chemin vers le fichier de configuration TOML (optionnel — les défauts s'appliquent sinon).
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

    // T1 P2.0c : SqliteQueue câblée sur storage.root/queue.db.
    // T2 P2.0c : Vault câblé sur storage.root/vault/ (créé si absent).
    // AUTH-T6 : SqliteRevocationStore câblé si revocation_store == "sqlite".
    // T10 : SqliteIndex câblé sur vault/.gradatum/index.db — index commun avec le vault
    //       (le worker écrit dans cet index via upsert_note dans Vault::write_note).
    //       cfg.storage.vault_index_path est l'alias de lisibilité (RT11 alpha.7) ;
    //       search_path est dérivé de storage.root directement (layout vault).
    if cfg.storage.legacy_alias_used() {
        tracing::warn!(
            "[storage].db_path is deprecated, use vault_index_path. \
             Retrait prévu en alpha.7+1. Voir CHANGELOG v0.1.0-alpha.7."
        );
    }
    let queue_path = cfg.storage.root.join("db/queue.sqlite");
    let vault_path = cfg.storage.root.join("vault");
    let search_path = cfg
        .storage
        .root
        .join("vault")
        .join(".gradatum")
        .join("index.db");

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
        .context("search index init failed")?;

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
    let jobs_db_path = cfg.storage.root.join("db/queue.sqlite");
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
    let app = build_router(state.clone(), &cfg.ratelimit);

    let listener = tokio::net::TcpListener::bind(cfg.server.bind).await?;
    let actual_addr = listener
        .local_addr()
        .expect("obtenir l'adresse locale après bind — le listener est actif");
    info!(addr = %actual_addr, "serveur en écoute");

    // Notify systemd READY (Type=notify dans gradatum-server.service).
    // Sans cet appel,
    // systemd attend indéfiniment le signal READY et passe le service en
    // état "activating" jusqu'au timeout (TimeoutStartSec=90s par défaut).
    // L'erreur est loggée en DEBUG uniquement — hors systemd (dev local,
    // tests, docker) sd_notify échoue silencieusement, ce qui est attendu.
    #[cfg(target_os = "linux")]
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify ready ignoré (exécution hors systemd)");
    }

    // Spawn du listener métriques en tâche tokio parallèle (C7).
    let metrics_bind = cfg.server.metrics_bind;
    let app_metrics = state.metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = metrics::spawn_metrics_listener(metrics_bind, app_metrics).await {
            error!(error = %e, "metrics listener arrêté avec erreur");
        }
    });

    let shutdown = shutdown_signal();

    // V3 rate limiting : into_make_service_with_connect_info injecte ConnectInfo<SocketAddr>
    // dans les extensions — requis par PeerIpKeyExtractor + loopback_bypass.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| {
        error!(error = %e, "serveur arrêté avec erreur");
        anyhow::anyhow!("erreur axum serve : {e}")
    })?;
    info!("gradatum-server arrêté proprement");
    Ok(())
}

/// Initialise le subscriber tracing selon le format demandé.
///
/// - `"json"` : sortie JSON structurée (prod)
/// - tout autre valeur : sortie pretty lisible (dev)
fn init_tracing(format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::fmt().with_env_filter(filter);
    if format == "json" {
        registry.json().init();
    } else {
        registry.pretty().init();
    }
}

/// Construit le routeur Axum.
///
/// T1 : /health stub.
/// T8 : /api/v1 avec les 10 handlers MCP read.
/// T9 : middleware::auth_middleware câble la vraie vérification JWT Ed25519.
/// AUTH-T5 : /auth/exchange monté AVANT le middleware JWT (Path 2 bootstrap).
/// W1 : WardenLayer (gradatum-warden) sur /api/v1 + /auth/exchange.
///
/// # Ordre de montage (critique)
///
/// Couches appliquées sur les routes rate-limitées (`api_v1` + `auth_exchange`) :
/// 1. `WardenLayer` (outer) — bypass loopback réel + filtres IP CIDR + rate limit per-IP.
/// 2. `auth_middleware` (sur api_v1 seulement) — vérification JWT Ed25519.
///
/// Routes exemptes de tout rate limiting : `/health`, `/metrics`.
/// Ces routes sont fusionnées via `Router::merge` sans layer warden.
///
/// `merge` sur un Router sans layer n'hérite pas du layer de l'autre router.
///
/// # Bypass loopback
///
/// [`WardenLayer`] appelle `inner.call(req)` directement pour les IPs loopback
/// (quand `bypass_loopback=true`) — le handler retourne son body réel.
/// Aucune réponse synthétique `Body::empty()` n'est produite.
fn build_router(state: AppState, rl: &crate::config::RateLimitConfig) -> axum::Router {
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

    // Routeur non soumis au rate limiting : /health (monitoring agent + smoke).
    let unauthed = Router::new()
        .route("/health", get(health::handler))
        .merge(auth_exchange);

    // Fusion : les routes unauthed ne voient pas le layer JWT.
    authed.merge(unauthed).with_state(state)
}

/// Attend SIGTERM ou SIGINT, puis attend 50ms de drain minimal.
///
/// Budget de drain total par spec : 30s.
/// T1 livre un drain symbolique (50ms) — le drain complet est implémenté avec le routeur en T10.
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
