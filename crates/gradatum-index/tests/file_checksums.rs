//! Tests de la table `file_checksums` — upsert, list, round-trip des tableaux [u8; 32].

mod common;
use common::make_checksum_entry;

use gradatum_core::index::FileKind;
use gradatum_index::SqliteIndex;
use tempfile::TempDir;

#[tokio::test]
async fn upsert_then_list_round_trip() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("note.md");
    std::fs::write(&file_path, "# Test\n\nContenu de la note.").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry = make_checksum_entry(&file_path, "note.md", FileKind::Note);

    idx.upsert_file_checksum(&entry).await.unwrap();

    let all = idx.list_file_checksums().await.unwrap();
    assert_eq!(all.len(), 1, "doit retourner exactement 1 entrée");

    let stored = &all[0];
    assert_eq!(stored.relative_path, "note.md");
    assert_eq!(stored.file_kind, FileKind::Note);
    assert_eq!(stored.expected_size, entry.expected_size);
    assert_eq!(
        stored.expected_hash_prefix_4kb, entry.expected_hash_prefix_4kb,
        "expected_hash_prefix_4kb doit être [u8; 32] identique"
    );
    assert_eq!(
        stored.expected_hash, entry.expected_hash,
        "expected_hash doit être [u8; 32] identique"
    );
}

#[tokio::test]
async fn upsert_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("config.toml");
    std::fs::write(&file_path, "[vault]\nid = \"main\"\n").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry = make_checksum_entry(&file_path, "config.toml", FileKind::Config);

    idx.upsert_file_checksum(&entry).await.unwrap();
    idx.upsert_file_checksum(&entry).await.unwrap(); // 2ème appel identique

    let all = idx.list_file_checksums().await.unwrap();
    assert_eq!(all.len(), 1, "upsert idempotent — toujours 1 entrée");
}

#[tokio::test]
async fn upsert_updates_existing_entry() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("override.toml");
    std::fs::write(&file_path, "priority = 0").unwrap();

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let entry_v1 = make_checksum_entry(&file_path, "override.toml", FileKind::Override);
    idx.upsert_file_checksum(&entry_v1).await.unwrap();

    // Modifie le fichier et recalcule
    std::fs::write(&file_path, "priority = 100").unwrap();
    let entry_v2 = make_checksum_entry(&file_path, "override.toml", FileKind::Override);
    idx.upsert_file_checksum(&entry_v2).await.unwrap();

    let all = idx.list_file_checksums().await.unwrap();
    assert_eq!(all.len(), 1, "toujours 1 entrée après update");
    assert_ne!(
        all[0].expected_hash, entry_v1.expected_hash,
        "le hash doit avoir changé"
    );
    assert_eq!(
        all[0].expected_hash, entry_v2.expected_hash,
        "le hash doit correspondre à la v2"
    );
}

#[tokio::test]
async fn list_multiple_checksums() {
    let dir = TempDir::new().unwrap();
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let files = ["a.md", "b.md", "config.toml"];
    for name in &files {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("contenu {name}")).unwrap();
        let kind = if name.ends_with(".toml") {
            FileKind::Config
        } else {
            FileKind::Note
        };
        let entry = make_checksum_entry(&p, name, kind);
        idx.upsert_file_checksum(&entry).await.unwrap();
    }

    let all = idx.list_file_checksums().await.unwrap();
    assert_eq!(all.len(), 3, "doit retourner les 3 entrées");
}
