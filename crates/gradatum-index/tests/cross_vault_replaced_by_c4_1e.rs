//! Isolation cross-vault de `get_replaced_by` (C4-1e, Slice A / A4).
//!
//! `get_replaced_by` est une méthode inhérente non-trait, seul point de lecture
//! directe de `notes.replaced_by` (utilisée en tests d'intégration pour vérifier la
//! persistance de `patch_note`/`downgrade_note`). Avant le durcissement, sa clause
//! `WHERE id = ?` n'était pas scopée par `vault_id` : une lecture ciblant un vault
//! pouvait renvoyer le `replaced_by` d'une note homonyme (même ULID) d'un autre vault.
//! Ces deux tests verrouillent le comportement scopé :
//!
//! - `get_replaced_by_does_not_cross_vault` : régime multi-vault (isolation) ;
//! - `get_replaced_by_off_single_vault_unchanged` : régime mono-vault (comportement
//!   inchangé).
//!
//! Écriture de `replaced_by` : `downgrade_note` (déjà scopé `AND vault_id = ?`,
//! C3a EX-C3a P0) — seul chemin d'écriture disponible pour ce champ. Le pré-check
//! d'existence du remplaçant impose de semer une note remplaçante dans chaque vault.
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_core::scope::{AclCheckedVaultId, VaultId};

/// Construit le témoin ACL-Write pour un vault donné (contexte test = système,
/// cf. `checked_main`/`checked_other` internes à `sqlite.rs`).
fn checked(vault: &str) -> AclCheckedVaultId {
    AclCheckedVaultId::for_system_task(VaultId::new(vault))
}

/// Une lecture de `replaced_by` ciblant `vault-b` ne doit pas renvoyer le
/// `replaced_by` de la note homonyme de `main`.
///
/// Séquence : deux notes de MÊME ULID sont semées dans deux vaults distincts, chacune
/// accompagnée d'une note remplaçante propre (pré-check d'existence de
/// `downgrade_note`). `downgrade_note` pose ensuite un `replaced_by` distinct par
/// vault ; une lecture ciblée sur chaque vault prouve l'isolation.
#[tokio::test]
async fn get_replaced_by_does_not_cross_vault() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("collision-a4");

    // Deux notes homonymes, une par vault.
    seed_colliding_note(&idx, "main", "collision-a4", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "collision-a4", "corps-b").await;

    // Une note remplaçante distincte par vault (pré-check d'existence downgrade_note).
    let replacement_main = colliding_note_id("collision-a4-repl-main");
    let replacement_b = colliding_note_id("collision-a4-repl-b");
    seed_colliding_note(&idx, "main", "collision-a4-repl-main", "corps-repl-main").await;
    seed_colliding_note(&idx, "vault-b", "collision-a4-repl-b", "corps-repl-b").await;

    idx.downgrade_note(
        &checked("main"),
        &nid,
        "reason-main",
        Some(&replacement_main),
    )
    .await
    .expect("downgrade_note vault main");
    idx.downgrade_note(&checked("vault-b"), &nid, "reason-b", Some(&replacement_b))
        .await
        .expect("downgrade_note vault-b");

    let main_replaced_by = idx
        .get_replaced_by("main", &nid.to_string())
        .await
        .expect("get_replaced_by main");
    let b_replaced_by = idx
        .get_replaced_by("vault-b", &nid.to_string())
        .await
        .expect("get_replaced_by vault-b");

    assert_eq!(
        main_replaced_by.as_deref(),
        Some(replacement_main.to_string().as_str()),
        "replaced_by de `main` ne doit PAS être écrasé par la lecture ciblant `vault-b`"
    );
    assert_eq!(
        b_replaced_by.as_deref(),
        Some(replacement_b.to_string().as_str()),
        "replaced_by de `vault-b` doit refléter sa propre écriture, distinct de `main`"
    );
    assert_ne!(
        main_replaced_by, b_replaced_by,
        "les deux valeurs doivent être distinctes — sinon le test ne prouve rien"
    );
}

/// Régime mono-vault : comportement inchangé (byte-identical flag OFF).
///
/// La valeur lue via `get_replaced_by("main", id)` doit être exactement celle écrite
/// via `downgrade_note` — identique à l'ancien comportement id-only.
#[tokio::test]
async fn get_replaced_by_off_single_vault_unchanged() {
    let idx = two_vault_index().await; // un seul vault "main" est peuplé ci-dessous
    let nid = colliding_note_id("mono-a4");
    let replacement = colliding_note_id("mono-a4-repl");

    seed_colliding_note(&idx, "main", "mono-a4", "corps-t0").await;
    seed_colliding_note(&idx, "main", "mono-a4-repl", "corps-repl").await;

    idx.downgrade_note(&checked("main"), &nid, "reason", Some(&replacement))
        .await
        .expect("downgrade_note mono-vault");

    let stored = idx
        .get_replaced_by("main", &nid.to_string())
        .await
        .expect("get_replaced_by mono-vault");

    assert_eq!(
        stored.as_deref(),
        Some(replacement.to_string().as_str()),
        "valeur lue doit être exactement celle écrite (comportement mono-vault inchangé)"
    );
}
