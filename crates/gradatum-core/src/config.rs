//! Runtime configuration loaded from `<vault_root>/.gradatum/config.toml`.
//!
//! See ARCHITECTURE.md for the configuration design.
//!
//! All fields are `Option<T>` with `#[serde(default)]` to allow partial configs.
//! Defaults are applied at consumption sites
//! (e.g. `NoteStatus::is_embeddable_default()` when `embed.embeddable_status` is `None`).
//!
//! ## Loading
//!
//! ```rust,no_run
//! use gradatum_core::config::VaultConfig;
//! use std::path::Path;
//!
//! let cfg = VaultConfig::load_from_root(Path::new("/my/vault")).unwrap();
//! ```
//!
//! Missing file → `VaultConfig::default()` without error.
//! Malformed TOML → `ConfigError::Parse`.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Complete configuration for a Gradatum vault.
///
/// Loaded from `<vault_root>/.gradatum/config.toml`. All sections are optional —
/// a minimal file may contain only `[vault]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultConfig {
    /// General vault parameters (tenant, schema version).
    #[serde(default)]
    pub vault: VaultSection,

    /// Embedding pipeline configuration.
    #[serde(default)]
    pub embed: EmbedConfig,

    /// Curator pipeline configuration.
    #[serde(default)]
    pub curator: CuratorConfig,

    /// Index engine configuration.
    #[serde(default)]
    pub index: IndexConfig,

    /// Drift detector configuration.
    #[serde(default)]
    pub drift: DriftConfig,

    /// Audit log configuration.
    #[serde(default)]
    pub audit: AuditConfig,

    /// Snapshot retention policy for the `.history/` directory.
    #[serde(default)]
    pub history: HistoryConfig,
}

/// `[vault]` section — vault identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultSection {
    /// Default tenant. `None` → `"main"` applied by the storage layer.
    pub default_tenant_id: Option<String>,

    /// Expected SQLite schema version. `None` → no strict version check.
    pub schema_version: Option<u32>,
}

/// `[embed]` section — embedding pipeline configuration.
///
/// Controls which backend is used, with which model, and which note statuses
/// are eligible for embedding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// Note statuses eligible for embedding (kebab-case, e.g. `["live", "pending-review"]`).
    ///
    /// `None` → use `NoteStatus::is_embeddable_default()`.
    ///
    /// **Architectural note**: `Vec<String>` (not `Vec<NoteStatus>`) keeps `config.rs`
    /// free of domain types and avoids circular dependencies.
    /// Comparison is performed in `NoteStatus::is_embeddable(&EmbedConfig)` via
    /// `serde_kebab_repr()`.
    ///
    /// **VIVANT** — lu par `NoteStatus::is_embeddable` dans `gradatum-core::status`.
    pub embeddable_status: Option<Vec<String>>,

    /// Embedding model identifier (e.g. `"bge-m3"`, `"bge-small-en-v1.5"`).
    ///
    /// **not yet wired (v0.6.x)** — présent pour rétrocompat config ; non lu par gradatum-server
    /// (qui utilise `gradatum_server::config::EmbedConfig.model` directement).
    pub embedder_id: Option<String>,

    /// Output vector dimensions. `None` → inferred from `embedder_id`.
    ///
    /// **not yet wired (v0.6.x)** — non lu par les handlers de production.
    pub dim: Option<u16>,

    /// Selected embedding backend.
    ///
    /// Values: `"http"` | `"fastembed"` | `"noop"`. `None` → `"http"`.
    ///
    /// **not yet wired (v0.6.x)** — sélection backend via `gradatum_server::config::EmbedConfig`.
    pub backend: Option<String>,

    /// Fallback backend when the primary backend is unavailable.
    ///
    /// **not yet wired (v0.6.x)** — `FallbackEmbedder` non câblé en production.
    pub fallback_backend: Option<String>,

    /// HTTP backend URL. Required when `backend = "http"`.
    ///
    /// **not yet wired (v0.6.x)** — URL lue depuis `gradatum_server::config::EmbedConfig`.
    pub http_url: Option<String>,

    /// HTTP embedding request timeout in milliseconds.
    ///
    /// **not yet wired (v0.6.x)** — timeout lu depuis `gradatum_server::config::EmbedConfig`.
    pub http_timeout_ms: Option<u32>,

    /// Model name sent in the HTTP request.
    ///
    /// **not yet wired (v0.6.x)** — model lu depuis `gradatum_server::config::EmbedConfig`.
    pub http_model: Option<String>,
}

/// `[curator]` section — curator pipeline configuration.
///
/// Controls heuristic thresholds and LLM review for low-confidence notes.
///
/// ## Câblage
///
/// Les 9 champs ci-dessous sont lus depuis `config.toml` par sérialisation, mais
/// le consommateur réel est `gradatum_worker::curator_loader::WorkerCuratorConfig`
/// (via figment `extract_inner::<WorkerCuratorConfig>("curator")`).
/// Ce struct `CuratorConfig` coexiste pour la cohérence schemale (VaultConfig) mais
/// ne pilote PAS directement le pipeline — toujours passer par `WorkerCuratorConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Heuristic direct-admit threshold (0.0–1.0).
    /// Notes scoring above are admitted without LLM review.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub heuristic_admit_threshold: Option<f32>,

    /// Default status assigned by the heuristic (kebab-case string).
    ///
    /// **Architectural note**: `String` (not `NoteStatus`) keeps `config.rs`
    /// free of domain types. Resolution happens in `gradatum-worker` via kebab-case comparison.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub heuristic_default_status: Option<String>,

    /// Enables LLM review for notes below `confidence_threshold`.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_enabled: Option<bool>,

    /// Confidence threshold below which LLM review is triggered.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub confidence_threshold: Option<f32>,

    /// LLM review endpoint URL (OpenAI Chat API compatible).
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_endpoint: Option<String>,

    /// LLM model used for review.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_model: Option<String>,

    /// LLM review request timeout in milliseconds.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_timeout_ms: Option<u32>,

    /// Maximum tokens the LLM reviewer may generate.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_max_tokens: Option<u32>,

    /// Behaviour on LLM failure or timeout.
    ///
    /// Values: `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    ///
    /// **not yet wired (v0.6.x)** — consommateur réel : `gradatum_worker::curator_loader::WorkerCuratorConfig`
    pub llm_review_fallback: Option<String>,
}

/// `[index]` section — index engine configuration.
///
/// **Section entière non-câblée (v0.6.x)** — le backend SQLite est initialisé
/// directement par `gradatum-server::state` sans passer par cette section.
/// Déverrouillage prévu lors de la séparation index pluggable v0.6.x.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Index backend. Values: `"sqlite"`. `None` → `"sqlite"`.
    ///
    /// **not yet wired (v0.6.x)**
    pub backend: Option<String>,

    /// FTS5 tokeniser for full-text search.
    ///
    /// Values: `"unicode61"` | `"ascii"` | `"porter"`. `None` → `"unicode61"`.
    ///
    /// **not yet wired (v0.6.x)**
    pub fts_tokenizer: Option<String>,
}

/// `[drift]` section — drift detector configuration.
///
/// **Section entière non-câblée (v0.6.x)** — détection de drift non implémentée
/// dans le worker. Déverrouillage prévu v0.6.x.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriftConfig {
    /// Interval between drift scans in seconds. `None` → `3600`.
    ///
    /// **not yet wired (v0.6.x)**
    pub scan_interval_seconds: Option<u32>,
}

/// `[history]` section — CoW snapshot retention policy.
///
/// Controls how many `.history/<id>/` snapshots are kept per note
/// and for how many days they are retained.
///
/// ## Defaults
///
/// Without a `[history]` section in the TOML, defaults apply:
/// - `max_versions = 50` — count cap
/// - `ttl_days = None` — no age-based purge
///
/// ## Application order
///
/// 1. **TTL first**: snapshots older than `ttl_days` days are removed,
///    regardless of `max_versions`.
/// 2. **Count cap next**: if the remaining count still exceeds `max_versions`,
///    the oldest snapshots (lowest timestamps) are removed.
///
/// This order guarantees that snapshots retained after TTL are always the
/// `max_versions` most recent. The behaviour is deterministic and idempotent.
///
/// ## TOML example
///
/// ```toml
/// [history]
/// max_versions = 20
/// ttl_days = 90
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryConfig {
    /// Maximum number of CoW snapshots retained per note.
    ///
    /// After each successful CoW write, snapshots exceeding this limit are
    /// removed, starting from the oldest. A value of `0` is treated as `1`
    /// (at least one snapshot is always kept when the CoW write succeeds).
    ///
    /// Default: `50`.
    pub max_versions: usize,

    /// Snapshot retention period in days.
    ///
    /// `None` (default) — no age-based purge; only `max_versions` applies.
    /// `Some(n)` — snapshots with a timestamp older than `n` days are purged
    /// before the count cap is applied.
    pub ttl_days: Option<u32>,
}

impl Default for HistoryConfig {
    /// Returns the defaults: `max_versions = 50`, `ttl_days = None`.
    ///
    /// These values keep at most 50 snapshots per note with no age-based purge.
    fn default() -> Self {
        Self {
            max_versions: 50,
            ttl_days: None,
        }
    }
}

/// `[audit]` section — audit log configuration.
///
/// Controls rotation, retention, and fsync mode for audit events.
///
/// **Section entière non-câblée (v0.6.x)** — l'audit JSONL (`audit_jsonl.rs`)
/// n'est pas piloté par `VaultConfig.audit` mais par sa propre config inline.
/// Déverrouillage prévu lors de l'unification audit v0.6.x.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Audit log rotation policy.
    ///
    /// Values: `"daily"` | `"weekly"` | `"size-100mb"`. `None` → `"daily"`.
    ///
    /// **not yet wired (v0.6.x)**
    pub rotation: Option<String>,

    /// Retention period in days. `0` = infinite retention. `None` → `30`.
    ///
    /// **not yet wired (v0.6.x)**
    pub retention_days: Option<u32>,

    /// Strict fsync mode.
    ///
    /// `false` (default) = 64 KB `BufWriter` + fsync every 100 ms or 100 events.
    /// `true` = fsync per event, bypasses buffer (~200 µs/event on NVMe — forensic-grade).
    ///
    /// **not yet wired (v0.6.x)**
    #[serde(default)]
    pub strict_mode: bool,
}

impl VaultConfig {
    /// Loads `<vault_root>/.gradatum/config.toml`.
    ///
    /// - Missing file → `Ok(VaultConfig::default())`.
    /// - Malformed TOML → `Err(ConfigError::Parse(...))`.
    /// - Other IO error → `Err(ConfigError::Io(...))`.
    ///
    /// # Panics
    ///
    /// Never. All errors are propagated via `Result`.
    pub fn load_from_root(root: &Path) -> Result<Self, ConfigError> {
        let path = root.join(".gradatum").join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).map_err(ConfigError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

/// Configuration loading errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// IO error (permissions, invalid path, etc.).
    #[error("config IO: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed TOML or incorrect field type.
    #[error("config parse: {0}")]
    Parse(#[from] toml::de::Error),
}
