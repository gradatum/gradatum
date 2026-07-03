//! Public types for `gradatum-gateway` — entry point for integration tests.
//!
//! Features:
//! - Inline vendoring of shared LLM commons → `commons/` module
//! - Handler `POST /v1/rerank` via `Arc<dyn Reranker>`
//! - CORS: configured origin allowlist (replaces `CorsLayer::permissive()`)
//! - Tools: hard cap per request
//! - Rate limit: per real TCP socket IP address (`ConnectInfo`)
//! - Timeout: real `elapsed_secs` in `LlmError::Timeout`
//! - SmartRouter: alias default parameters + `AgentAware` overrides
//! - VaultAware hook: `QaEvent` fire-and-forget

pub mod anthropic;
pub mod auth;
pub mod commons;
pub mod config;
pub mod cors;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod provider_pool;
pub mod providers;
pub mod rate_limit;
pub mod registry;
pub mod slot_passthrough;
pub mod smart_router;
pub mod token_counter;
pub mod vault_aware;

use std::path::Path;
use std::sync::Arc;

use config::Config;
use gradatum_embed::Embedder;
use gradatum_search::reranker::Reranker;
use metrics::Metrics;
use provider_pool::ProviderPool;
use rate_limit::RateLimiter;
use registry::Registry;
use secrecy::SecretString;
use vault_aware::VaultAwareSender;

/// Canonical set of routes declared in `build_router`.
///
/// Single source of truth for the metrics allowlist (`known_routes` in `Metrics`).
/// Both `AppState::new` (prod) and `AppState::for_test` (tests) consume this constant,
/// preventing divergence between the two code paths.
///
/// Update this list whenever a route is added or removed in `build_router`.
pub const KNOWN_ROUTES: [&str; 8] = [
    "/v1/chat/completions",
    "/v1/embeddings",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/rerank",
    "/v1/models",
    "/health",
    "/metrics",
];

/// Shared state injected into handlers via Axum's `State<AppState>`.
///
/// Optional extensions:
/// - `embedder`          : `Embedder` trait from gradatum-embed (`None` = local embedding disabled)
/// - `local_embed_alias` : alias that activates local embedding mode (`None` = never)
/// - `reranker`          : `Reranker` trait from gradatum-search (`None` = `/v1/rerank` disabled)
/// - `vault_aware`       : fire-and-forget `QaEvent` sender (disabled if endpoint absent)
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Shared provider pool — O(1) lookup by name.
    pub providers: ProviderPool,
    /// SQLite registry — request logging and provider status tracking.
    pub registry: Registry,
    /// Prometheus metrics — exported via `/metrics`.
    pub metrics: Metrics,
    /// Inbound bearer token resolved from an env var at startup.
    ///
    /// Wrapped in `SecretString`: zeroize-on-drop, no `Debug`/`Display`,
    /// exposed only via `.expose_secret()` in `auth.rs`.
    pub bearer_token: Option<Arc<SecretString>>,
    /// Per-IP rate limiter.
    pub rate_limiter: Arc<RateLimiter>,
    /// When `true`, loopback connections bypass bearer authentication.
    pub trust_localhost: bool,
    /// Local embedder from gradatum-embed — optional.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Alias that activates local embedding mode.
    pub local_embed_alias: Option<String>,
    /// Cross-encoder reranker from gradatum-search — optional.
    pub reranker: Option<Arc<dyn Reranker>>,
    /// VaultAware sender — no-op if the endpoint is not configured.
    pub vault_aware: VaultAwareSender,
}

impl AppState {
    /// Builds the shared state from a config — production path.
    ///
    /// `registry_path`: path to the SQLite file. Defaults to
    /// `"./gradatum-gateway-registry.db"` when `None`.
    pub fn new(config: Config, registry_path: Option<&Path>) -> Self {
        let bearer_token = config
            .server
            .bearer_token_env
            .as_deref()
            .and_then(|env_name| match std::env::var(env_name) {
                Ok(token) if !token.is_empty() => {
                    tracing::info!(
                        env_var = %env_name,
                        "authentification Bearer inbound activée"
                    );
                    Some(Arc::new(SecretString::from(token)))
                }
                Ok(_) => {
                    tracing::warn!(
                        env_var = %env_name,
                        "bearer_token_env présent mais vide — auth désactivée"
                    );
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        env_var = %env_name,
                        "bearer_token_env non défini — auth désactivée"
                    );
                    None
                }
            });

        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();

        let db_path = registry_path
            .map(|p| p.to_owned())
            .unwrap_or_else(|| Path::new("./gradatum-gateway-registry.db").to_owned());

        let registry = Registry::new(&db_path).unwrap_or_else(|e| {
            tracing::warn!(
                path = %db_path.display(),
                error = %e,
                "impossible d'ouvrir le registre SQLite — utilisation d'un db en mémoire"
            );
            Registry::new(Path::new(":memory:")).expect("Registry en mémoire impossible")
        });

        // Metrics allowlists — bound label cardinality to prevent unbounded memory growth.
        // See metrics.rs for the full rationale.
        let known_aliases: std::collections::HashSet<String> =
            config.aliases.keys().cloned().collect();
        // KNOWN_ROUTES — single source of truth (see const above).
        let known_routes: std::collections::HashSet<String> =
            KNOWN_ROUTES.iter().map(|s| s.to_string()).collect();
        // Providers configured at startup — bound the cardinality of the `provider` label.
        let known_providers: std::collections::HashSet<String> =
            config.provider_names().into_iter().collect();
        let metrics = Metrics::new(
            providers_count,
            known_aliases,
            known_routes,
            known_providers,
        );
        let rate_limiter = Arc::new(RateLimiter::new(config.server.rate_limit_per_minute));
        let trust_localhost = config.server.trust_localhost;

        let vault_aware_cfg = Arc::new(config.vault_aware.clone());
        let vault_aware = vault_aware::start_vault_aware_task(vault_aware_cfg)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    "construction client HTTP vault_aware impossible — hook désactivé (TLS absent ?)"
                );
                vault_aware::VaultAwareSender::disabled()
            });

        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
            bearer_token,
            rate_limiter,
            trust_localhost,
            embedder: None,
            local_embed_alias: None,
            reranker: None,
            vault_aware,
        }
    }

    /// Builds a test state — in-memory registry, no auth, no embedder or reranker.
    pub fn for_test(config: Config) -> Self {
        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();
        let registry = Registry::new(Path::new(":memory:"))
            .expect("Registry en mémoire pour tests impossible");
        let known_aliases: std::collections::HashSet<String> =
            config.aliases.keys().cloned().collect();
        // KNOWN_ROUTES — single source of truth (see const above).
        let known_routes: std::collections::HashSet<String> =
            KNOWN_ROUTES.iter().map(|s| s.to_string()).collect();
        let known_providers: std::collections::HashSet<String> =
            config.provider_names().into_iter().collect();
        let metrics = Metrics::new(
            providers_count,
            known_aliases,
            known_routes,
            known_providers,
        );
        let rate_limiter = Arc::new(RateLimiter::new(config.server.rate_limit_per_minute));
        let trust_localhost = config.server.trust_localhost;

        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
            bearer_token: None,
            rate_limiter,
            trust_localhost,
            embedder: None,
            local_embed_alias: None,
            reranker: None,
            vault_aware: VaultAwareSender::disabled(),
        }
    }

    /// Attaches a local embedder.
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>, alias: impl Into<String>) -> Self {
        self.local_embed_alias = Some(alias.into());
        self.embedder = Some(embedder);
        self
    }

    /// Attaches a reranker.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }
}

/// Builds the Axum router — shared between `main.rs` and integration tests.
///
/// CORS is configured via `config.server.allowed_origins` (origin allowlist).
pub fn build_router(state: AppState) -> axum::Router {
    use axum::extract::DefaultBodyLimit;
    use axum::middleware;
    use axum::routing;
    use tower_http::trace::TraceLayer;

    let router = axum::Router::new()
        .route("/health", routing::get(handlers::health::handler))
        .route("/metrics", routing::get(handlers::metrics_handler::handler))
        .route("/v1/models", routing::get(handlers::models::handler))
        .route(
            "/v1/chat/completions",
            routing::post(handlers::chat::handler),
        )
        .route(
            "/v1/embeddings",
            routing::post(handlers::embeddings::handler),
        )
        .route("/v1/messages", routing::post(handlers::messages::handler))
        .route(
            "/v1/messages/count_tokens",
            routing::post(handlers::messages::count_tokens_handler),
        )
        .route("/v1/rerank", routing::post(handlers::rerank::handler))
        // Body limit 4 MB — guards against oversized payloads.
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        // Inbound bearer authentication — automatically bypassed for /health.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_auth,
        ))
        .layer(TraceLayer::new_for_http());

    // CORS origin allowlist — no layer added when allowed_origins is empty (secure default).
    match cors::build_cors_layer(&state.config.server.allowed_origins) {
        Some(cors_layer) => router.layer(cors_layer).with_state(state),
        _ => router.with_state(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prod (`AppState::new`) and test (`AppState::for_test`) both consume `KNOWN_ROUTES`.
    /// This test verifies the constant itself — anti-regression lock against manual
    /// divergence if a route is added to the router but not to `KNOWN_ROUTES`.
    #[test]
    fn known_routes_contains_all_expected_routes() {
        let routes: std::collections::HashSet<&str> = KNOWN_ROUTES.iter().copied().collect();
        assert!(
            routes.contains("/v1/chat/completions"),
            "missing /v1/chat/completions"
        );
        assert!(routes.contains("/v1/embeddings"), "missing /v1/embeddings");
        assert!(routes.contains("/v1/messages"), "missing /v1/messages");
        assert!(
            routes.contains("/v1/messages/count_tokens"),
            "missing /v1/messages/count_tokens"
        );
        assert!(routes.contains("/v1/rerank"), "missing /v1/rerank");
        assert!(routes.contains("/v1/models"), "missing /v1/models");
        assert!(routes.contains("/health"), "missing /health");
        assert!(routes.contains("/metrics"), "missing /metrics");
        assert_eq!(
            KNOWN_ROUTES.len(),
            8,
            "KNOWN_ROUTES length changed — update test"
        );
    }
}
