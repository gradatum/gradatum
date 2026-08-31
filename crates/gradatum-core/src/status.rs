//! Gradatum note lifecycle — `NoteStatus` and state machine.
//!
//! ## Standard flow
//!
//! ```text
//! Draft ──┬─→ PendingReview ──┬─→ Live          (curator admit)
//!         │                    ├─→ Garbage        (curator reject)
//!         │                    └─→ Staging ──┬─→ Live  (optional human review)
//!         │                                  └─→ Garbage
//!         └─→ Garbage          (CLI direct trash)
//! Live ──┬─→ Deprecated         (replaced by another NoteId)
//!        └─→ Garbage             (explicit delete)
//! Deprecated ──→ Live            (restore)
//! Garbage    ──→ Live            (restore before async cleanup)
//! ```

use serde::{Deserialize, Serialize};

use crate::config::EmbedConfig;

/// Note lifecycle status.
///
/// Workflow-aware defaults: `Live`, `PendingReview`, and `Staging` are embeddable by default;
/// this set is configurable at runtime via `[embed] embeddable_status` in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteStatus {
    /// Local draft, not yet submitted to the curator pipeline.
    Draft,
    /// Submitted, awaiting optional human review.
    Staging,
    /// Awaiting curator judgement (heuristic or LLM).
    ///
    /// Default state assigned by `gradatum-chat::Heuristic`.
    PendingReview,
    /// Admitted, indexed, searchable, embeddable.
    Live,
    /// Replaced by another `NoteId`. Successor referenced in `extra`.
    Deprecated,
    /// Rejected → async cleanup by the worker.
    Garbage,
}

impl NoteStatus {
    /// Checks whether the transition to `target` is valid per the state machine.
    ///
    /// Used by `gradatum-vault::update_status` to enforce lifecycle invariants.
    pub fn can_transition_to(&self, target: NoteStatus) -> bool {
        use NoteStatus::*;
        matches!(
            (self, target),
            (Draft, PendingReview)
                | (Draft, Garbage)
                | (PendingReview, Live)
                | (PendingReview, Garbage)
                | (PendingReview, Staging)
                | (Staging, Live)
                | (Staging, Garbage)
                | (Live, Deprecated)
                | (Live, Garbage)
                | (Deprecated, Live)  // restore
                | (Garbage, Live) // restore avant cleanup async
        )
    }

    /// Returns `true` if this status is visible in the read API by default.
    pub fn is_visible_default(&self) -> bool {
        matches!(self, NoteStatus::Live)
    }

    /// Returns `true` if this status is embeddable by default.
    ///
    /// **Workflow-aware defaults**: `[Live, PendingReview, Staging]` — embeddings are
    /// pre-computed for "review-or-better" statuses so that:
    /// - The curator can semantically compare a candidate against equivalent notes
    ///   awaiting admission.
    /// - The embedding cost is not re-paid at the `PendingReview → Live` transition.
    ///
    /// Excluded by default: `Draft` (no commitment), `Deprecated`/`Garbage` (outgoing).
    pub fn is_embeddable_default(&self) -> bool {
        matches!(
            self,
            NoteStatus::Live | NoteStatus::PendingReview | NoteStatus::Staging
        )
    }

    /// Every `NoteStatus` variant, in a fixed order — the single enumeration roster.
    ///
    /// Kept in core so no consumer recopies the variant list just to iterate it. The
    /// embedding backfill (which repairs) and the drift detector (which alerts) both derive
    /// their status filter from this roster + [`Self::is_embeddable_default`], so they can
    /// never disagree on "should have a vector". A compile-time exhaustiveness guard below
    /// breaks compilation if a variant is added.
    pub const ALL: [NoteStatus; 6] = [
        NoteStatus::Draft,
        NoteStatus::Staging,
        NoteStatus::PendingReview,
        NoteStatus::Live,
        NoteStatus::Deprecated,
        NoteStatus::Garbage,
    ];

    /// SQL `IN (...)` quoted list of the statuses embeddable by default, derived from
    /// [`Self::ALL`] filtered by [`Self::is_embeddable_default`] — e.g.
    /// `'live', 'pending-review', 'staging'`.
    ///
    /// **Single source of "should have a vector".** The embedding backfill and the drift
    /// detector both call this: a note the backfill would embed is exactly a note the
    /// detector flags when its vector is missing. Scoping the detector any narrower (e.g.
    /// `live` only) would leave a repairable-but-unsignalled note — the very blind spot
    /// the drift scan closes.
    ///
    /// Values come from the enum's kebab [`Display`](std::fmt::Display), never a
    /// hand-written literal — no injection surface.
    #[must_use]
    pub fn embeddable_default_sql_list() -> String {
        Self::ALL
            .into_iter()
            .filter(Self::is_embeddable_default)
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Resolves embeddability taking runtime configuration into account.
    ///
    /// Used by `gradatum-worker` in the embedding pipeline.
    /// If `embed.embeddable_status` is `None` → delegates to `is_embeddable_default()`.
    ///
    /// ## Implementation note
    ///
    /// `EmbedConfig.embeddable_status` is `Option<Vec<String>>` (kebab-case strings) —
    /// deliberately kept as `String` so that `config.rs` remains free of domain types
    /// (zero dependency cycle). Comparison is performed via `serde_kebab_repr()`.
    pub fn is_embeddable(&self, cfg: &EmbedConfig) -> bool {
        match cfg.embeddable_status.as_ref() {
            Some(allowed) => allowed.iter().any(|s| s == self.serde_kebab_repr()),
            None => self.is_embeddable_default(),
        }
    }

    /// Kebab-case representation of this status (identical to the serde serialisation).
    ///
    /// Used for comparison with `EmbedConfig.embeddable_status: Vec<String>`.
    fn serde_kebab_repr(&self) -> &'static str {
        match self {
            NoteStatus::Draft => "draft",
            NoteStatus::Staging => "staging",
            NoteStatus::PendingReview => "pending-review",
            NoteStatus::Live => "live",
            NoteStatus::Deprecated => "deprecated",
            NoteStatus::Garbage => "garbage",
        }
    }
}

impl std::fmt::Display for NoteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.serde_kebab_repr())
    }
}

// Compile-time guard: if `NoteStatus` gains a variant, this match stops compiling — a
// signal to update `NoteStatus::ALL`. Anonymous `const _`: always evaluated, never
// `dead_code`, zero runtime cost.
const _: () = {
    match NoteStatus::Live {
        NoteStatus::Draft
        | NoteStatus::Staging
        | NoteStatus::PendingReview
        | NoteStatus::Live
        | NoteStatus::Deprecated
        | NoteStatus::Garbage => {}
    }
};
