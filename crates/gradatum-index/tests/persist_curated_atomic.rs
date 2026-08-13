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
use common::{colliding_note_id, make_note, make_note_with_id};

use gradatum_core::IndexStore;
use gradatum_core::index_store::CuratedLinks;
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
            CuratedLinks {
                edges: &[(nonexistent_src, dst.clone())],
                authoritative: false, // comportement historique : upsert seul
            },
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
            CuratedLinks {
                edges: &[(src.clone(), dst.clone())],
                authoritative: false,
            },
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

// ── F-147 : autorité des liens (remplacement vs accumulation) ─────────────────

/// F-147 (cas 1) : une note dont un lien change de cible.
/// Après la 2e persistance AUTORITAIRE, l'ancienne arête n'existe plus, la nouvelle existe.
/// C'est le cœur du correctif : sans autorité, l'ancienne arête restait pour toujours.
#[tokio::test]
async fn authoritative_persist_removes_stale_edge_and_keeps_new() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");
    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "corps A");
    idx.upsert_note(&note_a).await.expect("seed A");
    let src = note_a.id.to_string();

    // 1er corps : A → dst-old (autoritatif).
    idx.persist_curated_index_atomic(
        &note_a.id,
        "T1",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-old".to_string())],
            authoritative: true,
        },
        None,
        "main",
    )
    .await
    .expect("persist 1 (autoritatif)");
    assert!(
        idx.backlinks("main", "dst-old")
            .await
            .expect("backlinks dst-old")
            .contains(&src),
        "arête A→dst-old présente après le 1er persist"
    );

    // 2e corps : le lien change de cible → A → dst-new (autoritatif).
    idx.persist_curated_index_atomic(
        &note_a.id,
        "T1",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-new".to_string())],
            authoritative: true,
        },
        None,
        "main",
    )
    .await
    .expect("persist 2 (autoritatif)");

    assert!(
        idx.backlinks("main", "dst-old")
            .await
            .expect("backlinks dst-old après")
            .is_empty(),
        "F-147 : l'arête périmée A→dst-old doit être supprimée (autorité)"
    );
    assert!(
        idx.backlinks("main", "dst-new")
            .await
            .expect("backlinks dst-new")
            .contains(&src),
        "la nouvelle arête A→dst-new doit exister"
    );
}

/// F-147 (cas 2) : une note dont les liens ne changent pas.
/// Deux persistances autoritaires identiques → exactement une arête, sans effet de bord.
#[tokio::test]
async fn authoritative_persist_is_idempotent_on_unchanged_links() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");
    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "corps A");
    idx.upsert_note(&note_a).await.expect("seed A");
    let src = note_a.id.to_string();
    let links = [(src.clone(), "dst-b".to_string())];

    for pass in 1..=2 {
        idx.persist_curated_index_atomic(
            &note_a.id,
            "T",
            None,
            CuratedLinks {
                edges: &links,
                authoritative: true,
            },
            None,
            "main",
        )
        .await
        .unwrap_or_else(|e| panic!("persist pass {pass}: {e:?}"));
    }

    let back = idx
        .backlinks("main", "dst-b")
        .await
        .expect("backlinks dst-b");
    assert_eq!(
        back.len(),
        1,
        "liens inchangés : exactement une arête, aucun doublon ni perte — got {back:?}"
    );
    assert!(back.contains(&src), "l'arête A→dst-b doit être intacte");
}

/// F-147 (cas 3) : isolation par vault. La suppression autoritaire des arêtes de la note
/// dans `main` ne touche PAS les arêtes d'une note HOMONYME (même ULID) dans un autre vault.
#[tokio::test]
async fn authoritative_delete_is_scoped_by_vault() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");

    // Même ULID dans deux vaults distincts (collision volontaire).
    let note_main = make_note_with_id(
        "main",
        colliding_note_id("shared"),
        Section::Decisions,
        NoteStatus::Live,
        "corps main",
    );
    let note_b = make_note_with_id(
        "vault-b",
        colliding_note_id("shared"),
        Section::Decisions,
        NoteStatus::Live,
        "corps vault-b",
    );
    idx.upsert_note(&note_main).await.expect("seed main");
    idx.upsert_note(&note_b).await.expect("seed vault-b");
    let src = note_main.id.to_string();

    // Une arête vers dst-x dans CHAQUE vault, autoritaire.
    idx.persist_curated_index_atomic(
        &note_main.id,
        "T",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-x".to_string())],
            authoritative: true,
        },
        None,
        "main",
    )
    .await
    .expect("persist main");
    idx.persist_curated_index_atomic(
        &note_b.id,
        "T",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-x".to_string())],
            authoritative: true,
        },
        None,
        "vault-b",
    )
    .await
    .expect("persist vault-b");
    assert!(
        idx.backlinks("vault-b", "dst-x")
            .await
            .expect("backlinks vault-b avant")
            .contains(&src),
        "arête présente dans vault-b avant l'opération sur main"
    );

    // Persist autoritaire dans `main` avec liste VIDE → efface les arêtes de main SEULEMENT.
    idx.persist_curated_index_atomic(
        &note_main.id,
        "T",
        None,
        CuratedLinks {
            edges: &[],
            authoritative: true,
        },
        None,
        "main",
    )
    .await
    .expect("persist main vide autoritaire");

    assert!(
        idx.backlinks("main", "dst-x")
            .await
            .expect("backlinks main après")
            .is_empty(),
        "l'arête de main doit être supprimée"
    );
    assert!(
        idx.backlinks("vault-b", "dst-x")
            .await
            .expect("backlinks vault-b après")
            .contains(&src),
        "isolation : l'arête homonyme de vault-b doit rester intacte"
    );
}

/// F-147 (cas 4) : un appel SANS autorité ne supprime rien.
/// C'est la garde qui rend le correctif incapable d'effacer une arête valide par défaut :
/// un chemin qui ne recalcule pas les liens (classify/downgrade) ne peut pas nettoyer.
#[tokio::test]
async fn non_authoritative_persist_never_deletes_edges() {
    let idx = SqliteIndex::open_in_memory()
        .await
        .expect("open_in_memory — invariant test");
    let note_a = make_note("main", Section::Decisions, NoteStatus::Live, "corps A");
    idx.upsert_note(&note_a).await.expect("seed A");
    let src = note_a.id.to_string();

    // Arête initiale posée en mode autoritaire.
    idx.persist_curated_index_atomic(
        &note_a.id,
        "T",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-old".to_string())],
            authoritative: true,
        },
        None,
        "main",
    )
    .await
    .expect("persist autoritaire initial");

    // Persist NON-autoritaire avec une autre cible : comportement historique (upsert seul),
    // l'ancienne arête NE doit PAS disparaître.
    idx.persist_curated_index_atomic(
        &note_a.id,
        "T",
        None,
        CuratedLinks {
            edges: &[(src.clone(), "dst-new".to_string())],
            authoritative: false,
        },
        None,
        "main",
    )
    .await
    .expect("persist non-autoritaire");

    assert!(
        idx.backlinks("main", "dst-old")
            .await
            .expect("backlinks dst-old")
            .contains(&src),
        "sans autorité, l'arête A→dst-old NE doit PAS être supprimée"
    );
    assert!(
        idx.backlinks("main", "dst-new")
            .await
            .expect("backlinks dst-new")
            .contains(&src),
        "l'arête A→dst-new est ajoutée (upsert)"
    );
}
