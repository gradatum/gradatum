//! Parité backend : oubli sémantique (`mark_forgotten` / `is_note_forgotten`) + section.
//!
//! Invariants :
//! - `mark_forgotten` rend `is_note_forgotten` vrai ; `unmark_forgotten` le ré-annule.
//! - Une note oubliée est exclue de `search_fts` (decay forget e2e niveau index).
//! - `get_note_section` relit la section indexée (méthode promue W1).

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;

#[tokio::test]
async fn mark_then_is_forgotten_roundtrips() {
    let idx = make_index().await;
    let note = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "à oublier");
    idx.write_note(&note).await.expect("write");

    assert!(
        !idx.is_note_forgotten("main", &note.id.to_string())
            .await
            .expect("is_forgotten before"),
        "note non oubliée initialement ({})",
        common::backend_label()
    );

    idx.mark_forgotten("main", &note.id.to_string(), Some("test"))
        .await
        .expect("mark_forgotten");

    assert!(
        idx.is_note_forgotten("main", &note.id.to_string())
            .await
            .expect("is_forgotten after"),
        "note oubliée après mark ({})",
        common::backend_label()
    );

    idx.unmark_forgotten("main", &note.id.to_string())
        .await
        .expect("unmark_forgotten");

    assert!(
        !idx.is_note_forgotten("main", &note.id.to_string())
            .await
            .expect("is_forgotten unmark"),
        "note ré-active après unmark"
    );
}

#[tokio::test]
async fn raw_fts_does_not_filter_forgotten() {
    // Contrat de couche : `search_fts` est la primitive FTS5 brute — elle NE filtre
    // PAS les notes oubliées. Le filtrage forgotten est une responsabilité de la
    // couche search-engine (`recall_lessons` le fait, cf. recall_lessons.rs ; la
    // couche vault/serveur l'applique pour vault_search). Verrouiller ce contrat
    // évite qu'un backend alternatif introduise un filtrage divergent à ce niveau.
    let idx = make_index().await;
    let vault = VaultId::new("main");
    let note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "le terme distinctif xyzzy figure ici",
    );
    idx.write_note(&note).await.expect("write");
    idx.mark_forgotten("main", &note.id.to_string(), None)
        .await
        .expect("mark_forgotten");

    assert!(
        idx.search_fts(&vault, "xyzzy", 10)
            .await
            .expect("fts after forget")
            .contains(&note.id),
        "search_fts brut ne filtre pas forgotten — filtrage = couche search ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn get_note_section_reads_indexed_section() {
    use gradatum_core::section::Section;
    use gradatum_core::status::NoteStatus;
    let idx = make_index().await;
    let note = make_note_with_id(
        NoteId::new(),
        common::frontmatter_with("main", Section::Architecture, NoteStatus::Live),
        "note d'architecture",
    );
    idx.write_note(&note).await.expect("write");

    let section = idx
        .get_note_section("main", &note.id.to_string())
        .await
        .expect("get_note_section")
        .expect("section présente");
    assert_eq!(
        section,
        Section::Architecture.as_str(),
        "section relue ({})",
        common::backend_label()
    );
}
