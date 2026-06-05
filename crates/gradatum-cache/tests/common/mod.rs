//! Helpers partagés entre les tests d'intégration de `gradatum-cache`.

use std::sync::Arc;

use gradatum_core::{
    author::AuthorRef,
    frontmatter::Frontmatter,
    identity::{ContentHash, NoteId, NoteVersion},
    note::{EffectiveNote, NoteBody},
    scope::VaultId,
    section::Section,
    status::NoteStatus,
};

/// Construit un `EffectiveNote` minimal valide pour les tests.
///
/// - `vault_id` = `"test"`
/// - `section` = `Section::Decisions`
/// - `status` = `NoteStatus::Live`
/// - `author` = humain `"test-user"`
/// - `content_hash` = `ContentHash([0x00; 32])` (surchargeable par le caller)
pub fn dummy_effective_note(id: NoteId) -> Arc<EffectiveNote> {
    Arc::new(EffectiveNote {
        id,
        frontmatter: Frontmatter {
            schema_version: 1,
            vault_id: VaultId::new("test"),
            locus: None,
            section: Section::Decisions,
            status: NoteStatus::Live,
            status_reason: None,
            status_changed: None,
            tags: Default::default(),
            author: Some(AuthorRef::human("test-user")),
            created: chrono::DateTime::from_timestamp(0, 0)
                .expect("timestamp Unix 0 est une date valide (1970-01-01T00:00:00Z)"),
            updated: None,
            extra: Default::default(),
        },
        body: NoteBody {
            markdown: "# Test note".to_string(),
        },
        version: NoteVersion::initial(),
        content_hash: ContentHash([0x00; 32]),
    })
}
