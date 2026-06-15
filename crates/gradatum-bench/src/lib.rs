//! # gradatum-bench
//!
//! Bibliothèque utilitaire partagée entre les benches criterion.
//!
//! Fournit des helpers de construction de fixtures (`Frontmatter`, `Note`, etc.)
//! afin d'éviter la duplication entre les 10 fichiers de bench.
//!
//! ## Usage
//!
//! Chaque bench importe uniquement ce dont il a besoin depuis ce module.
//! Les fonctions `build_*` sont volontairement déterministes (pas de `Ulid::new()`
//! en boucle chaude) pour que les mesures criterion restent stables.

#![forbid(unsafe_code)]

use chrono::Utc;
use smallvec::SmallVec;

use gradatum_core::author::AuthorRef;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Construit un `Frontmatter` minimal reproductible pour les benches.
///
/// Déterministe — pas de clock ni d'aléatoire dans la boucle chaude.
pub fn build_frontmatter() -> Frontmatter {
    Frontmatter {
        schema_version: 1,
        vault_id: VaultId("bench".into()),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: SmallVec::new(),
        author: Some(AuthorRef::human("bench-user")),
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    }
}

/// Construit une `Note` complète avec un body de `body_len` bytes (répétition de 'x').
///
/// Utilisée par les benches B3, B6 pour peupler l'index SQLite.
pub fn build_note(body_len: usize) -> Note {
    let fm = build_frontmatter();
    let body = NoteBody {
        markdown: "x".repeat(body_len),
    };
    let content_hash = ContentHash::compute(&fm, &body.markdown);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body,
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    }
}
