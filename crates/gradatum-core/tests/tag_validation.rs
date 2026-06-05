//! Tests de validation du type Tag.

use gradatum_core::tag::Tag;

/// Tags valides selon le format `^[a-z0-9][a-z0-9-]{0,63}$`.
#[test]
fn valid_tags() {
    assert!(Tag::new("foo").is_ok(), "tag simple");
    assert!(Tag::new("foo-bar").is_ok(), "tag avec tiret");
    assert!(Tag::new("a1b2c3").is_ok(), "tag alphanumérique");
    assert!(
        Tag::new("0starts-with-digit").is_ok(),
        "commence par chiffre"
    );
    assert!(Tag::new("a").is_ok(), "un seul caractère");
    assert!(
        Tag::new("council-art19").is_ok(),
        "tag avec chiffre en milieu"
    );
    assert!(
        Tag::new("a".repeat(64)).is_ok(),
        "exactement 64 caractères (limite max)"
    );
}

/// Tags invalides.
#[test]
fn invalid_tags() {
    assert!(Tag::new("").is_err(), "vide");
    assert!(Tag::new("UPPER").is_err(), "majuscules");
    assert!(Tag::new("space inside").is_err(), "espace");
    assert!(Tag::new("-leading-dash").is_err(), "tiret en tête");

    // Trop long.
    let too_long = "a".repeat(65);
    assert!(Tag::new(&too_long).is_err(), "65 caractères : trop long");

    // Caractères spéciaux.
    assert!(Tag::new("under_score").is_err(), "underscore interdit");
    assert!(Tag::new("dot.dot").is_err(), "point interdit");
    assert!(Tag::new("MIXED-Case").is_err(), "mixte majuscules");
    assert!(Tag::new("with space").is_err(), "espace interdit");
    assert!(Tag::new("emoji💥").is_err(), "emoji interdit");
}

/// Tiret en queue est valide selon spec (pattern [a-z0-9-]* sans contrainte sur dernier char).
#[test]
fn trailing_dash_is_valid() {
    assert!(
        Tag::new("trailing-").is_ok(),
        "tiret en queue autorisé : les caractères intérieurs [a-z0-9-] incluent le tiret"
    );
}

/// as_str() retourne la valeur exacte fournie.
#[test]
fn as_str_returns_value() {
    let t = Tag::new("council-art19").unwrap();
    assert_eq!(t.as_str(), "council-art19");
}

/// Sérialisation serde : transparent, pas de wrapper.
#[test]
fn serde_roundtrip_transparent() {
    let t = Tag::new("my-tag").unwrap();
    let json = serde_json::to_string(&t).unwrap();
    // Transparent : le tag est sérialisé comme une simple string JSON.
    assert_eq!(json, r#""my-tag""#);

    let deserialized: Tag = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, t);
}
