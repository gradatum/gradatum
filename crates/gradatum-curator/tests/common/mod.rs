//! Helpers partagés entre les tests d'intégration de gradatum-curator.

use chrono::Utc;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use smallvec::SmallVec;

/// Construit une `Note` minimale avec le corps Markdown donné.
///
/// Section fixée à `Decisions`, statut initial `Draft`.
/// Utile pour tester le workflow curator sans avoir à construire
/// un `Note` complet à la main dans chaque test.
pub fn build_note_with_body(body: &str) -> Note {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId("test".into()),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: SmallVec::new(),
        author: Some(AuthorRef {
            kind: AuthorKind::Human,
            id: "test".into(),
            display_name: None,
        }),
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
    };
    let content_hash = ContentHash::compute(&frontmatter, body);
    Note {
        id: NoteId::new(),
        frontmatter,
        body: NoteBody {
            markdown: body.to_string(),
        },
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}
