//! Pool de providers partagés — construit au startup, distribué via `AppState`.
//!
//! Au démarrage, chaque provider déclaré dans `config.providers` est instancié
//! une seule fois. Les handlers résolvent ensuite via `alias → provider_name → &Arc<Provider>`.
//!
//! Avantages vs. création par requête :
//! - `reqwest::Client` partagé → pool de connexions TCP réutilisé
//! - Capabilities statiques résolues une seule fois
//! - Mémoire réduite (pas d'allocation par requête)
//!
//! Le `reqwest::Client` partagé est aussi exposé directement pour les handlers
//! qui font des requêtes HTTP sans passer par le trait `LlmProvider`
//! (ex: handler embeddings qui est un pur proxy HTTP).

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

/// Pool de providers partagés.
#[derive(Clone)]
pub struct ProviderPool {
    /// Providers indexés par nom (correspond aux clés de `config.providers`).
    providers: Arc<HashMap<String, Arc<dyn LlmProvider + Send + Sync>>>,
    /// Client HTTP partagé — pool de connexions réutilisé entre tous les handlers.
    http_client: Client,
    /// Registry circuit breakers per-provider — partagée via Arc.
    pub circuit_breakers: Arc<CircuitBreakerRegistry>,
    /// Clés API pré-résolues depuis env vars au startup.
    ///
    /// Map : provider_name → Option<api_key> (None si non configurée ou var absente).
    /// Évite les appels `std::env::var()` par requête.
    pub resolved_api_keys: Arc<HashMap<String, Option<String>>>,
}

impl ProviderPool {
    /// Construit le pool depuis la config.
    ///
    /// En cas d'erreur de construction d'un provider (ex: clé API env illisible),
    /// le pool se construit quand même avec les providers valides.
    /// Les providers en erreur sont loggés en `warn`.
    pub fn from_config(config: &Config) -> Self {
        // Client HTTP partagé — pool de connexions global pour tous les providers.
        let http_client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            // SAFETY : la construction d'un reqwest::Client ne peut échouer que si
            // TLS est indisponible sur le système.
            .expect("construction du client HTTP partagé impossible — TLS système absent");

        // Capabilities génériques par défaut pour les providers OpenAI-compat.
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

        // Pré-résolution des clés API depuis les variables d'env au startup.
        let mut resolved_api_keys: HashMap<String, Option<String>> = HashMap::new();

        for (name, cfg) in &config.providers {
            let api_key = cfg.api_key_env.as_deref().and_then(|env_name| {
                let key = std::env::var(env_name).ok();
                if key.is_none() {
                    tracing::debug!(
                        provider = %name,
                        env_var = %env_name,
                        "variable env api_key absente — provider fonctionnera sans auth"
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
                    tracing::info!(provider = %name, endpoint = %cfg.endpoint, "provider enregistré");
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "échec construction provider — exclu du pool"
                    );
                }
            }
        }

        // Circuit breaker registry — une instance par provider, config depuis [server].
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

    /// Résout un provider par nom.
    ///
    /// Retourne `None` si le nom n'est pas dans le pool.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn LlmProvider + Send + Sync>> {
        self.providers.get(name)
    }

    /// Nombre de providers dans le pool.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Retourne `true` si le pool ne contient aucun provider.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Retourne le client HTTP partagé pour les handlers qui font des requêtes directes.
    pub fn http_client(&self) -> Client {
        self.http_client.clone()
    }

    /// Itère sur les noms de providers dans le pool.
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
