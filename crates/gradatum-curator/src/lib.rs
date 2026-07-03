//! # gradatum-curator
//!
//! Note curation pipeline: heuristic gating + LLM review for low-confidence notes
//! + 3 fallback strategies + 5-step cascade.
//!
//! ## Legacy architecture (`Curator<C>`)
//!
//! ```text
//! Curator<C: Chat>::decide(note, ctx) → CuratorDecision
//!
//!   step 1 : Heuristic::classify_curator(note, ctx)
//!   step 2 : confidence > threshold → fast path (Heuristic verdict)
//!   step 3 : llm_review_enabled → C::classify_curator(note, ctx)
//!   step 4 : LLM error → FallbackStrategy applied
//! ```
//!
//! ## `CuratorPipeline` architecture
//!
//! ```text
//! CuratorPipeline::process(note) → CurateOutcome
//!
//!   step 1 : novelty   — SHA-256 exact match + MinHash 128-perm Jaccard ≥ 0.92
//!   step 2 : routing   — regex heuristic over 11 gradatum sections
//!   step 3 : tags      — TF-IDF top-5 + kebab-case
//!   step 4 : wikilinks — regex extraction + Jaro-Winkler 0.88 fuzzy
//!   step 5 : dedup     — cosine 0.95 over bge-small embeddings
//! ```
//!
//! ## Offline-first invariant
//!
//! The heuristic always runs first, with no network dependency.
//! The LLM is invoked only for low-confidence notes and only when
//! `llm_review_enabled = true`.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee.
//! See [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

// ── Modules legacy (Curator<C>) ──────────────────────────────────────────────
pub mod decision;
pub mod error;
pub mod workflow;

// ── Modules pipeline cascade 5 fonctions ────────────────────────────────────
pub mod dedup;
pub mod novelty;
pub mod routing;
pub mod tags;
pub mod wikilinks;
pub mod wikilinks_sync;

// ── Re-exports ───────────────────────────────────────────────────────────────
pub use decision::{CuratorDecision, FallbackStrategy};
pub use error::CuratorError;
pub use workflow::Curator;

// ── Imports pour from_config ─────────────────────────────────────────────────
use std::sync::Arc;
use std::time::Duration;

// ── Configuration curator locale (évite dépendance cyclique curator→server) ─

/// Configuration for the optional LLM backend used by the curation pipeline.
///
/// Local struct — mirrors `gradatum_server::config::LlmConfig` to avoid the
/// cyclic dependency `gradatum-curator → gradatum-server`. Same local-DTO
/// pattern used for the same reason.
///
/// ## Synchronisation
///
/// When `gradatum_server::config::LlmConfig` evolves, keep this struct in sync.
/// A `From<&LlmConfig> for CuratorLlmConfig` conversion is provided in
/// `gradatum-server/src/state.rs`.
#[derive(Debug, Clone)]
pub struct CuratorLlmConfig {
    /// Backend type: `"openai_compat"` | `"ollama_compat"` |
    ///               `"anthropic_compat"` | `"gemini_compat"`.
    pub backend: String,
    /// Base URL of the endpoint (no path component).
    pub base_url: String,
    /// Model name.
    pub model: String,
    /// Name of the environment variable holding the bearer token.
    /// `None` = no auth (unauthenticated LAN endpoint).
    pub api_key_env: Option<String>,
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Configuration for the curator pipeline (backend + optional LLM + gating fields).
///
/// Local struct — mirrors `gradatum_server::config::CuratorConfig`.
/// Passed to [`CuratorPipeline::from_config`] from `gradatum-server/src/state.rs`.
///
/// ## Gating fields
///
/// Fields such as `llm_review_enabled` and `confidence_threshold` control cascade
/// behaviour. Without explicit propagation they remain at their defaults (`false`,
/// `0.7`) regardless of the TOML configuration.
#[derive(Debug, Clone)]
pub struct CuratorPipelineConfig {
    /// Classification backend. Default: `"heuristic"`.
    pub backend: String,
    /// Optional LLM tier configuration. `None` = pure heuristic mode.
    pub llm: Option<CuratorLlmConfig>,

    // ── Gating fields — propagated from the [curator] section of the server TOML ──
    /// Heuristic direct-admit threshold (0.0–1.0). `None` → 0.8 applied by the
    /// pipeline. Notes above this threshold are admitted without LLM review.
    pub heuristic_admit_threshold: Option<f32>,
    /// Default status assigned by the heuristic (kebab-case string).
    /// `None` → `"pending-review"` by default in the pipeline.
    pub heuristic_default_status: Option<String>,
    /// Enables LLM review for notes below `confidence_threshold`.
    /// `None` → `false` (LLM never called).
    pub llm_review_enabled: Option<bool>,
    /// Confidence threshold below which LLM review is triggered.
    /// `None` → `0.7` (compatible with `Curator<C>::decide`).
    pub confidence_threshold: Option<f32>,
    /// LLM endpoint URL for review (OpenAI Chat API compatible).
    /// Redundant with `llm.base_url` but kept for TOML readability.
    pub llm_review_endpoint: Option<String>,
    /// LLM model used for review.
    /// Redundant with `llm.model` but kept for TOML readability.
    pub llm_review_model: Option<String>,
    /// Timeout in milliseconds for LLM review calls.
    /// `None` → falls back to `llm.timeout_ms` or 5000 ms.
    pub llm_review_timeout_ms: Option<u32>,
    /// Maximum number of tokens generated by the review LLM.
    /// `None` → defers to the LLM backend default.
    pub llm_review_max_tokens: Option<u32>,
    /// Behaviour on LLM failure or timeout.
    /// Values: `"pending-review-fallback"` | `"reject"` | `"admit-pending-review"`.
    /// `None` → `"pending-review-fallback"`.
    pub llm_review_fallback: Option<String>,
}

// ── Types CuratorPipeline ────────────────────────────────────────────────────

/// Data of a note submitted to the curation pipeline.
#[derive(Debug, Clone)]
pub struct Note {
    /// ULID identifier of the note.
    pub id: String,
    /// Title of the note.
    pub title: String,
    /// Full Markdown body of the note.
    pub body: String,
    /// Tags suggested by the creator (optional — supplemented by TF-IDF).
    pub tags_hint: Vec<String>,
    /// Section suggested by the creator (optional — validated by routing).
    pub section_hint: Option<String>,
}

/// Decisions produced by the curator cascade for a note.
#[derive(Debug, Clone)]
pub struct CuratorDecisions {
    /// Canonical section assigned by the heuristic router.
    pub canonical_section: String,
    /// TF-IDF tags extracted from the content.
    pub tags: Vec<String>,
    /// Novelty verdict (exact match / near-duplicate / new).
    pub novelty: novelty::NoveltyVerdict,
    /// Resolved wikilinks found in the body.
    pub wikilinks: Vec<wikilinks::WikilinkResolution>,
    /// Semantic deduplication verdict.
    pub dedup: dedup::DedupVerdict,
}

/// Result of the curation cascade.
#[derive(Debug, Clone)]
pub enum CurateOutcome {
    /// Note admitted — full decisions available.
    Admitted {
        /// Cascade decisions.
        decisions: CuratorDecisions,
    },
    /// Note rejected (exact duplicate, etc.).
    Rejected {
        /// Rejection reason.
        reason: String,
    },
    /// Note pending manual review (ambiguous case).
    Pending {
        /// Partial decisions (provided as review context).
        decisions: CuratorDecisions,
        /// Reason for pending status.
        reason: String,
    },
}

/// Returns the vault `NoteStatus` for a given [`CurateOutcome`] — single source of truth.
///
/// Single source of truth for the outcome → status mapping, ensuring all worker
/// write paths (creation + reclassify, `dispatch.rs` legacy + `apalis_handlers.rs`)
/// produce a consistent status rather than hardcoding it independently.
///
/// Mapping:
/// - [`CurateOutcome::Admitted`] → `Some(NoteStatus::Live)` (admitted, indexed, searchable).
/// - [`CurateOutcome::Pending`]  → `Some(NoteStatus::PendingReview)` (queued for `/review`).
/// - [`CurateOutcome::Rejected`] → `None` (no write — note rejected).
///
/// ## Status semantics
///
/// `PendingReview` means awaiting judgement (the correct state for the review queue).
/// `Staging` means optional human review — that mapping was incorrect and has been
/// replaced by `PendingReview` for the `Pending` variant.
#[must_use]
pub fn outcome_to_status(outcome: &CurateOutcome) -> Option<gradatum_core::status::NoteStatus> {
    use gradatum_core::status::NoteStatus;
    match outcome {
        CurateOutcome::Admitted { .. } => Some(NoteStatus::Live),
        CurateOutcome::Pending { .. } => Some(NoteStatus::PendingReview),
        CurateOutcome::Rejected { .. } => None,
    }
}

/// Curation pipeline — offline 5-step cascade.
///
/// ## Classification backend
///
/// Uses an injectable [`gradatum_chat::LlmBackend`]:
/// - Default mode: [`gradatum_chat::HeuristicBackend`] (offline, CPU only).
/// - LLM mode: backend specified in `[curator.llm]` TOML, instantiated directly.
///
/// ## Construction
///
/// - [`CuratorPipeline::new()`] — pure heuristic (offline).
/// - [`CuratorPipeline::heuristic()`] — explicit alias for testing.
/// - [`CuratorPipeline::from_config()`] — wired from the server TOML configuration.
///
/// ## Offline-first invariant
///
/// The heuristic always runs first. On LLM error, the `FallbackStrategy` determines
/// the final verdict.
///
/// ## LLM gating
///
/// The `process()` cascade operates in two stages:
/// 1. `routing::heuristic_route(title, body)` → `(section, confidence)`
///    - `confidence >= heuristic_admit_threshold` (default 0.8) → `Admitted` directly.
/// 2. If `llm_review_enabled = true` and `confidence < confidence_threshold` (default 0.7):
///    → calls `self.backend.classify(SYSTEM_PROMPT, user_prompt)`
///    → `Admitted` with LLM section/tags, or `Pending` on error per fallback strategy.
pub struct CuratorPipeline {
    /// Injected LLM backend — heuristic by default, optional LLM via TOML config.
    backend: Arc<dyn gradatum_chat::LlmBackend>,

    // ── Gating parameters (propagated from CuratorPipelineConfig) ────────────
    /// Heuristic direct-admit threshold (0.0–1.0). Default: 0.8.
    /// Notes above this threshold are admitted as `Admitted` without calling the LLM.
    heuristic_admit_threshold: f32,

    /// Enables LLM review for notes below `confidence_threshold`. Default: false.
    llm_review_enabled: bool,

    /// Confidence threshold below which LLM review is triggered. Default: 0.7.
    confidence_threshold: f32,

    /// Fallback strategy when the LLM is unavailable. Default: `PendingReviewFallback`.
    fallback: FallbackStrategy,
}

impl CuratorPipeline {
    /// Creates a new curator pipeline in heuristic mode (offline, CPU only).
    ///
    /// No network calls. Offline-first invariant guaranteed.
    pub fn new() -> Self {
        Self {
            backend: Arc::new(gradatum_chat::HeuristicBackend),
            heuristic_admit_threshold: 0.8,
            llm_review_enabled: false,
            confidence_threshold: 0.7,
            fallback: FallbackStrategy::PendingReviewFallback,
        }
    }

    /// Explicit alias for tests and readability.
    ///
    /// Equivalent to `CuratorPipeline::new()` — pure heuristic mode.
    pub fn heuristic() -> Self {
        Self::new()
    }

    /// Builds the pipeline from the curator configuration.
    ///
    /// - If `cfg.llm` is `None`: pure heuristic backend (offline).
    /// - If `cfg.llm` is `Some`: instantiates the corresponding LLM backend.
    ///   LLM errors are handled by `process()` via `self.fallback`
    ///   (`FallbackStrategy`). No circuit breaker here — add one at the
    ///   caller level (gradatum-server) if desired in production.
    ///
    /// The bearer token is read from the `api_key_env` environment variable
    /// when present. Absent or empty = no-auth mode (internal LAN endpoints).
    ///
    /// Takes [`CuratorPipelineConfig`] (local struct) rather than
    /// `gradatum_server::config::CuratorConfig` to avoid the
    /// `gradatum-curator → gradatum-server` cyclic dependency.
    /// Same local-DTO pattern used throughout.
    ///
    /// # Side effects
    ///
    /// Reads the environment variable named by `cfg.llm.api_key_env` if set.
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

    /// Returns the name of the active backend (for introspection in tests).
    #[doc(hidden)]
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Runs the curation cascade on a note.
    ///
    /// ## Workflow
    ///
    /// ```text
    /// 1. routing::heuristic_route(title, body) → (section, confidence)
    ///    - confidence >= heuristic_admit_threshold (default 0.8) → Admitted directly
    ///
    /// 2. llm_review_enabled = false → Pending (low confidence, LLM disabled)
    ///
    /// 3. llm_review_enabled = true → backend.classify(SYSTEM_PROMPT, user_prompt)
    ///    - Ok(decision)  → Admitted { canonical_section: decision.section, tags: decision.tags }
    ///    - Err(LlmError) → Pending or Rejected per fallback strategy
    /// ```
    ///
    /// ## Offline-first invariant
    ///
    /// The heuristic always runs first. When `llm_review_enabled = false`,
    /// no network calls are made.
    ///
    /// # Strong hint path (valid `section_hint`)
    ///
    /// When `note.section_hint` contains a valid canonical section (present in
    /// `routing::SECTIONS`), the cascade is short-circuited: the note is admitted
    /// directly with `canonical_section = hint`, without calling the heuristic or LLM.
    ///
    /// **This path produces no enrichment**: `tags` = `[]`, `wikilinks` = `[]`.
    /// Tags provided in `note.tags_hint` are preserved downstream by the caller
    /// (the worker passes them directly to the frontmatter, bypassing this pipeline).
    /// Wikilinks are extracted from the body independently via `process_wikilinks_b5`
    /// (called by the worker outside this function, on the raw body).
    ///
    /// An invalid `section_hint` (not in the canonical section list) is ignored with
    /// a `warn!` and the normal cascade runs.
    ///
    /// # Side effects
    ///
    /// Heuristic mode: none.
    /// LLM mode: HTTP call to the configured endpoint, with timeout.
    pub async fn process(&self, note: Note) -> CurateOutcome {
        // ── Étape 0 : hint fort — section_hint explicite valide (B3 piste a) ──────
        //
        // Si le créateur a fourni un `section_hint` ET que ce hint correspond à
        // l'une des 13 sections canoniques (via `Section::ALL`), on admet directement
        // sans consulter l'heuristique ni le LLM. Un hint invalide est ignoré avec un warn.
        if let Some(ref hint) = note.section_hint {
            if routing::is_valid_hint_section(hint) {
                tracing::debug!(
                    title = %note.title,
                    section = %hint,
                    "curator section_hint fort — admission directe"
                );
                return CurateOutcome::Admitted {
                    decisions: CuratorDecisions {
                        canonical_section: hint.clone(),
                        tags: vec![],
                        novelty: novelty::NoveltyVerdict::Admitted,
                        wikilinks: vec![],
                        dedup: dedup::DedupVerdict::Unique,
                    },
                };
            } else {
                tracing::warn!(
                    title = %note.title,
                    section_hint = %hint,
                    "curator section_hint invalide (hors sections canoniques) — ignoré, chemin normal"
                );
            }
        }

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
        // Format user_prompt établi par heuristic_routing.rs et curator_f1.rs.
        //
        // Injection conditionnelle du section_hint (B3 piste b — prompt v2).
        //
        // NOTE ARCHITECTURALE : cette règle LLM est dormante dans le pipeline
        // actuel en conditions normales. L'Étape 0 (hint fort) intercepte et
        // retourne directement tout hint valide parmi les 12 sections canoniques,
        // avant que l'on atteigne cette ligne. La hint_line ici sert deux cas :
        //   (a) hints invalides qui ont passé l'Étape 0 sans early-return mais
        //       portent de l'information sémantique utile au LLM ;
        //   (b) robustesse future si l'Étape 0 est assouplie ou contournée.
        let hint_line = note
            .section_hint
            .as_deref()
            .map(|h| format!("\nHint (caller-provided section): {h}"))
            .unwrap_or_default();
        let user_prompt = format!(
            "Classify this note.\nTitle: {}\nBody (truncated to 500 chars): {}{}",
            note.title, body_truncated, hint_line,
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
                    section_hint = ?note.section_hint,
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

// ── Trait CuratorProcess ─────────────────────────────────────────────────────

/// Abstraction of the curation pipeline for dependency injection and testing.
///
/// `CuratorPipeline` implements this trait. Tests can supply a mock
/// (`MockCuratorProcess`) without depending on an LLM backend.
///
/// ## Contract
///
/// - `process` is infallible: internal errors are absorbed into the returned
///   `CurateOutcome` (e.g. `Pending` or `Rejected` per fallback strategy).
/// - `process` is `Send + Sync` — the Tokio dispatcher can call it from any
///   thread in the runtime.
#[async_trait::async_trait]
pub trait CuratorProcess: Send + Sync {
    /// Runs the curation cascade on the given note.
    ///
    /// # Side effects
    ///
    /// Heuristic mode: none.
    /// LLM mode: HTTP call to the configured endpoint, with timeout.
    async fn process(&self, note: Note) -> CurateOutcome;
}

#[async_trait::async_trait]
impl CuratorProcess for CuratorPipeline {
    async fn process(&self, note: Note) -> CurateOutcome {
        // Délègue à l'implémentation existante.
        CuratorPipeline::process(self, note).await
    }
}

/// Extracts the Markdown H1 title from the first line of a note body.
///
/// Re-export of [`gradatum_index::extract_h1_title`] — single source of truth.
/// Returns `None` when the first line does not start with `"# "`, or when the
/// title is empty after trimming (consistent with the `title_lookup` SQL definition).
///
/// An empty H1 (`"# "`) returns `None` instead of `Some("")` — no empty title
/// is persisted.
///
/// ## Examples
///
/// ```
/// use gradatum_curator::extract_h1_title;
/// assert_eq!(extract_h1_title("# Mon Titre\n\nbody"), Some("Mon Titre".to_owned()));
/// assert_eq!(extract_h1_title("## Pas H1\nbody"),     None);
/// assert_eq!(extract_h1_title("pas de titre\nbody"),  None);
/// assert_eq!(extract_h1_title("# "),                  None); // empty H1 → None
/// ```
///
/// ## Usage
///
/// Called post-curation in the worker after `vault.write_note` to persist
/// the title into the `notes.title` column (migration 0005).
pub use gradatum_index::extract_h1_title;

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// System prompt for the LLM-mode classifier — classifier-v2 (11 sections + hint injection).
///
/// Embedded in the binary at compile time via `include_str!`.
/// Active **only** when `[curator] backend != "heuristic"`.
/// In the default installation (heuristic, CPU, offline), this prompt
/// is never submitted to an LLM endpoint.
///
/// ## Versions
///
/// - v1: 10 sections, no hint injection, no `council` section.
/// - v2: 11 sections (+ `council`), exclusion criteria for
///   `council`/`decisions`/`debug`/`retrospectives`, conditional
///   section hint in the user template.
pub const CLASSIFIER_SYSTEM_PROMPT: &str = include_str!("../prompts/curator-classifier-v2.txt");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }

    // ── F-37 S1.2 — parité du mapping outcome → status ──────────────────────

    /// Construit des `CuratorDecisions` minimales pour les tests de mapping.
    fn dummy_decisions() -> CuratorDecisions {
        CuratorDecisions {
            canonical_section: "reference".to_string(),
            tags: vec![],
            novelty: novelty::NoveltyVerdict::Admitted,
            wikilinks: vec![],
            dedup: dedup::DedupVerdict::Unique,
        }
    }

    /// `outcome_to_status` est l'unique source de vérité du mapping write-path.
    /// Ce test gèle le contrat post-flip S1.2 (Pending → PendingReview, pas Staging).
    #[test]
    fn outcome_to_status_parity() {
        use gradatum_core::status::NoteStatus;

        assert_eq!(
            outcome_to_status(&CurateOutcome::Admitted {
                decisions: dummy_decisions(),
            }),
            Some(NoteStatus::Live),
            "Admitted doit écrire Live"
        );
        assert_eq!(
            outcome_to_status(&CurateOutcome::Pending {
                decisions: dummy_decisions(),
                reason: "low confidence".to_string(),
            }),
            Some(NoteStatus::PendingReview),
            "Pending doit écrire PendingReview (flip S1.2, plus Staging)"
        );
        assert_eq!(
            outcome_to_status(&CurateOutcome::Rejected {
                reason: "exact dup".to_string(),
            }),
            None,
            "Rejected ne doit rien écrire"
        );
    }

    // ── Tests extract_h1_title (Task 6 M8 — alignés SSOT gradatum-index 2026-06-14) ──
    //
    // Signature mise à jour : Option<String> (au lieu de Option<&str>).
    // Comportement H1-vide : "# " → None (au lieu de Some("") — plus de titre vide).

    /// Aucun H1 → None retourné.
    #[test]
    fn extract_h1_title_returns_none_if_no_h1() {
        assert_eq!(extract_h1_title(""), None);
        assert_eq!(extract_h1_title("pas de titre\nbody"), None);
        assert_eq!(extract_h1_title("  # indent invalide\nbody"), None);
        // H1 vide : anciennement Some(""), désormais None (SSOT 2026-06-14).
        assert_eq!(extract_h1_title("# "), None);
        assert_eq!(extract_h1_title("# \ncorps"), None);
    }

    /// Première ligne `# Titre` → titre trimmé retourné (`Option<String>`).
    #[test]
    fn extract_h1_title_returns_title_if_h1_first_line() {
        assert_eq!(
            extract_h1_title("# Mon Titre\n\nbody"),
            Some("Mon Titre".to_owned())
        );
        assert_eq!(
            extract_h1_title("# Titre avec espaces   \nbody"),
            Some("Titre avec espaces".to_owned())
        );
        assert_eq!(extract_h1_title("# Seul"), Some("Seul".to_owned()));
    }

    /// H2, H3 ou `##` en première ligne → None (seul `# ` strict compte).
    #[test]
    fn extract_h1_title_ignores_h2_and_deeper() {
        assert_eq!(extract_h1_title("## Sous-titre\nbody"), None);
        assert_eq!(extract_h1_title("### Deep\nbody"), None);
        assert_eq!(extract_h1_title("#### H4\nbody"), None);
    }

    /// Vérifie que le prompt classifier-v2 est bien embedé (non vide) dans le binaire.
    ///
    /// Vérifie aussi les tokens caractéristiques v2 : section "council" et hint injection.
    #[test]
    fn classifier_system_prompt_non_empty() {
        assert!(
            !CLASSIFIER_SYSTEM_PROMPT.is_empty(),
            "CLASSIFIER_SYSTEM_PROMPT doit être non-vide — vérifier crates/gradatum-curator/prompts/curator-classifier-v2.txt"
        );
        // Sanité minimale : le prompt doit contenir des tokens caractéristiques classifier-v2
        assert!(
            CLASSIFIER_SYSTEM_PROMPT.len() > 100,
            "CLASSIFIER_SYSTEM_PROMPT trop court ({} bytes) — fichier tronqué ?",
            CLASSIFIER_SYSTEM_PROMPT.len()
        );
        // v2 : section council présente
        assert!(
            CLASSIFIER_SYSTEM_PROMPT.contains("council"),
            "CLASSIFIER_SYSTEM_PROMPT v2 doit contenir la section 'council'"
        );
        // v2 : hint injection présente
        assert!(
            CLASSIFIER_SYSTEM_PROMPT.contains("Caller-provided hint"),
            "CLASSIFIER_SYSTEM_PROMPT v2 doit contenir la section 'Caller-provided hint'"
        );
        // v2 : 11 sections déclarées (pas 10)
        assert!(
            CLASSIFIER_SYSTEM_PROMPT.contains("exactly 11"),
            "CLASSIFIER_SYSTEM_PROMPT v2 doit déclarer 'exactly 11' sections"
        );
    }

    // ── Tests invariant hint-only project-map (stage1 fix) ──────────────────────
    //
    // Trois tests couvrant l'invariant :
    // - Le hint fort "project-map" admet la note directement (strong-hint path).
    // - Sans hint, le contenu ne produit jamais "project-map" (heuristique).
    // - Les 11 sections auto-classifiables existantes passent toujours le hint fort.

    /// Construit une `Note` minimale avec le `section_hint` donné.
    fn build_note_with_hint(title: &str, body: &str, hint: Option<&str>) -> Note {
        Note {
            id: "01JTEST00000000000000000".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            tags_hint: vec![],
            section_hint: hint.map(str::to_string),
        }
    }

    /// `section_hint = "project-map"` → admission directe via strong-hint path.
    ///
    /// L'heuristique produirait "reference" (fallback) sur ce contenu ambigu.
    /// Seul le strong-hint path peut produire "project-map" → ce résultat prouve
    /// que le hint est pris en compte.
    #[tokio::test]
    async fn section_hint_project_map_strong_hint_path() {
        let note = build_note_with_hint(
            "Carte projet gradatum v0.6.0",
            "lifecycle wikilink stage1 project map work units",
            Some("project-map"),
        );
        let pipeline = CuratorPipeline::new();
        let outcome = pipeline.process(note).await;

        let section = match &outcome {
            CurateOutcome::Admitted { decisions } => &decisions.canonical_section,
            other => panic!("attendu Admitted, obtenu {other:?}"),
        };
        assert_eq!(
            section, "project-map",
            "section_hint='project-map' doit admettre la note en project-map via le strong-hint path"
        );
    }

    /// Sans `section_hint`, le contenu ne produit jamais "project-map".
    ///
    /// Corps contenant des termes qui pourraient ressembler à project-map
    /// (lifecycle, wikilink, project, map, stage1) → l'heuristique ne connaît
    /// aucun pattern keyword pour project-map (hint-only invariant).
    #[tokio::test]
    async fn no_section_hint_never_routes_to_project_map() {
        let note = build_note_with_hint(
            "lifecycle wikilink project map stage1",
            "project map lifecycle wikilink stage1 work unit project-map typed-wikilink",
            None,
        );
        let pipeline = CuratorPipeline::new();
        let outcome = pipeline.process(note).await;

        let section = match &outcome {
            CurateOutcome::Admitted { decisions } => decisions.canonical_section.clone(),
            CurateOutcome::Pending { decisions, .. } => decisions.canonical_section.clone(),
            CurateOutcome::Rejected { reason } => {
                panic!("attendu Admitted ou Pending, obtenu Rejected({reason})")
            }
        };
        assert_ne!(
            section, "project-map",
            "sans section_hint, l'heuristique ne doit jamais produire 'project-map' (hint-only invariant)"
        );
    }

    /// Non-régression : les 11 sections auto-classifiables passent toujours le strong-hint path.
    ///
    /// Ce test fige le contrat : les sections existantes doivent continuer à être
    /// acceptées comme hint valide après l'ajout de is_valid_hint_section.
    #[tokio::test]
    async fn section_hint_existing_11_sections_unchanged() {
        let existing_sections = [
            "decisions",
            "council",
            "architecture",
            "debug",
            "reasoning",
            "feedback",
            "lessons-learned",
            "retrospectives",
            "experiments",
            "agent-issues",
            "reference",
        ];

        let pipeline = CuratorPipeline::new();
        for section in existing_sections {
            let note = build_note_with_hint(
                &format!("Note test section {section}"),
                "Corps quelconque pour test de non-régression du hint fort.",
                Some(section),
            );
            let outcome = pipeline.process(note).await;
            let assigned = match &outcome {
                CurateOutcome::Admitted { decisions } => &decisions.canonical_section,
                other => panic!("section_hint='{section}' : attendu Admitted, obtenu {other:?}"),
            };
            assert_eq!(
                assigned, section,
                "section_hint='{section}' doit admettre la note dans cette section (non-régression)"
            );
        }
    }
}
