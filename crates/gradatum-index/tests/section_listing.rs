//! Tests — `IndexStore::list_notes_by_section` (résolution de scope `Section`, F-112).
//!
//! L'axe de consolidation du cron distill est la **section canonique**, pas le locus
//! (colonne `locus` NULL sur tout le corpus). Cette méthode est l'équivalent par section
//! de `list_notes_by_locus_prefix` : `(id, section)`, `forgotten = 0`.

mod common;
use common::make_note;

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Filtre par section canonique : les notes d'une section sont retournées, celles des
/// autres sections ne le sont pas.
#[tokio::test]
async fn list_notes_by_section_filters_by_canonical_section() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let d1 = make_note("main", Section::Debug, NoteStatus::Live, "# D1\n\ncorps");
    let d2 = make_note("main", Section::Debug, NoteStatus::Live, "# D2\n\ncorps");
    let r1 = make_note(
        "main",
        Section::Reference,
        NoteStatus::Live,
        "# R1\n\ncorps",
    );
    idx.upsert_note(&d1).await.unwrap();
    idx.upsert_note(&d2).await.unwrap();
    idx.upsert_note(&r1).await.unwrap();

    let debug = idx
        .list_notes_by_section("main", "debug")
        .await
        .expect("list debug");
    let debug_ids: std::collections::HashSet<String> =
        debug.iter().map(|(id, _)| id.clone()).collect();
    assert!(
        debug_ids.contains(&d1.id.to_string()) && debug_ids.contains(&d2.id.to_string()),
        "les deux notes debug doivent être listées : {debug_ids:?}"
    );
    assert!(
        !debug_ids.contains(&r1.id.to_string()),
        "la note reference ne doit pas apparaître en debug : {debug_ids:?}"
    );

    let reference = idx
        .list_notes_by_section("main", "reference")
        .await
        .expect("list reference");
    assert_eq!(reference.len(), 1, "une seule note reference");
    assert_eq!(reference[0].0, r1.id.to_string());
}

/// Section inconnue → liste vide (pas d'erreur).
#[tokio::test]
async fn list_notes_by_section_unknown_section_is_empty() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let d1 = make_note("main", Section::Debug, NoteStatus::Live, "# D1\n\ncorps");
    idx.upsert_note(&d1).await.unwrap();

    let rows = idx
        .list_notes_by_section("main", "inexistante")
        .await
        .expect("section inconnue — liste vide, pas d'erreur");
    assert!(rows.is_empty(), "section inconnue → 0 note");
}
