//! Isolation cross-vault de `get_content_hash` (C4-1e, Slice C / C2).
//!
//! `get_content_hash` est le point de lecture du `ContentHash` d'une note utilisé
//! par le validator de fraîcheur du cache moka (couche Vault). Avant le durcissement,
//! sa clause `WHERE id = ?` n'était pas scopée par `vault_id` : avec la clé primaire
//! composite `(vault_id, id)` (migration 0032), deux notes homonymes (même ULID, deux
//! vaults) satisfont la clause id-only et `query_row` renvoie une ligne arbitraire —
//! potentiellement le hash de l'AUTRE vault. Ces deux tests verrouillent la lecture
//! scopée :
//!
//! - `get_content_hash_returns_target_vault_hash_not_main` : régime multi-vault
//!   (isolation — chaque vault renvoie son propre hash) ;
//! - `get_content_hash_off_single_vault_unchanged` : régime mono-vault (comportement
//!   inchangé, hash relu identique + absente → `None`).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur n'est touchée.

mod common;

use common::{VAULT_B, VAULT_MAIN, colliding_note_id, make_note_with_id, two_vault_index};

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

/// `get_content_hash(vault, id)` renvoie le hash de la note DU vault ciblé, jamais
/// celui de la note homonyme d'un autre vault.
///
/// Deux notes de MÊME ULID mais de contenu (et donc de `ContentHash`) distinct sont
/// semées dans deux vaults ; le `vault_id` du frontmatter entre dans le hash (JCS),
/// donc `hash(main) != hash(vault-b)` même à corps identique.
#[tokio::test]
async fn get_content_hash_returns_target_vault_hash_not_main() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("collision-c2");

    let note_main = make_note_with_id(
        VAULT_MAIN,
        nid,
        Section::Reference,
        NoteStatus::Live,
        "# Main\n\ncorps-main",
    );
    let note_b = make_note_with_id(
        VAULT_B,
        nid,
        Section::Reference,
        NoteStatus::Live,
        "# VaultB\n\ncorps-b",
    );
    idx.upsert_note(&note_main).await.expect("upsert main");
    idx.upsert_note(&note_b).await.expect("upsert vault-b");

    assert_ne!(
        note_main.content_hash, note_b.content_hash,
        "les deux notes homonymes doivent avoir des hashes distincts (vault_id ∈ hash)"
    );

    let h_b = idx
        .get_content_hash(VAULT_B, nid)
        .await
        .expect("get_content_hash vault-b");
    assert_eq!(
        h_b,
        Some(note_b.content_hash),
        "vault-b doit renvoyer SON propre hash, pas celui de main"
    );

    let h_main = idx
        .get_content_hash(VAULT_MAIN, nid)
        .await
        .expect("get_content_hash main");
    assert_eq!(
        h_main,
        Some(note_main.content_hash),
        "main doit renvoyer son propre hash"
    );

    assert_ne!(
        h_b, h_main,
        "malgré l'ULID commun, les deux vaults ne partagent pas le ContentHash"
    );
}

/// Régime mono-vault : comportement inchangé (byte-identical flag OFF).
///
/// Un seul vault peuplé : le hash relu est identique à celui de la note insérée, et
/// une note absente renvoie `None` — exactement l'ancien contrat id-only.
#[tokio::test]
async fn get_content_hash_off_single_vault_unchanged() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("mono-c2");

    let note = make_note_with_id(
        VAULT_MAIN,
        nid,
        Section::Reference,
        NoteStatus::Live,
        "# Mono\n\ncorps-mono",
    );
    idx.upsert_note(&note).await.expect("upsert mono");

    let h = idx
        .get_content_hash(VAULT_MAIN, nid)
        .await
        .expect("get_content_hash mono");
    assert_eq!(
        h,
        Some(note.content_hash),
        "hash relu identique au hash inséré (mono-vault inchangé)"
    );

    let absent = colliding_note_id("absent-c2");
    let none = idx
        .get_content_hash(VAULT_MAIN, absent)
        .await
        .expect("get_content_hash absente");
    assert!(none.is_none(), "note absente doit renvoyer None");
}
