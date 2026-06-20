//! Server configuration loaded via figment with priority CLI > env > TOML > defaults.
//!
//! JWT TTL is scoped per audience (human vs. service).
//! bind/TLS is fail-closed via [`ServerConfig::validate_bind_tls`].

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use gradatum_core::paths::vault_index_path as canon_vault_index_path;
use serde::{Deserialize, Serialize};

/// Configuration globale du serveur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: HttpConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub acl: AclConfig,
    pub log: LogConfig,
    /// Curation pipeline. Default: CPU-only heuristic (offline).
    #[serde(default)]
    pub curator: CuratorConfig,
    /// Rate limiting.
    #[serde(default)]
    pub ratelimit: RateLimitConfig,
    /// HTTP embedder.
    #[serde(default)]
    pub embed: EmbedConfig,
    /// Event-log (`QaEvent`) retention.
    #[serde(default)]
    pub event_log: EventLogConfig,
    /// Session-log Tier 1 (`session_trace`) retention.
    #[serde(default)]
    pub session_trace: SessionTraceConfig,
    /// Search scoring — trust decay (RRF layer).
    #[serde(default)]
    pub scoring: ScoringConfig,
    /// Studio web UI — `ServeDir` for `/ui/*`.
    #[serde(default)]
    pub studio: StudioConfig,
    /// API interne server-to-worker (Wave 2, v0.5.3).
    ///
    /// Listener sur `127.0.0.1:19092` uniquement si `token` configuré.
    /// wired: `gradatum-server/src/main.rs` (`spawn_internal_listener`)
    #[serde(default)]
    pub internal_api: InternalApiConfig,

    /// Configuration du moteur de recherche sémantique (ANN vs brute-force).
    ///
    /// Wired: `main.rs` active le chemin ANN sur `SqliteIndex` si `ann_backend = sqlite_vec`.
    #[serde(default)]
    pub search: SearchConfig,
}

/// Studio web UI configuration.
///
/// `ui_dir`: directory containing the compiled bundle (`dist/`).
/// Default: `/usr/share/gradatum/ui` (systemd deployment path).
///
/// If the directory does not exist at startup → the server still starts;
/// `/ui/*` returns a clean 404 (never panics).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudioConfig {
    /// Directory containing the studio assets.
    /// Override via `[studio] ui_dir` in `server.toml`
    /// or env `GRADATUM_STUDIO__UI_DIR`.
    #[serde(default = "StudioConfig::default_ui_dir")]
    pub ui_dir: PathBuf,
}

impl StudioConfig {
    fn default_ui_dir() -> PathBuf {
        PathBuf::from("/usr/share/gradatum/ui")
    }
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            ui_dir: Self::default_ui_dir(),
        }
    }
}

/// Backend de recherche sémantique approximée (ANN).
///
/// Contrôle le moteur de recherche vectorielle utilisé lors de `vault_search`
/// avec embedding de requête.
///
/// ## Default rollback-safe
///
/// La valeur par défaut est `BruteForce` — comportement identique à avant v0.5.3.
/// Passer à `SqliteVec` nécessite que l'extension sqlite-vec soit chargée au boot
/// (via `vec_ext.rs` dans le bin `gradatum-server`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnBackend {
    /// Brute-force cosine similarity O(N×dim) — comportement historique (défaut).
    ///
    /// Rollback-safe : sans config explicite, comportement identique à avant v0.5.3.
    #[default]
    BruteForce,
    /// ANN via sqlite-vec `vec0` virtual table — sub-linéaire pour grands N.
    SqliteVec,
}

/// Configuration du moteur de recherche sémantique (v0.5.3 ANN-5).
///
/// Sérialisée sous la section `[search]` du fichier `server.toml`.
///
/// ## Exemple `server.toml`
///
///
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchConfig {
    /// Backend ANN. Défaut: `BruteForce` (byte-compat avec before v0.5.3).
    ///
    /// Wired: `gradatum-server/src/main.rs` — active le chemin ANN sur `SqliteIndex`
    /// via `index.set_ann_enabled(true)` après enregistrement de l'extension sqlite-vec.
    #[serde(default)]
    pub ann_backend: AnnBackend,

    /// Paramètre `ef_search` pour vec0 (facteur d'oversampling).
    ///
    /// Valeur recommandée : 64 (bon compromis recall/latence pour N < 100k).
    /// Cap interne dans `sqlite_vec.rs` : `limit × ef_search ≤ MAX_ANN_K = 1024`.
    #[serde(default = "SearchConfig::default_ef_search")]
    pub ann_ef_search: u32,
}

impl SearchConfig {
    fn default_ef_search() -> u32 {
        64
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            ann_backend: AnnBackend::default(),
            ann_ef_search: Self::default_ef_search(),
        }
    }
}

/// Search scoring configuration — trust decay.
///
/// Trust decay applies a multiplier `(1 + γ × trust × 0.5^(age/half_life))`
/// in the RRF layer of the `composite_score`. Globally disableable.
///
/// ## Non-regression
///
/// `trust_decay_enabled = false` ⇒ scores bit-identical to the pre-decay baseline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringConfig {
    /// Enables the trust multiplier in scoring. Default: `true`.
    ///
    /// `false` = bit-identical scores to the pre-decay baseline (golden-set gate).
    #[serde(default = "ScoringConfig::default_trust_decay_enabled")]
    pub trust_decay_enabled: bool,
    /// Decay half-lives (in days) per provenance.
    ///
    /// A provenance **absent** from this map → **no decay** (`half_life = None`,
    /// non-perishable trust, e.g. `human-decision`). Default: `distilled = 90`.
    #[serde(default = "ScoringConfig::default_half_lives")]
    pub half_life_days: std::collections::HashMap<String, f64>,
}

impl ScoringConfig {
    fn default_trust_decay_enabled() -> bool {
        true
    }

    /// Default half-lives: `distilled = 90 d`. `human-decision` absent = no decay.
    ///
    /// Single source of truth: delegates to `gradatum_search::default_half_lives` —
    /// literals are defined only in `gradatum-search::scoring`.
    fn default_half_lives() -> std::collections::HashMap<String, f64> {
        gradatum_search::default_half_lives()
    }
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            trust_decay_enabled: Self::default_trust_decay_enabled(),
            half_life_days: Self::default_half_lives(),
        }
    }
}

/// HTTP listener configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Bind address. Default: `127.0.0.1:19090` (loopback).
    /// A non-loopback address requires `tls` to be configured (fail-closed).
    pub bind: SocketAddr,
    /// Prometheus metrics listener address (loopback side-channel).
    pub metrics_bind: SocketAddr,
    /// Optional native TLS termination (required when `bind` is non-loopback).
    /// When `Some`, the server terminates TLS itself (see [`TlsConfig`]).
    pub tls: Option<TlsConfig>,
}

/// Native TLS termination — PEM certificate + private key paths.
///
/// When present, the server terminates TLS itself via `axum-server` + rustls
/// (`bind_rustls`). Boot is fail-closed: if the certificate or key fails to load,
/// the server refuses to start rather than falling back to cleartext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Path to the PEM-encoded certificate (or certificate chain).
    pub cert_path: PathBuf,
    /// Path to the PEM-encoded private key.
    pub key_path: PathBuf,
}

/// Persistent storage paths.
///
/// `vault_index_path` is the canonical name. `db_path` is kept as a backward-compat alias.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub vault_index_path: PathBuf,
    /// `true` if the TOML used the deprecated `db_path` key. Logged as WARN at boot.
    /// Prefer the accessor [`StorageConfig::legacy_alias_used`].
    #[doc(hidden)]
    pub legacy_alias_used: bool,
    /// `true` si `vault_index_path` a été fourni explicitement dans le TOML ET diffère
    /// du chemin canonique `canon_vault_index_path(root)`.
    ///
    /// Utilisé par le fail-fast P0 : un override divergent est refusé avant v0.5.3.
    /// `false` si absent du TOML (défaut appliqué) ou cohérent avec le canonique.
    #[doc(hidden)]
    pub vault_index_path_override_diverges: bool,
}

impl StorageConfig {
    /// Returns `true` if the loaded TOML used the deprecated `db_path` alias.
    pub fn legacy_alias_used(&self) -> bool {
        self.legacy_alias_used
    }

    /// Returns `true` si `vault_index_path` a été explicitement overridé dans le TOML
    /// vers une valeur différente du chemin canonique `canon_vault_index_path(root)`.
    ///
    /// Utilisé pour le fail-fast P0 de divergence (Slice A1 round 2).
    pub fn vault_index_path_override_diverges(&self) -> bool {
        self.vault_index_path_override_diverges
    }
}

impl serde::Serialize for StorageConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // On ne sérialise PAS `vault_index_path` dans les défauts figment.
        // Ainsi, seul le TOML utilisateur peut renseigner ce champ — ce qui permet
        // à `StorageConfigRaw::from` de distinguer "défaut appliqué" (None) de
        // "override TOML explicite" (Some), nécessaire pour le fail-fast P0.
        let mut state = s.serialize_struct("StorageConfig", 1)?;
        state.serialize_field("root", &self.root)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for StorageConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        StorageConfigRaw::deserialize(d).map(StorageConfig::from)
    }
}

/// Intermediate representation for handling the `db_path` → `vault_index_path` alias.
#[derive(Debug, serde::Deserialize)]
struct StorageConfigRaw {
    root: PathBuf,
    #[serde(default)]
    vault_index_path: Option<PathBuf>,
    #[serde(default)]
    db_path: Option<PathBuf>,
}

impl From<StorageConfigRaw> for StorageConfig {
    fn from(raw: StorageConfigRaw) -> Self {
        // SSOT : défaut dérivé via le helper canonique gradatum-core (jamais inventé ici).
        let canonical = canon_vault_index_path(&raw.root);
        let (vault_index_path, legacy_alias_used, vault_index_path_override_diverges) =
            match (raw.vault_index_path, raw.db_path) {
                // vault_index_path explicite dans le TOML + db_path legacy présent.
                (Some(explicit), Some(_legacy)) => {
                    let diverges = explicit != canonical;
                    (explicit, true, diverges)
                }
                // vault_index_path explicite dans le TOML uniquement.
                (Some(explicit), None) => {
                    let diverges = explicit != canonical;
                    (explicit, false, diverges)
                }
                // Seul db_path legacy présent → valeur legacy, pas d'override divergent au sens
                // du fail-fast (l'utilisateur a migré depuis db_path — on log WARN séparément).
                (None, Some(legacy)) => (legacy, true, false),
                // Aucun override → chemin canonique calculé depuis root.
                (None, None) => (canonical, false, false),
            };
        StorageConfig {
            root: raw.root,
            vault_index_path,
            legacy_alias_used,
            vault_index_path_override_diverges,
        }
    }
}

/// JWT authentication, revocation, and API-key configuration.
///
/// Example `server.toml` schema:
/// ```toml
/// [auth]
/// revocation_store = "sqlite"  # "sqlite" | "memory"
/// revocation_db_path = "/var/lib/gradatum/db/revocation.sqlite"
/// api_keys_db_path = "/var/lib/gradatum/db/api_keys.sqlite"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub jwt_public_key_path: PathBuf,
    pub jwt_private_key_path: PathBuf,
    /// TTL for human/Studio tokens (default 3600 s = 1 h).
    pub jwt_ttl_human_secs: u64,
    /// TTL for service bearer tokens (default 86400 s = 24 h).
    pub jwt_ttl_service_secs: u64,
    /// `"memory"` (DEV only, emits WARN) | `"sqlite"` (production default).
    pub revocation_store: String,
    pub revocation_db_path: Option<PathBuf>,
    /// Path to the API-key SQLite database.
    ///
    /// Default: `<storage.root>/db/api_keys.sqlite`.
    /// Configurable via `[auth].api_keys_db_path` in `server.toml`.
    /// Separate databases: `api_keys.sqlite` ≠ `revocation.sqlite`.
    #[serde(default)]
    pub api_keys_db_path: Option<PathBuf>,
}
// Invariant réseau privé — endpoints jobs ouverts sans auth conditionnelle.
// Auth granulaire multi-user JWT planifiée dans une version ultérieure.

/// ACL configuration — path to the presets file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclConfig {
    pub preset_path: PathBuf,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// `"json"` (production default) | `"pretty"` (development).
    pub format: String,
}

/// Default embedder HTTP configuration.
/// Points to `http://localhost:8436/v1/embeddings` (gradatum-gateway) with model `bge-m3-Q8_0` (1024 dimensions).
/// Override in server.toml for your deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// Enables or disables the HTTP embedder. Default: `true`.
    #[serde(default = "default_embed_enabled")]
    pub enabled: bool,
    /// Full URL of the embeddings endpoint (OpenAI v1 format).
    /// Default: `http://localhost:8436/v1/embeddings`.
    #[serde(default = "default_embed_endpoint")]
    pub endpoint: String,
    /// Model name to include in the request payload.
    /// Default: `bge-m3-Q8_0`. Override in `server.toml` for your deployment.
    #[serde(default = "default_embed_model")]
    pub model: String,
    /// Expected dimension of the returned vectors.
    /// Default: 1024 (`bge-m3-Q8_0`).
    #[serde(default = "default_embed_dim")]
    pub dim: u16,
    /// Per-request timeout in milliseconds. Default: 5000.
    #[serde(default = "default_embed_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_embed_enabled() -> bool {
    true
}
fn default_embed_endpoint() -> String {
    "http://localhost:8436/v1/embeddings".to_string()
}
fn default_embed_model() -> String {
    "bge-m3-Q8_0".to_string()
}
fn default_embed_dim() -> u16 {
    // Dimensions bge-m3-Q8_0.
    1024
}
fn default_embed_timeout_ms() -> u64 {
    5000
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            enabled: default_embed_enabled(),
            endpoint: default_embed_endpoint(),
            model: default_embed_model(),
            dim: default_embed_dim(),
            timeout_ms: default_embed_timeout_ms(),
        }
    }
}

/// Rate-limiting configuration.
///
/// Enabled by default on `/api/v1/*` and `/auth/exchange`.
/// Exempt: `/health`, `/metrics`, and loopback addresses (when `exempt_localhost` is set).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Enables or disables rate limiting. Default: `true`.
    #[serde(default = "default_ratelimit_enabled")]
    pub enabled: bool,
    /// Maximum requests per minute per IP. Default: 60.
    #[serde(default = "default_ratelimit_per_minute")]
    pub per_minute: u32,
    /// Allowed burst size (instantaneous requests). Default: 10.
    #[serde(default = "default_ratelimit_burst")]
    pub burst: u32,
    /// Exempts loopback addresses (127.x.x.x, ::1) from rate limiting. Default: `true`.
    #[serde(default = "default_ratelimit_exempt_localhost")]
    pub exempt_localhost: bool,
}

fn default_ratelimit_enabled() -> bool {
    true
}
fn default_ratelimit_per_minute() -> u32 {
    60
}
fn default_ratelimit_burst() -> u32 {
    10
}
fn default_ratelimit_exempt_localhost() -> bool {
    true
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: default_ratelimit_enabled(),
            per_minute: default_ratelimit_per_minute(),
            burst: default_ratelimit_burst(),
            exempt_localhost: default_ratelimit_exempt_localhost(),
        }
    }
}

/// Event-log retention configuration.
///
/// Default TTL: 30 days.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogConfig {
    /// Retention in days before age-based purge. Default: 30.
    #[serde(default = "default_event_log_retention_days")]
    pub retention_days: u64,
    /// Interval between purge passes in seconds. Default: 21600 (6 h).
    #[serde(default = "default_event_log_purge_interval_secs")]
    pub purge_interval_secs: u64,
    /// Maximum number of rows retained (anti-burst cap). Default: 5 000 000.
    #[serde(default = "default_event_log_max_rows")]
    pub max_rows: u64,
}

fn default_event_log_retention_days() -> u64 {
    30
}
fn default_event_log_purge_interval_secs() -> u64 {
    21_600 // 6h
}
fn default_event_log_max_rows() -> u64 {
    5_000_000
}

impl Default for EventLogConfig {
    fn default() -> Self {
        Self {
            retention_days: default_event_log_retention_days(),
            purge_interval_secs: default_event_log_purge_interval_secs(),
            max_rows: default_event_log_max_rows(),
        }
    }
}

/// Session-log retention configuration (`session_trace` table).
///
/// Default TTL: 90 days. Purged by a tokio interval task (6 h) —
/// DELETE by age + `max_rows` cap, never via ACL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTraceConfig {
    /// Retention in days before age-based purge. Default: 90.
    #[serde(default = "default_session_trace_retention_days")]
    pub retention_days: u32,
    /// Interval between purge passes in seconds. Default: 21600 (6 h).
    #[serde(default = "default_session_trace_purge_interval_secs")]
    pub purge_interval_secs: u64,
    /// Maximum number of rows retained (anti-burst cap). Default: 5 000 000.
    #[serde(default = "default_session_trace_max_rows")]
    pub max_rows: i64,
}

fn default_session_trace_retention_days() -> u32 {
    90
}
fn default_session_trace_purge_interval_secs() -> u64 {
    21_600 // 6h
}
fn default_session_trace_max_rows() -> i64 {
    5_000_000
}

impl Default for SessionTraceConfig {
    fn default() -> Self {
        Self {
            retention_days: default_session_trace_retention_days(),
            purge_interval_secs: default_session_trace_purge_interval_secs(),
            max_rows: default_session_trace_max_rows(),
        }
    }
}

/// Curation pipeline configuration.
///
/// Default (`backend = "heuristic"`): CPU-only, zero network calls, zero LLM.
/// The `llm` field is optional and only used when `backend != "heuristic"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Classification backend.
    /// Default: `"heuristic"` (minimal install, offline).
    /// Other values: `"openai_compat"` | `"ollama_compat"` |
    ///               `"anthropic_compat"` | `"gemini_compat"`.
    #[serde(default = "default_curator_backend")]
    pub backend: String,
    /// Optional LLM tier configuration.
    /// Absent = pure heuristic, no network calls.
    #[serde(default)]
    pub llm: Option<LlmConfig>,
}

fn default_curator_backend() -> String {
    "heuristic".to_string()
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            backend: default_curator_backend(),
            llm: None,
        }
    }
}

/// LLM backend configuration for the curator.
///
/// Supports an optional fallback chain (recursive via `Box`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Backend type: `"openai_compat"` | `"ollama_compat"` |
    ///               `"anthropic_compat"` | `"gemini_compat"`.
    pub backend: String,
    /// Base URL of the endpoint (no path).
    pub base_url: String,
    /// Model name (e.g. `"Qwen3-4B-Instruct-2507"`, `"claude-haiku-4-5"`).
    pub model: String,
    /// Name of the environment variable holding the bearer token.
    /// Absent = no authentication (unauthenticated local endpoints).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Per-request timeout in milliseconds. Default: 5000 ms.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Fallback backend on error (circuit-breaker).
    /// `None` = no fallback (error propagated).
    #[serde(default)]
    pub fallback: Option<Box<LlmConfig>>,
}

fn default_timeout_ms() -> u64 {
    5000
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            server: HttpConfig {
                bind: "127.0.0.1:19090"
                    .parse()
                    .expect("adresse loopback par défaut invalide — constante littérale"),
                metrics_bind: "127.0.0.1:19091"
                    .parse()
                    .expect("adresse métriques loopback par défaut invalide — constante littérale"),
                tls: None,
            },
            storage: StorageConfig {
                root: PathBuf::from("/var/lib/gradatum"),
                // SSOT : dérivé via le helper canonique (invariant golden garanti par paths::tests).
                vault_index_path: canon_vault_index_path(std::path::Path::new("/var/lib/gradatum")),
                legacy_alias_used: false,
                // Les défauts ne constituent jamais un override divergent.
                vault_index_path_override_diverges: false,
            },
            auth: AuthConfig {
                jwt_public_key_path: PathBuf::from("/var/lib/gradatum/config/jwt.public.pem"),
                jwt_private_key_path: PathBuf::from("/var/lib/gradatum/config/jwt.private.pem"),
                jwt_ttl_human_secs: 3600,
                jwt_ttl_service_secs: 86400,
                revocation_store: "sqlite".to_string(),
                // None = auto-dérivé depuis storage.root dans main.rs.
                // Un chemin absolu ici court-circuiterait le fallback et casserait le smoke test
                // (le répertoire /var/lib/gradatum/ n'existe pas en dev/test).
                revocation_db_path: None,
                // AUTH-T2 : default = <storage.root>/db/api_keys.sqlite (dérivé dans main.rs).
                // None = auto-dérivé depuis storage.root au chargement si absent.
                api_keys_db_path: None,
            },
            acl: AclConfig {
                preset_path: PathBuf::from("/var/lib/gradatum/config/bearer.toml"),
            },
            log: LogConfig {
                format: "json".to_string(),
            },
            curator: CuratorConfig::default(),
            ratelimit: RateLimitConfig::default(),
            embed: EmbedConfig::default(),
            event_log: EventLogConfig::default(),
            session_trace: SessionTraceConfig::default(),
            scoring: ScoringConfig::default(),
            studio: StudioConfig::default(),
            internal_api: InternalApiConfig::default(),
            search: SearchConfig::default(),
        }
    }
}

/// Configuration errors.
///
/// `figment::Error` is boxed to avoid `clippy::result_large_err`
/// (the variant exceeds 128 bytes unboxed). `From<figment::Error>` is implemented
/// manually because `#[from] Box<T>` does not generate `From<T>`.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("erreur figment : {0}")]
    Figment(Box<figment::Error>),
    #[error(
        "bind/TLS fail-closed : bind={bind} est non-loopback sans TLS configuré. \
        Conseil : définir [server.tls] ou changer bind à 127.0.0.1:19090"
    )]
    BindTlsRefused { bind: SocketAddr },
}

impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Figment(Box::new(e))
    }
}

impl ServerConfig {
    /// Loads configuration with priority CLI > env (`GRADATUM__*`) > TOML > defaults.
    ///
    /// # Side effects
    /// Calls [`validate_bind_tls`](ServerConfig::validate_bind_tls) — fails if bind is
    /// non-loopback without TLS (fail-closed).
    pub fn load(toml_path: Option<&std::path::Path>) -> Result<Self, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Self::default()));
        if let Some(p) = toml_path {
            fig = fig.merge(Toml::file(p));
        }
        fig = fig.merge(Env::prefixed("GRADATUM_").split("__"));
        let cfg: Self = fig.extract()?;
        cfg.validate_bind_tls()?;
        Ok(cfg)
    }

    /// Fail-closed bind/TLS check: refuses startup if bind is non-loopback without TLS.
    ///
    /// Accepted cases:
    /// - loopback (IPv4 127.x.x.x or IPv6 ::1), with or without TLS — internal topology;
    ///   cleartext is acceptable behind a reverse proxy, TLS is redundant but valid.
    /// - non-loopback with TLS — the server terminates TLS natively (see [`TlsConfig`]).
    ///   Certificate/key are loaded fail-closed at boot, so a misconfigured
    ///   `[server.tls]` aborts startup rather than serving cleartext.
    ///
    /// Refused case:
    /// - non-loopback without TLS — guards against accidental cleartext exposure on a
    ///   public/LAN interface.
    pub fn validate_bind_tls(&self) -> Result<(), ConfigError> {
        let is_loopback = self.server.bind.ip().is_loopback();
        match (is_loopback, self.server.tls.is_some()) {
            // Loopback: cleartext (behind reverse proxy) or TLS, both fine.
            (true, _) => Ok(()),
            // Non-loopback + TLS: native termination; boot fails closed if cert/key are bad.
            (false, true) => Ok(()),
            // Non-loopback + no TLS: refuse cleartext on a public interface.
            (false, false) => Err(ConfigError::BindTlsRefused {
                bind: self.server.bind,
            }),
        }
    }
}

#[cfg(test)]
mod scoring_defaults_tests {
    use super::*;

    /// D2.2 — source unique demi-vies : `ScoringConfig` (config) et
    /// `TrustDecayConfig` (scoring) doivent dériver leurs demi-vies par défaut
    /// du MÊME tableau `gradatum_search::DEFAULT_TRUST_HALF_LIVES`. Si l'un des
    /// deux redéfinit un littéral, ce test échoue (non-régression decay).
    #[test]
    fn scoring_config_half_lives_match_scoring_source() {
        let from_config = ScoringConfig::default().half_life_days;
        let from_scoring = gradatum_search::TrustDecayConfig::default().half_life_days;
        assert_eq!(
            from_config, from_scoring,
            "demi-vies config != scoring : la source unique D2.2 a divergé"
        );
        // Valeur de référence du gate v0.4.4 : distilled = 90j.
        assert_eq!(from_config.get("distilled").copied(), Some(90.0));
        // human-decision absent = pas de decay (trust non périssable).
        assert!(!from_config.contains_key("human-decision"));
    }
}

#[cfg(test)]
mod ratelimit_tests {
    use super::*;

    #[test]
    fn ratelimit_section_defaults_when_absent() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:19090"
            metrics_bind = "127.0.0.1:19091"

            [storage]
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/db/index.sqlite"

            [auth]
            jwt_public_key_path = "/x"
            jwt_private_key_path = "/y"
            jwt_ttl_human_secs = 3600
            jwt_ttl_service_secs = 86400
            revocation_store = "memory"

            [acl]
            preset_path = "/x"

            [log]
            format = "json"
        "#;
        let cfg: ServerConfig = toml::from_str(toml).expect("parse");
        assert!(cfg.ratelimit.enabled);
        assert_eq!(cfg.ratelimit.per_minute, 60);
        assert_eq!(cfg.ratelimit.burst, 10);
        assert!(cfg.ratelimit.exempt_localhost);
    }

    #[test]
    fn ratelimit_section_custom() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:19090"
            metrics_bind = "127.0.0.1:19091"

            [storage]
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/db/index.sqlite"

            [auth]
            jwt_public_key_path = "/x"
            jwt_private_key_path = "/y"
            jwt_ttl_human_secs = 3600
            jwt_ttl_service_secs = 86400
            revocation_store = "memory"

            [acl]
            preset_path = "/x"

            [log]
            format = "json"

            [ratelimit]
            enabled = false
            per_minute = 120
            burst = 20
            exempt_localhost = false
        "#;
        let cfg: ServerConfig = toml::from_str(toml).expect("parse");
        assert!(!cfg.ratelimit.enabled);
        assert_eq!(cfg.ratelimit.per_minute, 120);
        assert_eq!(cfg.ratelimit.burst, 20);
        assert!(!cfg.ratelimit.exempt_localhost);
    }
}

#[cfg(test)]
mod storage_alias_tests {
    use super::*;

    #[test]
    fn loads_with_vault_index_path() {
        let toml = r#"
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/vault/.gradatum/index.db"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert_eq!(
            cfg.vault_index_path,
            std::path::PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db")
        );
        assert!(!cfg.legacy_alias_used());
    }

    #[test]
    fn loads_with_legacy_db_path_alias() {
        let toml = r#"
            root = "/var/lib/gradatum"
            db_path = "/var/lib/gradatum/db/index.sqlite"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert_eq!(
            cfg.vault_index_path,
            std::path::PathBuf::from("/var/lib/gradatum/db/index.sqlite")
        );
        assert!(cfg.legacy_alias_used(), "alias legacy doit être détecté");
    }

    #[test]
    fn default_vault_index_path_when_absent() {
        let toml = r#"
            root = "/var/lib/gradatum"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert_eq!(
            cfg.vault_index_path,
            std::path::PathBuf::from("/var/lib/gradatum/vault/.gradatum/index.db")
        );
        assert!(!cfg.legacy_alias_used());
    }

    #[test]
    fn both_keys_uses_canonical() {
        let toml = r#"
            root = "/var/lib/gradatum"
            vault_index_path = "/canonical.db"
            db_path = "/legacy.db"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert_eq!(
            cfg.vault_index_path,
            std::path::PathBuf::from("/canonical.db")
        );
        assert!(cfg.legacy_alias_used(), "doublon doit lever flag");
    }

    /// P0 fail-fast gate : vault_index_path explicite divergent → override_diverges = true.
    ///
    /// Ce flag est lu par server/main.rs pour refuser le démarrage avant v0.5.3.
    #[test]
    fn vault_index_path_override_diverges_when_explicit_and_non_canonical() {
        let toml = r#"
            root = "/var/lib/gradatum"
            vault_index_path = "/custom/path/index.db"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert!(
            cfg.vault_index_path_override_diverges(),
            "override divergent doit lever le flag fail-fast"
        );
    }

    /// P0 fail-fast gate : vault_index_path == canon → override_diverges = false (boot autorisé).
    #[test]
    fn vault_index_path_no_diverge_when_explicit_and_canonical() {
        let toml = r#"
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/vault/.gradatum/index.db"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert!(
            !cfg.vault_index_path_override_diverges(),
            "override identique au canonique ne doit pas lever le flag fail-fast"
        );
    }

    /// P0 fail-fast gate : pas de vault_index_path dans le TOML → override_diverges = false.
    #[test]
    fn vault_index_path_no_diverge_when_absent() {
        let toml = r#"
            root = "/var/lib/gradatum"
        "#;
        let cfg: StorageConfig = toml::from_str(toml).expect("parse");
        assert!(
            !cfg.vault_index_path_override_diverges(),
            "absence d'override ne doit pas lever le flag fail-fast"
        );
    }
}

#[cfg(test)]
mod embed_tests {
    use super::*;

    #[test]
    fn embed_section_defaults() {
        let toml = r#"
            [server]
            bind = "127.0.0.1:19090"
            metrics_bind = "127.0.0.1:19091"

            [storage]
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/db/index.sqlite"

            [auth]
            jwt_public_key_path = "/x"
            jwt_private_key_path = "/y"
            jwt_ttl_human_secs = 3600
            jwt_ttl_service_secs = 86400
            revocation_store = "memory"

            [acl]
            preset_path = "/x"

            [log]
            format = "json"
        "#;
        let cfg: ServerConfig = toml::from_str(toml).expect("parse");
        assert!(cfg.embed.enabled);
        assert_eq!(cfg.embed.endpoint, "http://localhost:8436/v1/embeddings");
        assert_eq!(cfg.embed.model, "bge-m3-Q8_0");
        assert_eq!(cfg.embed.dim, 1024);
        assert_eq!(cfg.embed.timeout_ms, 5000);
    }

    /// Vérifie que les défauts `EmbedConfig::default()` correspondent aux valeurs documentées.
    ///
    /// Model par défaut : `bge-m3-Q8_0` (1024 dimensions).
    /// Override in server.toml for your deployment.
    #[test]
    fn embed_defaults_match_documented_values() {
        let cfg = EmbedConfig::default();
        assert_eq!(
            cfg.model, "bge-m3-Q8_0",
            "default model must be bge-m3-Q8_0"
        );
        assert_eq!(cfg.dim, 1024, "default dim must be 1024");
        assert!(cfg.enabled, "embed activé par défaut");
        assert_eq!(cfg.endpoint, "http://localhost:8436/v1/embeddings");
        assert_eq!(cfg.timeout_ms, 5000);
    }
}

#[cfg(test)]
mod bind_tls_tests {
    use super::*;
    use std::io::Write;

    /// Self-signed EC P-256 certificate (CN=gradatum-test, 10-year validity) for
    /// offline TLS-loading tests. Generated with:
    /// `openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes`.
    /// Test-only material; never used at runtime.
    const TEST_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
MIIBhTCCASugAwIBAgIUc16mBJgTVAjrOvmzslop4m4iJv0wCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNZ3JhZGF0dW0tdGVzdDAeFw0yNjA2MTUxMjQ0MDJaFw0zNjA2
MTIxMjQ0MDJaMBgxFjAUBgNVBAMMDWdyYWRhdHVtLXRlc3QwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAASpJt1FQ9CBU66lo8vgQH/hKEsKszSpDm4/5/Z7DNghx7Fo
807dfd2kNfSSDTJ+uDOzYttvbeOZTgYCnNr+WOkHo1MwUTAdBgNVHQ4EFgQUC0W0
fZW2elU+cFOADIi2jRJ8irQwHwYDVR0jBBgwFoAUC0W0fZW2elU+cFOADIi2jRJ8
irQwDwYDVR0TAQH/BAUwAwEB/zAKBggqhkjOPQQDAgNIADBFAiEA5kHy7gBTYHV1
H/dXoI7SRyC2J/H1TJRoACOxqrDvJvQCIF7AR/RKHKyhItmBeAJqcA8rPYjYmQZ7
hLZMKiRKA7zn
-----END CERTIFICATE-----
";
    const TEST_KEY_PEM: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZkgn1phUXhvym851
ER7MKKEMs6vHKfZuA05E8QpFqOehRANCAASpJt1FQ9CBU66lo8vgQH/hKEsKszSp
Dm4/5/Z7DNghx7Fo807dfd2kNfSSDTJ+uDOzYttvbeOZTgYCnNr+WOkH
-----END PRIVATE KEY-----
";

    /// Builds a minimal `ServerConfig` from TOML with the given bind address and
    /// optional `[server.tls]` block — exercises the real deserialization path.
    fn config_with(bind: &str, tls: Option<(&str, &str)>) -> ServerConfig {
        let tls_block = match tls {
            Some((cert, key)) => {
                format!("\n[server.tls]\ncert_path = \"{cert}\"\nkey_path = \"{key}\"\n")
            }
            None => String::new(),
        };
        let toml = format!(
            r#"
            [server]
            bind = "{bind}"
            metrics_bind = "127.0.0.1:19091"
            {tls_block}
            [storage]
            root = "/var/lib/gradatum"
            vault_index_path = "/var/lib/gradatum/db/index.sqlite"

            [auth]
            jwt_public_key_path = "/x"
            jwt_private_key_path = "/y"
            jwt_ttl_human_secs = 3600
            jwt_ttl_service_secs = 86400
            revocation_store = "memory"

            [acl]
            preset_path = "/x"

            [log]
            format = "json"
        "#
        );
        toml::from_str(&toml).expect("parse config TOML")
    }

    // --- validate_bind_tls: the four arms of the (loopback, tls) matrix ---

    #[test]
    fn arm_loopback_without_tls_ok() {
        let cfg = config_with("127.0.0.1:19090", None);
        assert!(
            cfg.validate_bind_tls().is_ok(),
            "loopback cleartext is allowed (reverse-proxy topology)"
        );
    }

    #[test]
    fn arm_loopback_with_tls_ok() {
        let cfg = config_with("127.0.0.1:19090", Some(("/tmp/c.pem", "/tmp/k.pem")));
        assert!(
            cfg.validate_bind_tls().is_ok(),
            "loopback with TLS is redundant but valid"
        );
    }

    #[test]
    fn arm_non_loopback_with_tls_ok() {
        let cfg = config_with("0.0.0.0:19090", Some(("/tmp/c.pem", "/tmp/k.pem")));
        assert!(
            cfg.validate_bind_tls().is_ok(),
            "non-loopback with TLS terminates natively — accepted"
        );
    }

    #[test]
    fn arm_non_loopback_without_tls_refused() {
        let cfg = config_with("0.0.0.0:19090", None);
        let err = cfg
            .validate_bind_tls()
            .expect_err("non-loopback cleartext must be refused (fail-closed)");
        match err {
            ConfigError::BindTlsRefused { bind } => {
                assert_eq!(bind.ip().to_string(), "0.0.0.0");
            }
            other => panic!("expected BindTlsRefused, got {other:?}"),
        }
    }

    // --- TLS material loading via the real axum-server/rustls path ---

    /// Installs the aws-lc-rs process-default crypto provider for the test process.
    /// Idempotent: a previous install (or a parallel test) is tolerated.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[tokio::test]
    async fn valid_pem_loads_successfully() {
        ensure_crypto_provider();
        let dir = tempfile::tempdir().expect("tempdir");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::File::create(&cert_path)
            .and_then(|mut f| f.write_all(TEST_CERT_PEM.as_bytes()))
            .expect("write cert");
        std::fs::File::create(&key_path)
            .and_then(|mut f| f.write_all(TEST_KEY_PEM.as_bytes()))
            .expect("write key");

        let res = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path).await;
        assert!(res.is_ok(), "valid PEM cert/key must load: {:?}", res.err());
    }

    #[tokio::test]
    async fn missing_cert_fails_closed() {
        ensure_crypto_provider();
        // Fail-closed contract: a configured [server.tls] whose cert path does not
        // exist must surface an error (boot refuses), never a silent cleartext fallback.
        let res = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            "/nonexistent/gradatum-test-cert.pem",
            "/nonexistent/gradatum-test-key.pem",
        )
        .await;
        assert!(
            res.is_err(),
            "a missing certificate file must fail (fail-closed boot), not succeed"
        );
    }
}

// `require_jwt_jobs_endpoint` retiré de AuthConfig (flag fantôme inopérant).
// Invariant réseau privé — auth granulaire JWT planifiée dans une version ultérieure.

// ── InternalApiConfig (Wave 2, v0.5.3) ───────────────────────────────────────

/// Configuration de l'API interne server-to-worker (Wave 2, v0.5.3).
///
/// Routes `/internal/v1/*` montées sur un binding séparé (loopback uniquement).
/// Le listener n'est PAS démarré si `token` est absent (opt-in, backward-compat).
///
/// ## Sécurité
///
/// - `bind` doit impérativement être loopback — validé au démarrage (`spawn_internal_listener`).
/// - `token` est un `SecretString` — zeroize-on-drop, jamais loggué, comparaison constant-time.
/// - Le listener est optionnel (défaut : désactivé) — aucun impact sur l'API publique.
///
/// ## Config TOML
///
/// ```toml
/// [internal_api]
/// bind = "127.0.0.1:19092"
/// token = "your-shared-secret-here"
/// ```
#[derive(Clone, Serialize, Deserialize)]
pub struct InternalApiConfig {
    /// Adresse d'écoute — toujours loopback en prod.
    ///
    /// wired: `gradatum-server/src/main.rs` (`spawn_internal_listener`).
    #[serde(default = "InternalApiConfig::default_bind")]
    pub bind: std::net::SocketAddr,

    /// Token partagé worker→server. Obligatoire pour activer le listener.
    ///
    /// wired: `gradatum-server/src/internal/auth.rs` (comparaison constant-time).
    ///
    /// `None` = listener interne désactivé (backward-compat).
    ///
    /// Note : stocké en `String` pour la désérialisation TOML. Converti en
    /// `secrecy::SecretString` par `main.rs` via `state.with_internal_api_token()`
    /// avant tout accès. La conversion se fait après lecture config, jamais loggué.
    #[serde(default)]
    pub token: Option<String>,
}

impl InternalApiConfig {
    fn default_bind() -> std::net::SocketAddr {
        // invariant statique : l'adresse loopback est toujours parseable.
        "127.0.0.1:19092"
            .parse()
            .expect("adresse loopback interne valide — invariant statique")
    }
}

impl Default for InternalApiConfig {
    fn default() -> Self {
        Self {
            bind: Self::default_bind(),
            token: None,
        }
    }
}

impl std::fmt::Debug for InternalApiConfig {
    /// Implémentation manuelle — masque le token pour éviter toute fuite en logs.
    ///
    /// Le champ `token` affiche `[REDACTED]` si `Some(...)`, `None` sinon.
    /// Jamais le contenu réel du token ne doit apparaître dans les logs ou traces.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalApiConfig")
            .field("bind", &self.bind)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Valide qu'un token API interne respecte la longueur minimale.
///
/// ## Contrainte
///
/// Longueur minimale : `MIN_INTERNAL_TOKEN_LEN` caractères (32).
/// Un token plus court est refusé au démarrage (fail-fast) — ne jamais démarrer
/// avec un token faible qui donnerait une fausse impression de sécurité.
///
/// ## Pourquoi 32 ?
///
/// 32 octets aléatoires = 256 bits d'entropie (équivalent AES-256).
/// Un hex string de 64 chars ou un base64 de 44 chars le satisfait.
/// `openssl rand -hex 32` est la commande recommandée.
///
/// ## Longueur publique-par-design
///
/// La longueur minimale est documentée dans la config et dans le message d'erreur.
/// Ceci permet au middleware auth de ne pas masquer la longueur via timing —
/// la longueur attendue est un fait public.
///
/// # Errors
///
/// Retourne `Err(String)` avec un message explicite si le token est trop court.
pub fn validate_internal_token(token: &str) -> Result<(), String> {
    if token.len() < MIN_INTERNAL_TOKEN_LEN {
        return Err(format!(
            "token API interne trop court ({} chars < {} minimum) —              utilisez un token fort (ex: `openssl rand -hex 32`)",
            token.len(),
            MIN_INTERNAL_TOKEN_LEN
        ));
    }
    Ok(())
}

/// Longueur minimale du token API interne (en octets/caractères).
///
/// 32 chars = 256 bits d'entropie minimum.
/// Publique-par-design : le middleware auth ne masque pas la longueur via timing
/// (cf. `validate_internal_token` doc).
pub const MIN_INTERNAL_TOKEN_LEN: usize = 32;

#[cfg(test)]
mod internal_api_config_tests {
    use super::*;

    /// V4 : vérifie que `{:?}` sur `InternalApiConfig` NE CONTIENT PAS le token réel.
    #[test]
    fn internal_api_token_debug_redacted() {
        let cfg = InternalApiConfig {
            bind: "127.0.0.1:19092".parse().unwrap(),
            token: Some("super-secret-token-32-chars-long".to_string()),
        };
        let debug_str = format!("{cfg:?}");
        assert!(
            !debug_str.contains("super-secret-token-32-chars-long"),
            "le token réel NE DOIT PAS apparaître dans Debug : {debug_str:?}"
        );
        assert!(
            debug_str.contains("[REDACTED]"),
            "Debug doit contenir [REDACTED] quand token=Some : {debug_str:?}"
        );
    }

    /// V4 : vérifie que `None` s'affiche sans `[REDACTED]` (pas de faux positif).
    #[test]
    fn internal_api_token_none_debug() {
        let cfg = InternalApiConfig::default();
        let debug_str = format!("{cfg:?}");
        assert!(
            !debug_str.contains("[REDACTED]"),
            "REDACTED ne doit pas apparaître quand token=None : {debug_str:?}"
        );
    }

    /// V6 : `validate_internal_token` accepte un token de longueur exactement minimale.
    #[test]
    fn validate_token_exactly_min_length_ok() {
        let token = "a".repeat(MIN_INTERNAL_TOKEN_LEN);
        assert!(
            validate_internal_token(&token).is_ok(),
            "token de longueur exactement MIN_INTERNAL_TOKEN_LEN doit être accepté"
        );
    }

    /// V6 : `validate_internal_token` rejette un token trop court.
    #[test]
    fn validate_token_too_short_fails() {
        let token = "short";
        let result = validate_internal_token(token);
        assert!(
            result.is_err(),
            "token trop court doit être rejeté — got Ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("trop court"),
            "message d'erreur doit mentionner 'trop court' : {err:?}"
        );
        assert!(
            err.contains("openssl rand -hex 32"),
            "message d'erreur doit mentionner la commande recommandée : {err:?}"
        );
    }

    /// V6 : `validate_internal_token` accepte un token plus long que le minimum.
    #[test]
    fn validate_token_longer_than_min_ok() {
        let token = "a".repeat(64);
        assert!(
            validate_internal_token(&token).is_ok(),
            "token plus long que MIN_INTERNAL_TOKEN_LEN doit être accepté"
        );
    }
}
