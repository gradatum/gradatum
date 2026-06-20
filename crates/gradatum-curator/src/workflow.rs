//! Main curator workflow (`Curator<C>` — legacy).
//!
//! ## Offline-first invariant
//!
//! The heuristic always runs FIRST, with no network access.
//! The LLM is invoked only when:
//! - heuristic confidence ≤ `confidence_threshold` (default 0.7), AND
//! - `llm_review_enabled = true`, AND
//! - an LLM backend `C` is provided (`Some(Arc<C>)`).

use std::sync::Arc;

use gradatum_chat::{Chat, ChatBackend, CuratorContext, Heuristic};
use gradatum_core::config::CuratorConfig;
use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::decision::{CuratorDecision, FallbackStrategy};

/// Orchestrator for the note curation pipeline.
///
/// `C` = optional LLM backend (e.g. `HttpChat`, `Noop`, or a test mock).
/// Must implement [`Chat`] + `Send + Sync + 'static` (via `async_trait`).
///
/// # Minimal example
///
/// ```rust,no_run
/// use gradatum_chat::{Heuristic, Noop};
/// use gradatum_core::config::CuratorConfig;
/// use gradatum_curator::Curator;
/// use std::sync::Arc;
///
/// let curator: Curator<Noop> = Curator::new(
///     Heuristic::new(),
///     None,
///     CuratorConfig::default(),
/// );
/// ```
pub struct Curator<C: Chat> {
    /// Offline heuristic classifier (offline-first invariant).
    pub heuristic: Heuristic,

    /// Optional LLM backend — `None` disables LLM review regardless of configuration.
    pub llm: Option<Arc<C>>,

    /// Runtime configuration loaded from `<vault_root>/.gradatum/config.toml`.
    pub cfg: CuratorConfig,
}

impl<C: Chat> Curator<C> {
    /// Creates a curator with the provided components.
    ///
    /// - `heuristic`: offline classifier (always runs first).
    /// - `llm`: optional LLM backend. `None` = LLM review never triggered.
    /// - `cfg`: curator configuration (thresholds, fallback strategy).
    pub fn new(heuristic: Heuristic, llm: Option<Arc<C>>, cfg: CuratorConfig) -> Self {
        Self {
            heuristic,
            llm,
            cfg,
        }
    }

    /// Decides the final status of a note.
    ///
    /// **Never returns an error**: all internal failures are absorbed into a
    /// `CuratorDecision` with `fallback_applied = true`. The caller always
    /// receives a valid, safe decision.
    ///
    /// ## Workflow
    ///
    /// ```text
    /// 1. Heuristic::classify_curator(note, ctx)
    ///    → internal error → PendingReview + fallback_applied=true (exceptional case)
    ///
    /// 2. confidence > threshold (default 0.7)
    ///    → fast path: returns heuristic verdict directly
    ///
    /// 3. confidence ≤ threshold + llm_review_enabled=false
    ///    → PendingReview, no LLM call, fallback_applied=false
    ///
    /// 4. confidence ≤ threshold + llm_review_enabled=true + llm=None
    ///    → PendingReview + fallback_applied=true (inconsistent configuration)
    ///
    /// 5. LLM::classify_curator(note, ctx)
    ///    → Ok(verdict_llm) : returns LLM verdict
    ///    → Err(e) : applies FallbackStrategy from configuration
    ///       - "pending-review-fallback" → PendingReview
    ///       - "reject"                 → Garbage
    ///       - "admit-pending-review"   → PendingReview (soft)
    /// ```
    pub async fn decide(&self, note: &Note, ctx: &CuratorContext) -> CuratorDecision {
        let threshold = self.cfg.confidence_threshold.unwrap_or(0.7);
        let llm_enabled = self.cfg.llm_review_enabled.unwrap_or(false);
        let fallback = FallbackStrategy::from_config(
            self.cfg
                .llm_review_fallback
                .as_deref()
                .unwrap_or("pending-review-fallback"),
        );

        // --- Étape 1 : classification heuristique (offline-first, invariant #3) ---
        let verdict_h = match self.heuristic.classify_curator(note, ctx).await {
            Ok(v) => v,
            Err(_) => {
                // L'heuristique ne devrait jamais échouer (pas de réseau, pas d'I/O).
                // Si ça arrive (bug interne), on reste dans un état safe.
                return CuratorDecision {
                    final_status: NoteStatus::PendingReview,
                    backend_used: ChatBackend::Heuristic,
                    confidence: 0.0,
                    reason: "heuristic error — safe PendingReview".into(),
                    fallback_applied: true,
                };
            }
        };

        // --- Étape 2 : fast path — confiance élevée ---
        if verdict_h.confidence > threshold {
            return CuratorDecision {
                final_status: verdict_h.proposed_status,
                backend_used: ChatBackend::Heuristic,
                confidence: verdict_h.confidence,
                reason: verdict_h.reason,
                fallback_applied: false,
            };
        }

        // --- Étape 3 : confiance faible + LLM désactivé ---
        if !llm_enabled {
            return CuratorDecision {
                final_status: NoteStatus::PendingReview,
                backend_used: ChatBackend::Heuristic,
                confidence: verdict_h.confidence,
                reason: format!("low conf ({:.2}), llm disabled", verdict_h.confidence),
                fallback_applied: false,
            };
        }

        // --- Étape 4 : LLM activé mais backend absent (config incohérente) ---
        let Some(llm) = &self.llm else {
            return CuratorDecision {
                final_status: NoteStatus::PendingReview,
                backend_used: ChatBackend::Heuristic,
                confidence: verdict_h.confidence,
                reason: "low conf, llm enabled but no backend configured".into(),
                fallback_applied: true,
            };
        };

        // --- Étape 5 : revue LLM ---
        match llm.classify_curator(note, ctx).await {
            Ok(verdict_llm) => CuratorDecision {
                final_status: verdict_llm.proposed_status,
                backend_used: ChatBackend::Http,
                confidence: verdict_llm.confidence,
                reason: verdict_llm.reason,
                fallback_applied: false,
            },
            Err(err) => apply_fallback(fallback, &verdict_h, &err),
        }
    }
}

/// Applies the fallback strategy when the LLM is unavailable.
///
/// Extracted to factor out the 3 branches and reduce nesting.
fn apply_fallback(
    strategy: FallbackStrategy,
    verdict_h: &gradatum_chat::CuratorVerdict,
    err: &gradatum_chat::ChatError,
) -> CuratorDecision {
    match strategy {
        FallbackStrategy::PendingReviewFallback => CuratorDecision {
            final_status: NoteStatus::PendingReview,
            backend_used: ChatBackend::Heuristic,
            confidence: verdict_h.confidence,
            reason: format!("llm down ({err}) → PendingReview fallback"),
            fallback_applied: true,
        },
        FallbackStrategy::Reject => CuratorDecision {
            final_status: NoteStatus::Garbage,
            backend_used: ChatBackend::Heuristic,
            confidence: 0.0,
            reason: format!("llm down ({err}) → reject (strict mode)"),
            fallback_applied: true,
        },
        FallbackStrategy::AdmitPendingReview => CuratorDecision {
            final_status: NoteStatus::PendingReview,
            backend_used: ChatBackend::Heuristic,
            confidence: verdict_h.confidence,
            reason: format!("llm down ({err}) → admit pending review (soft)"),
            fallback_applied: true,
        },
    }
}
