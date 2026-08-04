//! Tests d'atomicité et de contrat pour `persist_curated_index_atomic`.
//!
//! ## Contrat
//!
//! - Le lot (titre + temporal + links + trust) s'exécute dans une transaction unique :
//!   sur erreur SQL réelle, tout est rollback.
//! - C4-1d (option C) : la FK `note_links.src_note_id REFERENCES notes(id)` a été RETIRÉE
//!   (migration 0032, incompatible avec la PK composite `(vault_id, id)`). Un lien orphelin
//!   n'est donc plus rejeté par une FK — il est inséré, et le persist réussit. La cascade et
//!   l'intégrité référentielle des enfants passent en gestion manuelle (cf. `delete_note_from_index`).
//!
//! Les tests utilisent `SqliteIndex::open_in_memory()` directement (pas via HTTP).
//! `persist_curated_index_atomic` est accessible via le trait `IndexStore` (impl `SqliteIndex`).

mod common;
use common::make_note;

use gradatum_core::IndexStore;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// C4-1d (option C) : la FK `note_links.src_note_id REFERENCES notes(id)` a été RETIRÉE
/// par la migration 0032 (incompatible avec la PK composite `(vault_id, id)`). Un lien vers
/// un `src` inexistant n'est donc plus rejeté → il est inséré (lien orphelin) et
/// `persist_curated_index_atomic` RÉUSSIT (plus de rollback sur ce chemin).
///
/// Documente la perte d'intégrité FK-enforced actée par l'option C (cascade → manuelle) :
/// l'atomicité sur erreur SQL réelle reste (transaction), mais n'est plus déclenchable via un
/// lien orphelin. L'isolation référentielle par-vault des enfants est le follow-up option A.
#[tokio::test]
async fn persist_curated_atomic_orphan_link_no_longer_rolls_back() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");

    // Seed note A (titre NULL après upsert_note).
    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "corps test");
    idx.upsert_note(&note_a)
        .await
        .expect("seed note A — invariant test");

    let note_a_id_str = note_a.id.to_string();

    // Titre initial NULL.
    let titles_before = idx
        .get_titles_sections("main", std::slice::from_ref(&note_a_id_str))
        .await
        .expect("get_titles_sections avant — invariant test");
    assert!(
        titles_before
            .get(&note_a_id_str)
            .and_then(|(title, _s)| title.as_deref())
            .is_none(),
        "titre initial doit être NULL"
    );

    // `src` inexistant : ex-violation FK, désormais lien orphelin inséré (option C).
    let nonexistent_src = "01JZZZZZZZZZZZZZZZZZZZZZZZ".to_string();
    let dst = note_a_id_str.clone();

    let result = idx
        .persist_curated_index_atomic(
            &note_a.id,
            "Titre orphelin",
            None,
            &[(nonexistent_src, dst.clone())],
            None,
            "main",
        )
        .await;

    // Sans FK note_links, l'appel RÉUSSIT (le lien orphelin n'échoue plus).
    assert!(
        result.is_ok(),
        "sans FK note_links (option C), un lien orphelin ne fait plus échouer le persist — got {result:?}"
    );

    // Le titre EST persisté (pas de rollback).
    let titles_after = idx
        .get_titles_sections("main", std::slice::from_ref(&note_a_id_str))
        .await
        .expect("get_titles_sections après — invariant test");
    assert_eq!(
        titles_after
            .get(&note_a_id_str)
            .and_then(|(title, _s)| title.as_deref()),
        Some("Titre orphelin"),
        "le titre doit être persisté (aucun rollback puisque plus de FK)"
    );

    // Le lien orphelin est présent (documente la perte d'intégrité FK-enforced, option C).
    let backlinks = idx
        .backlinks("main", &dst)
        .await
        .expect("backlinks — invariant test");
    assert_eq!(
        backlinks.len(),
        1,
        "le lien orphelin est inséré (FK retirée, intégrité → cascade manuelle)"
    );
}

/// Vérifie que si TOUTES les mutations réussissent, l'état est correctement persisté.
///
/// ## Séquence
///
/// 1. Seed notes A et B.
/// 2. Appelle `persist_curated_index_atomic` avec titre, link A→B, trust.
/// 3. Vérifie titre = attendu + link A→B existant.
#[tokio::test]
async fn persist_curated_atomic_success_persists_all() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");

    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "note source");
    let note_b = make_note("main", Section::Decisions, NoteStatus::Live, "note dest");

    idx.upsert_note(&note_a).await.expect("seed note A");
    idx.upsert_note(&note_b).await.expect("seed note B");

    let src = note_a.id.to_string();
    let dst = note_b.id.to_string();

    let result = idx
        .persist_curated_index_atomic(
            &note_a.id,
            "Titre final",
            None, // temporal
            &[(src.clone(), dst.clone())],
            Some(0.85_f32),
            "main",
        )
        .await;

    assert!(
        result.is_ok(),
        "persist_curated_index_atomic doit réussir quand toutes les notes existent — got {:?}",
        result
    );

    // Titre persisté.
    let titles = idx
        .get_titles_sections("main", std::slice::from_ref(&src))
        .await
        .expect("get_titles_sections success — invariant test");
    let title = titles.get(&src).and_then(|(t, _)| t.as_deref());
    assert_eq!(
        title,
        Some("Titre final"),
        "titre doit être persisté après succès"
    );

    // Lien A→B persisté.
    let backlinks = idx
        .backlinks("main", &dst)
        .await
        .expect("backlinks après succès — invariant test");
    assert!(
        backlinks.contains(&src),
        "lien A→B doit exister après succès"
    );
}
