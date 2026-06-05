//! Historique des versions d'une note — `NoteHistoryEntry`.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.9.
//!
//! ## Phase 1 scaffold
//!
//! `NoteHistoryEntry` est déclarée mais non persistée en Phase 1.
//! Le champ `diff_text` est toujours une chaîne vide en Phase 1 — le diff unifié
//! (représentation textuelle des changements entre deux versions) sera implémenté en Phase 2.
//!
//! En Phase 2+, l'historique sera stocké dans une table dédiée `note_history`
//! et le diff calculé via la lib `similar` (diff Myers).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use gradatum_core::author::AuthorRef;
use gradatum_core::identity::{NoteId, NoteVersion};

/// Entrée d'historique pour une version d'une note.
///
/// Représente une transition atomique entre deux versions d'une note.
/// Phase 1 = scaffold ; Phase 2+ = diff unifié + persistance SQL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteHistoryEntry {
    /// Identifiant de la note concernée.
    pub note_id: NoteId,

    /// Version avant la transition.
    pub from_version: NoteVersion,

    /// Version après la transition.
    pub to_version: NoteVersion,

    /// Diff textuel unifié entre les deux versions.
    ///
    /// **Phase 1** : toujours chaîne vide `""`.
    /// **Phase 2+** : diff format unifié produit par `similar::TextDiff`.
    pub diff_text: String,

    /// Timestamp de la transition.
    pub committed_at: DateTime<Utc>,

    /// Auteur de la transition (humain, agent, système).
    pub committed_by: AuthorRef,

    /// Message décrivant le changement (optionnel).
    ///
    /// Analogue au commit message Git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,

    /// Identifiant de corrélation pour tracer les opérations multi-notes.
    ///
    /// Utile pour corréler une session d'édition batch ou un import massif.
    /// Phase 1 = toujours `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Ulid>,
}

impl NoteHistoryEntry {
    /// Crée une entrée d'historique Phase 1 (diff vide, correlation_id vide).
    pub fn new(
        note_id: NoteId,
        from_version: NoteVersion,
        to_version: NoteVersion,
        committed_by: AuthorRef,
        commit_message: Option<String>,
    ) -> Self {
        Self {
            note_id,
            from_version,
            to_version,
            diff_text: String::new(), // Phase 1 : diff vide
            committed_at: Utc::now(),
            committed_by,
            commit_message,
            correlation_id: None, // Phase 1 : pas de corrélation
        }
    }
}
