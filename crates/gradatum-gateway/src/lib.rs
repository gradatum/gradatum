//! Crate lib gradatum-gateway — expose les types publics pour les tests d'intégration.
//!
//! Portage de llm-free-gateway-v2 avec :
//! - Absorption inline de llm-commons → module `commons/`
//! - F-08 : handler POST /v1/rerank via `Arc<dyn Reranker>`
//! - F-MAJ-1 : CORS whitelist configurée (remplace CorsLayer::permissive())
//! - F-MAJ-2 : cap hard outils par requête
//! - F-MAJ-3 : rate limit par IP socket TCP réelle (ConnectInfo)
//! - F-MAJ-4 : elapsed_secs réel dans LlmError::Timeout (OpenAiCompatProvider)
//! - SmartRouter v81 : paramètres par défaut alias + overrides AgentAware
//! - VaultAware hook v81 : QaEvent fire-and-forget
//!
//! Port : `:8436` (distinct de llm-free-gateway-v2 :8435 pour coexistence migration).

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

/// État partagé entre handlers (injecté par Axum via `State<AppState>`).
///
/// Extensions v81 vs llm-free-gateway-v2 :
/// - `embedder`          : Embedder trait via gradatum-embed (None = local embed désactivé)
/// - `local_embed_alias` : alias qui trigger le mode local (None = jamais)
/// - `reranker`          : Reranker trait via gradatum-search (None = /v1/rerank désactivé)
/// - `vault_aware`       : sender QaEvent fire-and-forget (désactivé si endpoint absent)
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Pool de providers partagés — accès O(1) par nom.
    pub providers: ProviderPool,
    /// Registre SQLite — journalisation et statuts providers.
    pub registry: Registry,
    /// Métriques Prometheus — export via /metrics.
    pub metrics: Metrics,
    /// Bearer token inbound pré-résolu depuis env var au startup.
    ///
    /// Wrappé dans `SecretString` (ADN 5 / ANSSI R23) : zeroize-on-drop, interdiction
    /// Debug/Display, exposition uniquement via `.expose_secret()` dans `auth.rs`.
    pub bearer_token: Option<Arc<SecretString>>,
    /// Rate limiter par IP.
    pub rate_limiter: Arc<RateLimiter>,
    /// Si `true`, les connexions loopback sont exemptées du bearer auth.
    pub trust_localhost: bool,
    /// Embedder local (gradatum-embed) — optionnel.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Alias qui déclenche le mode embedder local.
    pub local_embed_alias: Option<String>,
    /// Reranker cross-encoder (gradatum-search) — optionnel.
    pub reranker: Option<Arc<dyn Reranker>>,
    /// Sender VaultAware — no-op si endpoint non configuré.
    pub vault_aware: VaultAwareSender,
}

impl AppState {
    /// Construit l'état depuis une config — version production.
    ///
    /// `registry_path` : chemin du fichier SQLite. Si `None`, utilise
    /// `"./gradatum-gateway-registry.db"`.
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

        let metrics = Metrics::new(providers_count);
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

    /// Construit l'état de test — registry en mémoire, pas d'auth, pas d'embedder/reranker.
    pub fn for_test(config: Config) -> Self {
        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();
        let registry = Registry::new(Path::new(":memory:"))
            .expect("Registry en mémoire pour tests impossible");
        let metrics = Metrics::new(providers_count);
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

    /// Configure l'embedder local.
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>, alias: impl Into<String>) -> Self {
        self.local_embed_alias = Some(alias.into());
        self.embedder = Some(embedder);
        self
    }

    /// Configure le reranker.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }
}

/// Construit le router Axum — partagé entre main.rs et les tests.
///
/// F-MAJ-1 : CORS configuré via `config.server.allowed_origins` (whitelist).
///           Remplace CorsLayer::permissive() de llm-free-gateway-v2.
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
        .route("/v1/rerank", routing::post(handlers::rerank::handler))
        // Body limit 4MB — protège contre les payloads excessifs.
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        // Auth Bearer inbound — bypass automatique pour /health.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_auth,
        ))
        .layer(TraceLayer::new_for_http());

    // F-MAJ-1 : CORS whitelist configurée (remplace CorsLayer::permissive()).
    // Aucun layer si allowed_origins est vide (défaut sécurisé).
    if let Some(cors_layer) = cors::build_cors_layer(&state.config.server.allowed_origins) {
        router.layer(cors_layer).with_state(state)
    } else {
        router.with_state(state)
    }
}
