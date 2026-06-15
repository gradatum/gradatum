//! Tests for `Note::verify_integrity` (P1-1 audit L0 2026-05-04).

use chrono::Utc;
use gradatum_core::author::{AuthorKind, AuthorRef};
use gradatum_core::error::DriftError;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use smallvec::SmallVec;

fn build_note(body: &str) -> Note {
    let frontmatter = Frontmatter {
        schema_version: 1,
        vault_id: VaultId("main".into()),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: SmallVec::new(),
        author: Some(AuthorRef {
            kind: AuthorKind::Human,
            id: "operator".into(),
            display_name: None,
        }),
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
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

#[test]
fn verify_integrity_ok_when_consistent() {
    let note = build_note("body");
    assert!(note.verify_integrity().is_ok());
}

#[test]
fn verify_integrity_detects_body_drift() {
    let mut note = build_note("original body");
    // Tamper with the body without recomputing the hash — simulates external edit.
    note.body.markdown = "tampered body".into();
    let result = note.verify_integrity();
    assert!(matches!(
        result,
        Err(DriftError::ContentHashMismatch { .. })
    ));
}

#[test]
fn verify_integrity_detects_frontmatter_drift() {
    let mut note = build_note("body");
    // Tamper with the frontmatter without recomputing the hash.
    note.frontmatter.status = NoteStatus::Garbage;
    assert!(note.verify_integrity().is_err());
}

#[test]
fn verify_integrity_idempotent() {
    let note = build_note("body");
    assert!(note.verify_integrity().is_ok());
    assert!(note.verify_integrity().is_ok());
}
