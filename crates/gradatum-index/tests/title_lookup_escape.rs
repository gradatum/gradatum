//! Tests TDD C1 — LIKE escape sur `title_lookup` (Task 19 alpha.15).
//!
//! Vérifie que `title_lookup` traite correctement les wildcards SQLite
//! `%` et `_` dans un titre Markdown : le match doit être EXACT et non
//! étendu aux notes dont le titre correspondrait via interprétation wildcard.

mod common;
use common::make_note;

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// C1 — titre contenant `%` wildcard : title_lookup doit matcher EXACTEMENT.
///
/// Stratégie : vault contient UNIQUEMENT "# UserAgent" (pas "# User%").
/// - Sans escape LIKE : `# User%\n%` matche `# UserAgent\n...` car `%` = N chars.
///   Résultat attendu si bug : `Some(id_agent)`.
/// - Avec escape correct : `# User\%\n%` ne matche PAS `# UserAgent\n...`.
///   Résultat attendu si correct : `None`.
#[tokio::test]
async fn title_lookup_percent_wildcard_matches_exactly() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Seule la note "UserAgent" est dans le vault.
    let note_agent = make_note(
        "test",
        Section::Debug,
        NoteStatus::Live,
        "# UserAgent\n\nNote agent — simulant injection percent",
    );

    idx.upsert_note(&note_agent).await.unwrap();

    // Cherche "User%" — seule UserAgent est présente.
    // Bug (sans escape) : retourne UserAgent car `%` LIKE matche "Agent".
    // Correct (avec escape) : retourne None car le title littéral "User%" n'existe pas.
    let result = idx
        .title_lookup("test", "User%")
        .await
        .expect("title_lookup");

    assert!(
        result.is_none(),
        "title_lookup('User%') doit retourner None — seule '# UserAgent' est présente, \
         le % doit être traité comme littéral (pas wildcard)"
    );
}

/// C1 — titre contenant `_` wildcard : title_lookup doit matcher EXACTEMENT
/// "# Note_1" et non "# NoteX1".
///
/// Stratégie de test robuste : vault contient UNIQUEMENT "# NoteX1" (pas "# Note_1").
/// - Sans escape LIKE : `# Note_1\n%` matche `# NoteX1\n...` car `_` = 1 char quelconque.
///   Résultat attendu si bug : `Some(id_note_x)`.
/// - Avec escape correct : `# Note\_1\n%` ne matche PAS `# NoteX1\n...`.
///   Résultat attendu si correct : `None`.
#[tokio::test]
async fn title_lookup_underscore_wildcard_matches_exactly() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Seule la note "NoteX1" est dans le vault (correspond à Note_1 via LIKE non-escaped).
    let note_x = make_note(
        "test",
        Section::Debug,
        NoteStatus::Live,
        "# NoteX1\n\nNote X — simulant injection underscore",
    );

    idx.upsert_note(&note_x).await.unwrap();

    // Cherche "Note_1" — seule NoteX1 est présente.
    // Bug (sans escape) : retourne NoteX1 car `_` LIKE matche `X`.
    // Correct (avec escape) : retourne None car le title littéral "Note_1" n'existe pas.
    let result = idx
        .title_lookup("test", "Note_1")
        .await
        .expect("title_lookup");

    assert!(
        result.is_none(),
        "title_lookup('Note_1') doit retourner None — seule '# NoteX1' est présente, \
         le _ doit être traité comme littéral (pas wildcard)"
    );
}

/// C1 — titre normal sans wildcards : comportement inchangé.
#[tokio::test]
async fn title_lookup_normal_title_still_works() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note = make_note(
        "test",
        Section::Architecture,
        NoteStatus::Live,
        "# Gradatum Architecture\n\nNote normale",
    );
    let id = note.id.to_string();

    idx.upsert_note(&note).await.unwrap();

    let result = idx
        .title_lookup("test", "Gradatum Architecture")
        .await
        .expect("title_lookup");

    assert_eq!(
        result,
        Some(id),
        "title_lookup sur titre normal doit fonctionner"
    );
}

/// C1 — titre avec backslash : escape correct du backslash.
#[tokio::test]
async fn title_lookup_backslash_in_title() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note = make_note(
        "test",
        Section::Debug,
        NoteStatus::Live,
        "# a\\b\n\nNote avec backslash",
    );
    let id = note.id.to_string();

    idx.upsert_note(&note).await.unwrap();

    let result = idx
        .title_lookup("test", "a\\b")
        .await
        .expect("title_lookup backslash");

    assert_eq!(
        result,
        Some(id),
        "title_lookup sur titre avec backslash doit fonctionner"
    );
}
