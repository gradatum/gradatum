//! Isolation cross-vault des overrides **Locus** sur `note_overrides`.
//!
//! Avant le correctif, les scopes `OverrideScope::Locus` / `Bearer` étaient persistés
//! avec une sentinelle `vault_id = "_unset"` (bucket GLOBAL, partagé par TOUS les vaults).
//! Sous la PK composite `(vault_id, note_id, scope_kind, scope_id, override_type)` (migration
//! 0034), deux vaults distincts portant un override Locus au MÊME `note_id` (ULID collisionné)
//! et au MÊME `locus` collisionnaient sur la clé `(_unset, note, 'locus', locus-x, type)` :
//!   * write-path (`upsert_override_raw`, `ON CONFLICT DO UPDATE`) → clobber : le write du
//!     second vault écrase le payload du premier ;
//!   * read-path (`get_override_raw`) → cross-read : la lecture d'un vault renvoie le payload
//!     de l'autre (dernier écrivain gagne).
//!
//! Le model-change (`OverrideScope::Locus { vault, locus }`) porte désormais le `vault` réel
//! jusqu'aux colonnes `vault_id`, fermant la classe hijack à la racine. Le régime multi-vault
//! est purement local au harnais (flag `multi_tenant.enabled` reste OFF) : les jeux mono-vault
//! prouvent que le round-trip (payload lu == payload écrit) reste inchangé.

mod common;

use common::{colliding_note_id, two_vault_index};
use gradatum_core::scope::{LocusId, OverrideScope, VaultId};
use gradatum_index::SqliteIndex;

/// Construit un `OverrideScope::Locus` pour un `(vault, locus)` donné.
fn locus_scope(vault: &str, locus: &str) -> OverrideScope {
    OverrideScope::Locus {
        vault: VaultId::new(vault),
        locus: LocusId::new(locus),
    }
}

/// Compte les lignes `note_overrides` scopées `(vault_id, note_id)` via le helper de test.
async fn override_count(idx: &SqliteIndex, vault_id: &str, note_id: &str) -> u64 {
    idx.count_child_rows_for_test("note_overrides", vault_id, note_id)
        .await
        .expect("count_child_rows_for_test note_overrides")
}

/// ON (multi-vault local) : un override Locus pour un `note_id` colliding, **même `locus`**,
/// écrit dans `main` puis `vault-b`, ne doit PAS clobber — chaque vault relit SON payload.
///
/// RED (sentinelle `_unset` partagée) : les deux writes partagent la PK
/// `(_unset, note, 'locus', private, metadata)` → le write de `vault-b` écrase celui de `main`
/// (`ON CONFLICT DO UPDATE`), et les DEUX reads renvoient le payload de `vault-b` (clobber +
/// cross-read). L'assertion `r_main.contains("main")` échoue.
#[tokio::test]
async fn override_locus_no_cross_vault_clobber() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3LOCUSON");
    let scope_main = locus_scope("main", "private");
    let scope_b = locus_scope("vault-b", "private");

    idx.upsert_override_raw(nid, &scope_main, "metadata", 1, "owner = \"main\"\n")
        .await
        .expect("upsert override Locus main");
    idx.upsert_override_raw(nid, &scope_b, "metadata", 1, "owner = \"vault-b\"\n")
        .await
        .expect("upsert override Locus vault-b");

    let r_main = idx
        .get_override_raw(nid, &scope_main, "metadata")
        .await
        .expect("get override Locus main")
        .expect("override Locus main présent");
    let r_b = idx
        .get_override_raw(nid, &scope_b, "metadata")
        .await
        .expect("get override Locus vault-b")
        .expect("override Locus vault-b présent");

    assert!(
        r_main.1.contains("main"),
        "l'override Locus de `main` doit conserver SON payload (pas de clobber par vault-b), got {:?}",
        r_main.1
    );
    assert!(
        r_b.1.contains("vault-b"),
        "l'override Locus de `vault-b` doit conserver SON payload, got {:?}",
        r_b.1
    );

    // Deux lignes distinctes, une par vault (isolation structurelle par PK composite).
    assert_eq!(
        override_count(&idx, "main", &nid.to_string()).await,
        1,
        "`main` doit porter exactement 1 override Locus"
    );
    assert_eq!(
        override_count(&idx, "vault-b", &nid.to_string()).await,
        1,
        "`vault-b` doit porter exactement 1 override Locus"
    );
}

/// OFF (mono-vault) : round-trip d'un override Locus sur `main` — le payload lu est
/// **identique** au payload écrit et une seule ligne existe (pas de doublon). Verrouille
/// l'invariant byte-identical côté comportement observable : le correctif substitue
/// `vault_id = "main"` à `"_unset"` mais la ligne relue == la ligne écrite (read/write
/// dérivent le `vault_id` à l'identique), donc l'override effectif est inchangé.
#[tokio::test]
async fn override_locus_main_readback_matches_write() {
    let idx = two_vault_index().await;
    let nid = colliding_note_id("01D3LOCUSMAIN");
    let scope = locus_scope("main", "private");

    idx.upsert_override_raw(nid, &scope, "metadata", 3, "k = \"v\"\n")
        .await
        .expect("upsert override Locus main");

    let r = idx
        .get_override_raw(nid, &scope, "metadata")
        .await
        .expect("get override Locus main")
        .expect("override Locus main présent");

    assert_eq!(r.0, 3, "schema_version relue == écrite");
    assert_eq!(
        r.1, "k = \"v\"\n",
        "payload_toml relu == écrit (override effectif inchangé)"
    );
    assert_eq!(
        override_count(&idx, "main", &nid.to_string()).await,
        1,
        "l'upsert Locus mono-vault ne doit pas dupliquer la ligne"
    );
}
