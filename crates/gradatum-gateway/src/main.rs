//! gradatum-gateway — binaire principal.
//!
//! Démarre un serveur Axum sur le port configuré (default: 127.0.0.1:8436).
//! Routes :
//! - `GET /health`               → status + providers
//! - `GET /metrics`              → export Prometheus text format
//! - `GET /v1/models`            → liste des aliases configurés
//! - `POST /v1/chat/completions` → proxy vers backend OpenAI-compat
//! - `POST /v1/embeddings`       → proxy embedding (remote ou local)
//! - `POST /v1/rerank`           → cross-encoder reranking (F-08)
//!
//! Configuration :
//! - `--config PATH` (argument CLI)
//! - `GATEWAY_CONFIG_PATH=/chemin/config.toml` (variable d'environnement)
//! - `./gateway.toml` (défaut local)
//!
//! Port : `:8436` (distinct de llm-free-gateway-v2 :8435 pour coexistence migration).
//!
//! Sécurité :
//! - Bearer token inbound configurable via `GRADATUM_GATEWAY_BEARER` env var
//! - CORS whitelist configurée dans `[server] allowed_origins`
//! - Rate limit par IP socket TCP réelle (F-MAJ-3)

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use gradatum_gateway::config::Config;
use gradatum_gateway::{build_router, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Résolution du chemin de config.
    let config_path = resolve_config_path();

    let config = Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("ERREUR configuration : {}", e);
        std::process::exit(1);
    });

    // Init tracing JSON structuré (env RUST_LOG prime si présent).
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(false),
        )
        .init();

    let listen_addr = config.server.listen.clone();

    let registry_path = config
        .server
        .registry_db
        .as_deref()
        .map(|p| Path::new(p).to_owned());

    let state = AppState::new(config, registry_path.as_deref());

    // Background task : purge TTL des statuts providers périmés (toutes les 5 min).
    {
        let registry = state.registry.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                match registry
                    .purge_stale_statuses(Duration::from_secs(3600))
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(purged = n, "statuts providers périmés purgés"),
                    Err(e) => tracing::warn!(error = %e, "échec purge statuts providers"),
                }
            }
        });
    }

    let router = build_router(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    tracing::info!(
        addr = %listen_addr,
        version = env!("CARGO_PKG_VERSION"),
        "gradatum-gateway listening"
    );

    // into_make_service_with_connect_info injecte ConnectInfo<SocketAddr> dans chaque requête.
    // Requis pour F-MAJ-3 (rate limit par IP socket) et bypass loopback bearer auth.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Résout le chemin de config depuis les sources disponibles.
///
/// Priorité : `--config <path>` > `GATEWAY_CONFIG_PATH` env > `./gateway.toml`.
fn resolve_config_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--config") {
        if let Some(path) = args.get(pos + 1) {
            return path.clone();
        }
    }

    if let Ok(path) = std::env::var("GATEWAY_CONFIG_PATH") {
        return path;
    }

    "./gateway.toml".to_string()
}
