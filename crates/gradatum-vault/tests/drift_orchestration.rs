//! Tests d'intégration T11d — `drift_check` Phase A orchestration.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

#[tokio::test]
async fn drift_check_empty_vault_returns_zero_counts() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Vault vide : file_checksums est vide → scan Phase A retourne tout à zéro
    let result = vault.drift_check().await.unwrap();

    assert_eq!(result.level2_prefix_match, 0);
    assert_eq!(result.level3_full_hash_match, 0);
    assert_eq!(result.level3_full_hash_mismatch, 0);
    assert!(result.missing.is_empty());
}

#[tokio::test]
async fn drift_check_after_write_note_has_no_mismatch() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    // Écrire une note — pas d'entrée file_checksums en Phase 1 (write_note n'en crée pas encore)
    // Le drift scan Phase A ne trouve aucune entrée → aucun mismatch
    let fm = build_minimal_frontmatter();
    vault
        .write_note(fm, "body drift test".into())
        .await
        .unwrap();

    let result = vault.drift_check().await.unwrap();

    // En Phase 1, write_note n'insère pas dans file_checksums
    // → drift_check scanne 0 entrées → 0 mismatch
    assert_eq!(result.level3_full_hash_mismatch, 0);
}
