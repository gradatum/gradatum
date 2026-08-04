//! Tests round-trip serde de Frontmatter.
//!
//! Vérifie que la sérialisation YAML puis désérialisation produit un Frontmatter identique.
//! La fixture YAML couvre tous les champs canoniques + 2 tags.

use gradatum_core::frontmatter::Frontmatter;

/// Round-trip YAML depuis fixture : parse → sérialise → re-parse → égalité.
#[test]
fn frontmatter_yaml_round_trip_canonical_fields() {
    let raw = include_str!("fixtures/frontmatter-with-extras.yaml");
    let fm: Frontmatter = serde_norway::from_str(raw).expect("parse frontmatter fixture");

    // Vérifications de base.
    assert_eq!(fm.schema_version, 1);
    assert_eq!(fm.vault_id, "main");
    // LocusId est un newtype — as_deref() n'est pas disponible ; on compare via as_str().
    assert_eq!(fm.locus.as_ref().map(|l| l.as_str()), Some("decisions"));
    assert_eq!(fm.tags.len(), 2);
    assert_eq!(fm.tags[0].as_str(), "council-art19");
    assert_eq!(fm.tags[1].as_str(), "recovery-skills");

    // Round-trip : sérialise puis re-parse.
    let written = serde_norway::to_string(&fm).expect("sérialise frontmatter");
    let reparsed: Frontmatter = serde_norway::from_str(&written).expect("re-parse frontmatter");

    assert_eq!(
        fm, reparsed,
        "round-trip doit produire un Frontmatter identique"
    );
}

/// ExtraFields vide : omis en sérialisation (skip_serializing_if).
#[test]
fn extra_fields_empty_omitted_in_serialization() {
    use gradatum_core::author::AuthorRef;
    use gradatum_core::frontmatter::ExtraFields;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let fm = Frontmatter {
        schema_version: 1,
        vault_id: "main".into(),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags: smallvec::SmallVec::new(),
        author: Some(AuthorRef::system("gradatum-test")),
        created: chrono::DateTime::parse_from_rfc3339("2026-05-04T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };

    let yaml = serde_norway::to_string(&fm).expect("sérialise");
    assert!(
        !yaml.contains("extra"),
        "extra vide ne doit pas apparaître dans le YAML : {yaml}"
    );
}

/// ExtraFields avec contenu : préservé après round-trip.
#[test]
fn extra_fields_with_content_round_trip() {
    use gradatum_core::author::AuthorRef;
    use gradatum_core::frontmatter::ExtraFields;
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;

    let mut extra = ExtraFields::empty();
    extra.insert(
        "custom_key".into(),
        toml::Value::String("custom_value".into()),
    );
    extra.insert("priority".into(), toml::Value::Integer(42));

    let fm = Frontmatter {
        schema_version: 1,
        vault_id: "main".into(),
        locus: None,
        section: Section::Reasoning,
        status: NoteStatus::Draft,
        status_reason: None,
        status_changed: None,
        tags: smallvec::SmallVec::new(),
        author: Some(AuthorRef::human("test-author@example.com")),
        created: chrono::DateTime::parse_from_rfc3339("2026-05-04T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        updated: None,
        extra,
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };

    // Round-trip JSON (JCS-compatible).
    let json = serde_json::to_string(&fm).expect("sérialise JSON");
    let reparsed: Frontmatter = serde_json::from_str(&json).expect("re-parse JSON");
    assert_eq!(fm, reparsed, "round-trip JSON avec extra");

    // Vérifie la valeur.
    assert_eq!(
        fm.extra.get("custom_key"),
        Some(&toml::Value::String("custom_value".into())),
    );
    assert_eq!(fm.extra.get("priority"), Some(&toml::Value::Integer(42)),);
    assert!(fm.extra.get("absent").is_none());
}

/// tags utilise SmallVec : 4 tags inline sans allocation.
#[test]
fn tags_inline_smallvec_up_to_4() {
    use gradatum_core::tag::Tag;

    let raw = include_str!("fixtures/frontmatter-with-extras.yaml");
    let fm: Frontmatter = serde_norway::from_str(raw).expect("parse fixture");

    // Vérification indirecte : SmallVec<[Tag; 4]> est inline jusqu'à 4.
    // On vérifie juste que les tags sont bien là.
    assert_eq!(fm.tags.len(), 2);
    assert!(fm.tags.iter().all(|t| Tag::new(t.as_str()).is_ok()));
}
