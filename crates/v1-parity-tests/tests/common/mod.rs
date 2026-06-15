//! Helpers partagés pour la suite v1-parity-tests.
#![allow(dead_code)]

use chrono::Utc;
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Construit un `Frontmatter` minimal pour les tests.
///
/// Champs non fournis → valeurs par défaut minimales (pas de locus, section Decisions,
/// statut Draft).
pub fn minimal_frontmatter(vault_id: &str) -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new(vault_id),
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

/// Construit un `Frontmatter` avec une section et un statut choisis.
pub fn frontmatter_with_status(
    vault_id: &str,
    section: Section,
    status: NoteStatus,
) -> Frontmatter {
    Frontmatter {
        section,
        status,
        ..minimal_frontmatter(vault_id)
    }
}
