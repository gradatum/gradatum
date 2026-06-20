//! Tests pour `SqliteIndex::id_lookup` — résolution par ULID.
//!
//! Couvre :
//! 1. Note existante et `live` → `Some(id)`
//! 2. ULID inexistant → `None`
//! 3. Note downgraded (status != live) → `None`

mod common;
use common::make_note;

use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Note existante et `live` — `id_lookup` doit retourner `Some(id)`.
#[tokio::test]
async fn id_lookup_live_note_returns_some() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "# Décision test\n\nContenu.",
    );
    let expected_id = note.id.to_string();

    idx.upsert_note(&note).await.unwrap();

    let result = idx
        .id_lookup("main", &expected_id)
        .await
        .expect("id_lookup");
    assert_eq!(
        result,
        Some(expected_id),
        "id_lookup doit retourner Some(id) pour une note live existante"
    );
}

/// ULID inexistant dans le vault — `id_lookup` doit retourner `None`.
#[tokio::test]
async fn id_lookup_nonexistent_ulid_returns_none() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // ULID valide syntaxiquement mais absent du vault
    let ghost_id = "01JZZZZZZZZZZZZZZZZZZZZZZ0";
    let result = idx.id_lookup("main", ghost_id).await.expect("id_lookup");
    assert!(
        result.is_none(),
        "id_lookup sur ULID inexistant doit retourner None"
    );
}

/// Note downgraded (status = garbage) — doit retourner `None`.
///
/// Garantit que les liens ne pointent pas vers des notes archivées/supprimées.
#[tokio::test]
async fn id_lookup_downgraded_note_returns_none() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Seed la note avec status Garbage (downgraded)
    let note = make_note(
        "main",
        Section::Debug,
        NoteStatus::Garbage,
        "# Note supprimée\n\nContenu.",
    );
    let note_id = note.id.to_string();

    idx.upsert_note(&note).await.unwrap();

    // Une note downgraded ne doit pas être résolvable par id_lookup
    let result = idx.id_lookup("main", &note_id).await.expect("id_lookup");
    assert!(
        result.is_none(),
        "id_lookup sur note non-live (Garbage) doit retourner None — pas de lien dangling"
    );
}

/// Note live dans un vault différent — ne doit PAS matcher sur l'autre vault.
#[tokio::test]
async fn id_lookup_wrong_vault_returns_none() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note = make_note(
        "vault-a",
        Section::Decisions,
        NoteStatus::Live,
        "# Note vault-a\n",
    );
    let note_id = note.id.to_string();

    idx.upsert_note(&note).await.unwrap();

    // Cherche dans vault-b — doit être absent
    let result = idx.id_lookup("vault-b", &note_id).await.expect("id_lookup");
    assert!(
        result.is_none(),
        "id_lookup ne doit pas traverser les vault boundaries"
    );
}
