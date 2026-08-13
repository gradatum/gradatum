//! Tests path traversal guard FileStorage.
//!
//! Vérifie que `validate_relative_path()` bloque les chemins malveillants avant
//! toute opération I/O. OpenDAL Fs 0.51 ne rejette pas les composants `..` nativement.

use gradatum_storage::{FileStorage, Storage, StorageError};
use tempfile::TempDir;

/// Crée un `FileStorage` sur un répertoire temporaire jetable.
fn make_storage() -> (TempDir, FileStorage) {
    let dir = TempDir::new().expect("TempDir::new() ne doit pas échouer sur un système sain");
    let storage =
        FileStorage::new(dir.path()).expect("FileStorage::new() sur un TempDir local doit reussir");
    (dir, storage)
}

// ── read ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn read_rejects_path_traversal_dotdot() {
    let (_dir, s) = make_storage();
    let result = s.read("../../../etc/passwd").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

#[tokio::test]
async fn read_rejects_embedded_dotdot() {
    let (_dir, s) = make_storage();
    let result = s.read("legit/../../etc/shadow").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── stat ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stat_rejects_path_traversal_dotdot() {
    let (_dir, s) = make_storage();
    let result = s.stat("../../secret.toml").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── write ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn write_rejects_absolute_path() {
    let (_dir, s) = make_storage();
    let result = s.write("/etc/passwd", b"evil").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

#[tokio::test]
async fn write_rejects_path_traversal_dotdot() {
    let (_dir, s) = make_storage();
    let result = s
        .write("../../../tmp/evil.sh", b"#!/bin/sh\nrm -rf /")
        .await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── delete ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_rejects_parent_dir_component() {
    let (_dir, s) = make_storage();
    let result = s.delete("../other_vault/note.md").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── list ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_rejects_mixed_traversal() {
    let (_dir, s) = make_storage();
    let result = s.list("legit/../../etc").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── create_dir ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_dir_rejects_dotdot() {
    let (_dir, s) = make_storage();
    let result = s.create_dir("../../malicious/").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── exists ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn exists_rejects_absolute_path() {
    let (_dir, s) = make_storage();
    let result = s.exists("/etc/hosts").await;
    assert!(
        matches!(result, Err(StorageError::InvalidPath(_))),
        "attendu InvalidPath, obtenu : {result:?}"
    );
}

// ── chemins légitimes (non-régression) ──────────────────────────────────────

#[tokio::test]
async fn read_accepts_legit_relative() {
    let (_dir, s) = make_storage();
    s.write("foo/bar.txt", b"content").await.unwrap();
    let bytes = s.read("foo/bar.txt").await.unwrap();
    assert_eq!(bytes, b"content");
}

#[tokio::test]
async fn write_accepts_nested_relative() {
    let (_dir, s) = make_storage();
    s.write("a/b/c.md", b"data").await.unwrap();
    assert!(s.exists("a/b/c.md").await.unwrap());
}

#[tokio::test]
async fn list_accepts_empty_prefix() {
    let (_dir, s) = make_storage();
    s.write("note.md", b"hello").await.unwrap();
    // Prefix vide = lister tout le vault root — valide.
    let entries = s.list("").await.unwrap();
    assert!(!entries.is_empty(), "au moins note.md doit apparaître");
}
