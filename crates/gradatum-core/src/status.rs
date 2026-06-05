//! Cycle de vie d'une note Gradatum — `NoteStatus` + state machine.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.6.
//!
//! ## Flux standard
//!
//! ```text
//! Draft ──┬─→ PendingReview ──┬─→ Live          (curator admit)
//!         │                    ├─→ Garbage        (curator reject)
//!         │                    └─→ Staging ──┬─→ Live  (humain review optional)
//!         │                                  └─→ Garbage
//!         └─→ Garbage          (CLI direct trash)
//! Live ──┬─→ Deprecated         (replace par autre NoteId)
//!        └─→ Garbage             (delete explicite)
//! Deprecated ──→ Live            (restore)
//! Garbage    ──→ Live            (restore avant cleanup async)
//! ```

use serde::{Deserialize, Serialize};

use crate::config::EmbedConfig;

/// Statut du cycle de vie d'une note.
///
/// Décision Q6 brainstorming the maintainer 2026-05-03 : β workflow-aware (Live+PendingReview+Staging
/// embeddables par défaut) + α configurable runtime via `[embed] embeddable_status` TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteStatus {
    /// Brouillon local, pas encore soumis au pipeline curator.
    Draft,
    /// Soumis, en attente de review humain (workflow optionnel).
    Staging,
    /// En attente de jugement curator (heuristique ou LLM).
    ///
    /// State par défaut attribué par `gradatum-chat::Heuristic` (invariant #3, B4).
    PendingReview,
    /// Admis, indexé, searchable, embeddable.
    Live,
    /// Remplacé par un autre NoteId. Successeur référencé dans `extra`.
    Deprecated,
    /// Rejeté → nettoyage async par le worker.
    Garbage,
}

impl NoteStatus {
    /// Vérifie si la transition vers `target` est valide selon la state machine spec §2.6.
    ///
    /// Utilisé par `gradatum-vault::update_status` pour enforcer les invariants de lifecycle.
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

    /// Est-ce que ce statut est visible dans l'API de lecture par défaut ?
    pub fn is_visible_default(&self) -> bool {
        matches!(self, NoteStatus::Live)
    }

    /// Est-ce que ce statut doit être embeddé par défaut ?
    ///
    /// **Workflow-aware β** (décision Q6 2026-05-03) :
    /// `[Live, PendingReview, Staging]` — l'embedding est précomputé pour les statuts
    /// "review-or-better" afin que :
    /// - Le curator puisse comparer sémantiquement une candidate à des notes équivalentes
    ///   en attente d'admission.
    /// - Le coût d'embed ne soit pas re-payé au passage `PendingReview → Live`.
    ///
    /// Exclus par défaut : `Draft` (pas d'engagement), `Deprecated`/`Garbage` (sortants).
    pub fn is_embeddable_default(&self) -> bool {
        matches!(
            self,
            NoteStatus::Live | NoteStatus::PendingReview | NoteStatus::Staging
        )
    }

    /// Résout l'embeddabilité en tenant compte de la config runtime.
    ///
    /// Utilisé par `gradatum-worker` dans le pipeline d'embedding.
    /// Si `embed.embeddable_status` est `None` → délègue à `is_embeddable_default()`.
    ///
    /// ## Note d'implémentation
    ///
    /// `EmbedConfig.embeddable_status` est `Option<Vec<String>>` (kebab-case string) —
    /// maintenu délibérément en `String` pour que `config.rs` reste libre de tout type
    /// métier (zéro cycle de dépendances). La comparaison s'effectue via `serde_kebab_repr()`.
    pub fn is_embeddable(&self, cfg: &EmbedConfig) -> bool {
        match cfg.embeddable_status.as_ref() {
            Some(allowed) => allowed.iter().any(|s| s == self.serde_kebab_repr()),
            None => self.is_embeddable_default(),
        }
    }

    /// Représentation kebab-case de ce statut (identique à la sérialisation serde).
    ///
    /// Utilisé pour la comparaison avec `EmbedConfig.embeddable_status: Vec<String>`.
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
