//! Tests d'intégration — `FileStorage` via backend OpenDAL Fs.
//!
//! Tous les tests utilisent un `TempDir` isolé — aucune dépendance sur l'état du système.

use gradatum_storage::{FileStorage, Storage};
use tempfile::TempDir;

/// Crée un `FileStorage` sur un répertoire temporaire jetable.
fn make_storage() -> (TempDir, FileStorage) {
    let dir = TempDir::new().expect("TempDir::new() ne doit pas échouer sur un système sain");
    let storage =
        FileStorage::new(dir.path()).expect("FileStorage::new() sur un TempDir local doit reussir");
    (dir, storage)
}

#[tokio::test]
async fn write_then_read_round_trip() {
    let (_dir, s) = make_storage();
    s.write("hello.txt", b"world").await.unwrap();
    let bytes = s.read("hello.txt").await.unwrap();
    assert_eq!(bytes, b"world");
}

#[tokio::test]
async fn read_missing_returns_not_found() {
    let (_dir, s) = make_storage();
    let result = s.read("absent.txt").await;
    assert!(result.is_err(), "lecture d'un fichier absent doit échouer");
    let err = result.unwrap_err();
    // Vérifier que c'est bien un NotFound (pas une erreur I/O générique).
    assert!(
        matches!(err, gradatum_storage::StorageError::NotFound(_)),
        "attendu StorageError::NotFound, obtenu : {err:?}"
    );
}

#[tokio::test]
async fn delete_removes_file() {
    let (_dir, s) = make_storage();
    s.write("to_delete.txt", b"data").await.unwrap();
    // Vérifier présence avant suppression.
    assert!(s.exists("to_delete.txt").await.unwrap());
    // Supprimer.
    s.delete("to_delete.txt").await.unwrap();
    // Vérifier absence après suppression.
    assert!(!s.exists("to_delete.txt").await.unwrap());
}

#[tokio::test]
async fn list_returns_files_under_prefix() {
    let (_dir, s) = make_storage();
    s.write("a/1.txt", b"x").await.unwrap();
    s.write("a/2.txt", b"y").await.unwrap();
    s.write("b/3.txt", b"z").await.unwrap();

    let entries = s.list("a/").await.unwrap();
    // On attend 2 entrées fichiers sous le préfixe "a/" (pas de "b/3.txt").
    // Le répertoire "a/" lui-même peut apparaître selon le backend — on filtre les is_dir.
    let file_entries: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();
    assert_eq!(
        file_entries.len(),
        2,
        "attendu 2 fichiers sous a/, obtenu : {entries:?}"
    );
    // Vérifier que les chemins sont bien sous a/.
    for e in &file_entries {
        assert!(
            e.path.starts_with("a/"),
            "chemin inattendu hors du préfixe a/ : {:?}",
            e.path
        );
    }
}

#[tokio::test]
async fn exists_returns_true_for_existing_file() {
    let (_dir, s) = make_storage();
    s.write("present.txt", b"ici").await.unwrap();
    assert!(s.exists("present.txt").await.unwrap());
}

#[tokio::test]
async fn exists_returns_false_for_missing_file() {
    let (_dir, s) = make_storage();
    assert!(!s.exists("ghost.txt").await.unwrap());
}

#[tokio::test]
async fn stat_returns_correct_size() {
    let (_dir, s) = make_storage();
    let content = b"gradatum storage test";
    s.write("sized.txt", content).await.unwrap();
    let entry = s.stat("sized.txt").await.unwrap();
    assert_eq!(
        entry.size,
        content.len() as u64,
        "stat.size doit correspondre au nombre d'octets écrits"
    );
    assert!(!entry.is_dir);
}

#[tokio::test]
async fn write_overwrites_existing_content() {
    let (_dir, s) = make_storage();
    s.write("overwrite.txt", b"v1").await.unwrap();
    s.write("overwrite.txt", b"v2").await.unwrap();
    let bytes = s.read("overwrite.txt").await.unwrap();
    assert_eq!(bytes, b"v2");
}

#[tokio::test]
async fn root_accessor_returns_configured_path() {
    let dir = TempDir::new().unwrap();
    let s = FileStorage::new(dir.path()).unwrap();
    assert_eq!(s.root(), dir.path());
}
