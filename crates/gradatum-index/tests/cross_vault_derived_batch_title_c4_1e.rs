//! Isolation cross-vault du title dans `write_note_derived_batch` (C4-1e, Slice A / A5).
//!
//! Miss résiduel C4-1d (M1) : l'UPDATE de titre à la fin de `write_note_derived_batch`
//! (`sqlite.rs:4581`) était id-only (`WHERE id = ?2`) alors que le reste de la fonction
//! est déjà scopé `(id, vault_id)` (INSERT `ON CONFLICT(vault_id, id)`, DELETE, FTS sync).
//! Un batch dérivé sur un vault `code-*` avec un `id` colliding réécrivait le titre
//! d'une note homonyme d'un AUTRE vault (ex. `main`). Ces deux tests verrouillent le
//! comportement scopé :
//!
//! - `derived_batch_title_does_not_hijack_other_vault` : régime multi-vault (isolation) ;
//! - `derived_batch_title_off_single_vault_unchanged_behavior` : régime mono-vault
//!   (comportement inchangé, le titre de la note dérivée est bien posé).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_index::DerivedNote;

/// Construit une `DerivedNote` minimale à ULID imposé (nécessaire pour forcer la
/// collision cross-vault — `make_derived_notes` de `sqlite.rs` dérive l'id depuis
/// `vault_id`+`source_path`, ce qui produirait des ids différents par vault).
fn derived_note_with_id(id: gradatum_core::identity::NoteId, title: &str) -> DerivedNote {
    DerivedNote {
        id,
        body_text: format!("corps dérivé — {title}"),
        tags: "code rust fn test_module".to_string(),
        title: Some(title.to_string()),
        code_meta: None,
    }
}

/// Un batch dérivé ciblant `code-test` ne doit pas écraser le titre de la note
/// homonyme (même ULID) du vault `main`.
///
/// Séquence : une note `main` existe avec un titre colonne posé (`upsert_note_title`,
/// scopé depuis A2). Un batch dérivé de MÊME ULID est écrit sur `code-test`. Le titre
/// de `main` doit rester intact.
#[tokio::test]
async fn derived_batch_title_does_not_hijack_other_vault() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("collision-a5");
    let id_str = nid.to_string();

    // Note `main` existante avec un titre colonne posé.
    seed_colliding_note(&idx, "main", "collision-a5", "corps-main").await;
    idx.upsert_note_title("main", &nid, "titre-main")
        .await
        .expect("upsert titre vault main");

    // Batch dérivé de MÊME ULID sur un vault code-* distinct.
    let derived = vec![derived_note_with_id(nid, "titre-derive")];
    idx.write_note_derived_batch(
        "code-test",
        "src/collision.rs",
        "hashsrc",
        "deadbeef",
        derived,
    )
    .await
    .expect("write_note_derived_batch code-test");

    let main_title = idx
        .get_note("main", &id_str)
        .await
        .expect("get_note main")
        .expect("note main présente")
        .title;

    assert_eq!(
        main_title.as_deref(),
        Some("titre-main"),
        "le titre de `main` ne doit PAS être écrasé par un batch dérivé ciblant `code-test`"
    );
}

/// Régime mono-vault : comportement inchangé (byte-identical flag OFF).
///
/// Le batch dérivé met bien à jour le titre de SA propre note dans `code-test`.
#[tokio::test]
async fn derived_batch_title_off_single_vault_unchanged_behavior() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("mono-a5");
    let id_str = nid.to_string();

    let derived = vec![derived_note_with_id(nid, "titre-derive-mono")];
    idx.write_note_derived_batch("code-test", "src/mono.rs", "hashsrc", "deadbeef", derived)
        .await
        .expect("write_note_derived_batch code-test mono-vault");

    let title = idx
        .get_note("code-test", &id_str)
        .await
        .expect("get_note code-test")
        .expect("note code-test présente")
        .title;

    assert_eq!(title.as_deref(), Some("titre-derive-mono"));
}
