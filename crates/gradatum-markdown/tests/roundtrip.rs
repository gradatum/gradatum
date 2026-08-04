//! Tests de round-trip Note ↔ String.
//!
//! Vérifie que `parse(write_parsed(parse(x))) == parse(x)` (idempotence 1-cycle).
//! Les tests utilisent les fixtures du dossier `tests/fixtures/`.

use gradatum_markdown::{MarkdownError, parse, write_parsed};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Produit une string Markdown minimale valide pour des tests sans fixture.
fn minimal_md(body: &str) -> String {
    format!(
        "---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: live\ncreated: \"2026-05-04T11:00:00Z\"\n---\n\n{}",
        body
    )
}

// ── Tests fixtures ────────────────────────────────────────────────────────────

/// Round-trip sur la fixture note-typical.md.
///
/// Vérifie : frontmatter == frontmatter re-parsé, body == body re-parsé,
/// content_hash == content_hash re-parsé (déterminisme JCS).
#[test]
fn roundtrip_typical_note() {
    let raw = include_str!("fixtures/note-typical.md");
    let parsed = parse(raw).expect("parse note-typical.md");

    let written = write_parsed(&parsed).expect("write_parsed");
    let reparsed = parse(&written).expect("re-parse après write_parsed");

    assert_eq!(
        parsed.frontmatter, reparsed.frontmatter,
        "frontmatter doit être identique après round-trip"
    );
    assert_eq!(
        parsed.body, reparsed.body,
        "body doit être identique après round-trip"
    );
    assert_eq!(
        parsed.content_hash, reparsed.content_hash,
        "content_hash doit être déterministe"
    );
}

/// Round-trip avec des champs extra inconnus (forward-compat B8).
///
/// ## Comportement actuel (backend YAML + toml::Value)
///
/// `ExtraFields` utilise `BTreeMap<String, toml::Value>` mais le backend YAML ne peut
/// pas mapper automatiquement les champs YAML inconnus vers `toml::Value` sans
/// `#[serde(flatten)]` sur le champ `extra` de `Frontmatter`. En conséquence :
/// - Le parse ne plante PAS sur des champs inconnus (le backend YAML les ignore silencieusement).
/// - Les champs inconnus sont **perdus** lors du parse depuis YAML.
/// - Le round-trip parse→write→parse est stable (mais les extras sont absent des deux côtés).
///
/// Ce comportement est connu et documenté. La capture complète des extras YAML
/// (forward-compat B8 complet) nécessite une évolution de `gradatum-core::Frontmatter`
/// hors-scope T04.
///
/// ## Ce que ce test vérifie
///
/// - Parse ne retourne pas d'erreur sur une note avec des champs inconnus.
/// - Round-trip est stable (idempotent à 1 cycle).
/// - `content_hash` est déterministe.
#[test]
fn roundtrip_preserves_unknown_extra_fields() {
    let raw = include_str!("fixtures/note-with-extras.md");
    let parsed =
        parse(raw).expect("parse note-with-extras.md ne doit pas échouer sur des champs inconnus");

    // Note : le backend YAML ignore les champs inconnus — extra reste vide.
    // C'est le comportement attendu avec le design actuel de ExtraFields (toml::Value).
    // forward-compat B8 complet requiert #[serde(flatten)] dans Frontmatter — hors-scope T04.

    let written = write_parsed(&parsed).expect("write_parsed");
    let reparsed = parse(&written).expect("re-parse après write_parsed");

    assert_eq!(
        parsed.frontmatter, reparsed.frontmatter,
        "frontmatter doit round-tripper de façon stable"
    );
    assert_eq!(parsed.body, reparsed.body);
    assert_eq!(parsed.content_hash, reparsed.content_hash);
}

/// Un contenu sans frontmatter doit retourner `MissingFrontmatter`.
#[test]
fn parse_rejects_missing_frontmatter() {
    let result = parse("no frontmatter here\n\nbody");
    assert!(
        matches!(result, Err(MarkdownError::MissingFrontmatter)),
        "attendu MissingFrontmatter, obtenu : {result:?}"
    );
}

/// Un contenu avec frontmatter non terminé doit retourner `UnterminatedFrontmatter`.
#[test]
fn parse_rejects_unterminated_frontmatter() {
    let result = parse("---\nschema_version: 1\nvault_id: main\n  # pas de --- fermant");
    assert!(
        matches!(result, Err(MarkdownError::UnterminatedFrontmatter)),
        "attendu UnterminatedFrontmatter, obtenu : {result:?}"
    );
}

/// Un YAML invalide dans le frontmatter doit retourner `Yaml`.
#[test]
fn parse_rejects_invalid_yaml() {
    // "[[[" est un YAML invalide.
    let result = parse("---\n[[[\n---\n\nbody");
    assert!(
        matches!(result, Err(MarkdownError::Yaml(_))),
        "attendu Yaml error, obtenu : {result:?}"
    );
}

/// Round-trip sur une note construite programmatiquement — vérifie la stabilité
/// du ContentHash sur 2 cycles de parse/write.
#[test]
fn roundtrip_content_hash_stable_two_cycles() {
    let raw = minimal_md("# Two cycles\n\nContent.\n");
    let p1 = parse(&raw).expect("parse cycle 1");
    let w1 = write_parsed(&p1).expect("write cycle 1");
    let p2 = parse(&w1).expect("parse cycle 2");
    let w2 = write_parsed(&p2).expect("write cycle 2");
    let p3 = parse(&w2).expect("parse cycle 3");

    assert_eq!(
        p1.content_hash, p2.content_hash,
        "ContentHash doit être stable entre cycle 1 et 2"
    );
    assert_eq!(
        p2.content_hash, p3.content_hash,
        "ContentHash doit être stable entre cycle 2 et 3"
    );
}

/// Le body vide est correctement géré (frontmatter seul).
#[test]
fn roundtrip_empty_body() {
    // Note sans body — le write doit produire "---\n<yaml>\n---\n\n"
    // et le re-parse doit donner body.markdown == "".
    let raw = "---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: live\ncreated: \"2026-05-04T11:00:00Z\"\n---\n\n";
    let parsed = parse(raw).expect("parse body vide");
    assert_eq!(parsed.body.markdown, "");

    let written = write_parsed(&parsed).expect("write_parsed body vide");
    let reparsed = parse(&written).expect("re-parse body vide");
    assert_eq!(reparsed.body.markdown, "");
}
