//! Chargement de la configuration curator depuis le TOML serveur.
//!
//! Module extrait de `main.rs` pour être testable via les tests d'intégration.
//! Expose [`build_curator_pipeline`] qui instancie le bon backend selon
//! la section `[curator]` du fichier de configuration.
//!
//! ## Backward compat
//!
//! - Fichier TOML absent → mode heuristique offline (pas une erreur).
//! - Section `[curator]` absente → mode heuristique (défaut TOML).
//! - `backend = "openai_compat"` + `[curator.llm]` valide → mode LLM + CircuitBreaker.
//! - `backend = "openai_compat"` MAIS `[curator.llm]` absent → warn + fallback heuristique.
//!
//! ## Évolution prévue
//!
//! Les backends `ollama_compat`, `anthropic_compat`, `gemini_compat` sont délégués
//! à `gradatum-curator::CuratorPipeline::from_config()` — leur implémentation
//! est transparente depuis ce module.

use figment::{
    providers::{Format, Toml},
    Figment,
};
use gradatum_curator::{CuratorLlmConfig, CuratorPipeline, CuratorPipelineConfig};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

// ── Structs de désérialisation pour la section [curator] du TOML ─────────────
//
// Ces structs locales évitent de dépendre de `gradatum-server` (dépendance cyclique).
// Synchronisées manuellement avec `gradatum_server::config::CuratorConfig` + `LlmConfig`.
// Pattern DTOs locaux identique à T5 (dispatch.rs).

/// Config curator lue depuis la section `[curator]` du TOML serveur.
///
/// Dupliquée depuis `gradatum_server::config::CuratorConfig` pour éviter
/// la dépendance `gradatum-worker → gradatum-server`.
///
/// ## Champs gating
///
/// Les champs `llm_review_enabled`, `confidence_threshold`, etc. sont propagés
/// vers [`CuratorPipelineConfig`] via [`From<&WorkerCuratorConfig>`].
/// Sans eux, `llm_review_enabled` reste à `false` (défaut) et le LLM n'est
/// jamais appelé quelle que soit la config TOML.
///
/// ## Synchronisation manuelle
///
/// Maintenir en sync avec `gradatum_core::config::CuratorConfig` (9 champs gating).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCuratorConfig {
    /// `"heuristic"` (défaut) | `"openai_compat"` | `"ollama_compat"` | ...
    #[serde(default = "default_curator_backend")]
    pub backend: String,
    /// Config LLM optionnelle. Absente = heuristique pur.
    #[serde(default)]
    pub llm: Option<WorkerLlmConfig>,

    // ── Champs gating (sync avec gradatum_core::config::CuratorConfig) ────────
    /// Seuil heuristique d'admission directe (0.0–1.0). `None` → 0.8.
    #[serde(default)]
    pub heuristic_admit_threshold: Option<f32>,
    /// Statut assigné par défaut par l'heuristique (kebab-case). `None` → "pending-review".
    #[serde(default)]
    pub heuristic_default_status: Option<String>,
    /// Active la revue LLM pour les notes sous `confidence_threshold`. `None` → false.
    #[serde(default)]
    pub llm_review_enabled: Option<bool>,
    /// Seuil de confiance sous lequel la revue LLM est déclenchée. `None` → 0.7.
    #[serde(default)]
    pub confidence_threshold: Option<f32>,
    /// URL de l'endpoint LLM pour la revue (compatible OpenAI Chat API).
    #[serde(default)]
    pub llm_review_endpoint: Option<String>,
    /// Modèle LLM utilisé pour la revue.
    #[serde(default)]
    pub llm_review_model: Option<String>,
    /// Timeout en millisecondes pour les appels LLM de revue. `None` → timeout du backend.
    #[serde(default)]
    pub llm_review_timeout_ms: Option<u32>,
    /// Nombre maximum de tokens générés par le LLM de revue.
    #[serde(default)]
    pub llm_review_max_tokens: Option<u32>,
    /// Comportement en cas d'échec LLM. `None` → "pending-review-fallback".
    /// Valeurs : `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    #[serde(default)]
    pub llm_review_fallback: Option<String>,
}

fn default_curator_backend() -> String {
    "heuristic".to_string()
}

impl Default for WorkerCuratorConfig {
    fn default() -> Self {
        Self {
            backend: default_curator_backend(),
            llm: None,
            heuristic_admit_threshold: None,
            heuristic_default_status: None,
            llm_review_enabled: None,
            confidence_threshold: None,
            llm_review_endpoint: None,
            llm_review_model: None,
            llm_review_timeout_ms: None,
            llm_review_max_tokens: None,
            llm_review_fallback: None,
        }
    }
}

/// Config LLM lue depuis `[curator.llm]` du TOML serveur.
///
/// Dupliquée depuis `gradatum_server::config::LlmConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLlmConfig {
    /// `"openai_compat"` | `"ollama_compat"` | `"anthropic_compat"` | `"gemini_compat"`.
    pub backend: String,
    /// URL de base de l'endpoint (sans path).
    pub base_url: String,
    /// Nom du modèle (ex. `"my-model"`, `"Qwen3-4B-Instruct-2507"`).
    pub model: String,
    /// Nom de la variable d'env portant le bearer token. Absent = pas d'auth.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Timeout par requête en millisecondes. Défaut : 5000 ms.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Nombre maximum de tokens générés par le LLM. `None` → défaut du backend (1024).
    ///
    /// Ce champ est redondant avec `WorkerCuratorConfig.llm_review_max_tokens` mais
    /// conservé ici pour la lisibilité TOML : on peut écrire `[curator.llm] max_tokens = 2048`.
    /// Priorité de résolution : `llm_review_max_tokens` (champ toplevel `[curator]`) est
    /// propagé directement dans `CuratorPipelineConfig` ; ce champ `[curator.llm] max_tokens`
    /// est ignoré par le pipeline (non propagé dans `From<&WorkerCuratorConfig>`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

fn default_timeout_ms() -> u64 {
    5000
}

impl From<&WorkerCuratorConfig> for CuratorPipelineConfig {
    /// Convertit la config TOML worker vers la config pipeline curator.
    ///
    /// Tous les champs gating sont propagés explicitement pour éviter que
    /// `llm_review_enabled` reste à `false` (sa valeur par défaut) malgré
    /// une config TOML `llm_review_enabled = true`.
    fn from(w: &WorkerCuratorConfig) -> Self {
        CuratorPipelineConfig {
            backend: w.backend.clone(),
            llm: w.llm.as_ref().map(|l| CuratorLlmConfig {
                backend: l.backend.clone(),
                base_url: l.base_url.clone(),
                model: l.model.clone(),
                api_key_env: l.api_key_env.clone(),
                timeout_ms: l.timeout_ms,
            }),
            // ── Champs gating — propagation directe depuis TOML ──────────────
            heuristic_admit_threshold: w.heuristic_admit_threshold,
            heuristic_default_status: w.heuristic_default_status.clone(),
            llm_review_enabled: w.llm_review_enabled,
            confidence_threshold: w.confidence_threshold,
            llm_review_endpoint: w.llm_review_endpoint.clone(),
            llm_review_model: w.llm_review_model.clone(),
            llm_review_timeout_ms: w.llm_review_timeout_ms,
            llm_review_max_tokens: w.llm_review_max_tokens,
            llm_review_fallback: w.llm_review_fallback.clone(),
        }
    }
}

/// Retourne `true` si l'erreur figment indique une section absente (pas une erreur TOML).
///
/// `figment::Error` est une linked-list d'erreurs (`prev: Option<Box<Error>>`).
/// `IntoIterator` est implémenté mais consomme la valeur — on clone pour itérer.
/// On considère une section "absente" si toutes les erreurs sont `MissingField`.
fn is_missing_field_error(e: &figment::Error) -> bool {
    // figment::Error est Clone — on clone pour pouvoir itérer sans consommer la ref.
    e.clone().into_iter().all(|inner| inner.missing())
}

/// Charge la section `[curator]` du fichier de config et construit le pipeline.
///
/// ## Comportement
///
/// - Fichier absent → log warn + retour heuristique offline (pas une erreur fatale).
/// - Section `[curator]` absente dans le fichier → mode heuristique (défaut TOML).
/// - `backend = "openai_compat"` + `[curator.llm]` valide → mode LLM avec CircuitBreaker.
/// - `backend = "openai_compat"` MAIS `[curator.llm]` absent → log warn + fallback heuristique.
///
/// ## Effets de bord
///
/// En mode LLM : lit la variable d'environnement `api_key_env` si configurée.
pub fn build_curator_pipeline(config_path: &std::path::Path) -> CuratorPipeline {
    if !config_path.exists() {
        warn!(
            config = %config_path.display(),
            "Fichier config absent — Curator initialisé en mode heuristic offline"
        );
        return CuratorPipeline::new();
    }

    // Charger uniquement la section [curator] du TOML via figment.
    //
    // Stratégie :
    //   1. extract_inner("curator") depuis le TOML (sans .nested() — non applicable ici).
    //   2. Si la section est absente (MissingField) → défauts WorkerCuratorConfig.
    //   3. Si la section est mal formée → log error + fallback heuristic.
    //
    // Note : .nested() est pour les TOML avec profils (ex: [default], [production]).
    // Notre server.toml est un TOML flat avec tables ([curator], [curator.llm]) —
    // il ne faut PAS utiliser .nested(), sinon figment traite [curator] comme un
    // profil et l'extrait retourne MissingField.
    //
    // Pourquoi ne pas utiliser Serialized::defaults() + extract_inner("curator") :
    // Serialized injecte les défauts à la racine du Figment (clés "backend", "llm"),
    // alors que extract_inner("curator") cherche sous le namespace "curator.*".
    // Les deux ne se composent pas — les défauts ne seraient pas vus par extract_inner.
    let curator_cfg: WorkerCuratorConfig = {
        let fig = Figment::new().merge(Toml::file(config_path));
        match fig.extract_inner::<WorkerCuratorConfig>("curator") {
            Ok(cfg) => cfg,
            Err(e) if is_missing_field_error(&e) => {
                // Section [curator] absente du TOML → mode heuristique par défaut.
                info!(
                    config = %config_path.display(),
                    "Section [curator] absente — mode heuristic offline par défaut"
                );
                WorkerCuratorConfig::default()
            }
            Err(e) => {
                error!(
                    config = %config_path.display(),
                    error = %e,
                    "Échec parse config curator — fallback heuristic offline"
                );
                return CuratorPipeline::new();
            }
        }
    };

    let pipeline_cfg = CuratorPipelineConfig::from(&curator_cfg);

    // Validation : si backend LLM mais llm absent → log warn + fallback.
    if curator_cfg.backend != "heuristic" && curator_cfg.llm.is_none() {
        warn!(
            backend = %curator_cfg.backend,
            "backend curator LLM configuré mais [curator.llm] absent — fallback heuristic offline"
        );
        return CuratorPipeline::new();
    }

    let pipeline = CuratorPipeline::from_config(&pipeline_cfg);

    // Logging observabilité au boot — champs gating visibles pour diagnostic.
    // Permet de vérifier que llm_review_enabled=true est bien chargé depuis server.toml.
    info!(
        backend = %curator_cfg.backend,
        llm_review_enabled = curator_cfg.llm_review_enabled.unwrap_or(false),
        confidence_threshold = curator_cfg.confidence_threshold.unwrap_or(0.7),
        heuristic_admit_threshold = ?curator_cfg.heuristic_admit_threshold,
        llm_review_fallback = curator_cfg.llm_review_fallback.as_deref().unwrap_or("pending-review-fallback"),
        base_url = ?curator_cfg.llm.as_ref().map(|l| &l.base_url),
        model = ?curator_cfg.llm.as_ref().map(|l| &l.model),
        timeout_ms = ?curator_cfg.llm.as_ref().map(|l| l.timeout_ms),
        "Curator config loaded"
    );

    pipeline
}
