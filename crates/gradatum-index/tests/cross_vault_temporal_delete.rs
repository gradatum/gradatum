//! Isolation cross-vault de `delete_temporal_entry`.
//!
//! Depuis la migration 0034, `temporal_index` porte une PRIMARY KEY composite
//! `(vault_id, note_id)`. `delete_temporal_entry` DOIT donc scoper son `DELETE` par
//! `vault_id` : sinon une collision d'ULID entre deux vaults fait qu'une suppression
//! ciblant `vault-b` détruit AUSSI l'entrée temporelle homonyme de `main`
//! (tampering / DoS cross-vault — TROU RÉEL, iso-audit).
//!
//! Le régime multi-vault est purement local au harnais (flag `multi_tenant.enabled`
//! reste OFF) : le round-trip mono-vault demeure byte-identical.

mod common;

use common::{colliding_note_id, two_vault_index};
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_index::SqliteIndex;

/// Construit une `TemporalEntry` minimale pour un `(vault_id, note_id, anchor_ms)`.
fn temporal_entry(note_id: &str, vault_id: &str, anchor_ms: i64) -> TemporalEntry {
    TemporalEntry {
        note_id: note_id.to_string(),
        vault_id: vault_id.to_string(),
        anchor_ms,
        anchor_src: AnchorSrc::Created,
        doc_kind: "Static".to_string(),
        valid_until_ms: None,
    }
}

/// Compte les entrées `temporal_index` scopées `(vault_id, note_id)` via le helper de test.
async fn temporal_count(idx: &SqliteIndex, vault_id: &str, note_id: &str) -> u64 {
    idx.count_child_rows_for_test("temporal_index", vault_id, note_id)
        .await
        .expect("count_child_rows_for_test temporal_index")
}

/// `delete_temporal_entry(vault-b, X)` ne supprime QUE l'entrée de `vault-b` : l'entrée
/// homonyme (même ULID) de `main` reste intacte.
///
/// RED avant scoping : `DELETE ... WHERE note_id = ?1` id-only détruit les DEUX lignes
/// (collision d'ULID) → l'entrée de `main` disparaît (count 0), assertion d'isolation FAIL.
#[tokio::test]
async fn delete_temporal_entry_scoped_does_not_clobber_other_vault() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3TEMPDEL").to_string();

    idx.write_temporal_entry(&temporal_entry(&nid, "main", 1000))
        .await
        .expect("write temporal main");
    idx.write_temporal_entry(&temporal_entry(&nid, "vault-b", 2000))
        .await
        .expect("write temporal vault-b");

    // Précondition : chaque vault porte exactement 1 entrée temporelle.
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "précondition : `main` doit porter 1 entrée avant suppression"
    );
    assert_eq!(
        temporal_count(&idx, "vault-b", &nid).await,
        1,
        "précondition : `vault-b` doit porter 1 entrée avant suppression"
    );

    // Suppression ciblée sur `vault-b`.
    let deleted = idx
        .delete_temporal_entry("vault-b", &nid)
        .await
        .expect("delete temporal vault-b");
    assert!(
        deleted,
        "la suppression scopée `vault-b` doit rapporter une ligne effacée"
    );

    // Isolation : `vault-b` vidé, `main` INTACT.
    assert_eq!(
        temporal_count(&idx, "vault-b", &nid).await,
        0,
        "l'entrée temporelle de `vault-b` doit être supprimée"
    );
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "l'entrée temporelle de `main` ne doit PAS être détruite par la suppression scopée `vault-b`"
    );
}

/// Idempotence préservée après scoping : supprimer une entrée absente d'un vault donné
/// renvoie `Ok(false)` sans toucher l'entrée homonyme de l'autre vault.
#[tokio::test]
async fn delete_temporal_entry_absent_in_vault_is_noop() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3TEMPABS").to_string();

    idx.write_temporal_entry(&temporal_entry(&nid, "main", 1000))
        .await
        .expect("write temporal main");

    // `vault-b` n'a aucune entrée à cet ULID : suppression = no-op scopé.
    let deleted = idx
        .delete_temporal_entry("vault-b", &nid)
        .await
        .expect("delete temporal vault-b (absent)");
    assert!(!deleted, "aucune ligne pour `vault-b` → Ok(false)");
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "l'entrée de `main` ne doit pas être affectée par un delete ciblant `vault-b`"
    );
}
