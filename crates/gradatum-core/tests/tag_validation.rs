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

// ── Tag::normalize ────────────────────────────────────────────────────────────

/// Tags invalides normalisables → valeur attendue après normalisation.
#[test]
fn normalize_invalid_becomes_valid() {
    // Colon et majuscules → tiret + lowercase.
    assert_eq!(
        Tag::normalize("status:OPEN").map(|t| t.as_str().to_owned()),
        Some("status-open".to_owned()),
        "colon et majuscules"
    );

    // Points → tiret, chiffre conservé.
    assert_eq!(
        Tag::normalize("v0.5.3").map(|t| t.as_str().to_owned()),
        Some("v0-5-3".to_owned()),
        "points remplacés par tiret"
    );

    // Majuscules seules → lowercase.
    assert_eq!(
        Tag::normalize("P1").map(|t| t.as_str().to_owned()),
        Some("p1".to_owned()),
        "majuscules lowercasées"
    );

    // Plusieurs mots spéciaux combinés.
    assert_eq!(
        Tag::normalize("status:IN_PROGRESS").map(|t| t.as_str().to_owned()),
        Some("status-in-progress".to_owned()),
        "colon + underscore + majuscules"
    );

    // Espaces et caractères spéciaux en tête/queue.
    assert_eq!(
        Tag::normalize("  --Hello World!! ").map(|t| t.as_str().to_owned()),
        Some("hello-world".to_owned()),
        "espaces et tirets en tête/queue trimés"
    );
}

/// Tags déjà valides inchangés après normalisation.
#[test]
fn normalize_already_valid_unchanged() {
    assert_eq!(
        Tag::normalize("knowledge-base").map(|t| t.as_str().to_owned()),
        Some("knowledge-base".to_owned()),
        "tag valide inchangé"
    );
    assert_eq!(
        Tag::normalize("foo").map(|t| t.as_str().to_owned()),
        Some("foo".to_owned()),
        "tag simple inchangé"
    );
    assert_eq!(
        Tag::normalize("v0-5-3").map(|t| t.as_str().to_owned()),
        Some("v0-5-3".to_owned()),
        "tag chiffres-tirets inchangé"
    );
}

/// Tags inrécupérables → None.
#[test]
fn normalize_irrecoverable_returns_none() {
    assert_eq!(Tag::normalize("___"), None, "underscores seuls");
    assert_eq!(Tag::normalize(""), None, "vide");
    assert_eq!(Tag::normalize("---"), None, "tirets seuls → trimés → vide");
    assert_eq!(Tag::normalize("!!!"), None, "caractères spéciaux seuls");
    assert_eq!(Tag::normalize("   "), None, "espaces seuls");
}

/// Troncature à 64 caractères, sans tiret final après troncature.
#[test]
fn normalize_truncates_at_64_no_trailing_dash() {
    // Tag de 70 chars valides → tronqué à 64.
    let long_tag = "a".repeat(70);
    let result = Tag::normalize(&long_tag);
    assert!(result.is_some(), "tag long normalisable");
    let norm = result.unwrap();
    assert_eq!(norm.as_str().len(), 64, "tronqué à 64 chars");
    assert!(
        Tag::new(norm.as_str()).is_ok(),
        "résultat satisfait Tag::new"
    );

    // Tag dont la troncature tombe sur un tiret → tiret final retiré.
    // Construire : 62 'a' + '--' + 'bb' = 66 chars → après lowercase + run-merge → '62a-bb'
    // tronqué à 64 = '62a-' → trim_end('-') → '62a'
    let tricky = format!("{}-bb", "a".repeat(62));
    // tricky = 65 chars : 'aaa...a-bb'
    let norm2 = Tag::normalize(&tricky);
    assert!(norm2.is_some(), "tricky normalisable");
    let norm2 = norm2.unwrap();
    assert!(
        !norm2.as_str().ends_with('-'),
        "pas de tiret final après troncature"
    );
    assert!(
        Tag::new(norm2.as_str()).is_ok(),
        "résultat satisfait Tag::new"
    );
}

/// Propriété : Tag::normalize(x) produit toujours un Tag::new-valide si Some.
#[test]
fn normalize_output_always_valid() {
    let samples = [
        "status:OPEN",
        "v0.5.3",
        "P1",
        "status:IN_PROGRESS",
        "  --Hello World!! ",
        "knowledge-base",
        "UPPER",
        "space inside",
        "-leading-dash",
        "under_score",
        "dot.dot",
        "MIXED-Case",
        "emoji💥",
        "foo",
        "already-valid",
        "a",
    ];
    for s in &samples {
        if let Some(norm) = Tag::normalize(*s) {
            assert!(
                Tag::new(norm.as_str()).is_ok(),
                "normalize({s:?}) → {norm:?} non valide pour Tag::new"
            );
        }
    }
}
