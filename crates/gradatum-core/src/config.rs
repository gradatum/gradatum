//! Configuration runtime chargée depuis `<vault_root>/.gradatum/config.toml`.
//!
//! See ARCHITECTURE.md for the configuration design.
//!
//! Tous les champs sont `Option<T>` avec `#[serde(default)]` pour permettre les
//! configs partielles. Les defaults sont appliqués aux sites de consommation
//! (ex. `NoteStatus::is_embeddable_default()` quand `embed.embeddable_status` est `None`).
//!
//! ## Chargement
//!
//! ```rust,no_run
//! use gradatum_core::config::VaultConfig;
//! use std::path::Path;
//!
//! let cfg = VaultConfig::load_from_root(Path::new("/my/vault")).unwrap();
//! ```
//!
//! Fichier absent → `VaultConfig::default()` sans erreur.
//! TOML malformé → `ConfigError::Parse`.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Configuration complète d'un vault Gradatum.
///
/// Chargée depuis `<vault_root>/.gradatum/config.toml`. Toutes les sections
/// sont optionnelles — un fichier minimal peut ne contenir que `[vault]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Paramètres généraux du vault (tenant, version schéma).
    #[serde(default)]
    pub vault: VaultSection,

    /// Configuration du pipeline d'embedding (D-perf-1, B21).
    #[serde(default)]
    pub embed: EmbedConfig,

    /// Configuration du curator (D-perf-3, B23).
    #[serde(default)]
    pub curator: CuratorConfig,

    /// Configuration du moteur d'indexation.
    #[serde(default)]
    pub index: IndexConfig,

    /// Configuration du drift detector.
    #[serde(default)]
    pub drift: DriftConfig,

    /// Configuration de l'audit log (caveat C2).
    #[serde(default)]
    pub audit: AuditConfig,
}

/// Section `[vault]` — identité du vault.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultSection {
    /// Tenant par défaut. `None` → "main" appliqué par le storage layer.
    pub default_tenant_id: Option<String>,

    /// Version du schéma SQLite attendue. `None` → pas de vérification stricte.
    pub schema_version: Option<u32>,
}

/// Section `[embed]` — pipeline d'embedding (D-perf-1, B21).
///
/// Contrôle quel backend est utilisé, avec quel modèle, et quels statuts de
/// notes sont éligibles à l'embedding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedConfig {
    /// Statuts de notes pouvant être embeddés (kebab-case, ex. `["live", "pending-review"]`).
    ///
    /// `None` → utiliser `NoteStatus::is_embeddable_default()`.
    ///
    /// **Choix architectural** : `Vec<String>` (pas `Vec<NoteStatus>`) pour que `config.rs`
    /// reste libre de tout type métier et évite tout cycle de dépendance.
    /// La comparaison s'effectue dans `NoteStatus::is_embeddable(&EmbedConfig)` via
    /// `serde_kebab_repr()`. Décision T03b 2026-05-04.
    pub embeddable_status: Option<Vec<String>>,

    /// Identifiant du modèle d'embedding (ex. "bge-m3", "bge-small-en-v1.5").
    pub embedder_id: Option<String>,

    /// Dimensions du vecteur de sortie. `None` → inféré depuis `embedder_id`.
    pub dim: Option<u16>,

    /// Backend d'embedding sélectionné (D-perf-1, B21).
    ///
    /// Valeurs : `"http"` | `"fastembed"` | `"noop"`. `None` → "http".
    pub backend: Option<String>,

    /// Backend de fallback si le backend principal est indisponible.
    pub fallback_backend: Option<String>,

    /// URL du backend HTTP. Requise si `backend = "http"`.
    pub http_url: Option<String>,

    /// Timeout en millisecondes pour les requêtes HTTP d'embedding.
    pub http_timeout_ms: Option<u32>,

    /// Nom du modèle passé dans la requête HTTP.
    pub http_model: Option<String>,
}

/// Section `[curator]` — configuration du pipeline de curation (D-perf-3, B23).
///
/// Contrôle les seuils heuristiques et le passage en revue LLM pour les notes
/// de faible confiance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Seuil heuristique d'admission directe (0.0–1.0).
    /// Au-dessus → admettre sans revue LLM.
    pub heuristic_admit_threshold: Option<f32>,

    /// Statut assigné par défaut par l'heuristique (kebab-case string).
    ///
    /// **Choix architectural** : `String` (pas `NoteStatus`) pour que `config.rs`
    /// reste libre de tout type métier. La résolution s'effectue dans `gradatum-chat`
    /// par comparaison kebab-case. Décision T03b 2026-05-04.
    pub heuristic_default_status: Option<String>,

    /// Active la revue LLM pour les notes sous `confidence_threshold`.
    pub llm_review_enabled: Option<bool>,

    /// Seuil de confiance sous lequel la revue LLM est déclenchée.
    pub confidence_threshold: Option<f32>,

    /// URL de l'endpoint LLM pour la revue (compatible OpenAI Chat API).
    pub llm_review_endpoint: Option<String>,

    /// Modèle LLM utilisé pour la revue.
    pub llm_review_model: Option<String>,

    /// Timeout en millisecondes pour les appels LLM de revue.
    pub llm_review_timeout_ms: Option<u32>,

    /// Nombre maximum de tokens générés par le LLM de revue.
    pub llm_review_max_tokens: Option<u32>,

    /// Comportement en cas d'échec ou de timeout LLM.
    ///
    /// Valeurs : `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    pub llm_review_fallback: Option<String>,
}

/// Section `[index]` — configuration du moteur d'indexation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Backend d'index. Valeurs : `"sqlite"`. `None` → "sqlite".
    pub backend: Option<String>,

    /// Tokeniseur FTS5 pour la recherche plein texte.
    ///
    /// Valeurs : `"unicode61"` | `"ascii"` | `"porter"`. `None` → "unicode61".
    pub fts_tokenizer: Option<String>,
}

/// Section `[drift]` — configuration du drift detector.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriftConfig {
    /// Intervalle entre deux scans de drift en secondes. `None` → 3600.
    pub scan_interval_seconds: Option<u32>,
}

/// Section `[audit]` — configuration de l'audit log (caveat C2).
///
/// Contrôle la rotation, la rétention et le mode de fsync des events d'audit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Politique de rotation du log d'audit.
    ///
    /// Valeurs : `"daily"` | `"weekly"` | `"size-100mb"`. `None` → "daily".
    pub rotation: Option<String>,

    /// Nombre de jours de rétention. `0` = rétention infinie. `None` → 30.
    pub retention_days: Option<u32>,

    /// Mode strict de fsync (council perf 2026-05-04, caveat C2).
    ///
    /// `false` (défaut) = BufWriter 64 KB + fsync toutes les 100 ms ou 100 events.
    /// `true`  = fsync par event, bypass buffer (~200 µs/event NVMe — forensic-grade).
    #[serde(default)]
    pub strict_mode: bool,
}

impl VaultConfig {
    /// Charge `<vault_root>/.gradatum/config.toml`.
    ///
    /// - Fichier absent → `Ok(VaultConfig::default())`.
    /// - TOML malformé → `Err(ConfigError::Parse(...))`.
    /// - Erreur IO autre que NotFound → `Err(ConfigError::Io(...))`.
    ///
    /// # Panics
    ///
    /// Jamais. Toutes les erreurs sont propagées via `Result`.
    pub fn load_from_root(root: &Path) -> Result<Self, ConfigError> {
        let path = root.join(".gradatum").join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).map_err(ConfigError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }
}

/// Erreurs de chargement de la configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Erreur IO (permissions, chemin invalide, etc.).
    #[error("config IO: {0}")]
    Io(#[from] std::io::Error),

    /// TOML malformé ou champ de type incorrect.
    #[error("config parse: {0}")]
    Parse(#[from] toml::de::Error),
}
