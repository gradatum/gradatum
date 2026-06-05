//! Configuration du gateway LLM — gradatum-gateway.
//!
//! Chargée au démarrage depuis un fichier TOML (chemin via `--config PATH`
//! ou variable d'environnement `GATEWAY_CONFIG_PATH`).
//!
//! Structure :
//! - `[server]` : adresse d'écoute, sécurité, rate limit
//! - `[logging]` : niveau tracing
//! - `[providers.<nom>]` : endpoint HTTP + timeout par provider
//! - `[aliases]` : map model_id → { provider, model, temperature_default, max_tokens_default }
//! - `[gateway.<feature_id>]` : paramètres par feature_id (AgentAware v81)
//! - `[vault_aware]` : config hook QaEvent fire-and-forget (v81)
//!
//! Port : `:8436` (distinct de llm-free-gateway-v2 :8435 pour coexistence migration).

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

/// Config racine du gateway.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Providers indexés par nom (ex: "my-llm-provider").
    ///
    /// `BTreeMap` pour garantir un ordre d'itération déterministe (ADN 2) :
    /// `provider_names()` et les logs/health exposent cet ordre comme output observable.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Aliases model_id → route provider (avec paramètres SmartRouter optionnels).
    #[serde(default)]
    pub aliases: HashMap<String, AliasTarget>,
    /// Paramètres par feature_id (AgentAware — sections TOML `[gateway."<feature_id>"]`).
    #[serde(default)]
    pub gateway: HashMap<String, AgentAwareParams>,
    /// Config du hook VaultAware (QaEvent fire-and-forget).
    #[serde(default)]
    pub vault_aware: VaultAwareConfig,
}

/// Config serveur.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Adresse d'écoute (ex: "127.0.0.1:8436").
    pub listen: String,
    /// Chemin du fichier SQLite pour le registre (défaut: "./gradatum-gateway-registry.db").
    #[serde(default)]
    pub registry_db: Option<String>,
    /// Nom de la variable d'environnement contenant le Bearer token inbound.
    ///
    /// Si absent ou `None` : aucune authentification requise (mode local/test).
    /// Si présent : tous les endpoints sauf `/health` exigent `Authorization: Bearer <token>`.
    /// Typiquement `GRADATUM_GATEWAY_BEARER`.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Limite de requêtes par minute par IP (sur les endpoints POST uniquement).
    ///
    /// Défaut : 60 req/min. `0` désactive le rate limiting.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_minute: u32,
    /// Nombre d'échecs consécutifs avant d'ouvrir le circuit breaker par provider.
    #[serde(default = "default_circuit_threshold")]
    pub circuit_threshold: u32,
    /// Fenêtre temporelle des échecs circuit breaker (secondes). Défaut : 60s.
    #[serde(default = "default_circuit_window_secs")]
    pub circuit_window_secs: u64,
    /// Durée du cooldown circuit breaker après ouverture (secondes). Défaut : 30s.
    #[serde(default = "default_circuit_cooldown_secs")]
    pub circuit_cooldown_secs: u64,
    /// Cap hard total de tokens (input + max_tokens demandé) par requête chat.
    ///
    /// `0` désactive le cap.
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u64,
    /// Bypass authentification Bearer pour les connexions loopback (127.0.0.1, ::1).
    ///
    /// Basé sur `ConnectInfo<SocketAddr>` (adresse TCP réelle), jamais sur headers HTTP.
    #[serde(default)]
    pub trust_localhost: bool,
    /// Active le passthrough du header `X-Slot-Id` → champ `slot_id` dans le body upstream.
    #[serde(default = "default_enable_slot_passthrough")]
    pub enable_slot_passthrough: bool,
    /// F-MAJ-1 fix : liste des origines CORS autorisées.
    ///
    /// Vide = CORS désactivé (défaut sécurisé). `["*"]` = permissif (déconseillé en prod).
    /// Remplace `CorsLayer::permissive()` de llm-free-gateway-v2.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// F-MAJ-2 fix : nombre maximum d'outils par requête (défaut : 64).
    ///
    /// Requêtes avec `tools.len() > max_tools_per_request` rejetées HTTP 400.
    #[serde(default = "default_max_tools_per_request")]
    pub max_tools_per_request: usize,
}

fn default_enable_slot_passthrough() -> bool {
    true
}

fn default_rate_limit() -> u32 {
    60
}

fn default_circuit_threshold() -> u32 {
    5
}

fn default_circuit_window_secs() -> u64 {
    60
}

fn default_circuit_cooldown_secs() -> u64 {
    30
}

fn default_max_total_tokens() -> u64 {
    180_000
}

fn default_max_tools_per_request() -> usize {
    64
}

/// Config logging.
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Config d'un provider HTTP OpenAI-compat.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// URL de base (ex: "http://127.0.0.1:8080").
    /// Le gateway append "/v1/chat/completions" pour les requêtes chat.
    pub endpoint: String,
    /// Timeout HTTP en secondes (défaut : 120).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Nom de la variable d'env contenant la clé API (optionnel).
    pub api_key_env: Option<String>,
}

fn default_timeout_secs() -> u64 {
    120
}

/// Cible d'un alias : provider nommé + model réel à envoyer.
///
/// Étendu pour le SmartRouter v81 : température et max_tokens par défaut optionnels.
#[derive(Debug, Deserialize, Clone)]
pub struct AliasTarget {
    /// Nom du provider dans `[providers]`.
    pub provider: String,
    /// Identifiant de modèle réel à transmettre au backend.
    pub model: String,
    /// Provider de fallback (optionnel).
    #[serde(default)]
    pub fallback_provider: Option<String>,
    /// Identifiant de modèle à envoyer au fallback (optionnel, même model si absent).
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Température par défaut pour cet alias (SmartRouter — override request si non fourni).
    #[serde(default)]
    pub temperature_default: Option<f32>,
    /// max_tokens par défaut pour cet alias (SmartRouter — override request si non fourni).
    #[serde(default)]
    pub max_tokens_default: Option<u32>,
}

impl AliasTarget {
    /// Construit un alias simple sans fallback — utile dans les tests.
    pub fn simple(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: None,
            max_tokens_default: None,
        }
    }
}

/// Paramètres AgentAware par feature_id (v81 §13).
///
/// Sections TOML : `[gateway."<feature_id>"]`
/// Permettent d'overrider la température et max_tokens par contexte agent.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AgentAwareParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub alias_override: Option<String>,
}

/// Config du hook VaultAware (fire-and-forget QaEvent → event-log).
///
/// Le hook envoie des événements QA à l'API gradatum-server via POST :19090/api/v1/event-log
/// en batch asynchrone (N=10 ou T=5s).
/// Si l'endpoint est absent ou KO : no-op silencieux (jamais bloquant).
#[derive(Debug, Deserialize, Clone, Default)]
pub struct VaultAwareConfig {
    /// URL de l'endpoint event-log (ex: "http://127.0.0.1:19090/api/v1/event-log").
    /// Si absent : hook désactivé.
    #[serde(default)]
    pub event_log_endpoint: Option<String>,
    /// Taille max du batch avant flush (défaut : 10).
    #[serde(default = "default_vault_batch_size")]
    pub batch_size: usize,
    /// Délai max entre deux flushes en secondes (défaut : 5).
    #[serde(default = "default_vault_flush_interval_secs")]
    pub flush_interval_secs: u64,
}

fn default_vault_batch_size() -> usize {
    10
}

fn default_vault_flush_interval_secs() -> u64 {
    5
}

impl Config {
    /// Charge et parse la config depuis le fichier au chemin donné.
    ///
    /// Retourne `Err` avec message précis si le fichier est absent ou malformé.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("impossible de lire le fichier config '{}': {}", path, e)
        })?;
        let mut config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("config TOML invalide dans '{}': {}", path, e))?;

        // Override MAX_TOTAL_TOKENS depuis env var.
        if let Ok(val) = std::env::var("MAX_TOTAL_TOKENS") {
            match val.parse::<u64>() {
                Ok(n) => {
                    tracing::info!(
                        max_total_tokens = n,
                        "cap tokens surchargé via MAX_TOTAL_TOKENS"
                    );
                    config.server.max_total_tokens = n;
                }
                Err(_) => {
                    anyhow::bail!(
                        "MAX_TOTAL_TOKENS='{}' invalide — doit être un entier positif",
                        val
                    );
                }
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Valide la cohérence de la config après parsing.
    fn validate(&self) -> anyhow::Result<()> {
        for (alias, target) in &self.aliases {
            if !self.providers.contains_key(&target.provider) {
                anyhow::bail!(
                    "alias '{}' référence le provider '{}' absent de [providers]",
                    alias,
                    target.provider
                );
            }
            if let Some(fb) = &target.fallback_provider {
                if !self.providers.contains_key(fb) {
                    anyhow::bail!(
                        "alias '{}' référence le fallback_provider '{}' absent de [providers]",
                        alias,
                        fb
                    );
                }
            }
        }
        Ok(())
    }

    /// Retourne les noms des providers configurés.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}
