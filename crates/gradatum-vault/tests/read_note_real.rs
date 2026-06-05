//! Tests TDD T4 P2.0c — `Vault::read_note()` implémentation réelle.
//!
//! Baseline spec : plan P2.0c Task 4, steps 1-7.

mod common;
use common::build_minimal_frontmatter;

use gradatum_core::scope::VaultId;
use gradatum_vault::Vault;
use tempfile::TempDir;

/// Vérifie qu'une note écrite via `write_note` est lisible via `read_note` avec le bon contenu.
///
/// Step 1 du plan T4 : test "round-trip" write → read.
#[tokio::test]
async fn read_note_returns_persisted_note_after_write() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "Corps de la note T4".into())
        .await
        .unwrap();

    let note_id = note.id;

    let read_note = vault
        .read_note(note_id)
        .await
        .expect("read_note doit retourner la note persistée");

    assert_eq!(
        read_note.body.markdown.trim(),
        "Corps de la note T4",
        "le corps de la note doit correspondre"
    );
    assert_eq!(
        read_note.frontmatter.section,
        gradatum_core::section::Section::Decisions,
        "la section doit correspondre"
    );
    assert_eq!(
        read_note.frontmatter.vault_id.as_str(),
        "main",
        "le vault_id doit être 'main'"
    );
}

/// Vérifie que le second appel à `read_note` incrémente le compteur de cache hits.
///
/// Step 1 du plan T4 : test "cache hit" via métrique `cache_hits()`.
#[tokio::test]
async fn read_note_uses_cache_on_second_call() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::create(dir.path(), VaultId::new("main"))
        .await
        .unwrap();

    let fm = build_minimal_frontmatter();
    let note = vault
        .write_note(fm, "Corps CacheTest T4".into())
        .await
        .unwrap();

    let note_id = note.id;

    // Premier appel : cache miss → fetch depuis index + storage
    let _first = vault
        .read_note(note_id)
        .await
        .expect("premier read_note doit réussir");

    let hits_before = vault.cache_hits();

    // Second appel : cache hit (le hash SQLite correspond)
    let _second = vault
        .read_note(note_id)
        .await
        .expect("second read_note doit réussir");

    let hits_after = vault.cache_hits();

    assert_eq!(
        hits_after - hits_before,
        1,
        "le second appel doit incrémenter cache_hits de 1 (cache hit path)"
    );
}
