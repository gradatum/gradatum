//! Vérifie que les 4 PRAGMA C12 sont appliquées à l'ouverture de la Queue.
//!
//! Spec §0.3 contrainte C12.

// Rétrocompatibilité P2.0b : LegacyQueue préserve l'API rusqlite Phase 1.
use gradatum_queue::LegacyQueue as Queue;

/// PRAGMA journal_mode, synchronous, busy_timeout, foreign_keys — base mémoire.
///
/// Sur `:memory:`, WAL n'est pas supporté par SQLite, qui revient à `MEMORY`.
/// On accepte les deux valeurs.
#[tokio::test]
async fn pragmas_applied_at_open() {
    let q = Queue::open_in_memory().await.unwrap();

    let journal_mode: String = q.pragma_value("journal_mode").await.unwrap();
    assert!(
        journal_mode.eq_ignore_ascii_case("wal") || journal_mode.eq_ignore_ascii_case("memory"),
        "journal_mode attendu wal ou memory, obtenu : {journal_mode}",
    );

    let synchronous: i64 = q.pragma_value("synchronous").await.unwrap();
    assert_eq!(synchronous, 1, "synchronous=NORMAL (1) attendu");

    let busy_timeout: i64 = q.pragma_value("busy_timeout").await.unwrap();
    assert_eq!(busy_timeout, 5000);

    let foreign_keys: i64 = q.pragma_value("foreign_keys").await.unwrap();
    assert_eq!(foreign_keys, 1);
}

/// Sur base fichier, WAL doit être actif (pas de fallback MEMORY).
#[tokio::test]
async fn pragmas_applied_at_open_file_db() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("queue.db");
    let q = Queue::open(&path).await.unwrap();

    let journal_mode: String = q.pragma_value("journal_mode").await.unwrap();
    assert!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "WAL requis sur base fichier, obtenu : {journal_mode}",
    );

    let synchronous: i64 = q.pragma_value("synchronous").await.unwrap();
    assert_eq!(synchronous, 1, "synchronous=NORMAL (1) attendu");

    let busy_timeout: i64 = q.pragma_value("busy_timeout").await.unwrap();
    assert_eq!(busy_timeout, 5000);

    let foreign_keys: i64 = q.pragma_value("foreign_keys").await.unwrap();
    assert_eq!(foreign_keys, 1);
}
