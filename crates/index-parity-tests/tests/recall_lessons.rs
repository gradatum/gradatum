//! Parité backend : rappel de leçons (`recall_lessons`, F-60 v0.4.4).
//!
//! Invariant : `recall_lessons(vault, class, limit)` retourne les notes de section
//! `lessons-learned` dont le corps/tags matchent la classe (FTS5 lexical), en
//! excluant les notes downgraded/forgotten. Backend-agnostique via `write_note`
//! (qui peuple `notes_fts`).

mod common;

use common::{make_index, make_note_with_id};
use gradatum_core::frontmatter::Frontmatter;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// Frontmatter de leçon : section `lessons-learned`, statut `Live`.
fn lesson_frontmatter() -> Frontmatter {
    common::frontmatter_with("main", Section::LessonsLearned, NoteStatus::Live)
}

#[tokio::test]
async fn recall_lessons_matches_class_in_body() {
    let idx = make_index().await;
    let vault = VaultId::new("main");

    // Leçon avec la classe "deploy" dans le corps.
    let lesson = make_note_with_id(
        NoteId::new(),
        lesson_frontmatter(),
        "Toujours vérifier le health check avant le deploy.",
    );
    idx.write_note(&lesson).await.expect("write lesson");

    // Note d'une autre section avec "deploy" → exclue (pas lessons-learned).
    let other = make_note_with_id(
        NoteId::new(),
        common::frontmatter_with("main", Section::Decisions, NoteStatus::Live),
        "deploy failed at boot",
    );
    idx.write_note(&other).await.expect("write other");

    let hits = idx
        .recall_lessons(&vault, "deploy", 5)
        .await
        .expect("recall_lessons");

    assert_eq!(
        hits.len(),
        1,
        "seule la leçon lessons-learned matche ({})",
        common::backend_label()
    );
    assert_eq!(hits[0].note_id, lesson.id, "la bonne leçon remonte");
    assert!(!hits[0].snippet.is_empty(), "snippet FTS5 non vide");
}

#[tokio::test]
async fn recall_lessons_respects_limit() {
    let idx = make_index().await;
    let vault = VaultId::new("main");

    for i in 0..5 {
        let lesson = make_note_with_id(
            NoteId::new(),
            lesson_frontmatter(),
            &format!("Leçon numéro {i} sur le sujet caching récurrent."),
        );
        idx.write_note(&lesson).await.expect("write lesson");
    }

    let hits = idx
        .recall_lessons(&vault, "caching", 3)
        .await
        .expect("recall_lessons");
    assert!(
        hits.len() <= 3,
        "limit respecté : {} <= 3 ({})",
        hits.len(),
        common::backend_label()
    );
    assert!(!hits.is_empty(), "au moins une leçon caching remonte");
}

#[tokio::test]
async fn recall_lessons_excludes_forgotten() {
    let idx = make_index().await;
    let vault = VaultId::new("main");

    let lesson = make_note_with_id(
        NoteId::new(),
        lesson_frontmatter(),
        "Leçon oubliée sur le sujet rollback prudent.",
    );
    idx.write_note(&lesson).await.expect("write lesson");

    // Présente avant forget.
    let before = idx
        .recall_lessons(&vault, "rollback", 5)
        .await
        .expect("recall before");
    assert!(
        before.iter().any(|h| h.note_id == lesson.id),
        "leçon présente avant forget"
    );

    idx.mark_forgotten("main", &lesson.id.to_string(), Some("test"))
        .await
        .expect("mark_forgotten");

    let after = idx
        .recall_lessons(&vault, "rollback", 5)
        .await
        .expect("recall after");
    assert!(
        !after.iter().any(|h| h.note_id == lesson.id),
        "leçon oubliée exclue de recall_lessons ({})",
        common::backend_label()
    );
}
