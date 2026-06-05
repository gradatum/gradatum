//! Workflow principal du curator.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md`
//! §2.13 (CuratorConfig) + §0.2 B23 (offline-first + LLM gating + fallbacks).
//! Plan Phase 1 T12 : `docs/superpowers/plans/2026-05-04-phase1-backend-plan.md` lignes 1082–1160.
//!
//! ## Invariant offline-first (#3 / R1)
//!
//! L'heuristique est toujours exécutée EN PREMIER, sans réseau.
//! Le LLM n'est sollicité que si :
//! - la confiance heuristique est ≤ `confidence_threshold` (défaut 0.7), ET
//! - `llm_review_enabled = true`, ET
//! - un backend LLM `C` est fourni (`Some(Arc<C>)`).

use std::sync::Arc;

use gradatum_chat::{Chat, ChatBackend, CuratorContext, Heuristic};
use gradatum_core::config::CuratorConfig;
use gradatum_core::note::Note;
use gradatum_core::status::NoteStatus;

use crate::decision::{CuratorDecision, FallbackStrategy};

/// Orchestrateur du pipeline de curation d'une note.
///
/// `C` = backend LLM optionnel (ex. `HttpChat`, `Noop`, ou un mock de test).
/// Doit implémenter [`Chat`] + `Send + Sync + 'static` (via `async_trait`).
///
/// # Exemple minimal
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
    /// Classificateur heuristique offline (invariant #3 / R1).
    pub heuristic: Heuristic,

    /// Backend LLM optionnel — `None` désactive la revue LLM quelle que soit la config.
    pub llm: Option<Arc<C>>,

    /// Configuration runtime chargée depuis `<vault_root>/.gradatum/config.toml`.
    pub cfg: CuratorConfig,
}

impl<C: Chat> Curator<C> {
    /// Crée un curator avec les composants fournis.
    ///
    /// - `heuristic` : classificateur offline (toujours exécuté en premier).
    /// - `llm` : backend LLM optionnel. `None` = jamais de revue LLM.
    /// - `cfg` : configuration curator (seuils, stratégie fallback).
    pub fn new(heuristic: Heuristic, llm: Option<Arc<C>>, cfg: CuratorConfig) -> Self {
        Self {
            heuristic,
            llm,
            cfg,
        }
    }

    /// Décide du statut final d'une note.
    ///
    /// **Jamais d'erreur** : toutes les défaillances internes sont absorbées en
    /// `CuratorDecision` avec `fallback_applied = true`. L'appelant n'a pas à
    /// gérer d'erreur — la décision est toujours valide et safe.
    ///
    /// ## Workflow
    ///
    /// ```text
    /// 1. Heuristic::classify_curator(note, ctx)
    ///    → erreur interne → PendingReview + fallback_applied=true (cas exceptionnel)
    ///
    /// 2. confidence > threshold (défaut 0.7)
    ///    → fast path : retourne le verdict heuristique directement
    ///
    /// 3. confidence ≤ threshold + llm_review_enabled=false
    ///    → PendingReview, pas d'appel LLM, fallback_applied=false
    ///
    /// 4. confidence ≤ threshold + llm_review_enabled=true + llm=None
    ///    → PendingReview + fallback_applied=true (config incohérente)
    ///
    /// 5. LLM::classify_curator(note, ctx)
    ///    → Ok(verdict_llm) : retourne verdict LLM
    ///    → Err(e) : applique FallbackStrategy depuis config
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

/// Applique la stratégie de fallback quand le LLM est indisponible.
///
/// Extrait pour factoriser les 3 branches et éviter l'imbrication.
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
