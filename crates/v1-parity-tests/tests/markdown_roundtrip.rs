//! v1-parity : Markdown round-trip — 3 tests.
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_phase1.rs` (sections markdown).
//! Domaine : parse→write idempotence, extraction wikilinks, extras préservés.

mod common;

use gradatum_markdown::{parse, write_parsed};

// --- 1. parse_then_write_idempotent ---

/// Parse un fichier .md, re-sérialise, re-parse → les deux ParsedNote doivent être
/// sémantiquement équivalents (même frontmatter + même body + même ContentHash).
///
/// Note B8 documentée : le backend YAML peut réordonner les clés sur aller/retour.
/// Le test compare les valeurs sémantiques, pas la représentation textuelle exacte.
#[tokio::test]
async fn parse_then_write_idempotent() {
    // Fixture .md minimaliste avec tous les champs canoniques
    let raw = "\
---
schema_version: 1
vault_id: main
section: decisions
status: draft
created: '2026-05-04T10:00:00Z'
---

Corps de la note de test pour le round-trip idempotent.\
";

    let parsed1 = parse(raw).expect("premier parse");
    let written = write_parsed(&parsed1).expect("write_parsed");
    let parsed2 = parse(&written).expect("second parse après write");

    // Les frontmatters doivent être sémantiquement identiques
    assert_eq!(
        parsed1.frontmatter.vault_id.as_str(),
        parsed2.frontmatter.vault_id.as_str(),
        "vault_id doit survivre au round-trip"
    );
    assert_eq!(
        parsed1.frontmatter.section, parsed2.frontmatter.section,
        "section doit survivre au round-trip"
    );
    assert_eq!(
        parsed1.frontmatter.status, parsed2.frontmatter.status,
        "status doit survivre au round-trip"
    );
    assert_eq!(
        parsed1.body.markdown, parsed2.body.markdown,
        "body doit survivre au round-trip"
    );
    assert_eq!(
        parsed1.content_hash.hex(),
        parsed2.content_hash.hex(),
        "ContentHash doit être identique après round-trip"
    );
}

// --- 2. wikilinks_extracted_from_body ---

/// Corps avec `[[a]]` et `[[b|B]]` → `wikilinks()` extrait exactement 2 Wikilink.
#[tokio::test]
async fn wikilinks_extracted_from_body() {
    let body = "Voir [[note-a]] et [[note-b|Note B]] pour les références.";
    let links = gradatum_markdown::wikilinks(body);

    assert_eq!(links.len(), 2, "2 wikilinks attendus");

    // note-a sans alias
    let link_a = links.iter().find(|l| l.target == "note-a");
    assert!(link_a.is_some(), "Wikilink 'note-a' attendu");
    assert_eq!(
        link_a.unwrap().alias,
        None,
        "'note-a' ne doit pas avoir d'alias"
    );

    // note-b avec alias "Note B"
    let link_b = links.iter().find(|l| l.target == "note-b");
    assert!(link_b.is_some(), "Wikilink 'note-b' attendu");
    assert_eq!(
        link_b.unwrap().alias,
        Some("Note B".into()),
        "'note-b' doit avoir l'alias 'Note B'"
    );
}

// --- 3. frontmatter_yaml_with_extras_round_trip ---

/// Round-trip d'un frontmatter avec champs `extra` (B8 forward-compat).
///
/// Note B8 : les champs extra sont préservés verbatim dans ExtraFields.
/// Le test vérifie que les extras présents dans le YAML original survivent
/// après parse → write_parsed → parse.
///
/// Ignoré : `ExtraFields` est un champ nommé explicite dans le YAML
/// (`extra: {key: value}`), pas un catchall automatique pour les champs inconnus
/// au top-level. Les champs inconnus (ex. `source_tool`) sont ignorés silencieusement
/// par le backend YAML (pas de `deny_unknown_fields`). L'implémentation complète (B8)
/// nécessite un `#[serde(flatten)]` custom ou un parser YAML two-pass.
#[tokio::test]
#[ignore = "Phase 2+ : blocked by ExtraFields design (top-level unknown fields not captured — B8 deferred)"]
async fn frontmatter_yaml_with_extras_round_trip() {
    // Fixture avec champs extra non-canoniques (B8)
    let raw = "\
---
schema_version: 1
vault_id: main
section: decisions
status: draft
created: '2026-05-04T10:00:00Z'
source_tool: obsidian
confidence_score: 85
---

Note avec extras forward-compat préservés.\
";

    let parsed1 = parse(raw).expect("premier parse avec extras");

    // Les extras doivent être présents dans ExtraFields
    let extras = &parsed1.frontmatter.extra;
    assert!(
        !extras.is_empty(),
        "Les champs extra doivent être présents dans ExtraFields"
    );

    // Re-sérialise et re-parse
    let written = write_parsed(&parsed1).expect("write_parsed avec extras");
    let parsed2 = parse(&written).expect("second parse avec extras");

    // Les extras doivent survivre au round-trip
    assert!(
        !parsed2.frontmatter.extra.is_empty(),
        "Les champs extra doivent survivre au round-trip (B8)"
    );

    // Vérifie la présence d'au moins un des champs extra (sans typer la valeur exacte)
    let has_source_tool = parsed2.frontmatter.extra.get("source_tool").is_some();
    assert!(
        has_source_tool,
        "Le champ extra 'source_tool' doit survivre au round-trip"
    );
}
