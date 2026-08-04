//! Distinction **compile-enforced** entre le PRINCIPAL
//! (`TenantId`, écho du JWT) et la CIBLE namespace (`VaultId`), plus isolation
//! cross-vault de `note_usage`.
//!
//! Ce test ne vérifie pas un comportement runtime « fragile » : il verrouille
//! l'invariant de convergence typée. Toute régression qui reconfondrait principal et
//! cible (ex. repasser un champ DTO en `String`, ou re-typer `counts_for_notes` en
//! `&str`) casse la COMPILATION de ce fichier — pas silencieusement la prod.
//!
//! Réfs : résolution conflation `note_usage`
//! (`arch/01KXWMDDX1`) ; choke-point `effective_read_vault`/`effective_write_vault`.

use std::collections::HashMap;

use gradatum_core::scope::{TenantId, VaultId};
use gradatum_dto::VaultSearchRequest;
use gradatum_server::note_usage_store::{KIND_READ, NoteUsageStore, UsageKey, UsageValue};

/// Compile-enforced : `TenantId` (principal) et `VaultId` (cible) sont des newtypes
/// DISTINCTS. Ces deux fns ne peuvent recevoir QUE leur type — toute confusion
/// principal/cible devient un échec de compilation, jamais un bug d'isolation runtime.
fn wants_principal(_t: &TenantId) {}
fn wants_target(_v: &VaultId) {}

/// Le DTO d'entrée porte les DEUX dimensions en champs typés séparés :
/// `tenant_id: Option<TenantId>` (écho principal, optionnel — A1) et
/// `vault_id: Option<VaultId>` (cible namespace).
/// La désérialisation reste octet-pour-octet la même chaîne filaire (newtype transparent).
#[test]
fn dto_carries_principal_and_target_as_distinct_newtypes() {
    let req: VaultSearchRequest = serde_json::from_value(serde_json::json!({
        "tenant_id": "main",
        "query": "hello",
        "vault_id": "vault-b",
    }))
    .expect("VaultSearchRequest deserialize");

    // Compile-enforced : `tenant_id` EST un `Option<TenantId>` (A1 — optionnel, résolu
    // serveur), `vault_id` EST `Option<VaultId>`. Le JSON fournit `tenant_id` → `Some`.
    let principal = req
        .tenant_id
        .as_ref()
        .expect("tenant_id present dans ce JSON");
    wants_principal(principal);
    let target = req.vault_id.as_ref().expect("vault_id present");
    wants_target(target);

    // Les valeurs restent les chaînes filaires (byte-identical wire).
    assert_eq!(principal.as_str(), "main");
    assert_eq!(target.as_str(), "vault-b");

    // Les lignes suivantes NE COMPILERAIENT PAS — verrou de convergence :
    //   wants_target(&req.tenant_id);   // TenantId n'est pas un VaultId
    //   wants_principal(target);        // VaultId n'est pas un TenantId
}

/// `note_usage` est scopé per-**NAMESPACE** (`VaultId`), pas par principal.
/// Deux vaults partageant un `note_id` collisionné restent isolés : `counts_for_notes`
/// d'un vault ne renvoie JAMAIS les compteurs d'un autre.
///
/// Compile-enforced : `counts_for_notes` exige `&VaultId` — passer le principal
/// (`&TenantId`) ne compile pas (voir commentaire final).
#[tokio::test]
async fn note_usage_counts_are_namespace_scoped_no_cross_vault_leak() {
    // `open_or_create` = constructeur exposé aux tests d'intégration (crée table + PRAGMAs).
    let dir = tempfile::tempdir().expect("tempdir");
    let store = NoteUsageStore::open_or_create(&dir.path().join("note_usage.db"))
        .await
        .expect("open store");

    // Même `note_id` "01X" écrit sous DEUX vaults distincts (ULID collisionné cross-vault) :
    // la clé d'usage porte le vault namespace en 1re composante (colonne SQL legacy `tenant_id`).
    let mut batch: HashMap<UsageKey, UsageValue> = HashMap::new();
    batch.insert(("main".into(), "01X".into(), KIND_READ.into()), (5, 1_000));
    batch.insert(
        ("vault-b".into(), "01X".into(), KIND_READ.into()),
        (2, 2_000),
    );
    store.flush_batch(batch).await.expect("flush");

    let ids = vec!["01X".to_string()];

    // Lecture scopée `vault-b` : compteur = 2 (jamais 5 de `main`).
    let counts_b = store
        .counts_for_notes(&VaultId::new("vault-b"), &ids)
        .await
        .expect("read vault-b");
    assert_eq!(
        counts_b.get("01X"),
        Some(&vec![(KIND_READ.to_string(), 2_u64)]),
        "vault-b ne voit QUE son propre compteur (pas de fuite depuis main)"
    );

    // Lecture scopée `main` : compteur = 5 (jamais 2 de `vault-b`).
    let counts_main = store
        .counts_for_notes(&VaultId::new("main"), &ids)
        .await
        .expect("read main");
    assert_eq!(
        counts_main.get("01X"),
        Some(&vec![(KIND_READ.to_string(), 5_u64)]),
        "main ne voit QUE son propre compteur (pas de fuite depuis vault-b)"
    );

    // Ligne NON compilable — verrou de dimension (principal ≠ namespace) :
    //   store.counts_for_notes(&TenantId::new("vault-b"), &ids).await;
    let _principal_is_a_different_type: TenantId = TenantId::new("main");
}
