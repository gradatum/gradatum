//! Tests du guard NFS — caveat C11.
//!
//! ## Test gating
//!
//! Le test `nfs_path_returns_error` est marqué `#[ignore]` et requiert la
//! variable d'environnement `GRADATUM_TEST_NFS_PATH` pointant vers un montage NFS.
//! Voir `tests/README.md` pour les instructions d'exécution.

use gradatum_storage::ensure_local_filesystem;
use tempfile::TempDir;

#[test]
fn local_path_returns_ok() {
    let dir = TempDir::new().expect("TempDir::new() ne doit pas échouer");
    let result = ensure_local_filesystem(dir.path());
    assert!(
        result.is_ok(),
        "ensure_local_filesystem doit retourner Ok sur un chemin local, obtenu : {result:?}"
    );
}

#[test]
fn nonexistent_path_with_local_parent_returns_ok() {
    let dir = TempDir::new().expect("TempDir::new() ne doit pas échouer");
    // Chemin qui n'existe pas encore — le parent est local → Ok attendu.
    let new_vault = dir.path().join("nouveau_vault");
    let result = ensure_local_filesystem(&new_vault);
    assert!(
        result.is_ok(),
        "ensure_local_filesystem sur chemin inexistant avec parent local doit retourner Ok, obtenu : {result:?}"
    );
}

#[test]
#[ignore = "requiert GRADATUM_TEST_NFS_PATH pointant vers un montage NFS"]
fn nfs_path_returns_error() {
    let nfs_path = std::env::var("GRADATUM_TEST_NFS_PATH")
        .expect("GRADATUM_TEST_NFS_PATH non défini — test ignoré si non fourni");
    let result = ensure_local_filesystem(std::path::Path::new(&nfs_path));
    assert!(
        result.is_err(),
        "ensure_local_filesystem doit retourner Err sur un montage NFS, obtenu : {result:?}"
    );
    // Vérifier le type d'erreur exact — doit être VaultOnNfs via Core.
    assert!(
        matches!(result.unwrap_err(), gradatum_storage::StorageError::Core(_)),
        "attendu StorageError::Core(VaultOnNfs)"
    );
}
