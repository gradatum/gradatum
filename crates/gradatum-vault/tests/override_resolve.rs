//! Tests unitaires T11c — `NoteMetadataOverride` : résolution `Overridable` + `OverridePayload`.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::overrides::{FrontmatterPatch, Overridable, OverridePayload};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_vault::NoteMetadataOverride;

#[test]
fn override_applies_status_change() {
    let base = build_minimal_frontmatter(); // status = Draft

    let patch = FrontmatterPatch {
        status: Some(NoteStatus::Live),
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);
    assert_eq!(effective.status, NoteStatus::Live);
    // Les autres champs restent inchangés
    assert_eq!(effective.section, base.section);
    assert_eq!(effective.tags, base.tags);
}

#[test]
fn override_applies_section_change() {
    let base = build_minimal_frontmatter(); // section = Decisions

    let patch = FrontmatterPatch {
        section: Some(Section::Reasoning),
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);
    assert_eq!(effective.section, Section::Reasoning);
    assert_eq!(effective.status, base.status);
}

#[test]
fn override_appends_tags_no_duplicate() {
    let mut base = build_minimal_frontmatter();
    base.tags.push(Tag::new("rust").unwrap());

    let patch = FrontmatterPatch {
        // "rust" déjà présent → pas de doublon
        // "gradatum" absent → ajouté
        tags_add: vec![Tag::new("rust").unwrap(), Tag::new("gradatum").unwrap()],
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);

    assert_eq!(
        effective.tags.len(),
        2,
        "rust ne doit pas être dupliqué ; gradatum doit être ajouté"
    );
    assert!(effective.tags.contains(&Tag::new("rust").unwrap()));
    assert!(effective.tags.contains(&Tag::new("gradatum").unwrap()));
}

#[test]
fn override_removes_tags() {
    let mut base = build_minimal_frontmatter();
    base.tags.push(Tag::new("old-tag").unwrap());
    base.tags.push(Tag::new("keep-tag").unwrap());

    let patch = FrontmatterPatch {
        tags_remove: vec![Tag::new("old-tag").unwrap()],
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);

    assert_eq!(effective.tags.len(), 1);
    assert!(!effective.tags.contains(&Tag::new("old-tag").unwrap()));
    assert!(effective.tags.contains(&Tag::new("keep-tag").unwrap()));
}

#[test]
fn override_empty_patch_is_identity() {
    let base = build_minimal_frontmatter();
    let patch = FrontmatterPatch::default();

    let effective = NoteMetadataOverride::resolve(&base, &patch);

    assert_eq!(effective.status, base.status);
    assert_eq!(effective.section, base.section);
    assert_eq!(effective.tags, base.tags);
    assert_eq!(effective.author, base.author);
}

#[test]
fn override_type_discriminant_is_metadata() {
    assert_eq!(NoteMetadataOverride::OVERRIDE_TYPE, "metadata");
}

#[test]
fn override_schema_version_is_1() {
    assert_eq!(NoteMetadataOverride::SCHEMA_VERSION, 1);
}

#[test]
fn override_status_reason_is_applied() {
    let base = build_minimal_frontmatter();
    let patch = FrontmatterPatch {
        status_reason: Some("test reason".into()),
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);
    assert_eq!(effective.status_reason.as_deref(), Some("test reason"));
}

#[test]
fn override_author_override_applies() {
    use gradatum_core::author::AuthorRef;

    let base = build_minimal_frontmatter(); // author = None
    let new_author = AuthorRef::human("steph@test.com");

    let patch = FrontmatterPatch {
        author_override: Some(new_author.clone()),
        ..Default::default()
    };

    let effective = NoteMetadataOverride::resolve(&base, &patch);
    assert_eq!(effective.author, Some(new_author));
}
