//! Isolation cross-vault des write-paths sur les tables filles recomposées en PK composite
//! par la migration 0034 (C4-1e, Slice D3) : `temporal_index` et `note_overrides`.
//!
//! Depuis 0034, ces tables portent une PRIMARY KEY incluant `vault_id`
//! (`temporal_index` → `(vault_id, note_id)`, `note_overrides` →
//! `(vault_id, note_id, scope_kind, scope_id, override_type)`). Deux vaults distincts peuvent
//! donc porter des lignes filles de MÊME `note_id` sans collision d'écriture :
//!   * `write_temporal_entry` (`INSERT OR REPLACE`) keye désormais sur la PK composite → un
//!     write de `vault-b` ne remplace pas l'entrée temporelle de `main` au même ULID.
//!   * `upsert_override_raw` (`ON CONFLICT`) cible la clé composite (couplage obligatoire de la
//!     migration : l'ancienne cible `ON CONFLICT(note_id, scope_kind, scope_id, override_type)`
//!     ne matcherait plus aucune contrainte UNIQUE après le recompose PK).
//!
//! Le régime multi-vault est purement local au harnais (flag `multi_tenant.enabled` reste OFF) :
//! les jeux OFF (mono-vault) prouvent que le round-trip reste byte-identical.

mod common;

use common::{colliding_note_id, two_vault_index};
use gradatum_core::index::{AnchorSrc, TemporalEntry};
use gradatum_core::scope::{OverrideScope, VaultId};
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

/// ON (multi-vault local) : `write_temporal_entry` sur `vault-b` ne remplace PAS l'entrée
/// temporelle de `main` partageant le même `note_id`.
///
/// RED avant 0034 : PK `temporal_index(note_id)` seule → l'`INSERT OR REPLACE` de `vault-b`
/// écrasait l'unique ligne, faisant basculer son `vault_id` à `vault-b` (main perdu).
#[tokio::test]
async fn write_temporal_on_no_cross_vault_clobber() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3TEMPON").to_string();

    // Écriture `main` puis `vault-b` sur le MÊME note_id.
    idx.write_temporal_entry(&temporal_entry(&nid, "main", 1000))
        .await
        .expect("write temporal main");
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "main doit porter exactement 1 entrée temporelle après son write"
    );
    assert_eq!(
        temporal_count(&idx, "vault-b", &nid).await,
        0,
        "vault-b ne doit avoir aucune entrée avant son propre write"
    );

    idx.write_temporal_entry(&temporal_entry(&nid, "vault-b", 2000))
        .await
        .expect("write temporal vault-b");

    // Isolation : l'entrée de `main` subsiste, `vault-b` a la sienne (2 lignes distinctes).
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "l'entrée temporelle de `main` ne doit PAS être clobberée par le write de `vault-b`"
    );
    assert_eq!(
        temporal_count(&idx, "vault-b", &nid).await,
        1,
        "`vault-b` doit porter sa propre entrée temporelle"
    );
}

/// OFF (mono-vault) : deux `write_temporal_entry` successifs sur le même `(vault, note_id)`
/// restent un `REPLACE` en place — une seule ligne, pas de doublon (byte-identical). La mise à
/// jour de la valeur est déjà couverte par `write_temporal_entry_updates_existing_entry`
/// (tests unitaires sqlite.rs) ; ici on verrouille l'absence de doublon post-recompose PK.
#[tokio::test]
async fn write_temporal_off_replace_in_place() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3TEMPOFF").to_string();

    idx.write_temporal_entry(&temporal_entry(&nid, "main", 1000))
        .await
        .expect("write temporal main v1");
    idx.write_temporal_entry(&temporal_entry(&nid, "main", 5555))
        .await
        .expect("write temporal main v2 (REPLACE)");

    // Toujours une seule ligne pour `main` — le REPLACE mono-vault n'a pas dupliqué.
    assert_eq!(
        temporal_count(&idx, "main", &nid).await,
        1,
        "REPLACE mono-vault doit conserver une unique entrée (pas de doublon)"
    );
}

/// ON (multi-vault local) : `upsert_override_raw` en scope `Vault(main)` puis `Vault(vault-b)`
/// sur le MÊME `note_id` produit deux overrides distincts, chacun lisible avec son payload —
/// aucun clobber. Exercice du write ET du read publics réels.
///
/// RED avant 0034 : la migration recompose la PK ; sans le passage de
/// `ON CONFLICT(note_id, scope_kind, scope_id, override_type)` à la cible composite, tout upsert
/// échouerait (« ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint »).
#[tokio::test]
async fn upsert_override_on_no_cross_vault_clobber() {
    let idx = two_vault_index().await;
    let scope_main = OverrideScope::Vault(VaultId::new("main"));
    let scope_b = OverrideScope::Vault(VaultId::new("vault-b"));

    idx.upsert_override_raw(
        colliding_note_id("01D3OVON"),
        &scope_main,
        "trust",
        1,
        "owner = \"main\"\n",
    )
    .await
    .expect("upsert override main");
    idx.upsert_override_raw(
        colliding_note_id("01D3OVON"),
        &scope_b,
        "trust",
        1,
        "owner = \"vault-b\"\n",
    )
    .await
    .expect("upsert override vault-b (ON CONFLICT composite valide)");

    let r_main = idx
        .get_override_raw(colliding_note_id("01D3OVON"), &scope_main, "trust")
        .await
        .expect("get override main")
        .expect("override main présent");
    let r_b = idx
        .get_override_raw(colliding_note_id("01D3OVON"), &scope_b, "trust")
        .await
        .expect("get override vault-b")
        .expect("override vault-b présent");

    assert!(
        r_main.1.contains("main"),
        "l'override de `main` doit conserver son payload, obtenu {:?}",
        r_main.1
    );
    assert!(
        r_b.1.contains("vault-b"),
        "l'override de `vault-b` doit conserver son payload, obtenu {:?}",
        r_b.1
    );
}

/// OFF (mono-vault) : un second `upsert_override_raw` sur la même clé met à jour en place
/// (`ON CONFLICT DO UPDATE`) — pas de doublon, payload remplacé. Non-régression du couplage
/// migration↔write-path (la cible `ON CONFLICT` composite doit rester valide après 0034).
#[tokio::test]
async fn upsert_override_off_updates_in_place() {
    let idx = two_vault_index().await;
    let scope = OverrideScope::Vault(VaultId::new("main"));

    idx.upsert_override_raw(
        colliding_note_id("01D3OVOFF"),
        &scope,
        "trust",
        1,
        "v = 1\n",
    )
    .await
    .expect("upsert override v1");
    idx.upsert_override_raw(
        colliding_note_id("01D3OVOFF"),
        &scope,
        "trust",
        2,
        "v = 2\n",
    )
    .await
    .expect("upsert override v2 (DO UPDATE)");

    let r = idx
        .get_override_raw(colliding_note_id("01D3OVOFF"), &scope, "trust")
        .await
        .expect("get override")
        .expect("override présent");
    assert_eq!(
        r.0, 2,
        "schema_version doit être mise à jour à 2 (DO UPDATE)"
    );
    assert_eq!(r.1, "v = 2\n", "payload doit être remplacé par la v2");

    // Une seule ligne pour `main` (pas de doublon inséré).
    assert_eq!(
        idx.count_child_rows_for_test(
            "note_overrides",
            "main",
            &colliding_note_id("01D3OVOFF").to_string()
        )
        .await
        .expect("count override main"),
        1,
        "l'upsert en place ne doit pas dupliquer la ligne d'override"
    );
}
