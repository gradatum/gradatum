//! Isolation cross-vault de la cascade `delete_note_from_index` (C4-1e, Slice D1 / D1.2).
//!
//! Avant le durcissement, la cascade des tables filles supprimait par `note_id` seul :
//! `DELETE FROM {enfant} WHERE {col} = ?1`. En régime multi-vault (deux notes de MÊME
//! ULID, un vault chacune), supprimer la note d'un vault effaçait aussi les lignes filles
//! homonymes de l'AUTRE vault — un clobber cross-vault. Ces deux tests verrouillent la
//! cascade scopée `(vault_id, note_id)` pour les 3 tables filles porteuses de `vault_id`
//! (`note_index`, `note_overrides`, `note_links`) + le statement séparé `temporal_index`.
//!
//! Contrainte de schéma (HEAD, migration 0032) : `note_index` et `temporal_index` ont
//! `note_id` en PK **seule** — deux vaults ne peuvent PAS y coexister avec le même id.
//! C'est précisément ce qui rendait la cascade id-only dangereuse : la ligne unique
//! appartenant à un vault était effacée par un delete ciblant l'autre. Le test ON les
//! sème donc dans `main` seul et prouve qu'un `delete("vault-b")` ne les touche pas.
//! `note_links` (PK inclut `vault_id`) et `note_overrides` (scope_id distinct par vault)
//! coexistent, eux, dans les deux vaults et prouvent la suppression scopée de `vault-b`.
//!
//! `note_audit_trail` / `note_embeddings` / `note_history` (pas de colonne `vault_id`)
//! restent hors scope D1 (cascade id-only conservée → Slice D2 / migration 0033).
//!
//! Le régime multi-vault est purement local au harnais de test ; aucune configuration
//! serveur LIVE n'est touchée (flag `multi_tenant.enabled` reste OFF).

mod common;

use common::{colliding_note_id, seed_colliding_note, two_vault_index};
use gradatum_index::SqliteIndex;

/// `true` si au moins une ligne fille scopée `(vault_id, note_id)` subsiste dans `table`.
async fn child_exists(idx: &SqliteIndex, table: &str, vault_id: &str, note_id: &str) -> bool {
    idx.count_child_rows_for_test(table, vault_id, note_id)
        .await
        .expect("count_child_rows_for_test (table de la liste blanche)")
        > 0
}

/// Régime multi-vault : `delete_note_from_index("vault-b", id)` ne doit toucher AUCUNE
/// ligne fille de `main` à même ULID, et doit supprimer les enfants de `vault-b`.
///
/// RED avant le fix : la cascade id-only supprime les enfants de `main` (note_index,
/// temporal_index, note_links, note_overrides) en plus de ceux de `vault-b`.
#[tokio::test]
async fn cascade_delete_preserves_other_vault_children() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01AAA").to_string();

    // `main` : note parente + les 4 enfants scopés (note_index/temporal via PK note_id seul).
    seed_colliding_note(&idx, "main", "01AAA", "corps-main").await;
    for table in [
        "note_index",
        "temporal_index",
        "note_links",
        "note_overrides",
    ] {
        idx.seed_child_row_for_test(table, "main", &nid)
            .await
            .expect("seed enfant main");
    }

    // `vault-b` : note parente + enfants à PK vault-inclusive uniquement (note_index /
    // temporal_index sont note_id-PK → une 2e ligne même-id collisionnerait avec `main`).
    seed_colliding_note(&idx, "vault-b", "01AAA", "corps-b").await;
    for table in ["note_links", "note_overrides"] {
        idx.seed_child_row_for_test(table, "vault-b", &nid)
            .await
            .expect("seed enfant vault-b");
    }

    idx.delete_note_from_index("vault-b", &nid)
        .await
        .expect("delete_note_from_index vault-b");

    // Enfants de `main` : TOUS subsistent (isolation cross-vault).
    for table in [
        "note_index",
        "temporal_index",
        "note_links",
        "note_overrides",
    ] {
        assert!(
            child_exists(&idx, table, "main", &nid).await,
            "l'enfant `{table}` de `main` ne doit PAS être supprimé par un delete ciblant `vault-b`"
        );
    }

    // Enfants de `vault-b` : supprimés par la cascade scopée.
    for table in ["note_links", "note_overrides"] {
        assert!(
            !child_exists(&idx, table, "vault-b", &nid).await,
            "l'enfant `{table}` de `vault-b` doit être supprimé par le delete ciblé"
        );
    }
}

/// Régime mono-vault (byte-identical flag OFF) : `delete_note_from_index` supprime la
/// note ET tous ses enfants scopés — 0 orphelin, comportement inchangé.
///
/// En mono-vault, `vault_id` de chaque ligne fille == vault ciblé : le prédicat
/// `AND vault_id = ?` sélectionne exactement les mêmes lignes que la cascade id-only.
#[tokio::test]
async fn cascade_delete_single_vault_complete() {
    let idx = two_vault_index().await; // un seul vault "main" est peuplé ci-dessous
    let nid = colliding_note_id("01OFF").to_string();

    seed_colliding_note(&idx, "main", "01OFF", "corps-off").await;
    for table in [
        "note_index",
        "temporal_index",
        "note_links",
        "note_overrides",
    ] {
        idx.seed_child_row_for_test(table, "main", &nid)
            .await
            .expect("seed enfant mono-vault");
    }

    let deleted = idx
        .delete_note_from_index("main", &nid)
        .await
        .expect("delete_note_from_index main");
    assert!(deleted, "la note existante doit être supprimée (Ok(true))");

    // 0 orphelin sur les 4 tables filles scopées.
    for table in [
        "note_index",
        "temporal_index",
        "note_links",
        "note_overrides",
    ] {
        assert!(
            !child_exists(&idx, table, "main", &nid).await,
            "orphelin détecté dans `{table}` après delete mono-vault"
        );
    }
}
