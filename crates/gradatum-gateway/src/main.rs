//! gradatum-gateway — main binary.
//!
//! Starts an Axum server on the address configured in `[server] listen`.
//! Routes:
//! - `GET /health`               → status + providers
//! - `GET /metrics`              → Prometheus text-format export
//! - `GET /v1/models`            → list of configured aliases
//! - `POST /v1/chat/completions` → proxy to an OpenAI-compat backend
//! - `POST /v1/embeddings`       → embedding proxy (remote or local)
//! - `POST /v1/rerank`           → cross-encoder reranking
//!
//! Configuration (in priority order):
//! - `--config PATH` (CLI argument)
//! - `GATEWAY_CONFIG_PATH=/path/to/config.toml` (environment variable)
//! - `./gateway.toml` (local default)
//!
//! Security:
//! - Inbound bearer token configurable via the `GRADATUM_GATEWAY_BEARER` env var
//! - CORS origin allowlist configured in `[server] allowed_origins`
//! - Rate limit based on the real TCP socket IP address

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use gradatum_gateway::config::Config;
use gradatum_gateway::{build_router, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Resolve the configuration file path.
    let config_path = resolve_config_path();

    let config = Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("ERREUR configuration : {}", e);
        std::process::exit(1);
    });

    // Initialize structured JSON tracing (RUST_LOG env var takes precedence).
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

    // Background task: purge stale provider statuses every 5 minutes.
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

    // into_make_service_with_connect_info injects ConnectInfo<SocketAddr> into every request.
    // Required for per-socket-IP rate limiting and loopback bearer bypass.
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Resolves the configuration file path from available sources.
///
/// Priority order: `--config <path>` > `GATEWAY_CONFIG_PATH` env var > `./gateway.toml`.
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
