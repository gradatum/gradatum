//! Isolation cross-vault de `upsert_note_title` (C4-1e, Slice A / A2).
//!
//! La colonne `notes.title` n'est peuplée que par `upsert_note_title`. Avant le
//! durcissement, sa clause `WHERE id = ?` n'était pas scopée par `vault_id` : une
//! écriture ciblant un vault écrasait le titre d'une note homonyme (même ULID) dans
//! un autre vault. Ces deux tests verrouillent le comportement scopé :
//!
//! - `upsert_title_does_not_cross_vault` : régime multi-vault (isolation) ;
//! - `upsert_title_off_single_vault_unchanged_behavior` : régime mono-vault
//!   (comportement inchangé, rows-affected == 1).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};

/// Une écriture de titre ciblant `vault-b` ne doit pas toucher la note homonyme de `main`.
///
/// Séquence : deux notes de MÊME ULID sont semées dans deux vaults distincts (titre
/// colonne `NULL` après seed, le corps portant le H1). Les titres colonne sont posés
/// EN EXERÇANT la méthode scopée elle-même, puis une modification ciblée sur
/// `vault-b` prouve l'isolation.
#[tokio::test]
async fn upsert_title_does_not_cross_vault() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("collision-a2");
    let id_str = nid.to_string();

    // Deux notes homonymes, une par vault ; colonne `title` NULL à ce stade.
    seed_colliding_note(&idx, "main", "collision-a2", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "collision-a2", "corps-b").await;

    // Pose un titre colonne distinct par vault via la méthode scopée.
    idx.upsert_note_title("main", &nid, "titre-main")
        .await
        .expect("upsert titre vault main");
    idx.upsert_note_title("vault-b", &nid, "titre-b")
        .await
        .expect("upsert titre vault-b");

    // Modification ciblée sur vault-b uniquement.
    idx.upsert_note_title("vault-b", &nid, "titre-b-modifie")
        .await
        .expect("upsert titre vault-b (modif)");

    let main_title = idx
        .get_note("main", &id_str)
        .await
        .expect("get_note main")
        .expect("note main présente")
        .title;
    let b_title = idx
        .get_note("vault-b", &id_str)
        .await
        .expect("get_note vault-b")
        .expect("note vault-b présente")
        .title;

    assert_eq!(
        main_title.as_deref(),
        Some("titre-main"),
        "le titre de `main` ne doit PAS être écrasé par une écriture ciblant `vault-b`"
    );
    assert_eq!(
        b_title.as_deref(),
        Some("titre-b-modifie"),
        "le titre de `vault-b` doit refléter la dernière écriture ciblée"
    );
}

/// Régime mono-vault : comportement inchangé (byte-identical flag OFF).
///
/// `rows-affected == 1` (un seul UPDATE sur la note existante) et le titre est bien
/// persisté — identique à l'ancien comportement id-only.
#[tokio::test]
async fn upsert_title_off_single_vault_unchanged_behavior() {
    let idx = two_vault_index().await; // un seul vault "main" est peuplé ci-dessous
    let nid = colliding_note_id("mono-a2");
    let id_str = nid.to_string();

    seed_colliding_note(&idx, "main", "mono-a2", "corps-t0").await;

    let rows = idx
        .upsert_note_title("main", &nid, "t1")
        .await
        .expect("upsert titre mono-vault");
    assert_eq!(
        rows, 1,
        "rows-affected doit valoir 1 (comportement mono-vault inchangé)"
    );

    let title = idx
        .get_note("main", &id_str)
        .await
        .expect("get_note main")
        .expect("note main présente")
        .title;
    assert_eq!(title.as_deref(), Some("t1"));
}
