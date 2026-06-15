//! Helpers partagés pour les tests d'intégration `gradatum-vault`.
#![allow(dead_code)]

use chrono::Utc;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Construit un `Frontmatter` minimal valide pour les tests.
///
/// - `vault_id` : `"main"`
/// - `section` : `Section::Decisions`
/// - `status` : `NoteStatus::Draft`
/// - tous les champs optionnels à `None` / défaut.
pub fn build_minimal_frontmatter() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: Utc::now(),
        updated: None,
        extra: Default::default(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}
