//! Tests CRUD sur la table `notes` via `SqliteIndex`.

mod common;
use common::make_note;

use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

#[tokio::test]
async fn upsert_then_get_content_hash() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note("main", Section::Decisions, NoteStatus::Live, "contenu test");

    idx.upsert_note(&note).await.unwrap();

    let stored = idx.get_content_hash(note.id).await.unwrap();
    assert_eq!(
        stored,
        Some(note.content_hash),
        "le content_hash stocké doit être identique à celui de la note insérée"
    );
}

#[tokio::test]
async fn get_content_hash_missing_returns_none() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let id = gradatum_core::identity::NoteId::new();

    let result = idx.get_content_hash(id).await.unwrap();
    assert!(result.is_none(), "note absente doit retourner None");
}

#[tokio::test]
async fn upsert_update_content_hash() {
    use chrono::Utc;
    use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
    use gradatum_core::identity::{ContentHash, NoteVersion};
    use gradatum_core::note::{Note, NoteBody};

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note("main", Section::Debug, NoteStatus::Draft, "version 1");
    idx.upsert_note(&note).await.unwrap();

    // Construit une version mise à jour avec le même NoteId mais body différent
    let new_body = "version 2 modifiée";
    let updated_fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Debug,
        status: NoteStatus::PendingReview,
        status_reason: None,
        status_changed: None,
        tags: Default::default(),
        author: None,
        created: note.frontmatter.created,
        updated: Some(Utc::now()),
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let updated_hash = ContentHash::compute(&updated_fm, new_body);
    let updated = Note {
        id: note.id, // même NoteId → upsert
        frontmatter: updated_fm,
        body: NoteBody {
            markdown: new_body.to_string(),
        },
        version: NoteVersion::initial().next(),
        content_hash: updated_hash,
        integrity_signature: None,
    };
    idx.upsert_note(&updated).await.unwrap();

    let stored = idx.get_content_hash(note.id).await.unwrap().unwrap();
    assert_eq!(
        stored, updated_hash,
        "après upsert, le content_hash doit être mis à jour"
    );
}

#[tokio::test]
async fn list_by_status_filters_correctly() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("test-vault");

    let note_live = make_note("test-vault", Section::Reasoning, NoteStatus::Live, "live");
    let note_draft = make_note("test-vault", Section::Reasoning, NoteStatus::Draft, "draft");
    let note_live2 = make_note(
        "test-vault",
        Section::Reasoning,
        NoteStatus::Live,
        "live aussi",
    );

    idx.upsert_note(&note_live).await.unwrap();
    idx.upsert_note(&note_draft).await.unwrap();
    idx.upsert_note(&note_live2).await.unwrap();

    let live_ids = idx.list_by_status(&vault, NoteStatus::Live).await.unwrap();
    assert_eq!(live_ids.len(), 2, "doit retourner exactement 2 notes Live");
    assert!(
        live_ids.contains(&note_live.id),
        "note_live doit être dans les résultats"
    );
    assert!(
        live_ids.contains(&note_live2.id),
        "note_live2 doit être dans les résultats"
    );
    assert!(
        !live_ids.contains(&note_draft.id),
        "note_draft ne doit PAS être dans les résultats Live"
    );

    let draft_ids = idx.list_by_status(&vault, NoteStatus::Draft).await.unwrap();
    assert_eq!(draft_ids.len(), 1);
    assert_eq!(draft_ids[0], note_draft.id);
}

#[tokio::test]
async fn list_by_status_other_vault_excluded() {
    // Notes d'un autre vault ne doivent pas apparaître
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note_main = make_note("main", Section::Decisions, NoteStatus::Live, "main vault");
    let note_other = make_note("other", Section::Decisions, NoteStatus::Live, "other vault");
    idx.upsert_note(&note_main).await.unwrap();
    idx.upsert_note(&note_other).await.unwrap();

    let main_results = idx
        .list_by_status(&VaultId::new("main"), NoteStatus::Live)
        .await
        .unwrap();
    assert_eq!(main_results.len(), 1);
    assert_eq!(main_results[0], note_main.id);
}
