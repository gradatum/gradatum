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

    /// Storage backend for the vault's Markdown notes.
    ///
    /// Absent section → local filesystem (default), byte-identical to prior behaviour.
    #[serde(default)]
    pub storage: StorageBackendConfig,
}

/// `[storage]` section — where the vault's Markdown notes physically live.
///
/// This is the **declarative switch** between a local-disk vault and a remote
/// S3-compatible object vault. Changing it is a configuration edit — no recompilation,
/// no binary variant. An absent `[storage]` section behaves exactly like today
/// (`service = "fs"`), so existing installations are untouched.
///
/// ## What lives here vs. what does not
///
/// - **Here** (declarative, non-secret): the service selector and its connection
///   parameters — `endpoint`, `bucket`, `region`, `root` prefix.
/// - **Never here**: credentials. Access keys are read exclusively from the process
///   environment by OpenDAL's native credential chain (e.g. `AWS_ACCESS_KEY_ID`).
///   Gradatum neither reads nor forwards any secret. A field for a secret would
///   eventually be used, then committed — so none exists.
///
/// ## Scope
///
/// This selects the backend for the **Markdown notes only**. The SQLite index always
/// remains on the local filesystem: a remote-notes / local-index deployment is the
/// supported combination.
///
/// ## Example (S3 on OVH)
///
/// ```toml
/// [storage]
/// service = "s3"
/// endpoint = "https://s3.gra.io.cloud.ovh.net"
/// bucket = "my-vault"
/// region = "gra"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackendConfig {
    /// OpenDAL service selector. `"fs"` (default) or `"s3"`.
    ///
    /// An object service configured in a build that did not enable the matching
    /// Cargo feature fails **at construction** with a clear error naming the service —
    /// a silent no-op is never produced.
    #[serde(default = "default_storage_service")]
    pub service: String,

    /// Endpoint URL of the object service (e.g. OVH S3). Ignored by `fs`.
    ///
    /// When omitted for `s3`, OpenDAL falls back to `AWS_ENDPOINT_URL` from the
    /// environment if present.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Bucket (a.k.a. container) name. Required by object services.
    #[serde(default)]
    pub bucket: Option<String>,

    /// Region. Optional depending on the provider.
    #[serde(default)]
    pub region: Option<String>,

    /// Root prefix within the backend (a bucket key prefix, or an `fs` sub-path).
    /// Optional — defaults to the backend's own root.
    #[serde(default)]
    pub root: Option<String>,
}

/// Default storage service: local filesystem.
fn default_storage_service() -> String {
    "fs".to_owned()
}

impl Default for StorageBackendConfig {
    fn default() -> Self {
        Self {
            service: default_storage_service(),
            endpoint: None,
            bucket: None,
            region: None,
            root: None,
        }
    }
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
    /// **Wired** — read by `NoteStatus::is_embeddable` in [`crate::status`].
    pub embeddable_status: Option<Vec<String>>,

    /// Embedding model identifier (e.g. `"bge-m3"`, `"bge-small-en-v1.5"`).
    ///
    /// **Not wired.** Kept so that existing configuration files stay loadable; the
    /// server reads `gradatum_server::config::EmbedConfig.model` instead.
    pub embedder_id: Option<String>,

    /// Output vector dimensions. `None` → inferred from `embedder_id`.
    ///
    /// **Not wired.** No production handler reads this field.
    pub dim: Option<u16>,

    /// Selected embedding backend.
    ///
    /// Values: `"http"` | `"fastembed"` | `"noop"`. `None` → `"http"`.
    ///
    /// **Not wired.** The backend is selected through
    /// `gradatum_server::config::EmbedConfig`.
    pub backend: Option<String>,

    /// Fallback backend when the primary backend is unavailable.
    ///
    /// **Not wired.** `FallbackEmbedder` is not enabled in production.
    pub fallback_backend: Option<String>,

    /// HTTP backend URL. Required when `backend = "http"`.
    ///
    /// **Not wired.** The effective URL comes from
    /// `gradatum_server::config::EmbedConfig`.
    pub http_url: Option<String>,

    /// HTTP embedding request timeout in milliseconds.
    ///
    /// **Not wired.** The effective timeout comes from
    /// `gradatum_server::config::EmbedConfig`.
    pub http_timeout_ms: Option<u32>,

    /// Model name sent in the HTTP request.
    ///
    /// **Not wired.** The effective model comes from
    /// `gradatum_server::config::EmbedConfig`.
    pub http_model: Option<String>,
}

/// `[curator]` section — curator pipeline configuration.
///
/// Controls heuristic thresholds and LLM review for low-confidence notes.
///
/// ## Wiring
///
/// The nine fields below are deserialised from the vault `config.toml`, but this
/// struct does **not** drive the curator pipeline. The pipeline is configured from
/// the `[curator]` table of the server TOML, extracted into
/// `gradatum_worker::curator_loader::WorkerCuratorConfig`. `CuratorConfig` exists so
/// that [`VaultConfig`] describes the whole schema; change `WorkerCuratorConfig` when
/// you want to change actual behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Heuristic direct-admit threshold (0.0–1.0).
    /// Notes scoring above are admitted without LLM review.
    ///
    /// **Not wired here** — effective consumer:
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`.
    pub heuristic_admit_threshold: Option<f32>,

    /// Default status assigned by the heuristic (kebab-case string).
    ///
    /// **Architectural note**: `String` (not `NoteStatus`) keeps `config.rs`
    /// free of domain types.
    ///
    /// **Dead knob — no effective consumer.** The value is copied from here into
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`, then into
    /// `gradatum_curator::CuratorPipelineConfig`, and is **never read** by any of them. Setting it
    /// in the TOML changes nothing: the status is decided by the pipeline's own logic.
    /// (Contrast with `heuristic_admit_threshold`, whose chain does terminate in a read.)
    pub heuristic_default_status: Option<String>,

    /// Enables LLM review for notes below `confidence_threshold`.
    ///
    /// **Not wired here** — effective consumer:
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`.
    pub llm_review_enabled: Option<bool>,

    /// Confidence threshold below which LLM review is triggered.
    ///
    /// **Not wired here** — effective consumer:
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`.
    pub confidence_threshold: Option<f32>,

    /// LLM review endpoint URL (OpenAI Chat API compatible).
    ///
    /// **Deprecated — ignored.** Nothing reads it: the effective URL is
    /// `[curator.llm] base_url`. The field is kept so that existing `server.toml`
    /// files still load, and `gradatum-worker` logs a warning at boot when it is set,
    /// naming the setting that actually applies.
    pub llm_review_endpoint: Option<String>,

    /// LLM model used for review.
    ///
    /// **Deprecated — ignored.** The effective model is `[curator.llm] model`.
    /// See [`llm_review_endpoint`](Self::llm_review_endpoint).
    pub llm_review_model: Option<String>,

    /// LLM review request timeout in milliseconds.
    ///
    /// **Deprecated — ignored.** The effective timeout is `[curator.llm] timeout_ms`.
    /// See [`llm_review_endpoint`](Self::llm_review_endpoint).
    pub llm_review_timeout_ms: Option<u32>,

    /// Maximum tokens the LLM reviewer may generate.
    ///
    /// **Not wired here** — effective consumer:
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`.
    pub llm_review_max_tokens: Option<u32>,

    /// Behaviour on LLM failure or timeout.
    ///
    /// Values: `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    ///
    /// **Not wired here** — effective consumer:
    /// `gradatum_worker::curator_loader::WorkerCuratorConfig`.
    pub llm_review_fallback: Option<String>,
}

/// `[index]` section — index engine configuration.
///
/// **The whole section is inert.** The SQLite backend is initialised directly by
/// `gradatum-server::state`, which never consults this section. It will only take
/// effect once the index backend becomes pluggable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Index backend. Values: `"sqlite"`. `None` → `"sqlite"`.
    ///
    /// **Not wired** — parsed but never read.
    pub backend: Option<String>,

    /// FTS5 tokeniser for full-text search.
    ///
    /// Values: `"unicode61"` | `"ascii"` | `"porter"`. `None` → `"unicode61"`.
    ///
    /// **Not wired** — parsed but never read.
    pub fts_tokenizer: Option<String>,
}

/// `[drift]` section — drift detector configuration.
///
/// **The whole section is inert.** Drift detection is not implemented in the worker
/// yet; the field below is parsed but never acted upon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriftConfig {
    /// Interval between drift scans in seconds. `None` → `3600`.
    ///
    /// **Not wired** — parsed but never read.
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
/// **The whole section is inert.** The JSONL audit writer (`audit_jsonl.rs`) is not
/// driven by `VaultConfig.audit` but by its own inline configuration. This section
/// will only take effect once both audit paths are unified.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Audit log rotation policy.
    ///
    /// Values: `"daily"` | `"weekly"` | `"size-100mb"`. `None` → `"daily"`.
    ///
    /// **Not wired** — parsed but never read.
    pub rotation: Option<String>,

    /// Retention period in days. `0` = infinite retention. `None` → `30`.
    ///
    /// **Not wired** — parsed but never read.
    pub retention_days: Option<u32>,

    /// Strict fsync mode.
    ///
    /// `false` (default) = 64 KB `BufWriter` + fsync every 100 ms or 100 events.
    /// `true` = fsync per event, bypasses buffer (~200 µs/event on NVMe — forensic-grade).
    ///
    /// **Not wired** — parsed but never read.
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

/// Builds an operator-facing message for a `figment::Error` that never echoes a
/// value read from a configuration source.
///
/// # Why this exists (security)
/// figment's own `Display` embeds the *offending value*: a type mismatch prints
/// `found string "<value>"` (via `figment::error::Actual`), and a TOML syntax
/// error is wrapped as `Kind::Message` carrying the source line verbatim. A secret
/// mistakenly placed in a config file — pasted into a typed field such as a port,
/// a timeout, or a dimension — would then reach the boot log. This helper rebuilds
/// the message from the *structured* fields only — the key path (`section.key`)
/// and the source file — plus a kind category whose value payload is dropped. The
/// operator still learns *what* is wrong, *which key*, and *which file*, but never
/// *the value*.
///
/// # Single guard for every figment consumer
/// Every binary that loads TOML through figment funnels its error rendering here:
/// `gradatum-server` (boot config), `gradatum-worker` (per-section fallback,
/// including the `[curator]` section that carries `base_url`/`model`), and
/// `gradatum-engine` (local config load). Centralising the redaction — and its
/// anti-regression test — in one place means a new leak cannot slip back into one
/// binary while the others stay safe.
///
/// The `match` on `figment::error::Kind` is intentionally exhaustive (no `_`
/// arm): should a future figment version add a variant, compilation fails loudly
/// rather than let a new leak slip through a catch-all.
#[must_use]
pub fn redact_figment_error(err: &figment::Error) -> String {
    use figment::error::Kind;

    // A figment error may chain several sub-errors; redact and join each.
    err.clone()
        .into_iter()
        .map(|e| {
            // Kind category. Value-bearing payloads (the `Actual` value, the raw
            // `Message` text, the user-typed enum variant) are deliberately
            // discarded; only schema-side facts (expected type, declared
            // field/key names) are kept.
            let kind = match &e.kind {
                Kind::Message(_) => {
                    "invalid syntax or value (withheld to avoid leaking config content)".to_string()
                }
                Kind::InvalidType(_, expected) => format!("invalid type, expected {expected}"),
                Kind::InvalidValue(_, expected) => format!("invalid value, expected {expected}"),
                Kind::InvalidLength(_, expected) => {
                    format!("invalid length, expected {expected}")
                }
                // The offending variant string is a *value*; surface only the
                // accepted set.
                Kind::UnknownVariant(_, expected) => {
                    format!("unknown value, expected one of {expected:?}")
                }
                // A field name is a *key*, safe (and useful) to name.
                Kind::UnknownField(name, expected) => {
                    format!("unknown field `{name}`, expected one of {expected:?}")
                }
                Kind::MissingField(name) => format!("missing field `{name}`"),
                Kind::DuplicateField(name) => format!("duplicate field `{name}`"),
                Kind::ISizeOutOfRange(_) | Kind::USizeOutOfRange(_) => {
                    "integer value out of range".to_string()
                }
                Kind::Unsupported(_) => "unsupported value type".to_string(),
                Kind::UnsupportedKey(_, expected) => {
                    format!("unsupported key type, must be {expected}")
                }
            };

            // Key location — configuration keys, never values.
            let at = if e.path.is_empty() {
                String::new()
            } else {
                format!(" at key `{}`", e.path.join("."))
            };

            // Source file when known ("which file"); otherwise the provider name
            // (e.g. environment), which carries no value either.
            let source = match &e.metadata {
                Some(md) => match md.source.as_ref().and_then(|s| s.file_path()) {
                    Some(path) => format!(" in {}", path.display()),
                    None => format!(" in {}", md.name),
                },
                None => String::new(),
            };

            format!("{kind}{at}{source}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod redact_figment_error_tests {
    use super::redact_figment_error;
    use figment::error::{Actual, Kind};

    /// Chaîne distinctive jouant le rôle du secret mal placé. Choisie pour n'être
    /// sous-chaîne d'aucun mot de schéma (types attendus, noms de champs).
    const SECRET: &str = "s3cr3t-do-not-leak-deadbeef-cafe";

    /// Chaque variante de `Kind` porteuse d'une valeur fuit nativement mais est
    /// masquée par [`redact_figment_error`]. Le `match` de la fonction est
    /// exhaustif : cette liste couvre toutes les variantes dont le `Display`
    /// figment embarque la valeur. Ce garde-fou unique couvre les trois binaires
    /// consommateurs (server, worker, engine).
    #[test]
    fn every_value_bearing_kind_is_redacted() {
        let s = SECRET.to_string();
        let cases = [
            figment::Error::from(Kind::Message(format!("erreur près de {s} ligne 3"))),
            figment::Error::from(Kind::InvalidType(Actual::Str(s.clone()), "u64".into())),
            figment::Error::from(Kind::InvalidValue(Actual::Str(s.clone()), "un port".into())),
            figment::Error::from(Kind::UnknownVariant(s.clone(), &["json", "text"])),
            figment::Error::from(Kind::Unsupported(Actual::Str(s.clone()))),
            figment::Error::from(Kind::UnsupportedKey(
                Actual::Str(s.clone()),
                "une chaîne".into(),
            )),
        ];
        for e in &cases {
            // Sanity : figment fuit bien la valeur nativement (sinon test vacué).
            assert!(
                e.to_string().contains(SECRET),
                "pré-condition : figment doit fuiter nativement pour ce Kind"
            );
            let redacted = redact_figment_error(e);
            assert!(
                !redacted.contains(SECRET),
                "redact_figment_error a laissé fuiter la valeur : {redacted}"
            );
        }
    }
}
