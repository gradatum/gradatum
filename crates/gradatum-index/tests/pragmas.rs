//! Tests PRAGMA C12 (spec §0.3) — vérification que les 4 PRAGMA sont appliqués
//! dès l'ouverture de la base, sur file et en mémoire.

use gradatum_index::SqliteIndex;
use tempfile::TempDir;

#[tokio::test]
async fn pragmas_applied_at_open_in_memory() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // journal_mode : en mémoire SQLite retourne "memory" (WAL non applicable)
    let journal_mode: String = idx.pragma("journal_mode").await.unwrap();
    assert!(
        journal_mode.eq_ignore_ascii_case("wal") || journal_mode.eq_ignore_ascii_case("memory"),
        "journal_mode attendu wal ou memory, obtenu : {journal_mode}"
    );

    // synchronous = 1 (NORMAL)
    let synchronous: i64 = idx.pragma("synchronous").await.unwrap();
    assert_eq!(synchronous, 1, "synchronous doit être 1 (NORMAL)");

    // busy_timeout = 5000
    let busy_timeout: i64 = idx.pragma("busy_timeout").await.unwrap();
    assert_eq!(busy_timeout, 5000, "busy_timeout doit être 5000ms");

    // foreign_keys = 1 (ON)
    let foreign_keys: i64 = idx.pragma("foreign_keys").await.unwrap();
    assert_eq!(foreign_keys, 1, "foreign_keys doit être 1 (ON)");
}

#[tokio::test]
async fn pragmas_applied_at_open_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("idx.db");

    let idx = SqliteIndex::open(&path).await.unwrap();

    // Sur un fichier, WAL est effectif
    let journal_mode: String = idx.pragma("journal_mode").await.unwrap();
    assert!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "journal_mode doit être wal sur un fichier, obtenu : {journal_mode}"
    );

    let synchronous: i64 = idx.pragma("synchronous").await.unwrap();
    assert_eq!(synchronous, 1);

    let busy_timeout: i64 = idx.pragma("busy_timeout").await.unwrap();
    assert_eq!(busy_timeout, 5000);

    let foreign_keys: i64 = idx.pragma("foreign_keys").await.unwrap();
    assert_eq!(foreign_keys, 1);
}

#[tokio::test]
async fn open_file_idempotent() {
    // Ouvrir deux fois le même fichier ne doit pas planter (migrations idempotentes)
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("idx2.db");

    SqliteIndex::open(&path).await.unwrap();
    SqliteIndex::open(&path).await.unwrap(); // 2e open, migrations déjà appliquées
}
