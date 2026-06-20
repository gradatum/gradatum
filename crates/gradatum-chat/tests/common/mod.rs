//! Helpers partagés entre les tests d'intégration de `gradatum-chat`.

use chrono::Utc;
use gradatum_core::{
    frontmatter::{ExtraFields, Frontmatter},
    identity::{ContentHash, NoteId, NoteVersion},
    note::{Note, NoteBody},
    scope::VaultId,
    section::Section,
    status::NoteStatus,
};
use smallvec::SmallVec;

/// Construit une `Note` minimale avec le body Markdown fourni.
///
/// `vault_id` = "test-vault", `section` = Decisions, `status` = Draft.
/// Toutes les autres métadonnées sont à leurs valeurs par défaut.
pub fn build_note_with_body(body: impl Into<String>) -> Note {
    let body_str: String = body.into();
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("test-vault"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: SmallVec::new(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let content_hash = ContentHash::compute(&frontmatter, &body_str);
    Note {
        id: NoteId::new(),
        frontmatter,
        body: NoteBody { markdown: body_str },
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}

/// Construit une `Note` minimale sans contenu (body vide).
///
/// Utilisé dans les tests circuit_breaker où le contenu n'a pas d'importance.
#[allow(dead_code)]
pub fn build_note() -> Note {
    build_note_with_body("")
}

/// Construit une `Note` avec un body court (< 50 chars).
///
/// Prévu pour les tests heuristiques de rejet (body insuffisant).
#[allow(dead_code)]
pub fn build_short_note() -> Note {
    build_note_with_body("trop court")
}
