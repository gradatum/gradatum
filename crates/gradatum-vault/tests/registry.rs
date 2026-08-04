//! Tests du trait `Registry` — T2 P2.0c.
//!
//! Vérifie que `Vault` implémente le trait `Registry` avec des comptages réels
//! depuis l'index SQLite (pas les valeurs stub 0/0 de `VaultRegistryStub`).

mod common;

use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_vault::{Registry, Vault};
use tempfile::TempDir;

/// Vérifie que `tenant_count` et `locus_count` retournent des valeurs réelles.
///
/// Après `Vault::create` sur un vault vide, les deux compteurs sont 0.
/// Après `ensure_tenant("main")`, `tenant_count` passe à 1.
#[tokio::test]
async fn vault_registry_returns_real_counts() {
    let dir = TempDir::new().unwrap();
    // create initialise le layout (.gradatum/index.db) — open échoue sans layout.
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("vault create");

    assert_eq!(vault.tenant_count().await.unwrap(), 0);
    assert_eq!(vault.locus_count().await.unwrap(), 0);

    // Après ensure_tenant, le compteur tenant doit passer à 1.
    vault.ensure_tenant("main").await.unwrap();
    assert_eq!(vault.tenant_count().await.unwrap(), 1);
}

/// C4-1b (P0 security review) : `write_note_with_id_internal` exige un témoin
/// [`AclCheckedVaultId`] égal à `frontmatter.vault_id` (anti-oubli + anti ex-hardcode).
/// Témoin cohérent → écriture ; témoin divergent → refus fail-closed avant tout write.
#[tokio::test]
async fn write_note_internal_rejects_witness_vault_mismatch() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("vault create");
    let fm = common::build_minimal_frontmatter(); // vault_id = "main"

    // Témoin cohérent (main == frontmatter.vault_id) → écriture réussie.
    let ok = AclCheckedVaultId::for_system_task(VaultId::new("main"));
    vault
        .write_note_with_id_internal(&ok, fm.clone(), "corps".into(), NoteId::new())
        .await
        .expect("témoin cohérent → écriture réussie");

    // Témoin divergent (research ≠ frontmatter.vault_id main) → refus fail-closed.
    let bad = AclCheckedVaultId::for_system_task(VaultId::new("research"));
    let err = vault
        .write_note_with_id_internal(&bad, fm, "corps".into(), NoteId::new())
        .await
        .expect_err("témoin divergent doit être refusé");
    assert!(
        matches!(
            err,
            gradatum_core::error::GradatumError::Storage(ref m) if m.contains("ACL witness")
        ),
        "erreur attendue = incohérence témoin/vault_id, obtenu: {err:?}"
    );
}

/// C4-1c (P2 security review) : un write vers un AUTRE vault avec un ULID collisionné à une
/// note *live* de `main` NE DOIT PAS déclencher de CoW-snapshot dans `main/.history/` (le
/// read-before-write lit désormais le vault CIBLE, pas `main`). La note live de `main` reste
/// intacte. Discrimination : sans le fix, `main/.history/<id>/` reçoit un snapshot du contenu main.
#[tokio::test]
async fn write_to_other_vault_does_not_cow_into_main_history() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .expect("vault create");

    // 1. Note LIVE dans `main` (frontmatter.vault_id = "main").
    let victim = vault
        .write_note(
            common::build_minimal_frontmatter(),
            "main live corpus".into(),
        )
        .await
        .expect("write main victim");
    let vid = victim.id;

    // Pré-condition : aucune version d'historique dans main/.history/<id>/.
    assert!(
        Registry::history_versions(&vault, &vid.to_string())
            .await
            .unwrap()
            .is_empty(),
        "précondition : pas d'historique avant l'attaque"
    );

    // 2. Un tenant tiers `research` écrit un frontmatter avec le MÊME ULID (collision).
    let mut fm_research = common::build_minimal_frontmatter();
    fm_research.vault_id = VaultId::new("research");
    let witness = AclCheckedVaultId::for_system_task(VaultId::new("research"));
    vault
        .write_note_with_id_internal(&witness, fm_research, "research payload".into(), vid)
        .await
        .expect("write research (ULID collisionné)");

    // 3. AUCUN snapshot CoW dans main/.history/<id>/ (read-before-write a lu `research`, vide).
    assert!(
        Registry::history_versions(&vault, &vid.to_string())
            .await
            .unwrap()
            .is_empty(),
        "collision ULID cross-vault NE DOIT PAS créer de snapshot dans main/.history/ (CoW-into-main)"
    );

    // 4. La note live de `main` reste intacte (contenu + vault_id d'origine).
    let main_note = vault.read_note(vid).await.expect("read main victim");
    assert_eq!(
        main_note.body.markdown, "main live corpus",
        "le contenu de la note main NE DOIT PAS avoir changé"
    );
    assert_eq!(
        main_note.frontmatter.vault_id.as_str(),
        "main",
        "la note main reste dans le vault main"
    );
}
