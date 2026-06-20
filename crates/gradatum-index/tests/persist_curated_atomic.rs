//! Tests d'atomicité pour `persist_curated_index_atomic`.
//!
//! ## Contrat testé
//!
//! - Si l'une des mutations index échoue (ex: violation FK `note_links`),
//!   TOUTES les mutations du lot sont rollback.
//! - En particulier : si `upsert_note_title` réussit mais `upsert_link` échoue
//!   (FK sur `src_note_id`), le titre NE DOIT PAS être persisté.
//!
//! ## Stratégie d'injection de défaillance
//!
//! `note_links` a une FK : `FOREIGN KEY (src_note_id) REFERENCES notes(id)`.
//! Un `src_note_id` inexistant → `SQLITE_CONSTRAINT_FOREIGNKEY` → rollback.
//!
//! Le test utilise `SqliteIndex::open_in_memory()` directement (pas via HTTP).
//! La méthode `persist_curated_index_atomic` est accessible depuis le trait
//! `IndexStore` (impl concrète sur `SqliteIndex`).

mod common;
use common::make_note;

use gradatum_core::IndexStore;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Vérifie que le titre reste NULL si `upsert_link` échoue (FK violation).
///
/// ## Séquence
///
/// 1. Seed note A dans l'index (titre initial NULL).
/// 2. Appelle `persist_curated_index_atomic` avec :
///    - titre = "Titre à rollback"
///    - links = [("ULID_INEXISTANT", note_a.id)] → viole FK `src_note_id`
///    - temporal = None, trust = None
/// 3. Vérifie que `persist_curated_index_atomic` retourne `Err(...)`.
/// 4. Vérifie que le titre de note A est TOUJOURS NULL (rollback effectif).
#[tokio::test]
async fn persist_curated_atomic_rollback_on_fk_violation() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");

    // Seed note A (titre NULL après upsert_note).
    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "corps test");
    idx.upsert_note(&note_a)
        .await
        .expect("seed note A — invariant test");

    let note_a_id_str = note_a.id.to_string();

    // Vérifier que le titre initial est bien NULL.
    let titles_before = idx
        .get_titles_sections("main", std::slice::from_ref(&note_a_id_str))
        .await
        .expect("get_titles_sections avant — invariant test");
    let title_before = titles_before
        .get(&note_a_id_str)
        .and_then(|(title, _section)| title.as_deref());
    assert!(
        title_before.is_none(),
        "titre initial doit être NULL avant persist_curated_index_atomic"
    );

    // ULID inexistant → violation FK `src_note_id REFERENCES notes(id)`.
    let nonexistent_src = "01JZZZZZZZZZZZZZZZZZZZZZZZ".to_string();
    let dst = note_a_id_str.clone();

    let result = idx
        .persist_curated_index_atomic(
            &note_a.id,
            "Titre à rollback",
            None, // temporal
            &[(nonexistent_src, dst.clone())],
            None, // trust
            "main",
        )
        .await;

    // L'appel doit échouer (FK violation).
    assert!(
        result.is_err(),
        "persist_curated_index_atomic doit retourner Err quand FK violée — got Ok"
    );

    // Le titre DOIT être NULL (rollback de upsert_note_title).
    let titles_after = idx
        .get_titles_sections("main", std::slice::from_ref(&note_a_id_str))
        .await
        .expect("get_titles_sections après — invariant test");
    let title_after = titles_after
        .get(&note_a_id_str)
        .and_then(|(title, _section)| title.as_deref());
    assert!(
        title_after.is_none(),
        "le titre doit être NULL après rollback (atomicité violée si Some)"
    );

    // Aucun lien ne doit exister.
    let backlinks = idx
        .backlinks("main", &dst)
        .await
        .expect("backlinks — invariant test");
    assert!(
        backlinks.is_empty(),
        "aucun backlink ne doit exister après rollback"
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
