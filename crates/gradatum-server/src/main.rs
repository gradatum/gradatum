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
mod audit_job;
mod audit_jsonl;
mod auth_routes;
mod config;
mod context;
mod curated_metrics;
mod event_log_store;
mod health;
mod internal;
mod mcp_usage;
mod metrics;
mod middleware;
mod note_usage_store;
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
use gradatum_db_sqlite::{SqliteQueueStore, open_queue_db, run_migrations};

/// Chaîne rendue par `--version` : version sémantique **suivie du SHA du commit de build**.
///
/// Format stable, garanti extractible par script (`deploy-gradatum-local.sh`) :
/// `gradatum-server <semver> (build_sha <sha>)`
///
/// La valeur `<sha>` est celle injectée au compile-time par `build.rs`
/// (`cargo:rustc-env=BUILD_SHA`), identique au champ `build_sha` de `GET /health`.
/// Elle vaut `unknown` lorsque le SHA n'a pas pu être résolu au build (absence de
/// `.git`, build depuis un tarball) — le repli est porté par `build.rs`, qui
/// n'échoue jamais. `env!` est donc toujours résoluble ici : `build.rs` émet la
/// variable de façon inconditionnelle.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (build_sha ",
    env!("BUILD_SHA"),
    ")"
);

#[derive(Parser, Debug)]
#[command(version = VERSION, about = "gradatum-server HTTP/MCP facade")]
struct Cli {
    /// Path to the TOML configuration file (optional — defaults apply otherwise).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = ServerConfig::load(cli.config.as_deref())
        .map_err(|e| anyhow::anyhow!("config loading failed: {e}"))?;
    init_tracing(&cfg.log.format);

    // OpenDAL object backends (S3) : installe le fournisseur crypto puis le transport HTTP
    // au démarrage, dans cet ordre, AVANT toute écriture via la couche `Storage`. Hors du
    // chemin TLS (`load_tls_config`), qui n'est emprunté que si `[server.tls]` est configuré :
    // un déploiement loopback sans TLS n'y passe jamais, mais écrit quand même sur S3.
    // No-op si ce binaire est bâti sans backend objet.
    gradatum_storage::install_object_backend_defaults();

    // F-110 Phase 2 : validation fail-loud de la config salience (k_norm > 0).
    if let Err(e) = cfg.salience.validate() {
        anyhow::bail!("invalid config: {e}");
    }
    // F-111 : validation fail-loud de la config downgrade (bornes spec §3).
    if let Err(e) = cfg.downgrade.validate() {
        anyhow::bail!("invalid config: {e}");
    }
    // C3 (post-mortem L6) : garde deploy — chaque override salience per-vault doit être valide
    // (k_norm > 0) avant que la salience globale ne consulte la map `salience_per_vault`.
    if let Err(e) = cfg.validate_per_vault_salience() {
        anyhow::bail!("invalid config: {e}");
    }

    info!(
        bind = %cfg.server.bind,
        metrics_bind = %cfg.server.metrics_bind,
        version = env!("CARGO_PKG_VERSION"),
        "gradatum-server starting"
    );

    // C7 strict : le listener métriques doit être loopback — pas de TLS escape (contrairement à C3).
    if !cfg.server.metrics_bind.ip().is_loopback() {
        anyhow::bail!(
            "metrics_bind must be loopback (caveat C7): address rejected = {}",
            cfg.server.metrics_bind
        );
    }

    // AUTH-T6 : boot_guard_check caveat C2 — refuse memory store si bind est non-loopback.
    let bind_is_loopback = cfg.server.bind.ip().is_loopback();
    boot_guard_check(bind_is_loopback, cfg.auth.revocation_store.as_str())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // F-13 + fix P0 persistance clé JWT :
    // Load-or-generate la seed Ed25519 depuis le répertoire de config.
    // - Si jwt-signing-key.secret existe (perms ≤ 600) → charger.
    // - Si absent → générer + écrire atomiquement (tmp+chmod600+rename) + log WARN.
    // La clé n'est JAMAIS loggée — seul le chemin est tracé.
    //
    // SSOT : répertoire dérivé par `gradatum_core::paths::config_dir`, le helper
    // qu'utilise aussi `gradatum-admin token issue`. Une dérivation locale ici
    // (ancienne heuristique sur le parent de `auth.jwt_private_key_path`) pouvait
    // désigner un répertoire que la CLI ne trouvait pas : les jetons émis par
    // l'opérateur étaient alors signés d'une clé absente du serveur → 401.
    let jwt_key_dir = gradatum_core::paths::config_dir(&cfg.storage.root);

    // Créer le répertoire si absent (idempotent).
    tokio::fs::create_dir_all(&jwt_key_dir)
        .await
        .with_context(|| format!("creating JWT key directory: {}", jwt_key_dir.display()))?;
    // V2 : restreindre le répertoire à 0o700 (owner only) APRÈS create_dir_all.
    // Nécessaire même si write_atomic le refait : le répertoire existant depuis un boot
    // précédent pourrait avoir des permissions trop ouvertes (ex. 0o755 umask).
    tokio::fs::set_permissions(&jwt_key_dir, std::fs::Permissions::from_mode(0o700))
        .await
        .with_context(|| {
            format!(
                "chmod 0o700 on JWT key directory: {}",
                jwt_key_dir.display()
            )
        })?;

    // `kid` et audience sont portés par `gradatum_auth::key_store` — jamais
    // redéclarés ici : `JwtService::verify` rejette un `kid` divergent avant
    // toute crypto, signataire et vérificateur doivent partager le même littéral.
    let jwt_service = gradatum_auth::key_store::load_or_generate(
        &jwt_key_dir,
        cfg.auth.jwt_ttl_human_secs,
        cfg.auth.jwt_ttl_service_secs,
    )
    .context("loading or generating JWT signing key")?;

    // F-177 : la file legacy `jobs_v2` (SqliteQueue) est supprimée — le serveur ne
    // câble plus de queue legacy. La file LIVE `gradatum_jobs` est ouverte plus bas
    // via SqliteQueueStore (P0-1 Phase 4.2bis).
    // T2 P2.0c : Vault câblé sur storage.root/vault/ (créé si absent).
    // AUTH-T6 : SqliteRevocationStore câblé si revocation_store == "sqlite".
    // T10 : SqliteIndex câblé sur cfg.storage.vault_index_path (SSOT — config.toml respectée).
    //       Le chemin est soit lu depuis [storage].vault_index_path dans server.toml,
    //       soit dérivé par StorageConfigRaw::from() via gradatum_core::paths::vault_index_path.
    //       JAMAIS inventé ici : toute dérivation manuelle root.join(...) est interdite.
    if cfg.storage.legacy_alias_used() {
        tracing::warn!(
            "[storage].db_path is deprecated, use vault_index_path. \
             Removal planned in alpha.7+1. See CHANGELOG v0.1.0-alpha.7."
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
            "vault_index_path diverges from the canonical path — split index not supported before v0.5.3.\n\
             \tConfigured: {}\n\
             \tCanonical : {}\n\
             Align [storage].vault_index_path with the canonical path or remove the override.",
            cfg.storage.vault_index_path.display(),
            canonical.display()
        );
    }

    let vault_path = cfg.storage.root.join("vault");
    // F-100 P1-1 — piste d'audit durable : sous-répertoire `audit/` sous la racine storage.
    // Câblé au boot (fail-fast si création impossible) → `state.audit` = JsonlFileSink en prod,
    // condition dure du tombstone (delete.rs) réellement armée (jamais le no-op sink).
    let audit_dir = cfg.storage.root.join("audit");
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
                tracing::info!("sqlite-vec extension registered (vec0 available)");
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "sqlite-vec extension not registered — ann_backend forced to brute-force"
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
        .with_vault_path(&vault_path, gradatum_core::scope::VaultId::new("main"))
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
        // F-100 P1-1 — câblage du sink d'audit JSONL durable (remplace NoopAuditSink).
        // Créé au boot ; échec (permissions) = fail-fast, cohérent avec la précondition
        // dure « échec tombstone → abort delete » qui doit pouvoir se déclencher en prod.
        .with_audit_dir(&audit_dir)
        .await
        .context("audit sink init failed")?
        // F-17 — decay-trust depuis [scoring] config (défaut : activé, distilled=90j).
        .with_scoring(gradatum_search::TrustDecayConfig {
            enabled: cfg.scoring.trust_decay_enabled,
            half_life_days: cfg.scoring.half_life_days.clone(),
        })
        // F-110 Phase 2 — salience depuis [salience] config (défaut OFF ⇒ None ⇒ byte-identical).
        .with_salience(cfg.salience.resolve())
        // L6 — overrides salience per-vault (A6) pré-résolus au boot (défaut : map vide ⇒ tout
        // vault retombe sur le global ⇒ byte-identical). Consultés seulement à salience ON.
        .with_salience_per_vault(cfg.resolve_salience_per_vault())
        // F-35 Task 11 — câblage context config (budget, top_n, skills, embed_timeout).
        .with_context(cfg.context.clone())
        // v0.7.5 F-85 T5 — intervalles des tâches récurrentes disponibles dans
        // `GET /api/v1/system/scheduled` via task_interval_secs SSOT.
        .with_server_config(cfg.clone());

    // A7 (Task 4) — bootstrap des handles de vaults au boot. À flag `multi_tenant` OFF
    // (défaut LIVE) : no-op strict, le registre reste le singleton `{main}` (byte-identical).
    // À flag ON : enregistre un handle réel par vault actif (`list_active_vaults`).
    state
        .bootstrap_active_vaults()
        .await
        .context("vault bootstrap failed")?;

    // ANN-5 backfill au boot : remplir la table vec0 depuis les embeddings existants.
    //
    // Exécuté UNIQUEMENT si sqlite-vec a été enregistré avec succès (ann_ext_registered).
    // Non-fatal : une erreur dégrade vers brute-force sans interrompre le démarrage.
    // Chemin BruteForce (ann_ext_registered=false) → skippé byte-identique.
    if ann_ext_registered {
        match state.search.backfill_ann_index().await {
            Ok(n) => {
                tracing::info!(backfilled = n, "ANN vec0 backfill at boot");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ANN vec0 backfill at boot failed — service continues"
                );
            }
        }

        // F-100 1.1 — GC one-shot des vecteurs ANN orphelins (deploy = restart = boot).
        // C4-1e Groupe B Task 17 : GC scopé par partition — une passe par vault actif à
        // flag ON (`list_active_vaults`), sur `["main"]` seul à flag OFF (byte-identical
        // mono-vault). Idempotent, non-fatal : une erreur n'interrompt pas le démarrage.
        let gc_vaults: Vec<gradatum_core::scope::VaultId> = if cfg.multi_tenant.enabled {
            match state.search.list_active_vaults().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "ANN vec0 GC: list_active_vaults failed — GC skipped this boot"
                    );
                    Vec::new()
                }
            }
        } else {
            vec![gradatum_core::scope::VaultId::new("main")]
        };
        for vault_id in &gc_vaults {
            match state.search.gc_orphan_ann(vault_id).await {
                Ok(n) => {
                    tracing::info!(
                        vault_id = %vault_id,
                        orphans_removed = n,
                        "ANN vec0 GC orphans at boot"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        vault_id = %vault_id,
                        error = %e,
                        "ANN vec0 GC orphans at boot failed — service continues"
                    );
                }
            }
        }

        // Gate de santé ANN au boot — compare, par partition `(vault_id, embedder_id)`, les
        // paires éligibles de `note_embeddings` (source de vérité) aux lignes réellement
        // présentes dans `note_embeddings_ann` (index dérivé).
        //
        // Placé APRÈS le backfill ET le GC : le backfill doit avoir écrit, et une ligne
        // orpheline (note supprimée) gonflerait le comptage indexé.
        //
        // Fail-closed assumé, appliqué côté index (`SqliteIndex::ann_health_gate` détient le
        // flag) : un déficit — ou une mesure non concluante — coupe le chemin ANN et la
        // recherche sémantique repasse en brute-force exact. Le boot n'est JAMAIS interrompu :
        // la correction des résultats ne dépend pas de l'ANN, seule la latence en dépend.
        // Motif : `search_semantic` ne retombe en brute-force que sur `Err`, jamais sur
        // `Ok(vec![])` — une partition trouée rend l'axe sémantique silencieusement muet.
        //
        // Skippé byte-identique sur le chemin BruteForce : ce bloc vit dans
        // `if ann_ext_registered`, et le gate lui-même sort avant toute requête quand le flag
        // ANN est à `false` (double garde).
        match state.search.ann_health_gate().await {
            Ok(deficits) => {
                state
                    .metrics
                    .ann_deficit_partitions
                    .set(i64::try_from(deficits.len()).unwrap_or(i64::MAX));
                if deficits.is_empty() {
                    tracing::info!("ANN health gate at boot: every partition fully indexed");
                } else {
                    for deficit in &deficits {
                        tracing::error!(
                            vault_id = %deficit.vault_id,
                            embedder_id = %deficit.embedder_id,
                            eligible = deficit.eligible,
                            indexed = deficit.indexed,
                            missing = deficit.eligible.saturating_sub(deficit.indexed),
                            "ANN health gate at boot: incomplete partition — rows missing from note_embeddings_ann"
                        );
                    }
                    tracing::error!(
                        deficit_partitions = deficits.len(),
                        "ANN health gate at boot FAILED — ANN path disabled, semantic search \
                         served by exact brute-force until a restart rebuilds the index"
                    );
                }
            }
            Err(e) => {
                // Jauge laissée INCHANGÉE (pas de remise à `0`) : une mesure qui n'a pas pu
                // conclure ne doit pas se présenter comme « zéro déficit » — cohérent avec la
                // réconciliation disque du boot. Le chemin ANN est déjà fermé par le gate.
                tracing::error!(
                    error = %e,
                    "ANN health gate at boot could not conclude — ANN path disabled (a gate that \
                     did not run is not a pass)"
                );
            }
        }
    }

    // AUTH-T6 : câbler le revocation store. Le `match` est exhaustif par construction —
    // le champ est typé (`RevocationStoreKind`), donc aucune troisième valeur ne peut
    // atteindre ce point : une coquille est refusée au chargement de la configuration.
    let state = match cfg.auth.revocation_store {
        crate::config::RevocationStoreKind::Sqlite => {
            // Créer le répertoire db/ si absent (le store crée le fichier, pas le dossier).
            if let Some(parent) = revocation_db_path.parent() {
                tokio::fs::create_dir_all(parent).await.with_context(|| {
                    format!("creating revocation db directory: {}", parent.display())
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
        }
        crate::config::RevocationStoreKind::Memory => {
            // InMemoryRevocationStore déjà initialisé par AppState::new().
            // Le WARN DEV ONLY est émis par InMemoryRevocationStore::new() dans with_jwt().
            tracing::warn!(
                "revocation_store=memory — InMemoryRevocationStore active (DEV ONLY): \
                 revocations are lost on restart"
            );
            state
        }
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
            .with_context(|| format!("creating api_keys db directory: {}", parent.display()))?;
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

    // B6′b : intégrité référentielle `api_keys.owner` ↔ `consumer.identity`, mesurée ici
    // parce que c'est le premier point du boot où les DEUX moitiés de la relation sont
    // chargées (store api-keys câblé plus haut, preset ACL juste au-dessus).
    // Non bloquante par construction : signale, ne refuse jamais le démarrage.
    state.reconcile_key_owners().await;

    // P0-1 Phase 4.2bis : câblage QueueStore v81 pour endpoints F-16 (/api/v1/jobs/*).
    //
    // Utilise le même fichier SQLite que le worker (Option A code audit — cohérence single
    // source of truth, pool multi-reader WAL). Les migrations sont idempotentes : safe à
    // exécuter même si le worker a déjà appliqué le schéma.
    //
    // v0.2.0 Bronze : endpoints jobs ouverts sans auth conditionnelle (invariant réseau privé).
    // Auth granulaire F-45 multi-user JWT planifiée v1.0.0 Gold.
    // SSOT : chemin de la file LIVE via helper canonique (interdit root.join("db/queue.sqlite") direct).
    let jobs_db_path = queue_db_path(&cfg.storage.root);
    if let Some(parent) = jobs_db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating jobs db directory: {}", parent.display()))?;
    }
    // 5s busy_timeout : sans ce réglage, SQLite renvoie SQLITE_BUSY
    // immédiatement si le worker tient le verrou WAL lors d'un ack
    // (store.complete/fail). Avec busy_timeout, SQLite réessaie jusqu'à 5s
    // avant d'échouer — évite les jobs coincés en Running sur contention.
    let jobs_db = open_queue_db(&jobs_db_path)
        .await
        .context("jobs db init failed")?;
    run_migrations(&jobs_db)
        .await
        .context("jobs migrations failed")?;
    let job_store = Arc::new(SqliteQueueStore::new(jobs_db.clone()));
    let state = state.with_job_store(job_store, jobs_db);
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
        tracing::warn!("embed.enabled=false — Noop embedder active (no embedding generated)");
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
        "EventLogStore (B1) wired on index.db"
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
        "SessionTraceStore (session-log Tier 1) wired on index.db"
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
        "ProactiveSurfaceStore (proactive surface F-46) wired on index.db"
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
        "ProactiveRecallStore (proactive recall sessions+feedback F-46) wired on index.db"
    );

    // Télémétrie usage read-paths (v0.5.3 #4) — ReadUsageCounterStore sur index.db.
    // La migration 0019 (table read_usage_counters) est exécutée par `with_search_path`.
    let state = state
        .with_read_usage_path(&search_path)
        .await
        .context("ReadUsageCounterStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "ReadUsageCounterStore (read-paths telemetry) wired on index.db"
    );

    // Télémétrie usage PAR NOTE (F-110 Phase 1) — NoteUsageStore sur index.db.
    // La migration 0029 (table note_usage) est exécutée par `with_search_path`.
    let state = state
        .with_note_usage_path(&search_path)
        .await
        .context("NoteUsageStore init failed")?;
    tracing::info!(
        path = %search_path.display(),
        "NoteUsageStore (per-note salience telemetry F-110) wired on index.db"
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
            .expect("ReadUsageCounterStore wired — invariant post with_read_usage_path");
        if let Err(e) =
            crate::telemetry_flush::seed_metrics_from_db(read_usage_store_seed, &state.metrics)
                .await
        {
            tracing::warn!(
                error = %e,
                "seed_metrics_from_db failed at boot — Prometheus families not seeded (non fatal)"
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
                    "seed_scheduled_task failed at boot (non fatal)"
                );
            }
        }
        tracing::info!(
            count = ALL_SCHEDULED_TASKS.len(),
            "recurring tasks seeded at boot"
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
            .expect("ReadUsageCounterStore wired — invariant post with_read_usage_path");
        // F-110 : accumulateur + store per-note, flushés au même tick (second flush best-effort).
        let note_usage_accumulators = state.note_usage_accumulators.clone();
        let note_usage_store = state
            .note_usage
            .clone()
            .expect("NoteUsageStore wired — invariant post with_note_usage_path");
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

                // F-110 : second flush best-effort per-note — un échec n'impacte ni la
                // requête ni le flush read_usage/MCP (télémétrie, fenêtre en cours perdue).
                if let Err(e) = crate::telemetry_flush::flush_note_usage(
                    &note_usage_accumulators,
                    &note_usage_store,
                    &metrics,
                )
                .await
                {
                    tracing::warn!(
                        error = %e,
                        "telemetry-flush: note_usage flush failed — window lost"
                    );
                }

                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match &flush_result {
                    Ok(()) => (TaskOutcome::Ok, None),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "telemetry-flush: flush_batch failed — hits lost for this window"
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
                    tracing::warn!(error = %e, "record_task_run telemetry-flush failed (non fatal)");
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
                        tracing::warn!(error = %e, "metric-sample: sampling failed");
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
                    tracing::warn!(error = %e, "record_task_run metric-sample failed (non fatal)");
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
            .expect("EventLogStore wired — invariant post with_event_log_path");
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
                            "event_log retention: purge complete"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event_log purge failed — non fatal");
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
                    tracing::warn!(error = %e, "record_task_run purge-event-log failed (non fatal)");
                }

                // Mise à jour gauge Prometheus.
                match event_log_store.count().await {
                    Ok(count) => {
                        metrics.event_log_rows.set(count as i64);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "event_log count failed — gauge not updated");
                    }
                }
            }
        });
    }

    // F-100 incrément 1.6 — GC de rétention des archives (piloté par le registre).
    //
    // Tâche interval serveur : le filesystem du vault est mono-propriétaire (côté
    // serveur), donc la destruction physique des archives ne peut pas être déléguée
    // au worker. `Vault::run_archive_gc` sélectionne les archives échues via le
    // registre `archive_index`, détruit leurs fichiers et marque `gc_at` (la ligne
    // survit comme trace). Self-contained — ne touche QUE `.archive/` + registre.
    {
        let vault_gc = state.vault.clone();
        let gc_interval_secs = cfg.archive.gc_interval_secs.max(60);
        let gc_batch_limit = cfg.archive.gc_batch_limit as usize;

        tokio::spawn(async move {
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            let mut ticker = interval(Duration::from_secs(gc_interval_secs));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Premier tick immédiat consommé — la 1re passe réelle arrive à t=interval.
            ticker.tick().await;

            loop {
                ticker.tick().await;
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                match vault_gc.run_archive_gc(now_ms, gc_batch_limit).await {
                    Ok(0) => {}
                    Ok(destroyed) => {
                        tracing::info!(destroyed, "GC archives: retention — archives destroyed");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "GC archives: pass failed — non-fatal");
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
            .expect("SessionTraceStore wired — invariant post with_session_trace_path");
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
                            "session_trace retention: purge complete"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "session_trace purge failed — non fatal");
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
                    tracing::warn!(error = %e, "record_task_run purge-session-trace failed (non fatal)");
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
            .expect("ReadUsageCounterStore wired — invariant post with_read_usage_path");
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
                                "read_usage_counters retention: purge complete"
                            );
                        }
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "read_usage_counters purge failed — non fatal");
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
                    tracing::warn!(error = %e, "record_task_run purge-read-usage failed (non fatal)");
                }
            }
        });
    }

    // Review auto-promote — promeut staging/pending-review âgés > N jours.
    // Miroir structurel de la tâche event_log (non-fatal, self-contained, MissedTickBehavior::Skip).
    {
        // L6 : la config COMPLÈTE est capturée (plus seulement `[review_promote]`) pour que la
        // boucle per-vault ON de `promote_tick` puisse résoudre `review_promote_for(vault_id)`.
        // Le chemin OFF (`promote_once`) continue d'utiliser `server_cfg.review_promote` global.
        let promote_server_cfg = cfg.clone();
        let multi_tenant_enabled = cfg.multi_tenant.enabled;
        let index = state.search.clone();
        let vault = state.vault.clone();
        // A1 (caveat pré-flip) : registre de handles pour router chaque write ON vers le
        // Vault CIBLE (`resolve(&vault_id)`), plus le singleton `main`. À flag OFF le tick
        // délègue à `promote_once` (singleton `vault`) — le registre n'est pas consulté.
        let vaults = state.vaults.clone();
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
                let stats = crate::review_promote::promote_tick(
                    &index,
                    &vault,
                    &vaults,
                    &metrics,
                    &promote_server_cfg,
                    now_ms,
                    multi_tenant_enabled,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = if stats.errors > 0 {
                    (
                        TaskOutcome::Error,
                        Some(format!("{} promotion error(s)", stats.errors)),
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
                    tracing::warn!(error = %e, "record_task_run review-promote failed (non fatal)");
                }
            }
        });
    }

    // F-51 audit / dedup — passe rétrospective (rapport pur, jamais de mutation).
    // Miroir structurel de review_promote (non-fatal, self-contained, MissedTickBehavior::Skip).
    // Désactivée par défaut (cfg.audit.enabled = false).
    {
        let audit_cfg = cfg.audit.clone();
        // F-111 : capturer AVANT le spawn (la task ne voit pas `state`/AppState).
        let downgrade_cfg = cfg.downgrade.clone();
        let note_usage = state.note_usage.clone();
        let index = state.search.clone();
        let metrics = state.metrics.clone();
        let storage_root = cfg.storage.root.clone();
        // Backend de stockage du vault (fs local ou objet S3) — LA MÊME configuration que
        // le vault (`<vault_path>/.gradatum/config.toml`), lue une fois au boot comme lui.
        // La passe d'audit écrit ses rapports via cette couche, jamais en accès fichier
        // direct : sur un backend objet, les rapports suivent les notes au lieu de rester
        // silencieusement en local. Chargement déjà validé par `with_vault_path` ci-dessus.
        let storage_backend = gradatum_core::config::VaultConfig::load_from_root(&vault_path)
            .with_context(|| {
                format!(
                    "load vault storage config from {} (audit report writer)",
                    vault_path.display()
                )
            })?
            .storage;
        let interval_secs_audit = crate::scheduled_tasks::task_interval_secs(
            crate::scheduled_tasks::TASK_AUDIT_DEDUP,
            &cfg,
        );

        tokio::spawn(async move {
            use gradatum_core::scheduled_health::TaskOutcome;
            use std::time::{SystemTime, UNIX_EPOCH};
            use tokio::time::{Duration, MissedTickBehavior, interval};

            let mut ticker = interval(Duration::from_secs(interval_secs_audit));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            ticker.tick().await; // premier tick consommé immédiatement

            loop {
                ticker.tick().await;

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let start = std::time::Instant::now();
                // F-111 : downgrader concret (mute l'index directement, post-guards).
                let downgrader = crate::audit_job::IndexDowngrader(index.clone());
                let stats = crate::audit_job::audit_once(
                    &index,
                    &metrics,
                    &audit_cfg,
                    &downgrade_cfg,
                    note_usage.as_ref(),
                    Some(&downgrader),
                    &storage_backend,
                    &storage_root,
                    "main",
                    now_ms,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = if stats.errors > 0 {
                    (
                        TaskOutcome::Error,
                        Some(format!("{} audit error(s)", stats.errors)),
                    )
                } else {
                    (TaskOutcome::Ok, None)
                };
                if let Err(e) = index
                    .record_task_run(
                        crate::scheduled_tasks::TASK_AUDIT_DEDUP,
                        outcome,
                        duration_ms,
                        err_msg.as_deref(),
                        now_ms,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "record_task_run audit-dedup failed (non fatal)");
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
        // Flag-gate du tick (A5, caveat pré-flip) : à OFF la surface reste mono-`"main"`
        // (byte-identical) ; à ON la boucle itère les vaults actifs, chacun scopé.
        let pr_multi_tenant_enabled = cfg.multi_tenant.enabled;
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
                let refresh_result = crate::proactive_recall::refresh::proactive_refresh_tick(
                    &pr_state,
                    &pr_cfg,
                    pr_multi_tenant_enabled,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as i64;
                let (outcome, err_msg) = match &refresh_result {
                    Ok(_) => (TaskOutcome::Ok, None),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "proactive_recall: refresh failed — skip tick (non fatal)"
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
                    tracing::warn!(error = %e, "record_task_run proactive-refresh failed (non fatal)");
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
                            "proactive_recall retention: purge complete"
                        );
                        (TaskOutcome::Ok, None)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "proactive_recall purge failed — non fatal");
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
                    tracing::warn!(error = %e, "record_task_run active-recall-purge failed (non fatal)");
                }
            }
        });
    }

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

    // API admin (F-100 1.6) — token admin DISTINCT du worker (delete/restore/purge CLI).
    // Même validation de longueur ; fail-closed si absent (endpoints admin désactivés).
    let state = if let Some(ref raw_token) = cfg.internal_api.admin_token {
        crate::config::validate_internal_token(raw_token).map_err(|e| anyhow::anyhow!("{e}"))?;
        let secret = secrecy::SecretString::from(raw_token.clone());
        state.with_admin_api_token(secret)
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
            tracing::debug!(error = %e, "sd_notify ready ignored (running outside systemd)");
        }
    };

    // Spawn du listener métriques en tâche tokio parallèle (C7).
    let metrics_bind = cfg.server.metrics_bind;
    let app_metrics = state.metrics.clone();
    tokio::spawn(async move {
        if let Err(e) = metrics::spawn_metrics_listener(metrics_bind, app_metrics).await {
            error!(error = %e, "metrics listener stopped with error");
        }
    });

    // API interne (v0.5.3 Wave 2) — spawn listener loopback :19092 si un token (worker
    // OU admin F-100 1.6) est configuré. Chaque sous-routeur reste fail-closed sur SON
    // token : worker et admin sont indépendants sur le même listener.
    if state.internal_api_token.is_some() || state.admin_api_token.is_some() {
        let internal_bind = cfg.internal_api.bind;
        let internal_router = internal::build_internal_router(state.clone());
        tokio::spawn(async move {
            if let Err(e) = internal::spawn_internal_listener(internal_bind, internal_router).await
            {
                error!(error = %e, "internal API listener stopped with error");
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
        signal(SignalKind::terminate()).expect("install SIGTERM — UNIX OS required");
    let mut shutdown_sigint =
        signal(SignalKind::interrupt()).expect("install SIGINT — UNIX OS required");

    match tls_config {
        // --- HTTPS path: axum-server terminates TLS via rustls ---
        Some(rustls_config) => {
            info!(addr = %cfg.server.bind, "server listening (native TLS)");
            #[cfg(target_os = "linux")]
            notify_ready();

            // axum-server drives graceful shutdown via a Handle (not with_graceful_shutdown).
            // On SIGTERM/SIGINT, signal a 30 s drain timeout for in-flight connections.
            // mcp_cancel est annulé simultanément pour arrêter les sessions rmcp internes.
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = shutdown_sigterm.recv() => info!("SIGTERM received, draining (TLS)"),
                    _ = shutdown_sigint.recv() => info!("SIGINT received, draining (TLS)"),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                info!("shutdown signal handled (TLS)");
                mcp_cancel.cancel();
                shutdown_handle.graceful_shutdown(Some(Duration::from_secs(30)));
            });

            axum_server::bind_rustls(cfg.server.bind, rustls_config)
                .handle(handle)
                .serve(make_service)
                .await
                .map_err(|e| {
                    error!(error = %e, "TLS server stopped with error");
                    anyhow::anyhow!("axum-server TLS serve error: {e}")
                })?;
        }
        // --- Cleartext path (LIVE, unchanged): loopback behind reverse proxy ---
        None => {
            let listener = tokio::net::TcpListener::bind(cfg.server.bind).await?;
            let actual_addr = listener
                .local_addr()
                .expect("obtaining local address after bind — the listener is active");
            info!(addr = %actual_addr, "server listening");

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
                        _ = shutdown_sigterm.recv() => info!("SIGTERM received, draining"),
                        _ = shutdown_sigint.recv() => info!("SIGINT received, draining"),
                    }
                    // Drain minimal T1 : 50ms. Budget complet (30s) implémenté au niveau router.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    info!("shutdown signal handled");
                    mcp_cancel.cancel();
                })
                .await
                .map_err(|e| {
                    error!(error = %e, "server stopped with error");
                    anyhow::anyhow!("axum serve error: {e}")
                })?;
        }
    }
    info!("gradatum-server shut down cleanly");
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
                "TLS fail-closed loading: cannot load cert={} / key={}",
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
    //
    // Métriques HTTP : appliquées EN DERNIER, donc couche la PLUS EXTERNE, sur le routeur
    // complet — toutes les routes sont comptées (/health, /api/v1/*, /mcp, /ui/*, fallback
    // 404) y compris les 403/429 rendus par le WardenLayer et les 401 de l'auth middleware.
    //
    // `Router::layer` s'exécute APRÈS le routage (axum 0.8) : l'extension `MatchedPath` est
    // disponible dans le middleware, donc le label `path` est le MOTIF de route
    // (`/api/v1/vault/unforgot/{ulid}`) et jamais l'URL concrète — cardinalité bornée par la
    // table de routage. Voir `crate::middleware::http_metrics_middleware`.
    authed
        .merge(unauthed)
        .layer(middleware::from_fn_with_state(
            state.metrics.clone(),
            crate::middleware::http_metrics_middleware,
        ))
        .with_state(state)
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

// ─── Tests unitaires du wiring routeur ───────────────────────────────────────

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    /// Construit le routeur **de production** (`build_router`) avec un `AppState`
    /// de test (clé JWT éphémère, registres placeholder — aucune I/O disque).
    ///
    /// Rate limiting désactivé : le `WardenLayer` exige `ConnectInfo` dans les extensions,
    /// que `oneshot` n'injecte pas. Sans effet sur ce qui est prouvé ici — le layer
    /// métriques est la couche la PLUS EXTERNE, en amont du warden.
    fn production_app() -> (AppState, axum::Router) {
        let state = AppState::new();
        let (mcp_service, _cancel) = api_v1::mcp::build_mcp_service(state.clone());
        let rl = crate::config::RateLimitConfig {
            enabled: false,
            ..Default::default()
        };
        let studio = crate::config::StudioConfig {
            ui_dir: PathBuf::from("/nonexistent-ui-dir-for-tests"),
        };
        let app = build_router(state.clone(), &rl, &studio, mcp_service);
        (state, app)
    }

    /// Lignes d'échantillons (hors `#`) de la famille demandée dans le registry.
    fn series_lines(state: &AppState, family: &str) -> Vec<String> {
        let mut buf = String::new();
        prometheus_client::encoding::text::encode(&mut buf, &state.metrics.registry)
            .expect("encoding du registry ne doit pas échouer");
        buf.lines()
            .filter(|l| !l.starts_with('#') && l.starts_with(family))
            .map(str::to_owned)
            .collect()
    }

    /// Le layer métriques est réellement monté sur le routeur de production :
    /// une requête traversant l'app produit un échantillon observable.
    #[tokio::test]
    async fn production_router_records_http_requests() {
        let (state, app) = production_app();

        let req = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("request builder invariant");
        let resp = app
            .oneshot(req)
            .await
            .expect("le routeur ne doit pas paniquer");
        assert_eq!(resp.status(), StatusCode::OK, "/health répond 200");

        let lines = series_lines(&state, "gradatum_http_requests_total");
        assert!(
            !lines.is_empty(),
            "le layer métriques doit être monté sur build_router — 0 échantillon = non câblé"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"path="/health""#) && l.contains(r#"status="200""#)),
            "échantillon /health status=200 attendu, lignes = {lines:?}"
        );

        let durations = series_lines(&state, "gradatum_http_request_duration_seconds_count");
        assert!(
            durations.iter().any(|l| l.contains(r#"path="/health""#)),
            "l'histogramme de durée doit être alimenté aussi, lignes = {durations:?}"
        );
    }

    /// Sur une route paramétrique RÉELLE de la table de production, le label `path` est le
    /// motif — l'ULID concret n'apparaît nulle part (garde-fou cardinalité).
    #[tokio::test]
    async fn production_router_labels_parametric_route_with_its_pattern() {
        const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let (state, app) = production_app();

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/vault/unforgot/{ULID}"))
            .body(Body::empty())
            .expect("request builder invariant");
        let resp = app
            .oneshot(req)
            .await
            .expect("le routeur ne doit pas paniquer");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "sans bearer, le handler rend 401"
        );

        let lines = series_lines(&state, "gradatum_http_requests_total");
        assert!(
            lines.iter().all(|l| !l.contains(ULID)),
            "aucune série ne doit contenir l'ULID concret, lignes = {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"path="/api/v1/vault/unforgot/{ulid}""#)
                    && l.contains(r#"status="401""#)),
            "le motif de route doit être le label, lignes = {lines:?}"
        );
    }
}
