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

use crate::proactive_recall::ProactiveRecallConfig;

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
    /// Archives des notes supprimées (F-100 incrément 1.6) — rétention avant GC physique.
    #[serde(default)]
    pub archive: ArchiveConfig,
    /// Search scoring — trust decay (RRF layer).
    #[serde(default)]
    pub scoring: ScoringConfig,
    /// Search scoring — usage salience. Default OFF.
    #[serde(default)]
    pub salience: SalienceConfig,
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

    /// Auto-promotion des notes review backlog (`staging` / `pending-review`) âgées.
    ///
    /// Promeut en `Live` les notes restées en phase review plus longtemps que `age_days`.
    /// Wired: tâche tokio interval dans `main.rs`.
    #[serde(default)]
    pub review_promote: ReviewPromoteConfig,

    /// Passe d'audit / déduplication rétrospective du vault (F-51, Option A).
    ///
    /// Détecte les doublons / déchets et produit un **rapport** (jamais de mutation).
    /// Désactivée par défaut. Wired: tâche tokio interval dans `main.rs`.
    #[serde(default)]
    pub audit: AuditConfig,

    /// Oubli gradué F-111 — axe pertinence de la passe d'audit + exécuteur
    /// auto-downgrade (réversible, jamais delete). Désactivé par défaut (gate §7).
    #[serde(default)]
    pub downgrade: DowngradeConfig,

    /// Configuration for the context assembly pipeline.
    ///
    /// Controls the constants used by `assemble_assembled`: budget, top_n, skills.
    /// Wired via `AppState::with_context` in `main.rs`.
    #[serde(default)]
    pub context: ContextConfig,

    /// Active Recall scheduler (F-46, v0.7.1) — calcul de surface in-process (B').
    ///
    /// Contrôle l'intervalle et les paramètres du calcul de surface proactif.
    /// Plancher de 60s appliqué dans `main.rs` au moment de construire le
    /// `tokio::interval` (`.max(60)`) — une valeur TOML < 60 est remontée à 60s.
    ///
    /// Wired: tâche `tokio::interval` dans `main.rs`
    /// (`proactive_recall::refresh::proactive_refresh_once`). Tenant : `"main"` (v0.7.1).
    ///
    /// ## Exemple `server.toml`
    ///
    /// ```toml
    /// [proactive_recall]
    /// refresh_interval_secs = 900  # défaut : 15 min (plancher 60s)
    /// recent_k = 20                # défaut : 20 notes récentes
    /// surface_size = 8             # défaut : 8 hits retenus
    /// ```
    #[serde(default)]
    pub proactive_recall: ProactiveRecallConfig,

    /// Substrat multi-vault C1 (F-63, plan v1.0.0 A5). Défaut : OFF.
    ///
    /// OFF ⇒ verrou legacy mono-vault `"main"` (`tenant_is_authorized`), comportement
    /// **inchangé** — les tables `tenants`/`tenant_vault_grants`
    /// (migration 0030) ne sont jamais consultées. ON ⇒ allow-list consultée à chaque
    /// requête (middleware) et sur chaque chemin d'écriture (EX-C1-2).
    /// Rollback = repasser le flag à OFF.
    #[serde(default)]
    pub multi_tenant: MultiTenantConfig,

    /// Overrides de configuration PAR vault (A6, F-63 multi-vault) — couche minimale.
    ///
    /// Map `vault_id -> override partiel`. **Défaut : vide** → toute résolution retombe sur
    /// la config globale (aucun override configuré
    /// ⇒ chemins inchangés). Lecture seule via [`ServerConfig::salience_for`] /
    /// [`ServerConfig::review_promote_for`] : pas de rechargement dynamique, pas d'UI, pas
    /// d'écriture — juste la capacité de lire un override s'il est présent (YAGNI). Étendre
    /// la surface surchargeable = ajouter un champ `Option<…>` dans [`PerVaultOverride`] +
    /// un résolveur `_for` dédié ; ne pas généraliser avant un besoin per-vault réel.
    #[serde(default)]
    pub per_vault: std::collections::HashMap<String, PerVaultOverride>,
}

/// Override de configuration pour un vault donné (A6, YAGNI minimal).
///
/// Chaque champ est optionnel : `None` ⇒ le résolveur retombe sur la config globale de
/// [`ServerConfig`]. Seuls `salience` et `review_promote` sont surchargeables (les seuls
/// besoins per-vault identifiés à l'horizon flip). Un override absent de la map est
/// équivalent à un `PerVaultOverride` tout-`None` : global exact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerVaultOverride {
    /// Surcharge de la config salience pour ce vault. `None` ⇒ global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salience: Option<SalienceConfig>,
    /// Surcharge de la config d'auto-promotion review pour ce vault. `None` ⇒ global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_promote: Option<ReviewPromoteConfig>,
}

/// Substrat multi-vault (C1, F-63) — voir [`ServerConfig::multi_tenant`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiTenantConfig {
    /// Active le chemin lookup allow-list `tenant_vault_grants`. Défaut : `false`.
    ///
    /// INV-P1-3 : aucune activation d'un 2e vault en écriture avant le fix ACL
    /// cross-vault (C2) — ce flag reste OFF partout en production sur le train C1.
    #[serde(default)]
    pub enabled: bool,
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

/// Search scoring configuration — usage salience.
///
/// Adds the 4th multiplicative factor `(1 + gamma × s/(s+k_norm))` to the composite,
/// fed by the `note_usage` table. **Default OFF** — when `enabled = false`,
/// responses are byte-identical to a build without salience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SalienceConfig {
    /// Enables the salience factor. Default: `false` (deferred activation gate).
    #[serde(default)]
    pub enabled: bool,
    /// Boost coefficient — max boost `× (1 + gamma)`. Default `0.10`.
    #[serde(default = "SalienceConfig::default_gamma")]
    pub gamma: f64,
    /// Soft-saturation constant (must be > 0 — validated at boot). Default `10.0`.
    #[serde(default = "SalienceConfig::default_k_norm")]
    pub k_norm: f64,
    /// Per-kind weights. Absent kind = weight 0 (ignored).
    #[serde(default = "SalienceConfig::default_kind_weights")]
    pub kind_weights: std::collections::HashMap<String, f64>,
}

impl SalienceConfig {
    fn default_gamma() -> f64 {
        0.10
    }
    fn default_k_norm() -> f64 {
        10.0
    }
    fn default_kind_weights() -> std::collections::HashMap<String, f64> {
        [
            ("read", 3.0),
            ("recall-accepted", 5.0),
            ("search-hit-top3", 2.0),
            ("recall-surfaced", 1.0),
            ("search-hit", 0.5),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Fail-loud boot validation: `k_norm` must be strictly positive.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when `k_norm <= 0`.
    pub fn validate(&self) -> Result<(), String> {
        if self.k_norm <= 0.0 {
            return Err(format!(
                "[salience] k_norm must be > 0 (got {})",
                self.k_norm
            ));
        }
        Ok(())
    }

    /// Resolves into scoring params — `None` when disabled (the disable lever).
    #[must_use]
    pub fn resolve(&self) -> Option<gradatum_search::SalienceParams> {
        self.enabled.then(|| gradatum_search::SalienceParams {
            gamma: self.gamma,
            k_norm: self.k_norm,
            kind_weights: self.kind_weights.clone(),
        })
    }
}

impl Default for SalienceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gamma: Self::default_gamma(),
            k_norm: Self::default_k_norm(),
            kind_weights: Self::default_kind_weights(),
        }
    }
}

/// Configuration for the context assembly pipeline.
///
/// Externalises the `assemble_assembled` hard-coded constants into server config.
/// Zero-config: all fields have functional defaults.
///
/// ## Exemple `server.toml`
///
/// ```toml
/// [context]
/// default_budget_tokens = 2000
/// top_n_candidates = 50
/// max_skills = 3
/// skills_budget_fraction = 0.15
/// embed_timeout_ms = 800
/// stub_budget_tokens = 1000
/// cache_breakpoint_threshold_tokens = 500
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Budget tokens par défaut quand `budget_tokens` et `max_tokens` sont absents.
    ///
    /// Borné en dur à `[1, 8000]` dans `assemble_assembled` (cap anti-DoS).
    #[serde(default = "ContextConfig::default_budget_tokens")]
    pub default_budget_tokens: u32,

    /// Plafond de candidats récupérés par le retrieval RRF.
    ///
    /// La sélection budget-aware borne la liste finale ; `top_n_candidates` est
    /// un plafond de retrieval (oversample pour que le scoring ait suffisamment de
    /// candidats à trier).
    #[serde(default = "ContextConfig::default_top_n_candidates")]
    pub top_n_candidates: usize,

    /// Nombre maximum de skills injectés dans le contexte (F-58).
    #[serde(default = "ContextConfig::default_max_skills")]
    pub max_skills: usize,

    /// Fraction du budget allouée aux skills (F-58).
    ///
    /// Plancher absolu appliqué après calcul : 64 tokens.
    /// Exemple : `0.15 × 2000 = 300` tokens max skills.
    #[serde(default = "ContextConfig::default_skills_budget_fraction")]
    pub skills_budget_fraction: f64,

    /// Timeout (ms) for the embedding computation during retrieval.
    #[serde(default = "ContextConfig::default_embed_timeout_ms")]
    pub embed_timeout_ms: u64,

    /// Token budget allocated to stub generation.
    ///
    /// Caps the sum of estimated tokens for stubs produced from candidates that exceed
    /// the inline budget. Candidates beyond this cap are dropped.
    /// Value 0 disables stubs (all notes are either inlined or dropped).
    #[serde(default = "ContextConfig::default_stub_budget_tokens")]
    pub stub_budget_tokens: u32,

    /// Token threshold above which `cache_breakpoint_hint = true`.
    ///
    /// Consumer signal: when `budget_used > cache_breakpoint_threshold_tokens`,
    /// set a `cache_control` on the `tool_result` to enable prompt caching.
    /// Value 0 → hint always false (threshold impossible to exceed).
    /// Default: 500 tokens.
    #[serde(default = "ContextConfig::default_cache_breakpoint_threshold_tokens")]
    pub cache_breakpoint_threshold_tokens: u32,
}

impl ContextConfig {
    fn default_budget_tokens() -> u32 {
        2000
    }

    fn default_top_n_candidates() -> usize {
        50
    }

    fn default_max_skills() -> usize {
        3
    }

    fn default_skills_budget_fraction() -> f64 {
        0.15
    }

    fn default_embed_timeout_ms() -> u64 {
        800
    }

    fn default_stub_budget_tokens() -> u32 {
        1000
    }

    fn default_cache_breakpoint_threshold_tokens() -> u32 {
        500
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            default_budget_tokens: Self::default_budget_tokens(),
            top_n_candidates: Self::default_top_n_candidates(),
            max_skills: Self::default_max_skills(),
            skills_budget_fraction: Self::default_skills_budget_fraction(),
            embed_timeout_ms: Self::default_embed_timeout_ms(),
            stub_budget_tokens: Self::default_stub_budget_tokens(),
            cache_breakpoint_threshold_tokens: Self::default_cache_breakpoint_threshold_tokens(),
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
    /// Used for fast-fail detection of storage path divergence.
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

/// Backing store for the JWT revocation list.
///
/// Typed rather than `String` so that any unknown value — a typo, a wrong case — is a
/// **deserialization error at startup** instead of a silent degraded mode. Before this
/// hardening, any third value passed [`gradatum_auth::revocation::boot_guard_check`]
/// (which rejects only the exact string `"memory"`) *and* selected the in-memory store in
/// `main.rs` (which selects SQLite only on the exact string `"sqlite"`): token revocations
/// were then lost on every restart, with a single `warn!` as the only signal.
///
/// The wire representation is lowercase: `"sqlite"` | `"memory"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevocationStoreKind {
    /// Persistent SQLite store — production default, survives a restart.
    Sqlite,
    /// In-memory store — DEV only, loses all revocations on restart. Refused by
    /// [`gradatum_auth::revocation::boot_guard_check`] on a non-loopback bind (caveat C2).
    Memory,
}

impl RevocationStoreKind {
    /// Wire representation — identical to the string produced by `serde`.
    ///
    /// Bridges to [`gradatum_auth::revocation::boot_guard_check`], whose `&str` signature
    /// is part of the published API of `gradatum-auth`. This is the **single** conversion
    /// point; its agreement with `serde` is pinned by a test, so the guard and the
    /// serialized form can never diverge.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Memory => "memory",
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
    // NOTE: `jwt_public_key_path` and `jwt_private_key_path` used to sit here.
    // Neither was ever read to obtain a signing key: the server signs and
    // verifies with the raw Ed25519 seed `<storage.root>/config/jwt-signing-key.secret`
    // (see `gradatum_auth::key_store`). `jwt_public_key_path` had no reader at
    // all; `jwt_private_key_path` was consumed only for its parent directory.
    // Keeping them advertised a key-rotation surface that did nothing — rotating
    // the PEM changed no signature, and the only real key was documented nowhere.
    // They are ignored when still present in `server.toml` (no `deny_unknown_fields`).
    /// TTL for human/Studio tokens (default 3600 s = 1 h).
    pub jwt_ttl_human_secs: u64,
    /// TTL for service bearer tokens (default 86400 s = 24 h).
    pub jwt_ttl_service_secs: u64,
    /// `"memory"` (DEV only, emits WARN) | `"sqlite"` (production default).
    ///
    /// Any other value is **refused when the configuration is loaded** — the server does
    /// not start. See [`RevocationStoreKind`].
    pub revocation_store: RevocationStoreKind,
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

/// Archive retention configuration (F-100 increment 1.6).
///
/// A deleted note is **archived** (moved under `.archive/`) rather than destroyed;
/// the registry-driven GC physically destroys archives older than `retention_days`.
/// Default: 60 days.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveConfig {
    /// Days an archived note is retained before the GC destroys it. Default: 60.
    #[serde(default = "default_archive_retention_days")]
    pub retention_days: u32,
    /// Interval between archive-GC passes in seconds. Default: 21600 (6 h).
    #[serde(default = "default_archive_gc_interval_secs")]
    pub gc_interval_secs: u64,
    /// Maximum archives destroyed per GC pass (anti-burst cap). Default: 500.
    #[serde(default = "default_archive_gc_batch_limit")]
    pub gc_batch_limit: u32,
}

fn default_archive_retention_days() -> u32 {
    60
}
fn default_archive_gc_interval_secs() -> u64 {
    21_600 // 6h
}
fn default_archive_gc_batch_limit() -> u32 {
    500
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            retention_days: default_archive_retention_days(),
            gc_interval_secs: default_archive_gc_interval_secs(),
            gc_batch_limit: default_archive_gc_batch_limit(),
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

/// Auto-promotion configuration for the review backlog job.
///
/// Default TTL: 14 days. Interval: 3600 s (1 h). Max per tick: 200 notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPromoteConfig {
    /// Enable or disable the auto-promotion job. Default: `true`.
    #[serde(default = "default_review_promote_enabled")]
    pub enabled: bool,
    /// Age in days before a review note is promoted to `Live`. Default: 14.
    #[serde(default = "default_review_promote_age_days")]
    pub age_days: u32,
    /// Interval between promotion passes in seconds. Default: 3600 (1 h).
    #[serde(default = "default_review_promote_interval_secs")]
    pub interval_secs: u64,
    /// Maximum notes promoted per tick (anti-burst cap). Default: 200.
    #[serde(default = "default_review_promote_max_per_tick")]
    pub max_per_tick: usize,
}

fn default_review_promote_enabled() -> bool {
    true
}
fn default_review_promote_age_days() -> u32 {
    14
}
fn default_review_promote_interval_secs() -> u64 {
    3_600 // 1h
}
fn default_review_promote_max_per_tick() -> usize {
    200
}

impl Default for ReviewPromoteConfig {
    fn default() -> Self {
        Self {
            enabled: default_review_promote_enabled(),
            age_days: default_review_promote_age_days(),
            interval_secs: default_review_promote_interval_secs(),
            max_per_tick: default_review_promote_max_per_tick(),
        }
    }
}

/// F-51 audit / dedup pass configuration.
///
/// Detection-only (Option A): the pass writes a report artifact under the storage root and
/// never mutates the vault. **Disabled by default** — no silent activation. Detection
/// thresholds are not exposed here: they live in
/// [`gradatum_curator::audit::AuditThresholds::default`] until an operator needs to vary them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Enable or disable the audit pass. Default: `false`.
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,
    /// Interval between audit passes in seconds. Default: 86400 (24 h). Floor 60 s in `main.rs`.
    #[serde(default = "default_audit_interval_secs")]
    pub interval_secs: u64,
    /// Safety cap on notes scanned per pass (anti-DoS on the O(n²) pairing). Default: 5000.
    #[serde(default = "default_audit_max_scan")]
    pub max_scan: usize,
}

fn default_audit_enabled() -> bool {
    false
}
fn default_audit_interval_secs() -> u64 {
    86_400 // 24 h
}
fn default_audit_max_scan() -> usize {
    5_000
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            interval_secs: default_audit_interval_secs(),
            max_scan: default_audit_max_scan(),
        }
    }
}

/// F-111 graduated-forgetting (auto-downgrade) configuration.
///
/// Drives the relevance axis of the audit pass: old, unused, low-trust `live`
/// notes are proposed (dry-run) and — behind `enabled` (default `false`) — auto
/// **downgraded** (reversible, never deleted), capped and window-guarded.
/// **Disabled by default** — activation is opt-in via configuration. Thresholds mirror
/// [`gradatum_curator::audit::IrrelevanceThresholds`] defaults (90 / 0.6 / 30).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DowngradeConfig {
    /// Enables the auto-downgrade executor. Default `false` (gated activation §7).
    #[serde(default)]
    pub enabled: bool,
    /// Minimum note age (days) before a note can be a candidate. Default 90.
    #[serde(default = "default_downgrade_age_min_days")]
    pub age_min_days: u32,
    /// Trust strictly below this value qualifies. Default 0.6.
    #[serde(default = "default_downgrade_trust_max")]
    pub trust_max: f64,
    /// Usage observation window (days). Default 30.
    #[serde(default = "default_downgrade_usage_window_days")]
    pub usage_window_days: u32,
    /// Safety cap on downgrades per pass. Default 50.
    #[serde(default = "default_downgrade_max_per_run")]
    pub max_per_run: usize,
    /// Extra protected sections (kebab-case) added to the base set. The base set
    /// ([`gradatum_core::section::Section::PROTECTED_DOWNGRADE`]) is never removable by config.
    #[serde(default)]
    pub protected_extra: Vec<String>,
}

fn default_downgrade_age_min_days() -> u32 {
    90
}
fn default_downgrade_trust_max() -> f64 {
    0.6
}
fn default_downgrade_usage_window_days() -> u32 {
    30
}
fn default_downgrade_max_per_run() -> usize {
    50
}

impl DowngradeConfig {
    /// Fail-loud boot validation of the configuration invariants.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a human-readable message when any bound is violated:
    /// `age_min_days ≥ 1`, `usage_window_days ≥ 1`, `max_per_run ≥ 1`,
    /// `0.0 < trust_max ≤ 1.0`.
    pub fn validate(&self) -> Result<(), String> {
        if self.age_min_days < 1 {
            return Err("[downgrade] age_min_days must be >= 1".to_string());
        }
        if self.usage_window_days < 1 {
            return Err("[downgrade] usage_window_days must be >= 1".to_string());
        }
        if self.max_per_run < 1 {
            return Err("[downgrade] max_per_run must be >= 1".to_string());
        }
        if !(self.trust_max > 0.0 && self.trust_max <= 1.0) {
            return Err(format!(
                "[downgrade] trust_max must be in (0.0, 1.0] (got {})",
                self.trust_max
            ));
        }
        Ok(())
    }

    /// Resolves the config thresholds into the curator rule input.
    #[must_use]
    pub fn thresholds(&self) -> gradatum_curator::audit::IrrelevanceThresholds {
        gradatum_curator::audit::IrrelevanceThresholds {
            age_min_days: self.age_min_days,
            trust_max: self.trust_max,
            usage_window_days: self.usage_window_days,
        }
    }

    /// The effective protected set: base [`gradatum_core::section::Section::PROTECTED_DOWNGRADE`]
    /// ∪ `protected_extra`. The base entries are never removable by config (add-only).
    #[must_use]
    pub fn protected_sections(&self) -> Vec<&str> {
        let mut out: Vec<&str> = gradatum_core::section::Section::PROTECTED_DOWNGRADE.to_vec();
        for s in &self.protected_extra {
            let s = s.as_str();
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

impl Default for DowngradeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            age_min_days: default_downgrade_age_min_days(),
            trust_max: default_downgrade_trust_max(),
            usage_window_days: default_downgrade_usage_window_days(),
            max_per_run: default_downgrade_max_per_run(),
            protected_extra: Vec::new(),
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
                    .expect("invalid default loopback address — literal constant"),
                metrics_bind: "127.0.0.1:19091"
                    .parse()
                    .expect("invalid default metrics loopback address — literal constant"),
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
                jwt_ttl_human_secs: 3600,
                jwt_ttl_service_secs: 86400,
                revocation_store: RevocationStoreKind::Sqlite,
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
            archive: ArchiveConfig::default(),
            scoring: ScoringConfig::default(),
            salience: SalienceConfig::default(),
            studio: StudioConfig::default(),
            internal_api: InternalApiConfig::default(),
            search: SearchConfig::default(),
            review_promote: ReviewPromoteConfig::default(),
            audit: AuditConfig::default(),
            downgrade: DowngradeConfig::default(),
            context: ContextConfig::default(),
            proactive_recall: ProactiveRecallConfig::default(),
            multi_tenant: MultiTenantConfig::default(),
            // Défaut vide : aucun override per-vault ⇒ résolveurs = global exact (byte-identical).
            per_vault: std::collections::HashMap::new(),
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
    #[error("figment error: {0}")]
    Figment(Box<figment::Error>),
    #[error(
        "bind/TLS fail-closed: bind={bind} is non-loopback without TLS configured. \
        Advice: set [server.tls] or change bind to 127.0.0.1:19090"
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

    /// Config salience EFFECTIVE pour `vault_id` — override per-vault si présent, sinon global (A6).
    ///
    /// Aucun override configuré (cas par défaut) ⇒ renvoie `&self.salience` (global exact).
    /// Lecture seule, sans allocation. Voir [`ServerConfig::per_vault`].
    ///
    /// Câblé au boot (L6) via [`ServerConfig::resolve_salience_per_vault`] : les params résolus
    /// par vault sont pré-calculés et injectés dans `AppState::salience_per_vault`, jamais
    /// résolus dans le read hot-path (aucune allocation par requête).
    #[must_use]
    pub fn salience_for(&self, vault_id: &str) -> &SalienceConfig {
        self.per_vault
            .get(vault_id)
            .and_then(|o| o.salience.as_ref())
            .unwrap_or(&self.salience)
    }

    /// Config review-promote EFFECTIVE pour `vault_id` — override per-vault si présent, sinon global (A6).
    ///
    /// Aucun override configuré (cas par défaut) ⇒ renvoie `&self.review_promote` (global exact).
    /// Lecture seule, sans allocation. Voir [`ServerConfig::per_vault`].
    ///
    /// Câblé (L6) dans la boucle per-vault ON de [`crate::review_promote::promote_tick`] : chaque
    /// vault actif est promu selon sa config effective. Le chemin OFF (`promote_once`, mono-`main`)
    /// n'en dépend pas (byte-identical).
    #[must_use]
    pub fn review_promote_for(&self, vault_id: &str) -> &ReviewPromoteConfig {
        self.per_vault
            .get(vault_id)
            .and_then(|o| o.review_promote.as_ref())
            .unwrap_or(&self.review_promote)
    }

    /// Pré-résout les params salience EFFECTIFS par vault, pour injection au boot dans
    /// `AppState::salience_per_vault` (L6, overrides A6 `[per_vault.<id>.salience]`).
    ///
    /// Contient une entrée pour CHAQUE vault porteur d'un override salience (présent), la valeur
    /// encodant l'état résolu — `Some(params)` si actif, `None` si désactivé (cf. sémantique à
    /// TROIS états ci-dessous, via [`SalienceConfig::resolve`]). **Map vide** si aucun override
    /// salience ⇒ tout vault retombe sur le global dans le hot-path (chemin byte-identical). Tout
    /// le coût de résolution est payé UNE fois au boot — aucune allocation au read-time.
    ///
    /// Sémantique à TROIS états — un override *présent* est TOUJOURS inséré, seul un vault
    /// **sans** override est absent de la map. Alignée sur `review_promote`
    /// (`review_promote_for` + `if !cfg_eff.enabled { continue }`) :
    /// - vault **absent** ⇒ aucun override ⇒ le hot-path retombe sur le global ;
    /// - `Some(params)` ⇒ override présent ET actif (`enabled = true`) ⇒ params raffinés ;
    /// - `None` ⇒ override présent qui **désactive** la salience (`enabled = false`) ⇒ le
    ///   hot-path honore la désactivation (salience neutralisée pour ce vault) au lieu de
    ///   retomber par erreur sur le global. C'est le fix du footgun C1 (post-mortem L6) :
    ///   `enabled = false` désactivait l'INVERSE (retombée silencieuse sur le global actif).
    #[must_use]
    pub fn resolve_salience_per_vault(
        &self,
    ) -> std::collections::HashMap<String, Option<std::sync::Arc<gradatum_search::SalienceParams>>>
    {
        self.per_vault
            .iter()
            .filter_map(|(vid, ov)| {
                // Ne retenir que les vaults porteurs d'un override salience (absent ⇒ global).
                ov.salience.as_ref()?;
                // `Some` si l'override est actif, `None` s'il désactive (symétrie disable).
                let entry = self.salience_for(vid).resolve().map(std::sync::Arc::new);
                Some((vid.clone(), entry))
            })
            .collect()
    }

    /// Garde deploy (C3, post-mortem L6) : valide fail-loud CHAQUE override salience per-vault
    /// (`[per_vault.<id>.salience]`) au boot, avec la même règle que le global
    /// ([`SalienceConfig::validate`] : `k_norm > 0`).
    ///
    /// La map `salience_per_vault` est consultée dès que la salience GLOBALE est active
    /// (indépendamment de `multi_tenant`) ; un override per-vault invalide (ex. `k_norm <= 0`)
    /// produirait sinon des `SalienceParams` corrompus injectés silencieusement dans le
    /// hot-path. Cette garde interdit le boot tant qu'un override n'est pas validé.
    ///
    /// # Errors
    ///
    /// Renvoie `Err` (message préfixé du vault fautif) dès qu'un override échoue à `validate`.
    pub fn validate_per_vault_salience(&self) -> Result<(), String> {
        for (vault_id, ov) in &self.per_vault {
            if let Some(sal) = ov.salience.as_ref() {
                sal.validate()
                    .map_err(|e| format!("[per_vault.{vault_id}.salience] {e}"))?;
            }
        }
        Ok(())
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
mod salience_config_tests {
    use super::*;

    // F-110 Phase 2 : défaut = OFF avec les tunables spec
    #[test]
    fn salience_config_default_is_disabled_with_spec_values() {
        let c = SalienceConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.gamma, 0.10);
        assert_eq!(c.k_norm, 10.0);
        assert_eq!(c.kind_weights.get("read"), Some(&3.0));
        assert_eq!(c.kind_weights.get("recall-accepted"), Some(&5.0));
        assert_eq!(c.kind_weights.get("search-hit-top3"), Some(&2.0));
        assert_eq!(c.kind_weights.get("recall-surfaced"), Some(&1.0));
        assert_eq!(c.kind_weights.get("search-hit"), Some(&0.5));
    }

    // k_norm ≤ 0 ⇒ rejet (fail-loud au boot)
    #[test]
    fn salience_config_rejects_non_positive_k_norm() {
        let c = SalienceConfig {
            k_norm: 0.0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = SalienceConfig {
            k_norm: -1.0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        assert!(SalienceConfig::default().validate().is_ok());
    }

    // resolve : OFF ⇒ None ; ON ⇒ Some(params fidèles)
    #[test]
    fn salience_config_resolve_maps_enabled_flag() {
        assert!(SalienceConfig::default().resolve().is_none());
        let c = SalienceConfig {
            enabled: true,
            ..Default::default()
        };
        let p = c.resolve().expect("enabled ⇒ Some");
        assert_eq!(p.gamma, 0.10);
        assert_eq!(p.k_norm, 10.0);
        assert_eq!(p.kind_weights.len(), 5);
    }

    // TOML partiel : section absente ⇒ défaut OFF (non-régression config existantes)
    #[test]
    fn salience_config_absent_section_defaults_off() {
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
        assert!(!cfg.salience.enabled);
        assert_eq!(cfg.salience.k_norm, 10.0);
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

    /// Token de l'API admin (F-100 incrément 1.6 — delete/restore/purge opérateur).
    ///
    /// **Distinct** de `token` (worker) : gate les endpoints `/internal/v1/admin/*`
    /// via `X-Gradatum-Admin: Bearer <token>`. Le worker (qui ne détient que `token`)
    /// ne peut PAS atteindre la surface de mutation admin. `None` = endpoints admin
    /// désactivés (fail-closed). Sur le même listener loopback que le worker.
    ///
    /// Note : stocké en `String` pour la désérialisation TOML. Converti en
    /// `secrecy::SecretString` par `main.rs` via `state.with_admin_api_token()`.
    /// wired: `gradatum-server/src/internal/admin_auth.rs`. Jamais loggué.
    #[serde(default)]
    pub admin_token: Option<String>,
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
            admin_token: None,
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
            .field(
                "admin_token",
                &self.admin_token.as_ref().map(|_| "[REDACTED]"),
            )
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
            "internal API token too short ({} chars < {} minimum) — use a strong token (e.g. `openssl rand -hex 32`)",
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
            admin_token: Some("super-secret-admin-token-32-chars".to_string()),
        };
        let debug_str = format!("{cfg:?}");
        assert!(
            !debug_str.contains("super-secret-token-32-chars-long"),
            "le token réel NE DOIT PAS apparaître dans Debug : {debug_str:?}"
        );
        assert!(
            !debug_str.contains("super-secret-admin-token-32-chars"),
            "le token admin réel NE DOIT PAS apparaître dans Debug : {debug_str:?}"
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
            err.contains("too short"),
            "message d'erreur doit mentionner 'too short' : {err:?}"
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

#[cfg(test)]
mod review_promote_config_tests {
    use super::*;

    /// Vérifie que `ReviewPromoteConfig` s'initialise avec les valeurs par défaut attendues
    /// depuis un TOML vide (ou via `Default`).
    #[test]
    fn review_promote_defaults() {
        let cfg: ServerConfig = toml::from_str("").unwrap_or_else(|_| ServerConfig::default());
        assert!(cfg.review_promote.enabled);
        assert_eq!(cfg.review_promote.age_days, 14);
        assert_eq!(cfg.review_promote.interval_secs, 3600);
        assert_eq!(cfg.review_promote.max_per_tick, 200);
    }
}

#[cfg(test)]
mod proactive_recall_config_tests {
    use super::*;

    /// TOML minimal partagé : sections obligatoires, pas de `[proactive_recall]`.
    const MINIMAL_TOML: &str = r#"
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

    /// TOML without a `[proactive_recall]` section → default values 900/20/8.
    ///
    /// The TOML → `ProactiveRecallConfig` deserialisation must produce the same values
    /// as `ProactiveRecallConfig::default()`.
    /// The tick logic is tested separately in `tests/proactive_refresh.rs`;
    /// this test verifies the config link.
    #[test]
    fn proactive_recall_config_defaults() {
        let cfg: ServerConfig = toml::from_str(MINIMAL_TOML).expect("parse");
        assert_eq!(
            cfg.proactive_recall.refresh_interval_secs, 900,
            "refresh_interval_secs par défaut doit être 900"
        );
        assert_eq!(
            cfg.proactive_recall.recent_k, 20,
            "recent_k par défaut doit être 20"
        );
        assert_eq!(
            cfg.proactive_recall.surface_size, 8,
            "surface_size par défaut doit être 8"
        );
    }

    /// Partial section: missing field → default value applied for that field.
    #[test]
    fn proactive_recall_config_partial_override() {
        let toml = format!("{MINIMAL_TOML}\n[proactive_recall]\nrecent_k = 5\n");
        let cfg: ServerConfig = toml::from_str(&toml).expect("parse");
        // Champ fourni explicitement.
        assert_eq!(cfg.proactive_recall.recent_k, 5);
        // Champs absents → défauts non modifiés.
        assert_eq!(cfg.proactive_recall.refresh_interval_secs, 900);
        assert_eq!(cfg.proactive_recall.surface_size, 8);
    }

    /// 60s floor: TOML value < 60 → `.max(60)` in `main.rs` clamps to 60.
    ///
    /// Deserialisation preserves the raw value (10 stays 10 in the config).
    /// The floor is applied at the point where the `tokio::interval` is built
    /// in `main.rs` via `refresh_interval_secs.max(60)`.
    /// This test verifies that the expression used in `main.rs` is correct.
    #[test]
    fn proactive_recall_config_floor_60s() {
        let toml = format!("{MINIMAL_TOML}\n[proactive_recall]\nrefresh_interval_secs = 10\n");
        let cfg: ServerConfig = toml::from_str(&toml).expect("parse");
        // La valeur brute est préservée dans la config.
        assert_eq!(
            cfg.proactive_recall.refresh_interval_secs, 10,
            "la désérialisation ne doit pas coercer la valeur brute"
        );
        // Le plancher appliqué par main.rs (.max(60)) donne bien 60.
        let effective = cfg.proactive_recall.refresh_interval_secs.max(60);
        assert_eq!(
            effective, 60,
            "plancher 60s : refresh_interval_secs = 10 → effective = 60"
        );
    }
}

#[cfg(test)]
mod context_config_tests {
    use super::*;

    /// Vérifie que `ContextConfig` s'initialise avec les valeurs par défaut attendues
    /// (via `Default` et via désérialisation depuis un TOML vide).
    #[test]
    fn context_config_defaults() {
        let c = ContextConfig::default();
        assert_eq!(c.default_budget_tokens, 2000);
        assert_eq!(c.top_n_candidates, 50);
        assert_eq!(c.max_skills, 3);
        assert!((c.skills_budget_fraction - 0.15).abs() < f64::EPSILON);
        assert_eq!(c.embed_timeout_ms, 800);
    }

    /// Vérifie que `ServerConfig` expose le champ `context` avec les bons défauts
    /// depuis un TOML vide (zéro section `[context]`).
    #[test]
    fn server_config_context_section_defaults() {
        let cfg: ServerConfig = toml::from_str("").unwrap_or_else(|_| ServerConfig::default());
        assert_eq!(cfg.context.default_budget_tokens, 2000);
        assert_eq!(cfg.context.top_n_candidates, 50);
        assert_eq!(cfg.context.max_skills, 3);
    }
}

#[cfg(test)]
mod downgrade_config_tests {
    use super::*;

    #[test]
    fn downgrade_config_defaults_off_with_spec_values() {
        let c = DowngradeConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.age_min_days, 90);
        assert_eq!(c.trust_max, 0.6);
        assert_eq!(c.usage_window_days, 30);
        assert_eq!(c.max_per_run, 50);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn downgrade_config_validation_fail_loud() {
        assert!(
            DowngradeConfig {
                age_min_days: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DowngradeConfig {
                usage_window_days: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DowngradeConfig {
                max_per_run: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DowngradeConfig {
                trust_max: 0.0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DowngradeConfig {
                trust_max: 1.5,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn downgrade_config_protected_base_not_removable() {
        let c = DowngradeConfig {
            protected_extra: vec!["experiments".into()],
            ..Default::default()
        };
        let p = c.protected_sections();
        assert!(p.contains(&"council")); // base toujours là
        assert!(p.contains(&"decisions")); // base toujours là
        assert!(p.contains(&"experiments")); // extension ajoutée
    }
}

#[cfg(test)]
mod per_vault_config_tests {
    use super::*;

    /// Défaut (map `per_vault` vide) : tout vault retombe sur la config globale exacte —
    /// résolveurs byte-identical au comportement pré-A6. Vérifie l'identité de référence
    /// (le résolveur renvoie bien `&self.salience` / `&self.review_promote`, pas une copie).
    #[test]
    fn per_vault_config_override_falls_back_to_global() {
        let cfg = ServerConfig::default();
        assert!(
            cfg.per_vault.is_empty(),
            "invariant : aucun override par défaut"
        );

        // salience : même pointeur que le global (aucune allocation, pas de divergence).
        assert!(
            std::ptr::eq(cfg.salience_for("main"), &cfg.salience),
            "salience_for(main) doit retomber sur la config globale"
        );
        assert!(
            std::ptr::eq(cfg.salience_for("vault-b"), &cfg.salience),
            "salience_for(vault inconnu) doit retomber sur la config globale"
        );

        // review_promote : idem.
        assert!(
            std::ptr::eq(cfg.review_promote_for("main"), &cfg.review_promote),
            "review_promote_for(main) doit retomber sur la config globale"
        );
        assert!(
            std::ptr::eq(cfg.review_promote_for("vault-b"), &cfg.review_promote),
            "review_promote_for(vault inconnu) doit retomber sur la config globale"
        );
    }

    /// Override présent : le vault ciblé lit l'override, les autres restent au global.
    #[test]
    fn override_applied_when_present() {
        let mut cfg = ServerConfig::default();
        // Global salience = OFF par défaut ; override vault-b = ON avec gamma distinct.
        assert!(!cfg.salience.enabled, "précondition : global salience OFF");

        let ov_salience = SalienceConfig {
            enabled: true,
            gamma: 0.99,
            ..SalienceConfig::default()
        };
        let ov_review = ReviewPromoteConfig {
            age_days: 999,
            ..ReviewPromoteConfig::default()
        };
        cfg.per_vault.insert(
            "vault-b".to_string(),
            PerVaultOverride {
                salience: Some(ov_salience),
                review_promote: Some(ov_review),
            },
        );

        // vault-b lit l'override.
        let sb = cfg.salience_for("vault-b");
        assert!(sb.enabled, "vault-b : salience override actif");
        assert!(
            (sb.gamma - 0.99).abs() < f64::EPSILON,
            "vault-b : gamma override lu"
        );
        assert_eq!(
            cfg.review_promote_for("vault-b").age_days,
            999,
            "vault-b : review_promote override lu"
        );

        // main (pas d'override) reste au global exact.
        assert!(
            !cfg.salience_for("main").enabled,
            "main : salience reste global (OFF)"
        );
        assert_eq!(
            cfg.review_promote_for("main").age_days,
            cfg.review_promote.age_days,
            "main : review_promote reste global"
        );
    }

    /// Preuve B (L6) — équivalence défaut per-vault == global résolu.
    ///
    /// Salience globale ON, AUCUN override per-vault : `resolve_salience_per_vault` renvoie une
    /// map VIDE, et `salience_for(<n'importe quel vault>).resolve()` égale le global résolu (ce
    /// qui est injecté dans `AppState::salience`). C'est la garantie que, map vide, le hot-path
    /// (`salience_per_vault.get(v).unwrap_or(global)`) utilise exactement le global.
    #[test]
    fn resolve_salience_per_vault_empty_matches_global_resolved() {
        let cfg = ServerConfig {
            salience: SalienceConfig {
                enabled: true,
                gamma: 0.42,
                k_norm: 7.5,
                ..SalienceConfig::default()
            },
            ..Default::default()
        };

        // Aucun override ⇒ map pré-résolue VIDE (le hot-path retombe donc sur le global).
        assert!(
            cfg.resolve_salience_per_vault().is_empty(),
            "sans override, la map per-vault pré-résolue est vide"
        );

        // Le global résolu (== contenu de AppState::salience) est bien Some avec les params posés.
        // `SalienceParams` n'implémente pas PartialEq ⇒ comparaison champ par champ.
        let global = cfg.salience.resolve().expect("global salience ON ⇒ Some");
        for vault in ["main", "vault-inconnu"] {
            let resolved = cfg
                .salience_for(vault)
                .resolve()
                .expect("défaut per-vault ⇒ global ON ⇒ Some");
            assert!(
                (resolved.gamma - global.gamma).abs() < f64::EPSILON,
                "{vault} : gamma == global résolu"
            );
            assert!(
                (resolved.k_norm - global.k_norm).abs() < f64::EPSILON,
                "{vault} : k_norm == global résolu"
            );
            assert_eq!(
                resolved.kind_weights, global.kind_weights,
                "{vault} : kind_weights == global résolu"
            );
        }
    }

    /// Preuve B (suite) — TOUT override salience présent est pré-résolu dans la map : actif ⇒
    /// `Some(params)`, désactivé (`enabled=false`) ⇒ `None` (fix C1). Un vault sans override en
    /// est absent (⇒ fallback global au read-time).
    #[test]
    fn resolve_salience_per_vault_contains_all_present_overrides() {
        let cfg = ServerConfig {
            salience: SalienceConfig {
                enabled: true,
                ..SalienceConfig::default()
            },
            per_vault: [
                (
                    "vault-on".to_string(),
                    PerVaultOverride {
                        salience: Some(SalienceConfig {
                            enabled: true,
                            gamma: 0.99,
                            ..SalienceConfig::default()
                        }),
                        review_promote: None,
                    },
                ),
                (
                    "vault-off".to_string(),
                    PerVaultOverride {
                        // Override qui DÉSACTIVE la salience pour ce vault (résout à None).
                        salience: Some(SalienceConfig {
                            enabled: false,
                            ..SalienceConfig::default()
                        }),
                        review_promote: None,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let map = cfg.resolve_salience_per_vault();
        // vault-on : override actif ⇒ pré-résolu en `Some(params)`.
        let on = map
            .get("vault-on")
            .expect("vault-on présent dans la map")
            .as_ref()
            .expect("vault-on : override actif ⇒ Some(params)");
        assert!(
            (on.gamma - 0.99).abs() < f64::EPSILON,
            "vault-on : gamma override pré-résolu"
        );
        // vault-off (fix C1) : override `enabled=false` ⇒ PRÉSENT avec valeur `None`
        // (désactivation honorée au read-time, symétrie review_promote), et NON plus absent.
        assert!(
            map.contains_key("vault-off"),
            "vault-off (override enabled=false) doit être présent (désactivation explicite)"
        );
        assert!(
            map.get("vault-off").expect("vault-off présent").is_none(),
            "vault-off : override enabled=false ⇒ valeur None (salience neutralisée)"
        );
        assert_eq!(
            map.len(),
            2,
            "les DEUX overrides (actif + désactivé) sont dans la map"
        );
    }
}
