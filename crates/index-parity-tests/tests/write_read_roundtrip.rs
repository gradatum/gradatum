//! Parité backend : write → read round-trip + content hash.
//!
//! Invariant : une note écrite via `DocumentStore::write_note` est relue à
//! l'identique via `get_note` / `get_content_hash` (id, section, statut, corps,
//! hash). Backend-agnostique : aucune dépendance au type concret.

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;

#[tokio::test]
async fn write_then_read_returns_same_note() {
    let idx = make_index().await;
    let id = NoteId::new();
    let fm = minimal_frontmatter("main");
    let note = make_note_with_id(id, fm, "# Titre\n\nCorps de la note round-trip.");

    idx.write_note(&note).await.expect("write_note");

    let record = idx
        .get_note("main", &id.to_string())
        .await
        .expect("get_note")
        .expect("note présente après write");

    assert_eq!(
        record.id,
        id.to_string(),
        "id préservé ({})",
        common::backend_label()
    );
    assert_eq!(record.vault_id, "main");
    assert_eq!(record.section, Section::Decisions.as_str());
    assert_eq!(record.status, NoteStatus::Live.to_string());
    assert!(record.body_text.contains("round-trip"), "corps préservé");
}

#[tokio::test]
async fn content_hash_roundtrips() {
    let idx = make_index().await;
    let id = NoteId::new();
    let fm = minimal_frontmatter("main");
    let note = make_note_with_id(id, fm, "Corps pour content hash.");
    let expected = note.content_hash;

    idx.write_note(&note).await.expect("write_note");

    let stored = idx
        .get_content_hash("main", id)
        .await
        .expect("get_content_hash")
        .expect("hash présent");

    assert_eq!(
        stored,
        expected,
        "content hash préservé ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn get_note_absent_returns_none() {
    let idx = make_index().await;
    let absent = NoteId::new();

    let record = idx
        .get_note("main", &absent.to_string())
        .await
        .expect("get_note ne doit pas erreur sur absent");
    assert!(
        record.is_none(),
        "note absente → None ({})",
        common::backend_label()
    );

    let hash = idx
        .get_content_hash("main", absent)
        .await
        .expect("get_content_hash absent");
    assert!(hash.is_none(), "hash absent → None");
}

#[tokio::test]
async fn list_by_status_filters() {
    use gradatum_core::scope::VaultId;
    let idx = make_index().await;
    let vault = VaultId::new("main");

    let live = make_note_with_id(
        NoteId::new(),
        common::frontmatter_with("main", Section::Decisions, NoteStatus::Live),
        "note live",
    );
    let staging = make_note_with_id(
        NoteId::new(),
        common::frontmatter_with("main", Section::Decisions, NoteStatus::Staging),
        "note staging",
    );
    idx.write_note(&live).await.expect("write live");
    idx.write_note(&staging).await.expect("write staging");

    let live_ids = idx
        .list_by_status(&vault, NoteStatus::Live)
        .await
        .expect("list live");
    let staging_ids = idx
        .list_by_status(&vault, NoteStatus::Staging)
        .await
        .expect("list staging");

    assert!(live_ids.contains(&live.id), "live listée");
    assert!(
        !live_ids.contains(&staging.id),
        "staging exclue du filtre live"
    );
    assert!(staging_ids.contains(&staging.id), "staging listée");
}
