//! Isolation cross-vault de `set_note_trust` (C4-1e, Slice A / A3).
//!
//! `set_note_trust` est le seul point d'écriture de `notes.trust` en dehors de
//! `upsert_note` (F-22, distillation). Avant le durcissement, sa clause
//! `WHERE id = ?` n'était pas scopée par `vault_id` : une écriture ciblant un vault
//! écrasait le trust d'une note homonyme (même ULID) dans un autre vault. Ces deux
//! tests verrouillent le comportement scopé :
//!
//! - `set_trust_does_not_cross_vault` : régime multi-vault (isolation) ;
//! - `set_trust_off_single_vault_unchanged` : régime mono-vault (comportement
//!   inchangé, rows-affected == 1).
//!
//! Lecture du trust par vault : `get_trust_and_provenance` (déjà scopé,
//! `sqlite.rs:3072`) — PAS `get_trust` (non scopé avant Slice B).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};

/// Une écriture de trust ciblant `vault-b` ne doit pas toucher le trust de la note
/// homonyme de `main`.
///
/// Séquence : deux notes de MÊME ULID sont semées dans deux vaults distincts
/// (`upsert_note` pose un trust statique via `TRUST_SCORES`, provenance `None`).
/// `set_note_trust` pose ensuite un trust dynamique distinct par vault ; une
/// modification ciblée sur `vault-b` prouve l'isolation.
#[tokio::test]
async fn set_trust_does_not_cross_vault() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("collision-a3");

    // Deux notes homonymes, une par vault.
    seed_colliding_note(&idx, "main", "collision-a3", "corps-main").await;
    seed_colliding_note(&idx, "vault-b", "collision-a3", "corps-b").await;

    idx.set_note_trust("main", &nid, 0.9)
        .await
        .expect("set_note_trust vault main");
    idx.set_note_trust("vault-b", &nid, 0.2)
        .await
        .expect("set_note_trust vault-b");

    let (main_trust, _) = idx
        .get_trust_and_provenance("main", &nid.to_string())
        .await
        .expect("get_trust_and_provenance main");
    let (b_trust, _) = idx
        .get_trust_and_provenance("vault-b", &nid.to_string())
        .await
        .expect("get_trust_and_provenance vault-b");

    assert!(
        (main_trust.expect("trust main présent") - 0.9).abs() < 1e-6,
        "le trust de `main` ne doit PAS être écrasé par une écriture ciblant `vault-b`"
    );
    assert!(
        (b_trust.expect("trust vault-b présent") - 0.2).abs() < 1e-6,
        "le trust de `vault-b` doit refléter sa propre écriture"
    );
}

/// Régime mono-vault : comportement inchangé (byte-identical flag OFF).
///
/// `rows-affected == 1` (un seul UPDATE sur la note existante) et le trust est bien
/// persisté — identique à l'ancien comportement id-only.
#[tokio::test]
async fn set_trust_off_single_vault_unchanged() {
    let idx = two_vault_index().await; // un seul vault "main" est peuplé ci-dessous
    let nid = colliding_note_id("mono-a3");

    seed_colliding_note(&idx, "main", "mono-a3", "corps-t0").await;

    let rows = idx
        .set_note_trust("main", &nid, 0.7)
        .await
        .expect("set_note_trust mono-vault");
    assert_eq!(
        rows, 1,
        "rows-affected doit valoir 1 (comportement mono-vault inchangé)"
    );

    let (trust, _) = idx
        .get_trust_and_provenance("main", &nid.to_string())
        .await
        .expect("get_trust_and_provenance main");
    assert!(
        (trust.expect("trust présent") - 0.7).abs() < 1e-6,
        "trust relu doit valoir 0.7"
    );
}
