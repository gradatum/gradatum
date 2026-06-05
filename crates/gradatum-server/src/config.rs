//! Configuration serveur chargée via figment avec priorité CLI > env > TOML > defaults.
//!
//! Décision R-A1 (design spec P2.0a) : TTL JWT par scope.
//! Décision C3 : bind/TLS fail-closed via [`ServerConfig::validate_bind_tls`].

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Configuration globale du serveur.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub server: HttpConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub acl: AclConfig,
    pub log: LogConfig,
    /// Pipeline de curation. Défaut : heuristic CPU offline.
    #[serde(default)]
    pub curator: CuratorConfig,
    /// Rate limiting (V3 — Phase 2.1.1 alpha.8).
    #[serde(default)]
    pub ratelimit: RateLimitConfig,
    /// Embedder HTTP (Phase 2.1.1 alpha.8).
    #[serde(default)]
    pub embed: EmbedConfig,
    /// Rétention de l'event-log QaEvents (B1 tranche v0.3.0).
    #[serde(default)]
    pub event_log: EventLogConfig,
}

/// Configuration de l'écoute HTTP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Adresse d'écoute. Défaut : 127.0.0.1:19090 (loopback).
    /// Une adresse non-loopback exige que `tls` soit configuré (C3 fail-closed).
    pub bind: SocketAddr,
    /// Adresse d'écoute des métriques Prometheus (canal latéral loopback).
    pub metrics_bind: SocketAddr,
    /// Configuration TLS optionnelle (requis si bind est non-loopback).
    pub tls: Option<TlsConfig>,
}

/// Certificat TLS — cert + clé PEM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Chemins de stockage persistant.
///
/// `vault_index_path` est le nom canonique depuis Phase 2.1 alpha.7.
/// `db_path` est conservé en alias backward-compat — retrait planifié alpha.7+1.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub vault_index_path: PathBuf,
    /// True si le toml a utilisé `db_path` (deprecated). Logué WARN au boot.
    /// Utiliser de préférence le getter [`StorageConfig::legacy_alias_used`].
    #[doc(hidden)]
    pub legacy_alias_used: bool,
}

impl StorageConfig {
    /// Retourne `true` si le fichier TOML chargé a utilisé l'alias deprecated `db_path`.
    pub fn legacy_alias_used(&self) -> bool {
        self.legacy_alias_used
    }
}

impl serde::Serialize for StorageConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = s.serialize_struct("StorageConfig", 2)?;
        state.serialize_field("root", &self.root)?;
        state.serialize_field("vault_index_path", &self.vault_index_path)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for StorageConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        StorageConfigRaw::deserialize(d).map(StorageConfig::from)
    }
}

/// Représentation intermédiaire pour gérer l'alias `db_path` → `vault_index_path`.
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
        let default_path = raw.root.join("db/index.sqlite");
        let (vault_index_path, legacy_alias_used) = match (raw.vault_index_path, raw.db_path) {
            (Some(canonical), Some(_legacy)) => (canonical, true),
            (Some(canonical), None) => (canonical, false),
            (None, Some(legacy)) => (legacy, true),
            (None, None) => (default_path, false),
        };
        StorageConfig {
            root: raw.root,
            vault_index_path,
            legacy_alias_used,
        }
    }
}

/// Configuration d'authentification JWT + révocation + API keys (AUTH-T2).
///
/// Schéma server.toml :
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
    /// Décision R-A1 : TTL token humain/Studio (défaut 3600s = 1h).
    pub jwt_ttl_human_secs: u64,
    /// Décision R-A1 : TTL bearer service (mcp-stub static) (défaut 86400s = 24h).
    pub jwt_ttl_service_secs: u64,
    /// `"memory"` (DEV WARN) | `"sqlite"` (prod défaut).
    pub revocation_store: String,
    pub revocation_db_path: Option<PathBuf>,
    /// Chemin vers la DB SQLite des API keys (AUTH-T2, spec V2 2026-05-06).
    ///
    /// Défaut : `<storage.root>/db/api_keys.sqlite`.
    /// Configurable via `[auth].api_keys_db_path` dans server.toml.
    /// DBs séparées (C2 spec V2) : `api_keys.sqlite` ≠ `revocation.sqlite`.
    #[serde(default)]
    pub api_keys_db_path: Option<PathBuf>,
}
// P1-4 Phase 4.2bis : require_jwt_jobs_endpoint retiré.
// v0.2.0 Bronze invariant réseau privé — endpoints jobs ouverts sans auth conditionnelle.
// Auth granulaire F-45 multi-user JWT planifiée v1.0.0 Gold.
// Voir spec §11 E-21.

/// Configuration ACL — chemin vers le fichier de préréglages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclConfig {
    pub preset_path: PathBuf,
}

/// Configuration de journalisation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    /// `"json"` (prod défaut) | `"pretty"` (dev).
    pub format: String,
}

/// Default embedder HTTP configuration.
/// Points to `http://localhost:8431/v1/embeddings` with model `bge-m3-Q8_0` (1024 dimensions).
/// Override in server.toml for your deployment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// Active ou désactive l'embedder HTTP. Défaut : `true`.
    #[serde(default = "default_embed_enabled")]
    pub enabled: bool,
    /// URL complète de l'endpoint embeddings (format OpenAI v1).
    /// Défaut : `http://localhost:8431/v1/embeddings`.
    #[serde(default = "default_embed_endpoint")]
    pub endpoint: String,
    /// Nom du modèle à passer dans le payload de la requête.
    /// Défaut : `bge-m3-Q8_0`. Override in server.toml for your deployment.
    #[serde(default = "default_embed_model")]
    pub model: String,
    /// Dimension attendue des vecteurs retournés.
    /// Défaut : 1024 (bge-m3-Q8_0).
    #[serde(default = "default_embed_dim")]
    pub dim: u16,
    /// Timeout par requête en millisecondes. Défaut : 5000.
    #[serde(default = "default_embed_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_embed_enabled() -> bool {
    true
}
fn default_embed_endpoint() -> String {
    "http://localhost:8431/v1/embeddings".to_string()
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

/// Configuration du rate limiting (V3 security review 2026-05-08).
///
/// Activé par défaut sur les endpoints `/api/v1/*` + `/auth/exchange`.
/// Exempts : `/health`, `/metrics`, et loopback (si `exempt_localhost`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Active ou désactive le rate limiting. Défaut : `true`.
    #[serde(default = "default_ratelimit_enabled")]
    pub enabled: bool,
    /// Nombre maximal de requêtes par minute par IP. Défaut : 60.
    #[serde(default = "default_ratelimit_per_minute")]
    pub per_minute: u32,
    /// Taille du burst autorisé (requêtes instantanées). Défaut : 10.
    #[serde(default = "default_ratelimit_burst")]
    pub burst: u32,
    /// Exempte les adresses loopback (127.x.x.x, ::1) du rate limiting. Défaut : `true`.
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

/// Configuration de la rétention event-log (B1 tranche v0.3.0).
///
/// Aligné v81 l.5938 : TTL 30 jours event-log, précurseur de F-32 lifecycle v0.5.0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogConfig {
    /// Rétention en jours avant purge par âge. Défaut : 30 (v81 TTL).
    #[serde(default = "default_event_log_retention_days")]
    pub retention_days: u64,
    /// Intervalle entre deux passes de purge en secondes. Défaut : 21600 (6h).
    #[serde(default = "default_event_log_purge_interval_secs")]
    pub purge_interval_secs: u64,
    /// Nombre maximal de lignes retenues (cap anti-burst). Défaut : 5_000_000.
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

/// Configuration du pipeline de curation.
///
/// Par défaut (`backend = "heuristic"`) : CPU only, zéro appel réseau, zéro LLM.
/// Le champ `llm` est optionnel et n'est utilisé (et le prompt classifier-v1 n'est
/// soumis) que si `backend != "heuristic"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Backend de classification.
    /// Valeur par défaut : `"heuristic"` (install minimale, offline).
    /// Autres valeurs : `"openai_compat"` | `"ollama_compat"` |
    ///                  `"anthropic_compat"` | `"gemini_compat"`.
    #[serde(default = "default_curator_backend")]
    pub backend: String,
    /// Configuration du tier LLM optionnel.
    /// Absent = heuristic pur, aucun appel réseau.
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

/// Configuration d'un backend LLM pour le curator.
///
/// Supporte une chaîne de fallback optionnelle (récursive via `Box`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Type de backend : `"openai_compat"` | `"ollama_compat"` |
    ///                    `"anthropic_compat"` | `"gemini_compat"`.
    pub backend: String,
    /// URL de base de l'endpoint (sans path).
    pub base_url: String,
    /// Nom du modèle (ex. `"Qwen3-4B-Instruct-2507"`, `"claude-haiku-4-5"`).
    pub model: String,
    /// Nom de la variable d'environnement portant le bearer token.
    /// Absent = pas d'authentification (endpoints locaux non authentifiés).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Timeout par requête en millisecondes. Défaut : 5000 ms.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Backend de fallback en cas d'erreur (circuit-breaker).
    /// `None` = pas de fallback (erreur propagée).
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
                vault_index_path: PathBuf::from("/var/lib/gradatum/db/index.sqlite"),
                legacy_alias_used: false,
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
        }
    }
}

/// Erreurs de configuration.
///
/// `figment::Error` est stockée dans un `Box` pour éviter `clippy::result_large_err`
/// (la variante dépasse 128 bytes sans boxing). Le `From<figment::Error>` est implémenté
/// manuellement car `#[from] Box<T>` ne génère pas `From<T>`.
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
    /// Charge la configuration avec priorité CLI > env (`GRADATUM__*`) > TOML > défauts.
    ///
    /// # Effets de bord
    /// Appelle [`validate_bind_tls`] — échoue si bind non-loopback sans TLS (C3).
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

    /// Décision C3 fail-closed : refuse le démarrage si bind non-loopback sans TLS.
    ///
    /// Cas acceptés :
    /// - loopback (IPv4 127.x.x.x ou IPv6 ::1) sans TLS — topology A interne OK
    /// - loopback avec TLS — redondant mais valide
    /// - non-loopback avec TLS — accès LAN/VPN/public sécurisé
    ///
    /// Cas refusé :
    /// - non-loopback sans TLS — protection contre exposition accidentelle en clair
    pub fn validate_bind_tls(&self) -> Result<(), ConfigError> {
        let is_loopback = self.server.bind.ip().is_loopback();
        match (is_loopback, self.server.tls.is_some()) {
            (true, _) => Ok(()),
            (false, true) => Ok(()),
            (false, false) => Err(ConfigError::BindTlsRefused {
                bind: self.server.bind,
            }),
        }
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
            std::path::PathBuf::from("/var/lib/gradatum/db/index.sqlite")
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
        assert_eq!(cfg.embed.endpoint, "http://localhost:8431/v1/embeddings");
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
        assert_eq!(cfg.endpoint, "http://localhost:8431/v1/embeddings");
        assert_eq!(cfg.timeout_ms, 5000);
    }
}

// auth_jobs_tests retiré Phase 4.2bis (P1-4) :
// require_jwt_jobs_endpoint = flag fantôme inopérant retiré de AuthConfig.
// v0.2.0 Bronze invariant réseau privé. Auth granulaire F-45 v1.0.0 Gold.
// Voir spec §11 E-21.
