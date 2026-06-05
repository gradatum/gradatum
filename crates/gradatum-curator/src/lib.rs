//! # gradatum-curator
//!
//! Pipeline de curation de notes : heuristic gating + LLM review low-confidence
//! + 3 fallback strategies (Phase 1) + cascade 5 fonctions (Phase 2.0b).
//!
//! ## Architecture Phase 1 (Curator<C>)
//!
//! ```text
//! Curator<C: Chat>::decide(note, ctx) → CuratorDecision
//!
//!   step 1 : Heuristic::classify_curator(note, ctx)
//!   step 2 : confidence > threshold → fast path (Heuristic verdict)
//!   step 3 : llm_review_enabled → C::classify_curator(note, ctx)
//!   step 4 : LLM error → FallbackStrategy appliquée
//! ```
//!
//! ## Architecture Phase 2.0b (CuratorPipeline)
//!
//! ```text
//! CuratorPipeline::process(note) → CurateOutcome
//!
//!   step 1 : novelty   — SHA-256 exact + MinHash 128-perm Jaccard ≥ 0.92
//!   step 2 : routing   — heuristique regex 10 sections gradatum
//!   step 3 : tags      — TF-IDF top-5 + kebab-case
//!   step 4 : wikilinks — extraction regex + Jaro-Winkler 0.88 fuzzy
//!   step 5 : dedup     — cosine 0.95 sur embeddings bge-small
//! ```
//!
//! ## Invariant offline-first
//!
//! L'heuristique est toujours exécutée en premier, sans dépendance réseau
//! (invariant #3 / R1 spec §0.4). Le LLM n'est sollicité que pour les notes
//! de faible confiance et seulement si `llm_review_enabled = true`.
//!
//! ## Stability
//!
//! `0.x` — pas de garantie de stabilité d'API. Phase 1 = baseline fonctionnel.
//! Voir [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// ── Modules Phase 1 (conservés pour compatibilité) ──────────────────────────
pub mod decision;
pub mod error;
pub mod workflow;

// ── Modules Phase 2.0b — cascade 5 fonctions ────────────────────────────────
pub mod dedup;
pub mod novelty;
pub mod routing;
pub mod tags;
pub mod wikilinks;

// ── Re-exports Phase 1 ──────────────────────────────────────────────────────
pub use decision::{CuratorDecision, FallbackStrategy};
pub use error::CuratorError;
pub use workflow::Curator;

// ── Imports pour from_config ─────────────────────────────────────────────────
use std::sync::Arc;
use std::time::Duration;

// ── Configuration curator locale (évite dépendance cyclique curator→server) ─

/// Configuration du backend LLM optionnel pour le pipeline curator.
///
/// Struct locale — dupliquée depuis `gradatum_server::config::LlmConfig`
/// pour éviter la dépendance cyclique `gradatum-curator → gradatum-server`.
/// Pattern DTOs locaux établi en T5 pour les mêmes raisons.
///
/// ## Synchronisation
///
/// Si `gradatum_server::config::LlmConfig` évolue, synchroniser cette struct.
/// Une conversion `From<&LlmConfig> for CuratorLlmConfig` est fournie dans
/// `gradatum-server/src/state.rs`.
#[derive(Debug, Clone)]
pub struct CuratorLlmConfig {
    /// Type de backend : `"openai_compat"` | `"ollama_compat"` |
    ///                    `"anthropic_compat"` | `"gemini_compat"`.
    pub backend: String,
    /// URL de base de l'endpoint (sans path).
    pub base_url: String,
    /// Nom du modèle.
    pub model: String,
    /// Nom de la variable d'environnement portant le bearer token.
    /// `None` = pas d'auth (endpoint LAN non authentifié).
    pub api_key_env: Option<String>,
    /// Timeout par requête en millisecondes.
    pub timeout_ms: u64,
}

/// Configuration du pipeline curator (backend + LLM optionnel + champs gating).
///
/// Struct locale — dupliquée depuis `gradatum_server::config::CuratorConfig`.
/// Passée à [`CuratorPipeline::from_config`] depuis `gradatum-server/src/state.rs`.
///
/// ## Champs gating
///
/// Les champs `llm_review_enabled`, `confidence_threshold`, etc. contrôlent
/// le comportement de la cascade. Sans propagation explicite, ils restent à
/// leurs valeurs par défaut (`false`, `0.7`) quelle que soit la config TOML.
#[derive(Debug, Clone)]
pub struct CuratorPipelineConfig {
    /// Backend de classification. Défaut : `"heuristic"`.
    pub backend: String,
    /// Configuration du tier LLM optionnel. `None` = heuristic pur.
    pub llm: Option<CuratorLlmConfig>,

    // ── Champs gating — propagés depuis [curator] du TOML serveur ────────────
    /// Seuil heuristique d'admission directe (0.0–1.0). `None` → 0.8 appliqué
    /// par le pipeline. Au-dessus → admettre sans revue LLM.
    pub heuristic_admit_threshold: Option<f32>,
    /// Statut assigné par défaut par l'heuristique (kebab-case string).
    /// `None` → `"pending-review"` par défaut dans le pipeline.
    pub heuristic_default_status: Option<String>,
    /// Active la revue LLM pour les notes sous `confidence_threshold`.
    /// `None` → `false` (LLM jamais appelé).
    pub llm_review_enabled: Option<bool>,
    /// Seuil de confiance sous lequel la revue LLM est déclenchée.
    /// `None` → `0.7` par défaut (compatible `Curator<C>::decide`).
    pub confidence_threshold: Option<f32>,
    /// URL de l'endpoint LLM pour la revue (compatible OpenAI Chat API).
    /// Redondant avec `llm.base_url` mais conservé pour la lisibilité TOML.
    pub llm_review_endpoint: Option<String>,
    /// Modèle LLM utilisé pour la revue.
    /// Redondant avec `llm.model` mais conservé pour la lisibilité TOML.
    pub llm_review_model: Option<String>,
    /// Timeout en millisecondes pour les appels LLM de revue.
    /// `None` → valeur du champ `llm.timeout_ms` ou 5000 ms.
    pub llm_review_timeout_ms: Option<u32>,
    /// Nombre maximum de tokens générés par le LLM de revue.
    /// `None` → laissé au défaut du backend LLM.
    pub llm_review_max_tokens: Option<u32>,
    /// Comportement en cas d'échec ou de timeout LLM.
    /// Valeurs : `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    /// `None` → `"pending-review-fallback"`.
    pub llm_review_fallback: Option<String>,
}

// ── Types Phase 2.0b ─────────────────────────────────────────────────────────

/// Données d'une note soumise au pipeline de curation (Phase 2.0b).
#[derive(Debug, Clone)]
pub struct Note {
    /// Identifiant ULID de la note.
    pub id: String,
    /// Titre de la note.
    pub title: String,
    /// Corps Markdown complet de la note.
    pub body: String,
    /// Tags suggérés par le créateur (optionnels — complétés par TF-IDF).
    pub tags_hint: Vec<String>,
    /// Section suggérée par le créateur (optionnelle — vérifiée par le routing).
    pub section_hint: Option<String>,
}

/// Décisions prises par la cascade curator pour une note.
#[derive(Debug, Clone)]
pub struct CuratorDecisions {
    /// Section canonique assignée par le routeur heuristique.
    pub canonical_section: String,
    /// Tags TF-IDF extraits du contenu.
    pub tags: Vec<String>,
    /// Verdict de nouveauté (exact / quasi-doublon / nouveau).
    pub novelty: novelty::NoveltyVerdict,
    /// Résolutions des wikilinks trouvés dans le corps.
    pub wikilinks: Vec<wikilinks::WikilinkResolution>,
    /// Verdict de déduplication sémantique.
    pub dedup: dedup::DedupVerdict,
}

/// Résultat de la cascade de curation (Phase 2.0b).
#[derive(Debug, Clone)]
pub enum CurateOutcome {
    /// Note admise — décisions complètes disponibles.
    Admitted {
        /// Décisions de la cascade.
        decisions: CuratorDecisions,
    },
    /// Note rejetée (doublon exact, etc.).
    Rejected {
        /// Raison du rejet.
        reason: String,
    },
    /// Note en attente de revue manuelle (zone grise).
    Pending {
        /// Décisions partielles (pour contexte de revue).
        decisions: CuratorDecisions,
        /// Raison de la mise en attente.
        reason: String,
    },
}

/// Pipeline de curation Phase 2.0b — cascade offline des 5 fonctions.
///
/// ## Backend de classification
///
/// Le pipeline utilise un backend [`gradatum_chat::LlmBackend`] injectable :
/// - Mode par défaut : [`gradatum_chat::HeuristicBackend`] (offline, CPU only)
/// - Mode LLM : backend spécifié dans `[curator.llm]` TOML, instancié brut.
///
/// ## Construction
///
/// - [`CuratorPipeline::new()`] — heuristique pur (offline)
/// - [`CuratorPipeline::heuristic()`] — alias explicite pour les tests
/// - [`CuratorPipeline::from_config()`] — wire depuis config TOML serveur (T6)
///
/// ## Invariant offline-first
///
/// L'heuristique est toujours exécutée en premier. En cas d'erreur LLM,
/// la stratégie `fallback_on_error` (`FallbackStrategy`) détermine le verdict.
///
/// ## Gating LLM (T6 P2.0c-tris)
///
/// La cascade `process()` est en deux temps :
/// 1. `routing::heuristic_route(title, body)` → `(section, confidence)`
///    - `confidence >= heuristic_admit_threshold` (défaut 0.8) → `Admitted` direct
/// 2. Si `llm_review_enabled = true` et `confidence < confidence_threshold` (défaut 0.7) :
///    → appel `self.backend.classify(SYSTEM_PROMPT, user_prompt)`
///    → `Admitted` avec section/tags du LLM | `Pending` sur erreur selon fallback
pub struct CuratorPipeline {
    /// Backend LLM injecté — heuristique par défaut, LLM optionnel via config TOML.
    backend: Arc<dyn gradatum_chat::LlmBackend>,

    // ── Paramètres de gating (propagés depuis CuratorPipelineConfig) ────────────
    /// Seuil heuristique d'admission directe (0.0–1.0). Défaut : 0.8.
    /// Au-dessus → `Admitted` sans appel LLM.
    heuristic_admit_threshold: f32,

    /// Active la revue LLM pour les notes sous `confidence_threshold`. Défaut : false.
    llm_review_enabled: bool,

    /// Seuil de confiance sous lequel la revue LLM est déclenchée. Défaut : 0.7.
    confidence_threshold: f32,

    /// Stratégie de fallback quand le LLM est indisponible. Défaut : PendingReviewFallback.
    fallback: FallbackStrategy,
}

impl CuratorPipeline {
    /// Crée un nouveau pipeline curator en mode heuristique (offline, CPU only).
    ///
    /// Aucun appel réseau. Invariant R1 garanti.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(gradatum_chat::HeuristicBackend),
            heuristic_admit_threshold: 0.8,
            llm_review_enabled: false,
            confidence_threshold: 0.7,
            fallback: FallbackStrategy::PendingReviewFallback,
        }
    }

    /// Alias explicite pour les tests et la lisibilité.
    ///
    /// Équivalent à `CuratorPipeline::new()` — heuristique pur.
    pub fn heuristic() -> Self {
        Self::new()
    }

    /// Construit le pipeline depuis la configuration curator.
    ///
    /// - Si `cfg.llm` est `None` : backend heuristique pur (offline).
    /// - Si `cfg.llm` est `Some` : instancie le backend LLM correspondant.
    ///   Les erreurs LLM sont gérées par `process()` via `self.fallback`
    ///   (FallbackStrategy). Pas de CircuitBreaker ici — l'ajouter au niveau
    ///   appelant (gradatum-server) si souhaité en production.
    ///
    /// ## Caveat T6 P2.0c
    ///
    /// Le bearer token est lu depuis la variable d'environnement `api_key_env`
    /// si présente. Absent ou vide = mode sans auth (endpoints LAN internes).
    ///
    /// ## Caveat architecture — dépendance cyclique évitée
    ///
    /// Prend [`CuratorPipelineConfig`] (struct locale) plutôt que
    /// `gradatum_server::config::CuratorConfig` pour éviter le cycle
    /// `gradatum-curator → gradatum-server`. Pattern identique au T5 (DTOs locaux).
    ///
    /// # Effets de bord
    ///
    /// Lit la variable d'environnement `cfg.llm.api_key_env` si configurée.
    pub fn from_config(cfg: &CuratorPipelineConfig) -> Self {
        let Some(llm_cfg) = &cfg.llm else {
            return Self::new();
        };

        let api_key = llm_cfg
            .api_key_env
            .as_deref()
            .and_then(|env_var| std::env::var(env_var).ok())
            .unwrap_or_default();
        let api_key_secret = secrecy::SecretString::new(api_key.into());

        let timeout = Duration::from_millis(llm_cfg.timeout_ms);

        // Nombre max tokens : lu depuis `llm_review_max_tokens` dans la config.
        // `None` → valeur par défaut du backend (1024, aligné sur le gatekeeper legacy).
        let max_tokens_override: Option<u32> = cfg.llm_review_max_tokens;

        let backend: Arc<dyn gradatum_chat::LlmBackend> = match llm_cfg.backend.as_str() {
            "openai_compat" => {
                let b = gradatum_chat::OpenAiCompatBackend::new(
                    llm_cfg.base_url.clone(),
                    llm_cfg.model.clone(),
                    api_key_secret,
                )
                .with_timeout(timeout);
                let b = if let Some(n) = max_tokens_override {
                    b.with_max_tokens(n)
                } else {
                    b
                };
                Arc::new(b)
            }
            "ollama_compat" => Arc::new(
                gradatum_chat::OllamaCompatBackend::new(
                    llm_cfg.base_url.clone(),
                    llm_cfg.model.clone(),
                )
                .with_timeout(timeout),
            ),
            "anthropic_compat" => {
                let b = gradatum_chat::AnthropicCompatBackend::new(
                    api_key_secret,
                    llm_cfg.model.clone(),
                )
                .with_timeout(timeout);
                let b = if let Some(n) = max_tokens_override {
                    b.with_max_tokens(n)
                } else {
                    b
                };
                Arc::new(b)
            }
            "gemini_compat" => Arc::new(
                gradatum_chat::GeminiCompatBackend::new(api_key_secret, llm_cfg.model.clone())
                    .with_timeout(timeout),
            ),
            other => {
                tracing::warn!(
                    backend = other,
                    "backend curator inconnu dans la config TOML — fallback heuristique"
                );
                return Self::new();
            }
        };

        // Propagation des paramètres de gating depuis la config TOML (T6 P2.0c-tris).
        let heuristic_admit_threshold = cfg.heuristic_admit_threshold.unwrap_or(0.8);
        let llm_review_enabled = cfg.llm_review_enabled.unwrap_or(false);
        let confidence_threshold = cfg.confidence_threshold.unwrap_or(0.7);
        let fallback = FallbackStrategy::from_config(
            cfg.llm_review_fallback
                .as_deref()
                .unwrap_or("pending-review-fallback"),
        );

        // NOTE : le backend LLM est utilisé directement, sans CircuitBreaker.
        // La gestion d'erreur est faite par `process()` via `self.fallback`
        // (FallbackStrategy::PendingReviewFallback | Reject). Encapsuler le
        // backend dans un CircuitBreaker avec fallback transparent vers
        // HeuristicBackend court-circuiterait cette logique : les erreurs LLM
        // seraient converties en Ok(heuristic_decision) avant d'atteindre
        // process(), rendant les strategies de fallback inopérantes.
        //
        // Si un circuit breaker est souhaité en production, l'ajouter au niveau
        // du composant appelant (gradatum-server/src/state.rs), après que
        // process() a déjà géré son fallback.
        Self {
            backend,
            heuristic_admit_threshold,
            llm_review_enabled,
            confidence_threshold,
            fallback,
        }
    }

    /// Retourne le backend actif (pour test d'introspection).
    #[doc(hidden)]
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Exécute la cascade de curation sur une note.
    ///
    /// ## Workflow (T6 P2.0c-tris)
    ///
    /// ```text
    /// 1. routing::heuristic_route(title, body) → (section, confidence)
    ///    - confidence >= heuristic_admit_threshold (défaut 0.8) → Admitted direct
    ///
    /// 2. llm_review_enabled = false → Pending (confiance faible, LLM désactivé)
    ///
    /// 3. llm_review_enabled = true → backend.classify(SYSTEM_PROMPT, user_prompt)
    ///    - Ok(decision)  → Admitted { canonical_section: decision.section, tags: decision.tags }
    ///    - Err(LlmError) → Pending ou Rejected selon fallback strategy
    /// ```
    ///
    /// ## Invariant offline-first (R1)
    ///
    /// L'heuristique est toujours exécutée en premier. Si `llm_review_enabled = false`,
    /// aucun appel réseau n'est effectué.
    ///
    /// # Effets de bord
    ///
    /// En mode heuristique : aucun.
    /// En mode LLM : appel HTTP vers le endpoint configuré, avec timeout.
    pub async fn process(&self, note: Note) -> CurateOutcome {
        // ── Étape 1 : pré-score heuristique (offline-first, invariant R1) ────────
        let body_truncated: String = note.body.chars().take(500).collect();
        let (heuristic_section, heuristic_confidence) =
            routing::heuristic_route(&note.title, &body_truncated);

        // Fast path : confiance heuristique élevée → admission directe sans LLM.
        if heuristic_confidence >= self.heuristic_admit_threshold {
            tracing::debug!(
                title = %note.title,
                section = heuristic_section,
                confidence = heuristic_confidence,
                "curator heuristic fast-path admit"
            );
            return CurateOutcome::Admitted {
                decisions: CuratorDecisions {
                    canonical_section: heuristic_section.to_string(),
                    tags: vec![],
                    novelty: novelty::NoveltyVerdict::Admitted,
                    wikilinks: vec![],
                    dedup: dedup::DedupVerdict::Unique,
                },
            };
        }

        // ── Étape 2 : confiance faible + LLM désactivé → Pending ─────────────────
        if !self.llm_review_enabled || heuristic_confidence > self.confidence_threshold {
            tracing::debug!(
                title = %note.title,
                heuristic_section,
                heuristic_confidence,
                llm_review_enabled = self.llm_review_enabled,
                confidence_threshold = self.confidence_threshold,
                "curator heuristic pending (llm disabled ou conf > threshold)"
            );
            return CurateOutcome::Pending {
                decisions: CuratorDecisions {
                    canonical_section: heuristic_section.to_string(),
                    tags: vec![],
                    novelty: novelty::NoveltyVerdict::Admitted,
                    wikilinks: vec![],
                    dedup: dedup::DedupVerdict::Unique,
                },
                reason: format!("low conf ({heuristic_confidence:.2}), llm disabled"),
            };
        }

        // ── Étape 3 : revue LLM ───────────────────────────────────────────────────
        // Format user_prompt établi par heuristic_routing.rs et curator_f1.rs
        let user_prompt = format!(
            "Classify this note.\nTitle: {}\nBody (truncated to 500 chars): {}",
            note.title, body_truncated,
        );

        tracing::info!(
            title = %note.title,
            heuristic_section,
            heuristic_confidence,
            backend = self.backend.name(),
            "curator LLM review triggered"
        );

        match self
            .backend
            .classify(CLASSIFIER_SYSTEM_PROMPT, &user_prompt)
            .await
        {
            Ok(decision) => {
                tracing::info!(
                    title = %note.title,
                    section = %decision.section,
                    backend = self.backend.name(),
                    "curator verdict from LLM"
                );
                CurateOutcome::Admitted {
                    decisions: CuratorDecisions {
                        canonical_section: decision.section,
                        tags: decision.tags,
                        novelty: novelty::NoveltyVerdict::Admitted,
                        wikilinks: vec![],
                        dedup: dedup::DedupVerdict::Unique,
                    },
                }
            }
            Err(llm_err) => {
                tracing::warn!(
                    title = %note.title,
                    error = %llm_err,
                    fallback = ?self.fallback,
                    "curator LLM error — applying fallback"
                );
                match self.fallback {
                    FallbackStrategy::Reject => CurateOutcome::Rejected {
                        reason: format!("llm down ({llm_err}) → reject (strict mode)"),
                    },
                    FallbackStrategy::PendingReviewFallback
                    | FallbackStrategy::AdmitPendingReview => CurateOutcome::Pending {
                        decisions: CuratorDecisions {
                            canonical_section: heuristic_section.to_string(),
                            tags: vec![],
                            novelty: novelty::NoveltyVerdict::Admitted,
                            wikilinks: vec![],
                            dedup: dedup::DedupVerdict::Unique,
                        },
                        reason: format!("llm down ({llm_err}) → PendingReview fallback"),
                    },
                }
            }
        }
    }
}

impl Default for CuratorPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ── Trait CuratorProcess — Phase 4 alpha.15 Task 18 ────────────────────────

/// Abstraction du pipeline de curation pour l'injection de dépendance et les tests.
///
/// `CuratorPipeline` implémente ce trait. Les tests peuvent fournir un mock
/// (`MockCuratorProcess`) sans dépendance vers un backend LLM.
///
/// ## Contrat
///
/// - `process` est infaillible : les erreurs internes sont absorbées dans le
///   `CurateOutcome` retourné (ex. `Pending` ou `Rejected` selon fallback).
/// - `process` est `Send + Sync` — le dispatcher Tokio peut appeler depuis
///   n'importe quel thread du runtime.
#[async_trait::async_trait]
pub trait CuratorProcess: Send + Sync {
    /// Exécute la cascade de curation sur la note donnée.
    ///
    /// # Effets de bord
    ///
    /// En mode heuristique : aucun.
    /// En mode LLM : appel HTTP vers le endpoint configuré, avec timeout.
    async fn process(&self, note: Note) -> CurateOutcome;
}

#[async_trait::async_trait]
impl CuratorProcess for CuratorPipeline {
    async fn process(&self, note: Note) -> CurateOutcome {
        // Délègue à l'implémentation existante.
        CuratorPipeline::process(self, note).await
    }
}

/// Extrait le titre H1 Markdown de la première ligne d'un corps de note.
///
/// Retourne `None` si la note ne commence pas par `# `.
/// Le titre retourné est trimmé (espaces aux extrémités supprimés).
///
/// ## Exemples
///
/// ```
/// use gradatum_curator::extract_h1_title;
/// assert_eq!(extract_h1_title("# Mon Titre\n\nbody"), Some("Mon Titre"));
/// assert_eq!(extract_h1_title("## Pas H1\nbody"), None);
/// assert_eq!(extract_h1_title("pas de titre\nbody"), None);
/// ```
///
/// ## Utilisation
///
/// Appelé post-curate dans le worker après `vault.write_note` pour persister
/// le titre dans la colonne `notes.title` (migration 0005).
pub fn extract_h1_title(body: &str) -> Option<&str> {
    let first_line = body.lines().next()?;
    first_line.strip_prefix("# ").map(|t| t.trim())
}

/// Version du crate (issue du `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// System prompt for LLM-mode classifier — classifier-v1 commit 0043407.
///
/// Ce prompt est intégré au binaire à la compilation (`include_str!`).
/// Il est activé **uniquement** quand `[curator] backend != "heuristic"`.
/// Pour l'installation par défaut (heuristic CPU offline), ce prompt
/// n'est jamais soumis à un endpoint LLM.
pub const CLASSIFIER_SYSTEM_PROMPT: &str =
    include_str!("../../../docs/prompts/curator-classifier-v1.txt");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }

    // ── Tests extract_h1_title (Task 6 M8) ──────────────────────────────────

    /// Aucun H1 → None retourné.
    #[test]
    fn extract_h1_title_returns_none_if_no_h1() {
        assert_eq!(extract_h1_title(""), None);
        assert_eq!(extract_h1_title("pas de titre\nbody"), None);
        assert_eq!(extract_h1_title("  # indent invalide\nbody"), None);
    }

    /// Première ligne `# Titre` → titre trimmé retourné.
    #[test]
    fn extract_h1_title_returns_title_if_h1_first_line() {
        assert_eq!(extract_h1_title("# Mon Titre\n\nbody"), Some("Mon Titre"));
        assert_eq!(
            extract_h1_title("# Titre avec espaces   \nbody"),
            Some("Titre avec espaces")
        );
        assert_eq!(extract_h1_title("# Seul"), Some("Seul"));
    }

    /// H2, H3 ou `##` en première ligne → None (seul `# ` strict compte).
    #[test]
    fn extract_h1_title_ignores_h2_and_deeper() {
        assert_eq!(extract_h1_title("## Sous-titre\nbody"), None);
        assert_eq!(extract_h1_title("### Deep\nbody"), None);
        assert_eq!(extract_h1_title("#### H4\nbody"), None);
    }

    /// Vérifie que le prompt classifier-v1 est bien embedé (non vide) dans le binaire.
    #[test]
    fn classifier_system_prompt_non_empty() {
        assert!(
            !CLASSIFIER_SYSTEM_PROMPT.is_empty(),
            "CLASSIFIER_SYSTEM_PROMPT doit être non-vide — vérifier docs/prompts/curator-classifier-v1.txt"
        );
        // Sanité minimale : le prompt doit contenir des tokens caractéristiques classifier-v1
        assert!(
            CLASSIFIER_SYSTEM_PROMPT.len() > 100,
            "CLASSIFIER_SYSTEM_PROMPT trop court ({} bytes) — fichier tronqué ?",
            CLASSIFIER_SYSTEM_PROMPT.len()
        );
    }
}
