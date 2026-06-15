//! Curator decision types.

use serde::{Deserialize, Serialize};

use gradatum_chat::ChatBackend;
use gradatum_core::status::NoteStatus;

/// Final curator decision for a note.
///
/// Produced by [`crate::workflow::Curator::decide`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorDecision {
    /// Final status assigned to the note.
    pub final_status: NoteStatus,

    /// Backend that produced the final decision.
    pub backend_used: ChatBackend,

    /// Decision confidence — range `0.0..=1.0`.
    pub confidence: f32,

    /// Textual explanation (logged; never exposed in the public API).
    pub reason: String,

    /// `true` if a fallback strategy was applied (LLM down, etc.).
    pub fallback_applied: bool,
}

/// Strategy applied when the LLM is unavailable.
///
/// Configured via `CuratorConfig.llm_review_fallback` (kebab-case string).
///
/// | Config value                        | Variant                    | Effect                        |
/// |------------------------------------|----------------------------|-------------------------------|
/// | `"pending-review-fallback"` (default) | `PendingReviewFallback` | `PendingReview` + audit hint |
/// | `"reject"`                          | `Reject`                   | `Garbage` (strict rejection)  |
/// | `"admit-pending-review"`            | `AdmitPendingReview`       | `PendingReview` (soft)        |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackStrategy {
    /// Default: LLM down → `PendingReview` + `fallback_applied` flag set.
    PendingReviewFallback,

    /// Strict: LLM down → `Garbage` (note permanently rejected).
    Reject,

    /// Soft: LLM down → `PendingReview` with "llm-unreachable" audit hint.
    AdmitPendingReview,
}

impl FallbackStrategy {
    /// Converts a kebab-case config string into a `FallbackStrategy`.
    ///
    /// Any unknown value is treated as `PendingReviewFallback` (safe default).
    pub fn from_config(s: &str) -> Self {
        match s {
            "reject" => FallbackStrategy::Reject,
            "admit-pending-review" => FallbackStrategy::AdmitPendingReview,
            _ => FallbackStrategy::PendingReviewFallback,
        }
    }
}
