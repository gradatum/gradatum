//! Shared provider pool — built at startup and distributed via `AppState`.
//!
//! At startup each provider declared in `config.providers` is instantiated once.
//! Handlers then resolve via `alias → provider_name → &Arc<Provider>`.
//!
//! Advantages over per-request construction:
//! - Shared `reqwest::Client` → reused TCP connection pool
//! - Static capabilities resolved once
//! - Reduced memory (no per-request allocation)
//!
//! The shared `reqwest::Client` is also exposed directly for handlers that make
//! HTTP requests without going through the `LlmProvider` trait
//! (e.g. the embeddings handler, which is a pure HTTP proxy).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::commons::{
    circuit_breaker::{CircuitBreakerConfig, CircuitBreakerRegistry},
    provider::{Capabilities, LlmProvider, ThinkingMode, ToolUseSupport},
};
use crate::config::Config;
use crate::providers::openai_compat::OpenAiCompatProvider;
use reqwest::Client;

/// Shared provider pool.
#[derive(Clone)]
pub struct ProviderPool {
    /// Providers indexed by name (matches the keys of `config.providers`).
    providers: Arc<HashMap<String, Arc<dyn LlmProvider + Send + Sync>>>,
    /// Shared HTTP client — reused TCP connection pool across all handlers.
    http_client: Client,
    /// Per-provider circuit breaker registry — shared via `Arc`.
    pub circuit_breakers: Arc<CircuitBreakerRegistry>,
    /// API keys pre-resolved from env vars at startup.
    ///
    /// Map: `provider_name → Option<api_key>` (`None` if not configured or var absent).
    /// Avoids `std::env::var()` calls per request.
    pub resolved_api_keys: Arc<HashMap<String, Option<String>>>,
}

impl ProviderPool {
    /// Builds the pool from the configuration.
    ///
    /// When a provider fails to construct (e.g. unreadable API key env var),
    /// the pool is still built with the valid providers.
    /// Failed providers are logged at the `warn` level.
    pub fn from_config(config: &Config) -> Self {
        // Shared HTTP client — global connection pool for all providers.
        let http_client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            // SAFETY: reqwest::Client construction can only fail if TLS is unavailable on the system.
            .expect("cannot build shared HTTP client — system TLS missing");

        // Generic default capabilities for OpenAI-compat providers.
        let default_caps = Capabilities {
            tool_use: ToolUseSupport::Native,
            streaming: true,
            vision: true,
            thinking: ThinkingMode::Switchable,
            context_max: 131_072,
            structured_output: false,
            prompt_caching: true,
            reasoning_levels: None,
        };

        let mut providers: HashMap<String, Arc<dyn LlmProvider + Send + Sync>> = HashMap::new();

        // Pre-resolve API keys from env vars at startup.
        let mut resolved_api_keys: HashMap<String, Option<String>> = HashMap::new();

        for (name, cfg) in &config.providers {
            let api_key = cfg.api_key_env.as_deref().and_then(|env_name| {
                let key = std::env::var(env_name).ok();
                if key.is_none() {
                    tracing::debug!(
                        provider = %name,
                        env_var = %env_name,
                        "api_key env variable absent — provider will run without auth"
                    );
                }
                key
            });

            resolved_api_keys.insert(name.clone(), api_key);

            match OpenAiCompatProvider::new(
                name,
                &cfg.endpoint,
                cfg.timeout_secs,
                cfg.api_key_env.as_deref(),
                default_caps.clone(),
            ) {
                Ok(provider) => {
                    providers.insert(name.clone(), Arc::new(provider));
                    tracing::info!(provider = %name, endpoint = %cfg.endpoint, "provider registered");
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "provider construction failed — excluded from pool"
                    );
                }
            }
        }

        // Circuit breaker registry — one instance per provider, config from [server].
        let cb_config = CircuitBreakerConfig {
            threshold: config.server.circuit_threshold,
            window: Duration::from_secs(config.server.circuit_window_secs),
            cooldown: Duration::from_secs(config.server.circuit_cooldown_secs),
        };
        let circuit_breakers = Arc::new(CircuitBreakerRegistry::new(cb_config));

        Self {
            providers: Arc::new(providers),
            http_client,
            circuit_breakers,
            resolved_api_keys: Arc::new(resolved_api_keys),
        }
    }

    /// Looks up a provider by name.
    ///
    /// Returns `None` if the name is not in the pool.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn LlmProvider + Send + Sync>> {
        self.providers.get(name)
    }

    /// Returns the number of providers in the pool.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Returns `true` if the pool contains no providers.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Returns the shared HTTP client for handlers that make direct HTTP requests.
    pub fn http_client(&self) -> Client {
        self.http_client.clone()
    }

    /// Returns the names of all providers in the pool.
    pub fn names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig};
    use std::collections::HashMap;

    fn test_config_with_two_providers() -> Config {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "p1".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:1".to_string(),
                timeout_secs: 5,
                api_key_env: None,
            },
        );
        providers.insert(
            "p2".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:2".to_string(),
                timeout_secs: 5,
                api_key_env: None,
            },
        );
        let mut aliases = HashMap::new();
        aliases.insert("m1".to_string(), AliasTarget::simple("p1", "model-real"));
        Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                registry_db: None,
                bearer_token_env: None,
                rate_limit_per_minute: 0,
                circuit_threshold: 5,
                circuit_window_secs: 60,
                circuit_cooldown_secs: 30,
                max_total_tokens: 0,
                trust_localhost: false,
                enable_slot_passthrough: true,
                allowed_origins: vec![],
                max_tools_per_request: 64,
            },
            logging: LoggingConfig {
                level: "error".to_string(),
            },
            providers,
            aliases,
            gateway: HashMap::new(),
            vault_aware: Default::default(),
            messages: Default::default(),
            router: Default::default(),
        }
    }

    #[test]
    fn test_pool_builds_from_config() {
        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        assert_eq!(pool.len(), 2);
        assert!(pool.get("p1").is_some());
        assert!(pool.get("p2").is_some());
        assert!(pool.get("p3").is_none());
    }

    #[test]
    fn test_http_client_shared() {
        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        let _c1 = pool.http_client();
        let _c2 = pool.http_client();
    }

    #[test]
    fn test_resolved_api_keys_populated() {
        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        assert!(pool.resolved_api_keys.contains_key("p1"));
        assert!(pool.resolved_api_keys.contains_key("p2"));
        assert_eq!(pool.resolved_api_keys.get("p1"), Some(&None));
        assert_eq!(pool.resolved_api_keys.get("p2"), Some(&None));
    }

    #[test]
    fn test_circuit_breaker_registry_accessible() {
        use crate::commons::circuit_breaker::CircuitState;

        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        assert_eq!(pool.circuit_breakers.state("p1"), CircuitState::Closed);
        assert_eq!(pool.circuit_breakers.state("p2"), CircuitState::Closed);
        assert_eq!(
            pool.circuit_breakers.state("inexistant"),
            CircuitState::Closed
        );
    }
}
