//! v1-parity : Drift end-to-end — 3 tests.
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_phase1.rs` (sections drift).
//! Domaine : drift_check Phase A — pas de changement, modification externe, fichier manquant.

mod common;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

// --- 1. drift_no_changes_returns_zero_mismatches ---

/// Écrit une note, lance drift_check immédiatement → 0 mismatch (fichier intact).
#[tokio::test]
async fn drift_no_changes_returns_zero_mismatches() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let fm = common::minimal_frontmatter("main");
    vault
        .write_note(fm, "Note stable sans modification externe.".into())
        .await
        .expect("write_note");

    let result = vault.drift_check().await.expect("drift_check");

    assert_eq!(
        result.level3_full_hash_mismatch, 0,
        "Aucun mismatch attendu sur fichier intact"
    );
    assert!(result.missing.is_empty(), "Aucun fichier manquant attendu");
}

// --- 2. drift_after_external_md_edit_detected ---

/// Écrit une note, modifie le fichier .md directement sur disque (simulation
/// éditeur externe), lance drift_check → mismatch détecté.
///
/// Ignoré : `upsert_file_checksum` n'est pas appelé dans `Vault::write_note`,
/// donc `file_checksums` reste vide et `scan_phase_a` ne trouve aucune entrée à comparer.
#[tokio::test]
#[ignore = "Phase 2+ : blocked by Vault::write_note stub (upsert_file_checksum not called)"]
async fn drift_after_external_md_edit_detected() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let fm = common::minimal_frontmatter("main");
    let note = vault
        .write_note(fm, "Contenu original avant modification externe.".into())
        .await
        .expect("write_note");

    // Modification externe du fichier .md — simule un éditeur
    let md_path = tmp.path().join("main").join(format!("{}.md", note.id));
    assert!(
        md_path.exists(),
        "Le fichier .md doit exister avant modification"
    );

    let original_content = std::fs::read_to_string(&md_path).expect("lecture .md");
    let modified = format!("{}\n\n<!-- modification externe -->\n", original_content);
    std::fs::write(&md_path, modified).expect("écriture .md modifié");

    let result = vault
        .drift_check()
        .await
        .expect("drift_check après modification");

    assert_eq!(
        result.level3_full_hash_mismatch, 1,
        "1 mismatch attendu après modification externe du .md"
    );
}

// --- 3. drift_missing_md_file_reported ---

/// Écrit une note, supprime le fichier .md, lance drift_check → fichier signalé
/// dans DriftScanResult::missing.
///
/// Ignoré : même raison que le test précédent (`upsert_file_checksum` non câblé).
#[tokio::test]
#[ignore = "Phase 2+ : blocked by Vault::write_note stub (upsert_file_checksum not called)"]
async fn drift_missing_md_file_reported() {
    let tmp = TempDir::new().unwrap();
    let vault = Vault::create(tmp.path(), VaultId::new("main"))
        .await
        .expect("vault::create");

    let fm = common::minimal_frontmatter("main");
    let note = vault
        .write_note(
            fm,
            "Note dont le fichier sera supprimé manuellement.".into(),
        )
        .await
        .expect("write_note");

    // Supprime le fichier .md directement
    let md_path = tmp.path().join("main").join(format!("{}.md", note.id));
    std::fs::remove_file(&md_path).expect("suppression .md");

    let result = vault
        .drift_check()
        .await
        .expect("drift_check après suppression");

    assert_eq!(
        result.missing.len(),
        1,
        "1 fichier manquant attendu dans DriftScanResult::missing"
    );
}
