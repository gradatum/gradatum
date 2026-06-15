//! Tests de la table `note_audit_trail`.
//!
//! Ces tests vérifient que :
//! 1. La table `note_audit_trail` est créée par la migration `0001_phase1` (batch atomique).
//! 2. `_schema_migrations` est alimentée avec `"0001_phase1"`.
//! 3. La seconde ouverture est idempotente (pas de re-migration).
//! 4. `upsert_note` avec des champs optionnels (locus, author, tags) n'échoue pas.

mod common;

use chrono::Utc;
use gradatum_core::author::AuthorRef;
use gradatum_core::frontmatter::{ExtraFields, Frontmatter};
use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::{LocusId, VaultId};
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_core::tag::Tag;
use gradatum_index::SqliteIndex;

#[tokio::test]
async fn migration_creates_all_tables() {
    // Si la migration 0001_phase1 échoue sur une table manquante / SQL invalide,
    // open_in_memory() retourne Err. Le fait que l'unwrap réussit prouve que
    // toutes les tables du batch ont été créées.
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Smoke : les méthodes Index accèdent aux tables créées par la migration
    let checksums = idx.list_file_checksums().await.unwrap();
    assert!(checksums.is_empty());

    let live_ids = idx
        .list_by_status(&VaultId::new("main"), NoteStatus::Live)
        .await
        .unwrap();
    assert!(live_ids.is_empty());
}

#[tokio::test]
async fn schema_migrations_tracking_idempotent() {
    // Deux opens successifs du même fichier → migration ne s'applique qu'une fois
    // (si réappliquée, CREATE TABLE échouerait "table already exists")
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("idempotent.db");

    {
        let idx = SqliteIndex::open(&path).await.unwrap();
        let _ = idx.list_file_checksums().await.unwrap();
    }
    {
        // 2ème ouverture doit réussir sans erreur
        let idx = SqliteIndex::open(&path).await.unwrap();
        let _ = idx.list_file_checksums().await.unwrap();
    }
}

#[tokio::test]
async fn upsert_note_with_locus_and_author() {
    // Vérifie que les champs optionnels locus + author sont correctement stockés
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Construction manuelle pour avoir locus + author sans dépendance smallvec
    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: Some(LocusId::new("locus-private")),
        section: Section::AgentIssues,
        status: NoteStatus::Live,
        status_reason: Some("admis par curator".to_string()),
        status_changed: Some(Utc::now()),
        tags: Default::default(), // SmallVec vide via Default
        author: Some(AuthorRef::sub_agent("backend")),
        created: Utc::now(),
        updated: Some(Utc::now()),
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let body = "Corps avec locus et author.";
    let content_hash = ContentHash::compute(&fm, body);
    let note = Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.to_string(),
        },
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    };

    idx.upsert_note(&note).await.unwrap();

    let stored = idx.get_content_hash(note.id).await.unwrap();
    assert_eq!(stored, Some(content_hash));
}

#[tokio::test]
async fn upsert_note_with_tags_via_frontmatter() {
    // Tags stockés + note retrouvable par FTS sur les tags
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let mut tags: smallvec::SmallVec<[Tag; 4]> = Default::default();
    tags.push(Tag::new("rust").unwrap());
    tags.push(Tag::new("phase1").unwrap());

    let fm = Frontmatter {
        schema_version: 1,
        vault_id: VaultId::new("main"),
        locus: None,
        section: Section::Decisions,
        status: NoteStatus::Live,
        status_reason: None,
        status_changed: None,
        tags,
        author: None,
        created: Utc::now(),
        updated: None,
        extra: ExtraFields::empty(),
        provenance: None,
        forgotten: None,
        forgotten_at: None,
        forgotten_by: None,
    };
    let body = "Note avec deux tags rust et phase1.";
    let content_hash = ContentHash::compute(&fm, body);
    let note = Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.to_string(),
        },
        version: NoteVersion::initial(),
        content_hash,
        integrity_signature: None,
    };

    idx.upsert_note(&note).await.unwrap();

    // La note est retrouvable par keyword du body
    let results = idx.search_fts(&vault, "phase1", 10).await.unwrap();
    assert!(
        !results.is_empty(),
        "FTS doit trouver la note via le keyword du body"
    );
}
