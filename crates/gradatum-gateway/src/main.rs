//! gradatum-gateway — main binary.
//!
//! Starts an Axum server on the address configured in `[server] listen`.
//! Routes:
//! - `GET  /health`                     → status + providers
//! - `GET  /metrics`                    → Prometheus text-format export
//! - `GET  /v1/models`                  → list of configured aliases
//! - `POST /v1/chat/completions`        → proxy to an OpenAI-compat backend
//! - `POST /v1/embeddings`              → embedding proxy (remote or local)
//! - `POST /v1/messages`                → Anthropic Messages API, JSON or SSE
//! - `POST /v1/messages/count_tokens`   → Anthropic token-count estimate
//! - `POST /v1/rerank`                  → cross-encoder reranking
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
use gradatum_gateway::{AppState, build_router};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// String rendered by `--version`: the binary name, semantic version, and the build
/// commit SHA.
///
/// Format stable, guaranteed to stay script-extractable:
/// `gradatum-gateway <semver> (build_sha <sha>)`
///
/// `<sha>` is injected at compile time by `build.rs` (`cargo:rustc-env=BUILD_SHA`)
/// and reads `unknown` when the SHA could not be resolved at build time — no `.git`
/// directory or a tarball build — a fallback carried by `build.rs`, which never fails.
/// `env!` is therefore always resolvable here, since `build.rs` emits the variable
/// unconditionally.
const VERSION: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " ",
    env!("CARGO_PKG_VERSION"),
    " (build_sha ",
    env!("BUILD_SHA"),
    ")"
);

/// Help text rendered by `--help` / `-h`, without loading any configuration.
const HELP: &str = concat!(
    env!("CARGO_PKG_NAME"),
    " — LLM proxy gateway (multi-provider routing, reranking, engine supervision)\n\n",
    "Usage: ",
    env!("CARGO_PKG_NAME"),
    " [OPTIONS]\n\n",
    "Options:\n",
    "      --config <PATH>  Path to the TOML configuration file\n",
    "  -h, --help           Print help\n",
    "  -V, --version        Print version\n"
);

/// Handles `--version`/`-V` and `--help`/`-h` before any configuration is touched.
///
/// The minimal executable contract: these must answer from any directory, with no
/// config file in scope. Config resolution happens only when no early flag is found.
///
/// The value following `--config` is skipped so `--config --version` cannot be
/// mistaken for the version flag.
fn handle_early_flags() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => i += 2,
            "--version" | "-V" => {
                println!("{VERSION}");
                return true;
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return true;
            }
            _ => i += 1,
        }
    }
    false
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `--version`/`-V` and `--help`/`-h` must answer without any configuration file,
    // from any directory — handled before config resolution (see `handle_early_flags`).
    if handle_early_flags() {
        std::process::exit(0);
    }

    // Resolve the configuration file path.
    let config_path = resolve_config_path();

    let config = Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("CONFIGURATION ERROR: {}", e);
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
                    Ok(n) => tracing::info!(purged = n, "stale provider statuses purged"),
                    Err(e) => tracing::warn!(error = %e, "provider status purge failed"),
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
    if let Some(pos) = args.iter().position(|a| a == "--config")
        && let Some(path) = args.get(pos + 1)
    {
        return path.clone();
    }

    if let Ok(path) = std::env::var("GATEWAY_CONFIG_PATH") {
        return path;
    }

    "./gateway.toml".to_string()
}
