//! Types de décision du curator.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md`
//! §2.13 (CuratorConfig) + §0.2 B23 (fallback strategies).

use serde::{Deserialize, Serialize};

use gradatum_chat::ChatBackend;
use gradatum_core::status::NoteStatus;

/// Décision finale du curator pour une note.
///
/// Produite par [`crate::workflow::Curator::decide`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorDecision {
    /// Statut final assigné à la note.
    pub final_status: NoteStatus,

    /// Backend ayant produit la décision finale.
    pub backend_used: ChatBackend,

    /// Confiance dans la décision — intervalle `0.0..=1.0`.
    pub confidence: f32,

    /// Explication textuelle (loggée, jamais exposée en API publique).
    pub reason: String,

    /// `true` si une stratégie de fallback a été appliquée (LLM down, etc.).
    pub fallback_applied: bool,
}

/// Stratégie appliquée quand le LLM est indisponible.
///
/// Configurée via `CuratorConfig.llm_review_fallback` (kebab-case string).
///
/// | Valeur config                  | Variant                    | Effet                         |
/// |-------------------------------|----------------------------|-------------------------------|
/// | `"pending-review-fallback"` (défaut) | `PendingReviewFallback` | `PendingReview` + audit hint |
/// | `"reject"`                    | `Reject`                   | `Garbage` (rejet strict)      |
/// | `"admit-pending-review"`      | `AdmitPendingReview`       | `PendingReview` (soft)        |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackStrategy {
    /// Défaut : LLM down → `PendingReview` + flag `fallback_applied`.
    PendingReviewFallback,

    /// Strict : LLM down → `Garbage` (note rejetée définitivement).
    Reject,

    /// Soft : LLM down → `PendingReview` avec indication audit "llm-unreachable".
    AdmitPendingReview,
}

impl FallbackStrategy {
    /// Convertit une string de config kebab-case en `FallbackStrategy`.
    ///
    /// Toute valeur inconnue est traitée comme `PendingReviewFallback` (comportement safe).
    pub fn from_config(s: &str) -> Self {
        match s {
            "reject" => FallbackStrategy::Reject,
            "admit-pending-review" => FallbackStrategy::AdmitPendingReview,
            _ => FallbackStrategy::PendingReviewFallback,
        }
    }
}
