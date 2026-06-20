//! Tests du helper `drift::scan_phase_a` — 3 niveaux (size → prefix-4KB → full sha256).

mod common;
use common::make_checksum_entry;

use gradatum_core::index::FileKind;
use gradatum_index::{SqliteIndex, drift};
use gradatum_storage::FileStorage;
use std::fs;
use tempfile::TempDir;

#[tokio::test]
async fn drift_no_changes_all_prefix_match() {
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();

    let path_a = vault_root.join("a.md");
    let path_b = vault_root.join("b.md");
    fs::write(&path_a, "# Note A\n\nhello world gradatum").unwrap();
    fs::write(&path_b, "# Note B\n\ncorps différent note B").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry_a = make_checksum_entry(&path_a, "a.md", FileKind::Note);
    let entry_b = make_checksum_entry(&path_b, "b.md", FileKind::Note);
    idx.upsert_file_checksum(&entry_a).await.unwrap();
    idx.upsert_file_checksum(&entry_b).await.unwrap();

    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    assert_eq!(
        result.level2_prefix_match, 2,
        "les 2 fichiers inchangés doivent passer niveau 2"
    );
    assert_eq!(result.level3_full_hash_mismatch, 0, "aucun drift détecté");
    assert!(result.missing.is_empty(), "aucun fichier manquant");
}

#[tokio::test]
async fn drift_size_changed_detected_as_mismatch() {
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();
    let path = vault_root.join("modified.md");

    // Créer le fichier + enregistrer la checksum
    fs::write(&path, "contenu original").unwrap();
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry = make_checksum_entry(&path, "modified.md", FileKind::Note);
    idx.upsert_file_checksum(&entry).await.unwrap();

    // Modifier le fichier APRÈS l'enregistrement de la checksum
    fs::write(&path, "contenu modifié — taille différente et hash changé").unwrap();

    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    assert_eq!(
        result.level3_full_hash_mismatch, 1,
        "le fichier modifié doit être détecté en mismatch niveau 3"
    );
    assert_eq!(result.level2_prefix_match, 0);
    assert!(result.missing.is_empty());
}

#[tokio::test]
async fn drift_missing_file_collected() {
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();
    let path = vault_root.join("will_be_deleted.md");

    fs::write(&path, "contenu qui va disparaître").unwrap();
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry = make_checksum_entry(&path, "will_be_deleted.md", FileKind::Note);
    idx.upsert_file_checksum(&entry).await.unwrap();

    // Supprimer le fichier
    fs::remove_file(&path).unwrap();

    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    assert_eq!(result.missing.len(), 1, "1 fichier manquant détecté");
    assert_eq!(result.level2_prefix_match, 0);
    assert_eq!(result.level3_full_hash_mismatch, 0);
}

#[tokio::test]
async fn drift_empty_index_no_scan() {
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();
    // Aucune entrée dans file_checksums
    fs::write(vault_root.join("untracked.md"), "non tracké").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    // Aucune entrée → rien à vérifier
    assert_eq!(result.level2_prefix_match, 0);
    assert_eq!(result.level3_full_hash_mismatch, 0);
    assert!(result.missing.is_empty());
}

#[tokio::test]
async fn drift_same_size_different_prefix_falls_to_level3() {
    // Cas rare mais important : même taille, prefix différent → niveau 3
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();
    let path = vault_root.join("same_size.md");

    // Contenu initial de longueur précise
    let original = "aaaaaaaaaaaaaaaa"; // 16 bytes
    fs::write(&path, original).unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry = make_checksum_entry(&path, "same_size.md", FileKind::Note);
    idx.upsert_file_checksum(&entry).await.unwrap();

    // Remplacer avec contenu différent mais même taille
    let modified = "bbbbbbbbbbbbbbbb"; // 16 bytes — même taille, prefix différent
    assert_eq!(
        original.len(),
        modified.len(),
        "les tailles doivent être identiques"
    );
    fs::write(&path, modified).unwrap();

    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    // Size identique mais prefix différent → niveau 3, hash différent
    assert_eq!(result.level3_full_hash_mismatch, 1);
    assert_eq!(result.level2_prefix_match, 0);
}

#[tokio::test]
async fn drift_mixed_unchanged_and_modified() {
    let dir = TempDir::new().unwrap();
    let vault_root = dir.path();

    let unchanged = vault_root.join("unchanged.md");
    let changed = vault_root.join("changed.md");
    let gone = vault_root.join("gone.md");

    fs::write(&unchanged, "stable depuis toujours").unwrap();
    fs::write(&changed, "va changer").unwrap();
    fs::write(&gone, "va disparaitre").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    idx.upsert_file_checksum(&make_checksum_entry(
        &unchanged,
        "unchanged.md",
        FileKind::Note,
    ))
    .await
    .unwrap();
    idx.upsert_file_checksum(&make_checksum_entry(&changed, "changed.md", FileKind::Note))
        .await
        .unwrap();
    idx.upsert_file_checksum(&make_checksum_entry(&gone, "gone.md", FileKind::Note))
        .await
        .unwrap();

    // Modifier + supprimer
    fs::write(&changed, "contenu modifié après enregistrement").unwrap();
    fs::remove_file(&gone).unwrap();

    let storage = FileStorage::new(vault_root).unwrap();
    let result = drift::scan_phase_a(&storage, &idx).await.unwrap();

    assert_eq!(
        result.level2_prefix_match, 1,
        "unchanged doit passer niveau 2"
    );
    assert_eq!(
        result.level3_full_hash_mismatch, 1,
        "changed doit échouer niveau 3"
    );
    assert_eq!(result.missing.len(), 1, "gone doit être dans missing");
    assert!(result.missing[0].ends_with("gone.md"));
}
