//! Isolation cross-vault de `redirect_table` recomposée en PK composite
//! `(vault_id, title_slug)` par la migration 0035 (Groupe B, M4).
//!
//! `redirect_table` (migration 0010) était la SEULE table filles restée à PK globale
//! (`title_slug` seul). Deux vaults distincts renommant une note vers le même titre
//! produisent le même `title_slug` : avec une PK globale, l'`INSERT OR REPLACE` du
//! second vault CLOBBE la ligne du premier (même clé), et le read-path (`WHERE
//! title_slug = ?`) résout le slug d'un vault vers l'ULID de l'autre — fuite
//! d'isolation. 0035 recompose la PK en `(vault_id, title_slug)` et scope les
//! write/read/delete-paths par `vault_id`, fermant la classe.
//!
//! Le régime multi-vault est purement local au harnais (flag `multi_tenant.enabled`
//! reste OFF) : le jeu OFF (mono-vault) prouve que le round-trip reste byte-identical.
//!
//! Assertions via l'API publique réelle (`upsert_redirect` / `lookup_redirect` /
//! `delete_redirect_by_ulid`) — c'est le comportement observable du read/write/delete
//! path qui est verrouillé, pas une inspection SQL brute.

mod common;

use common::{VAULT_B, VAULT_MAIN, two_vault_index};
use ulid::Ulid;

/// ON (multi-vault local) : un même `title_slug` enregistré dans `main` puis `vault-b`
/// résout, dans CHAQUE vault, vers SON propre ULID — jamais celui de l'autre.
///
/// RED avant 0035 (PK globale `title_slug` + `INSERT OR REPLACE` non scopé) :
/// l'upsert de `vault-b` écrase la ligne de `main` → `lookup_redirect("main", slug)`
/// renvoie l'ULID de `vault-b`. L'assertion échoue (clobber cross-vault prouvé).
#[tokio::test]
async fn redirect_slug_collision_is_isolated_per_vault() {
    let idx = two_vault_index().await;
    let slug = "titre-collisionne";
    let ulid_a = Ulid::generate();
    let ulid_b = Ulid::generate();

    idx.upsert_redirect(VAULT_MAIN, slug, &ulid_a, 1_000)
        .await
        .expect("upsert redirect main");
    idx.upsert_redirect(VAULT_B, slug, &ulid_b, 2_000)
        .await
        .expect("upsert redirect vault-b (PK composite → pas de clobber)");

    // Chaque vault résout vers SON ULID (deux lignes distinctes coexistent).
    let resolved_main = idx
        .lookup_redirect(VAULT_MAIN, slug)
        .await
        .expect("lookup redirect main");
    let resolved_b = idx
        .lookup_redirect(VAULT_B, slug)
        .await
        .expect("lookup redirect vault-b");

    assert_eq!(
        resolved_main,
        Some(ulid_a),
        "`main` doit résoudre vers SON ULID — jamais celui de `vault-b`"
    );
    assert_eq!(
        resolved_b,
        Some(ulid_b),
        "`vault-b` doit résoudre vers SON ULID — jamais celui de `main`"
    );
}

/// ON (multi-vault local) : purger le redirect d'un vault (`delete_redirect_by_ulid`)
/// ne touche PAS le redirect homonyme (même ULID) d'un autre vault.
///
/// RED avant 0035 (`DELETE ... WHERE ulid = ?` non scopé) : la purge de `vault-b`
/// supprimait aussi la ligne de `main` partageant l'ULID collisionné.
#[tokio::test]
async fn delete_redirect_by_ulid_is_scoped_per_vault() {
    let idx = two_vault_index().await;
    let slug = "titre-a-purger";
    // ULID collisionné volontairement entre les deux vaults.
    let ulid = Ulid::generate();

    idx.upsert_redirect(VAULT_MAIN, slug, &ulid, 1_000)
        .await
        .expect("upsert redirect main");
    idx.upsert_redirect(VAULT_B, slug, &ulid, 2_000)
        .await
        .expect("upsert redirect vault-b");

    // Purge du redirect de `vault-b` uniquement.
    let deleted = idx
        .delete_redirect_by_ulid(VAULT_B, &ulid.to_string())
        .await
        .expect("delete redirect vault-b");
    assert_eq!(
        deleted, 1,
        "exactement 1 ligne (vault-b) supprimée — pas la ligne homonyme de `main`"
    );

    // La ligne de `main` subsiste et résout toujours ; celle de `vault-b` est partie.
    assert_eq!(
        idx.lookup_redirect(VAULT_MAIN, slug)
            .await
            .expect("lookup redirect main post-purge"),
        Some(ulid),
        "`main` doit toujours résoudre après la purge de `vault-b`"
    );
    assert_eq!(
        idx.lookup_redirect(VAULT_B, slug)
            .await
            .expect("lookup redirect vault-b post-purge"),
        None,
        "le redirect de `vault-b` doit avoir été purgé"
    );
}

/// OFF (mono-vault, byte-identical) : deux upserts successifs sur le même
/// `(main, slug)` restent un `REPLACE` en place — dernier ULID gagnant, une seule
/// ligne. Comportement mono-vault inchangé par le recompose PK (miroir du test
/// unitaire `upsert_redirect_idempotent_last_wins`).
#[tokio::test]
async fn redirect_upsert_off_replace_in_place() {
    let idx = two_vault_index().await;
    let slug = "titre-mono-vault";
    let ulid1 = Ulid::generate();
    let ulid2 = Ulid::generate();

    idx.upsert_redirect(VAULT_MAIN, slug, &ulid1, 1_000)
        .await
        .expect("upsert v1");
    idx.upsert_redirect(VAULT_MAIN, slug, &ulid2, 2_000)
        .await
        .expect("upsert v2 (REPLACE en place)");

    // Dernier ULID gagnant.
    assert_eq!(
        idx.lookup_redirect(VAULT_MAIN, slug)
            .await
            .expect("lookup redirect"),
        Some(ulid2),
        "le second upsert remplace le premier (last wins)"
    );

    // Une seule ligne : la purge par ULID gagnant en supprime exactement 1
    // (un REPLACE en place n'a pas dupliqué la ligne).
    let deleted = idx
        .delete_redirect_by_ulid(VAULT_MAIN, &ulid2.to_string())
        .await
        .expect("delete redirect main");
    assert_eq!(
        deleted, 1,
        "le REPLACE mono-vault ne doit pas avoir dupliqué la ligne"
    );
}
