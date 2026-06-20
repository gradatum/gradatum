//! v1-parity : Index FTS5 + list_by_status — 3 tests.
//!
//! Parité avec `legacy-vault-v1/tests/integration/test_semantic.rs`.
//! Domaine : FTS5 search token, filter vault_id, list_by_status filter.

mod common;

use gradatum_core::identity::{ContentHash, NoteId, NoteVersion};
use gradatum_core::note::{Note, NoteBody};
use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

// --- Helper ---

async fn make_index() -> SqliteIndex {
    SqliteIndex::open_in_memory()
        .await
        .expect("SqliteIndex::open_in_memory")
}

fn make_note_with_body(vault_id: &str, body: &str) -> Note {
    let fm = common::minimal_frontmatter(vault_id);
    let hash = ContentHash::compute(&fm, body);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

fn make_note_with_status(vault_id: &str, status: NoteStatus) -> Note {
    let fm = common::frontmatter_with_status(vault_id, Section::Decisions, status);
    let body = "Corps pour test list_by_status.";
    let hash = ContentHash::compute(&fm, body);
    Note {
        id: NoteId::new(),
        frontmatter: fm,
        body: NoteBody {
            markdown: body.into(),
        },
        version: NoteVersion::initial(),
        content_hash: hash,
        integrity_signature: None,
    }
}

// --- 1. fts5_search_finds_token_in_body ---

/// Indexe 3 notes avec des corps différents, recherche un token présent dans
/// une seule note → retourne uniquement le NoteId correspondant.
#[tokio::test]
async fn fts5_search_finds_token_in_body() {
    let index = make_index().await;
    let vault_id = VaultId::new("main");

    let note_target = make_note_with_body(
        "main",
        "Ce texte contient le mot xylophone comme token rare.",
    );
    let note_other1 = make_note_with_body("main", "Note ordinaire sans le token recherché.");
    let note_other2 = make_note_with_body("main", "Autre note sans rapport avec la recherche.");

    let target_id = note_target.id;
    index
        .upsert_note(&note_target)
        .await
        .expect("upsert target");
    index
        .upsert_note(&note_other1)
        .await
        .expect("upsert other1");
    index
        .upsert_note(&note_other2)
        .await
        .expect("upsert other2");

    let results = index
        .search_fts(&vault_id, "xylophone", 10)
        .await
        .expect("search_fts");

    assert!(
        results.contains(&target_id),
        "La note avec 'xylophone' doit être dans les résultats"
    );
    // Les autres notes ne doivent pas être présentes
    assert_eq!(
        results.len(),
        1,
        "Seule la note avec 'xylophone' doit matcher"
    );
}

// --- 2. fts5_search_filters_by_vault_id ---

/// Indexe notes dans 2 vaults, recherche un token commun → filtre par vault_id.
#[tokio::test]
async fn fts5_search_filters_by_vault_id() {
    let index = make_index().await;

    // vault_a et vault_b ont chacun une note avec "gradatum"
    let note_a = make_note_with_body("vault-a", "gradatum est le nom du projet.");
    let note_b = make_note_with_body("vault-b", "gradatum est aussi dans vault-b.");

    let id_a = note_a.id;
    let id_b = note_b.id;

    index.upsert_note(&note_a).await.expect("upsert vault-a");
    index.upsert_note(&note_b).await.expect("upsert vault-b");

    let vault_a = VaultId::new("vault-a");
    let results_a = index
        .search_fts(&vault_a, "gradatum", 10)
        .await
        .expect("search vault-a");

    // vault-a ne doit retourner que la note de vault-a
    assert!(
        results_a.contains(&id_a),
        "Note vault-a doit être dans les résultats"
    );
    assert!(
        !results_a.contains(&id_b),
        "Note vault-b ne doit PAS apparaître dans les résultats vault-a"
    );
}

// --- 3. list_by_status_returns_only_matching ---

/// Indexe 3 notes avec 3 statuts différents, liste par statut Live → seule la note
/// Live est retournée.
#[tokio::test]
async fn list_by_status_returns_only_matching() {
    let index = make_index().await;
    let vault_id = VaultId::new("main");

    let note_live = make_note_with_status("main", NoteStatus::Live);
    let note_draft = make_note_with_status("main", NoteStatus::Draft);
    let note_pending = make_note_with_status("main", NoteStatus::PendingReview);

    let live_id = note_live.id;

    index.upsert_note(&note_live).await.expect("upsert Live");
    index.upsert_note(&note_draft).await.expect("upsert Draft");
    index
        .upsert_note(&note_pending)
        .await
        .expect("upsert PendingReview");

    let results = index
        .list_by_status(&vault_id, NoteStatus::Live)
        .await
        .expect("list_by_status Live");

    assert_eq!(results.len(), 1, "Seule la note Live doit être retournée");
    assert!(
        results.contains(&live_id),
        "Le NoteId de la note Live doit être dans les résultats"
    );
}
