//! Tests de la couche Identity — NoteId, ContentHash, NoteVersion.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.2.

use gradatum_core::author::AuthorRef;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Construit un Frontmatter minimal valide pour les tests.
fn test_frontmatter_minimal() -> Frontmatter {
    Frontmatter {
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
    }
}

/// Deux NoteId successifs sont distincts.
#[test]
fn note_id_new_is_unique() {
    let a = NoteId::new();
    let b = NoteId::new();
    assert_ne!(a, b, "deux NoteId successifs doivent être distincts");
}

/// Le timestamp du second NoteId est >= au premier (ULID monotone).
#[test]
fn note_id_timestamp_monotonic() {
    let a = NoteId::new();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = NoteId::new();
    assert!(
        b.timestamp_ms() >= a.timestamp_ms(),
        "timestamp ULID doit être monotone : b={} >= a={}",
        b.timestamp_ms(),
        a.timestamp_ms()
    );
}

/// ContentHash est déterministe pour le même input.
#[test]
fn content_hash_deterministic() {
    let fm = test_frontmatter_minimal();
    let h1 = ContentHash::compute(&fm, "body");
    let h2 = ContentHash::compute(&fm, "body");
    assert_eq!(h1, h2, "ContentHash doit être déterministe");
}

/// ContentHash JCS indépendant de l'ordre d'insertion dans ExtraFields.
///
/// BTreeMap garantit l'ordre par clé, et JCS normalise en plus l'ordre des clés JSON.
/// Ce test vérifie que deux frontmatters avec les mêmes extra (dans des ordres d'insertion
/// différents) produisent le même hash.
#[test]
fn content_hash_jcs_field_order_independent() {
    let mut fm_a = test_frontmatter_minimal();
    fm_a.extra.insert("z".into(), toml::Value::Integer(1));
    fm_a.extra.insert("a".into(), toml::Value::Integer(2));

    let mut fm_b = test_frontmatter_minimal();
    fm_b.extra.insert("a".into(), toml::Value::Integer(2));
    fm_b.extra.insert("z".into(), toml::Value::Integer(1));

    // BTreeMap ordonne les clés lexicographiquement quel que soit l'ordre d'insertion.
    // JCS garantit en plus l'ordre dans le JSON canonical.
    assert_eq!(
        ContentHash::compute(&fm_a, "body"),
        ContentHash::compute(&fm_b, "body"),
        "hash indépendant de l'ordre d'insertion dans ExtraFields"
    );
}

/// Corps différents → hashs différents.
#[test]
fn content_hash_changes_with_body() {
    let fm = test_frontmatter_minimal();
    let h1 = ContentHash::compute(&fm, "body 1");
    let h2 = ContentHash::compute(&fm, "body 2");
    assert_ne!(h1, h2, "corps différents → hash différents");
}

/// Frontmatter différent → hash différent.
#[test]
fn content_hash_changes_with_frontmatter() {
    let fm1 = test_frontmatter_minimal();
    let mut fm2 = test_frontmatter_minimal();
    fm2.section = Section::Debug;

    let h1 = ContentHash::compute(&fm1, "body");
    let h2 = ContentHash::compute(&fm2, "body");
    assert_ne!(h1, h2, "frontmatters différents → hash différents");
}

/// NoteVersion monotone : initial() < next().
#[test]
fn note_version_monotonic() {
    let v = NoteVersion::initial();
    let v2 = v.next();
    assert!(v2.0 > v.0, "next() doit être strictement supérieur");
    assert_eq!(v.0, 1, "initial() = 1");
    assert_eq!(v2.0, 2, "next() de initial() = 2");
}

/// hex() retourne exactement 64 chars hexadécimaux lowercase.
#[test]
fn content_hash_hex_format() {
    let fm = test_frontmatter_minimal();
    let h = ContentHash::compute(&fm, "body");
    let hex = h.hex();
    assert_eq!(hex.len(), 64, "hex SHA-256 = 64 caractères");
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "tous les caractères doivent être hexadécimaux : {hex}"
    );
    assert!(
        hex.chars().filter(|c| c.is_ascii_uppercase()).count() == 0,
        "hex doit être en minuscules"
    );
}

/// NoteId::display() retourne une string de 26 chars Base32.
#[test]
fn note_id_display_is_ulid_format() {
    let id = NoteId::new();
    let s = id.to_string();
    assert_eq!(s.len(), 26, "ULID = 26 chars Base32");
    // ULID charset = 0-9 + A-Z (Crockford Base32)
    assert!(
        s.chars().all(|c| c.is_ascii_alphanumeric()),
        "ULID ne contient que des caractères alphanumériques : {s}"
    );
}
