//! Configuration for gradatum-gateway.
//!
//! Loaded at startup from a TOML file (path via `--config PATH` or the
//! `GATEWAY_CONFIG_PATH` environment variable).
//!
//! Structure:
//! - `[server]`: listen address, security, rate limit
//! - `[logging]`: tracing level
//! - `[providers.<name>]`: HTTP endpoint + timeout per provider
//! - `[aliases]`: map of model_id → `{ provider, model, temperature_default, max_tokens_default }`
//! - `[gateway.<feature_id>]`: per-feature_id parameters (AgentAware)
//! - `[vault_aware]`: fire-and-forget `QaEvent` hook configuration
//!
//! Listen address: configurable via `[server] listen` (e.g. `"127.0.0.1:8436"`).

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

/// Root gateway configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Providers indexed by name (e.g. `"my-llm-provider"`).
    ///
    /// `BTreeMap` guarantees deterministic iteration order:
    /// `provider_names()` and logs/health expose this order as observable output.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Aliases: model_id → provider route (with optional SmartRouter parameters).
    #[serde(default)]
    pub aliases: HashMap<String, AliasTarget>,
    /// Per-feature_id parameters (AgentAware — TOML sections `[gateway."<feature_id>"]`).
    #[serde(default)]
    pub gateway: HashMap<String, AgentAwareParams>,
    /// VaultAware hook configuration (fire-and-forget `QaEvent`).
    #[serde(default)]
    pub vault_aware: VaultAwareConfig,
    /// Anthropic Messages API inbound configuration (`[messages]` section).
    ///
    /// Absent from TOML → serde default (sensible defaults, backward compat).
    #[serde(default)]
    pub messages: MessagesConfig,
}

/// Configuration for the Anthropic Messages API inbound gateway (`/v1/messages`).
///
/// Controls how inbound `model` names from Anthropic clients are mapped to internal
/// gateway aliases, without hardcoding any model or family names.
///
/// Example TOML:
/// ```toml
/// [messages]
/// default_alias = "default"
///
/// [messages.model_map]
/// "claude-3-5-sonnet-20241022" = "my-sonnet-alias"
/// "glm-4.6"                   = "my-glm-alias"
/// "gemini-2.0-flash"          = "my-gemini-alias"
/// ```
#[derive(Debug, Deserialize, Clone)]
pub struct MessagesConfig {
    /// Alias used when the inbound `model` name is not found in `model_map`.
    ///
    /// Default: `"default"` — matches the conventional gateway alias name.
    #[serde(default = "default_messages_alias")]
    pub default_alias: String,

    /// Map from inbound Anthropic model names to gateway alias names.
    ///
    /// Key: any model identifier the client may send (e.g. `"claude-3-5-sonnet-20241022"`).
    /// Value: a gateway alias name that must exist in `[aliases]`.
    ///
    /// Models absent from this map are routed to `default_alias`.
    #[serde(default)]
    pub model_map: HashMap<String, String>,
}

impl Default for MessagesConfig {
    fn default() -> Self {
        Self {
            default_alias: default_messages_alias(),
            model_map: HashMap::new(),
        }
    }
}

fn default_messages_alias() -> String {
    "default".to_string()
}

/// Server configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Listen address (e.g. `"127.0.0.1:8436"`).
    pub listen: String,
    /// Path to the SQLite registry file (default: `"./gradatum-gateway-registry.db"`).
    #[serde(default)]
    pub registry_db: Option<String>,
    /// Name of the environment variable holding the inbound bearer token.
    ///
    /// When absent or `None`: no authentication required (local/test mode).
    /// When present: all endpoints except `/health` require `Authorization: Bearer <token>`.
    /// Typically set to `GRADATUM_GATEWAY_BEARER`.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Per-IP request rate limit (applies to POST endpoints only).
    ///
    /// Default: 60 req/min. `0` disables rate limiting.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_minute: u32,
    /// Number of consecutive failures before opening the circuit breaker for a provider.
    #[serde(default = "default_circuit_threshold")]
    pub circuit_threshold: u32,
    /// Circuit breaker failure window in seconds. Default: 60 s.
    #[serde(default = "default_circuit_window_secs")]
    pub circuit_window_secs: u64,
    /// Circuit breaker cooldown duration after opening, in seconds. Default: 30 s.
    #[serde(default = "default_circuit_cooldown_secs")]
    pub circuit_cooldown_secs: u64,
    /// Hard token cap (input + requested `max_tokens`) per chat request.
    ///
    /// `0` disables the cap.
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u64,
    /// Bypasses bearer authentication for loopback connections (`127.0.0.1`, `::1`).
    ///
    /// Based on `ConnectInfo<SocketAddr>` (real TCP address), never on HTTP headers.
    #[serde(default)]
    pub trust_localhost: bool,
    /// Enables passthrough of the `X-Slot-Id` header into the `slot_id` field of the upstream body.
    #[serde(default = "default_enable_slot_passthrough")]
    pub enable_slot_passthrough: bool,
    /// List of allowed CORS origins.
    ///
    /// Empty = CORS disabled (secure default). `["*"]` = permissive (not recommended in production).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Maximum number of tools per request (default: 64).
    ///
    /// Requests with `tools.len() > max_tools_per_request` are rejected with HTTP 400.
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

/// Logging configuration.
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

/// Configuration for an OpenAI-compat HTTP provider.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// Base URL (e.g. `"http://127.0.0.1:8080"`).
    /// The gateway appends `"/v1/chat/completions"` for chat requests.
    pub endpoint: String,
    /// HTTP timeout in seconds (default: 120).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Name of the environment variable holding the API key (optional).
    pub api_key_env: Option<String>,
}

fn default_timeout_secs() -> u64 {
    120
}

/// Alias target: named provider and the real model identifier to forward.
///
/// Optional SmartRouter fields: default temperature and `max_tokens`.
#[derive(Debug, Deserialize, Clone)]
pub struct AliasTarget {
    /// Provider name in `[providers]`.
    pub provider: String,
    /// Real model identifier forwarded to the backend.
    pub model: String,
    /// Fallback provider (optional).
    #[serde(default)]
    pub fallback_provider: Option<String>,
    /// Model identifier sent to the fallback provider (optional; uses `model` when absent).
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Default temperature for this alias (SmartRouter — applied when the request omits it).
    #[serde(default)]
    pub temperature_default: Option<f32>,
    /// Default `max_tokens` for this alias (SmartRouter — applied when the request omits it).
    #[serde(default)]
    pub max_tokens_default: Option<u32>,
    /// Whether the provider for this alias supports multimodal (image) requests.
    ///
    /// Default: `false` (text only). Set to `true` only when the backend accepts
    /// `content: [{"type": "image_url", ...}]` (e.g. llama-server with an mmproj model).
    ///
    /// Vision gate (in `handlers/chat.rs`): any request containing an image sent to an alias
    /// with `vision_capable = false` is rejected with HTTP 400 before dispatch.
    #[serde(default)]
    pub vision_capable: bool,
}

impl AliasTarget {
    /// Builds a simple alias without a fallback — useful in tests.
    pub fn simple(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            fallback_provider: None,
            fallback_model: None,
            temperature_default: None,
            max_tokens_default: None,
            vision_capable: false,
        }
    }
}

/// Per-feature_id AgentAware parameters.
///
/// TOML sections: `[gateway."<feature_id>"]`
/// Allow overriding temperature and `max_tokens` per agent context.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AgentAwareParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub alias_override: Option<String>,
}

/// VaultAware hook configuration (fire-and-forget `QaEvent` → event-log).
///
/// The hook sends QA events to the gradatum-server API via `POST /api/v1/event-log`
/// in asynchronous batches (N events or T seconds, whichever comes first).
/// When the endpoint is absent or unavailable: silent no-op, never blocking.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct VaultAwareConfig {
    /// Event-log endpoint URL (e.g. `"http://127.0.0.1:19090/api/v1/event-log"`).
    /// When absent: hook disabled.
    #[serde(default)]
    pub event_log_endpoint: Option<String>,
    /// Maximum batch size before flush (default: 10).
    #[serde(default = "default_vault_batch_size")]
    pub batch_size: usize,
    /// Maximum interval between flushes in seconds (default: 5).
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
    /// Loads and parses the configuration from the file at the given path.
    ///
    /// Returns `Err` with a precise message if the file is missing or malformed.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("impossible de lire le fichier config '{}': {}", path, e)
        })?;
        let mut config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("config TOML invalide dans '{}': {}", path, e))?;

        // Override MAX_TOTAL_TOKENS from env var.
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

    /// Validates configuration consistency after parsing.
    ///
    /// Vérifie :
    /// 1. Chaque alias référence un provider existant dans `[providers]`.
    /// 2. `messages.default_alias` ∈ clés `[aliases]` (P1-A — typo = erreur au boot).
    /// 3. Chaque valeur de `messages.model_map` ∈ clés `[aliases]` (P1-A).
    ///
    /// # Errors
    /// Retourne une erreur descriptive nommant l'alias fautif, permettant un diagnostic
    /// immédiat sans relecture du TOML complet.
    fn validate(&self) -> anyhow::Result<()> {
        // ── Validation providers ────────────────────────────────────────────
        for (alias, target) in &self.aliases {
            if !self.providers.contains_key(&target.provider) {
                anyhow::bail!(
                    "alias '{}' référence le provider '{}' absent de [providers]",
                    alias,
                    target.provider
                );
            }
            if let Some(fb) = &target.fallback_provider
                && !self.providers.contains_key(fb)
            {
                anyhow::bail!(
                    "alias '{}' référence le fallback_provider '{}' absent de [providers]",
                    alias,
                    fb
                );
            }
        }

        // ── Validation aliases messages (P1-A relâché) ─────────────────────
        // La validation n'est déclenchée QUE si la section [messages] a été
        // explicitement configurée par l'utilisateur, c'est-à-dire si :
        //   - model_map est non vide (entrées explicites), OU
        //   - default_alias a été surchargé (différent de la valeur serde default).
        //
        // Raison : MessagesConfig::default() produit default_alias = "default" + model_map
        // vide. Une config qui omet entièrement [messages] (ex. chat-only, embeddings-only)
        // n'a pas nécessairement d'alias "default" — la valider serait une régression.
        // Les instances qui configurent [messages] voient toujours la validation complète
        // (fail-fast pour vraies typos).
        let messages_is_customised = !self.messages.model_map.is_empty()
            || self.messages.default_alias != default_messages_alias();

        if messages_is_customised {
            if !self.aliases.contains_key(&self.messages.default_alias) {
                anyhow::bail!(
                    "[messages] default_alias '{}' absent de [aliases] — alias disponibles : {:?}",
                    self.messages.default_alias,
                    {
                        let mut keys: Vec<_> = self.aliases.keys().cloned().collect();
                        keys.sort();
                        keys
                    }
                );
            }

            // Vérifier chaque valeur de model_map ∈ clés aliases.
            // On ne valide PAS les clés (noms de modèles Anthropic arbitraires).
            for (model_name, target_alias) in &self.messages.model_map {
                if !self.aliases.contains_key(target_alias) {
                    anyhow::bail!(
                        "[messages.model_map] entrée '{}' pointe vers l'alias '{}' absent de [aliases]",
                        model_name,
                        target_alias
                    );
                }
            }
        }

        Ok(())
    }

    /// Returns the names of configured providers.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

// ── Tests unitaires config ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit une `Config` minimale valide pour les tests de validation.
    ///
    /// Un provider "p" + alias "real-alias" pointant sur "p".
    fn base_config() -> Config {
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "p".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:8080".to_string(),
                timeout_secs: 30,
                api_key_env: None,
            },
        );

        let mut aliases = HashMap::new();
        aliases.insert(
            "real-alias".to_string(),
            AliasTarget::simple("p", "model-x"),
        );

        Config {
            server: ServerConfig {
                listen: "127.0.0.1:18436".to_string(),
                registry_db: None,
                bearer_token_env: None,
                rate_limit_per_minute: 60,
                circuit_threshold: 5,
                circuit_window_secs: 60,
                circuit_cooldown_secs: 30,
                max_total_tokens: 0,
                trust_localhost: true,
                enable_slot_passthrough: true,
                allowed_origins: vec![],
                max_tools_per_request: 64,
            },
            providers,
            aliases,
            gateway: HashMap::new(),
            logging: LoggingConfig::default(),
            vault_aware: VaultAwareConfig::default(),
            messages: MessagesConfig {
                default_alias: "real-alias".to_string(),
                model_map: HashMap::new(),
            },
        }
    }

    /// P1-A — default_alias inconnu → erreur de validation avec le nom de l'alias fautif.
    #[test]
    fn config_validation_rejects_unknown_default_alias() {
        let mut config = base_config();
        config.messages.default_alias = "ghost".to_string();
        let result = config.validate();
        assert!(result.is_err(), "default_alias inexistant doit être rejeté");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ghost"),
            "le message d'erreur doit nommer l'alias fautif 'ghost', obtenu: {}",
            msg
        );
    }

    /// P1-A — valeur de model_map pointant vers un alias inexistant → erreur de validation.
    #[test]
    fn config_validation_rejects_unknown_model_map_alias() {
        let mut config = base_config();
        config.messages.model_map.insert(
            "claude-3-5-sonnet-20241022".to_string(),
            "ghost".to_string(),
        );
        let result = config.validate();
        assert!(
            result.is_err(),
            "model_map avec alias inexistant doit être rejeté"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ghost"),
            "le message d'erreur doit nommer l'alias fautif 'ghost', obtenu: {}",
            msg
        );
    }

    /// P1-A — config entièrement valide (aliases existants) → validation OK.
    #[test]
    fn config_validation_accepts_valid_aliases() {
        let mut config = base_config();
        // Ajouter une deuxième entrée de model_map pointant sur un alias existant.
        config.messages.model_map.insert(
            "claude-3-5-sonnet-20241022".to_string(),
            "real-alias".to_string(),
        );
        let result = config.validate();
        assert!(
            result.is_ok(),
            "config avec aliases valides doit passer la validation, erreur: {:?}",
            result.err()
        );
    }

    /// Non-régression P1-A relâché — config sans section [messages] (défauts purs)
    /// avec aliases ne contenant PAS "default" → validation doit réussir.
    ///
    /// Reproduit le cas d'une instance chat/embeddings-only (ex. spike-engine-routing.toml)
    /// qui n'expose pas /v1/messages et n'a donc pas d'alias "default".
    #[test]
    fn config_without_messages_section_boots_without_default_alias() {
        // Config inspirée de spike-engine-routing.toml : aliases "curator" + "embed" uniquement.
        let mut providers = std::collections::BTreeMap::new();
        providers.insert(
            "engine-curator".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:11435".to_string(),
                timeout_secs: 30,
                api_key_env: None,
            },
        );
        providers.insert(
            "engine-embed".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:11436".to_string(),
                timeout_secs: 10,
                api_key_env: None,
            },
        );

        let mut aliases = HashMap::new();
        aliases.insert(
            "curator".to_string(),
            AliasTarget::simple("engine-curator", "curator-model"),
        );
        aliases.insert(
            "embed".to_string(),
            AliasTarget::simple("engine-embed", "embed-model"),
        );

        let config = Config {
            server: ServerConfig {
                listen: "127.0.0.1:8436".to_string(),
                registry_db: None,
                bearer_token_env: None,
                rate_limit_per_minute: 120,
                circuit_threshold: 5,
                circuit_window_secs: 60,
                circuit_cooldown_secs: 30,
                max_total_tokens: 0,
                trust_localhost: true,
                enable_slot_passthrough: false,
                allowed_origins: vec![],
                max_tools_per_request: 64,
            },
            providers,
            aliases,
            gateway: HashMap::new(),
            logging: LoggingConfig::default(),
            vault_aware: VaultAwareConfig::default(),
            // Section [messages] absente → serde default : default_alias = "default", model_map vide.
            // "default" n'est PAS dans aliases → aurait échoué avec P1-A non relâché.
            messages: MessagesConfig::default(),
        };

        let result = config.validate();
        assert!(
            result.is_ok(),
            "config sans [messages] (défauts purs) doit passer la validation même sans alias 'default', erreur: {:?}",
            result.err()
        );
    }
}
