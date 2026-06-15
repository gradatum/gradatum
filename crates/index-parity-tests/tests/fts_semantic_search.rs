//! Parité backend : recherche FTS5 + recherche sémantique (cosine).
//!
//! Invariants :
//! - `search_fts` trouve le token présent dans le corps, filtre par vault_id.
//! - `insert_note_embedding` → `search_semantic` retourne les notes triées par
//!   similarité cosine décroissante, exclut les `downgraded`.
//!
//! Note RRF : la fusion `Reciprocal Rank Fusion` (FTS ⊕ sémantique) vit dans la
//! couche `gradatum-search`/`gradatum-server`, pas sur le trait `Index`. Elle est
//! couverte par `gradatum-server/tests/vault_search_rrf_path.rs` (suite serveur).
//! La parité d'index garantit les deux entrées (ordres FTS et sémantique) que RRF
//! consomme.

mod common;

use common::{make_index, make_note_with_id, minimal_frontmatter};
use gradatum_core::identity::NoteId;
use gradatum_core::scope::VaultId;

#[tokio::test]
async fn fts_finds_token_in_body() {
    let idx = make_index().await;
    let vault = VaultId::new("main");

    let n1 = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "Le protocole zorglub gère la réplication.",
    );
    let n2 = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "Une note sans rapport sur les chats.",
    );
    idx.write_note(&n1).await.expect("write n1");
    idx.write_note(&n2).await.expect("write n2");

    let hits = idx
        .search_fts(&vault, "zorglub", 10)
        .await
        .expect("search_fts");

    assert_eq!(
        hits.len(),
        1,
        "1 seule note matche ({})",
        common::backend_label()
    );
    assert_eq!(hits[0], n1.id, "la bonne note remonte");
}

#[tokio::test]
async fn fts_filters_by_vault() {
    let idx = make_index().await;

    let main_note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("main"),
        "token partagé dans le vault main",
    );
    let other_note = make_note_with_id(
        NoteId::new(),
        minimal_frontmatter("other"),
        "token partagé dans le vault other",
    );
    idx.write_note(&main_note).await.expect("write main");
    idx.write_note(&other_note).await.expect("write other");

    let hits = idx
        .search_fts(&VaultId::new("main"), "partagé", 10)
        .await
        .expect("search_fts");

    assert!(hits.contains(&main_note.id), "note du vault main remonte");
    assert!(
        !hits.contains(&other_note.id),
        "note du vault other exclue ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn semantic_ranks_by_cosine_descending() {
    let idx = make_index().await;
    let embedder_id = "test-embedder";
    let dim: u16 = 4;

    // 3 notes avec des vecteurs orientés différemment.
    let near = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "proche");
    let mid = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "moyen");
    let far = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "loin");
    idx.write_note(&near).await.expect("write near");
    idx.write_note(&mid).await.expect("write mid");
    idx.write_note(&far).await.expect("write far");

    // query = [1,0,0,0]. near aligné, mid à 45°, far orthogonal.
    idx.insert_note_embedding(&near.id, embedder_id, dim, &[1.0, 0.0, 0.0, 0.0])
        .await
        .expect("emb near");
    idx.insert_note_embedding(&mid.id, embedder_id, dim, &[1.0, 1.0, 0.0, 0.0])
        .await
        .expect("emb mid");
    idx.insert_note_embedding(&far.id, embedder_id, dim, &[0.0, 0.0, 1.0, 0.0])
        .await
        .expect("emb far");

    let results = idx
        .search_semantic("main", embedder_id, &[1.0, 0.0, 0.0, 0.0], 10, None)
        .await
        .expect("search_semantic");

    assert!(results.len() >= 2, "au moins near + mid remontent");
    // Ordre décroissant strict sur le score.
    for w in results.windows(2) {
        assert!(
            w[0].1 >= w[1].1,
            "tri cosine décroissant ({}) : {:?}",
            common::backend_label(),
            results
        );
    }
    assert_eq!(results[0].0, near.id, "near (cosine=1.0) en tête");
}

#[tokio::test]
async fn semantic_roundtrips_embedding() {
    let idx = make_index().await;
    let note = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "emb roundtrip");
    idx.write_note(&note).await.expect("write");

    let vec = vec![0.1_f32, 0.2, 0.3, 0.4];
    idx.insert_note_embedding(&note.id, "emb", 4, &vec)
        .await
        .expect("insert emb");

    let got = idx
        .get_note_embedding(&note.id, "emb")
        .await
        .expect("get emb")
        .expect("emb présent");
    assert_eq!(
        got,
        vec,
        "embedding relu à l'identique ({})",
        common::backend_label()
    );
}

#[tokio::test]
async fn semantic_null_query_returns_empty() {
    let idx = make_index().await;
    let note = make_note_with_id(NoteId::new(), minimal_frontmatter("main"), "x");
    idx.write_note(&note).await.expect("write");
    idx.insert_note_embedding(&note.id, "emb", 4, &[1.0, 0.0, 0.0, 0.0])
        .await
        .expect("insert emb");

    let results = idx
        .search_semantic("main", "emb", &[0.0, 0.0, 0.0, 0.0], 10, None)
        .await
        .expect("search_semantic vecteur nul");
    assert!(
        results.is_empty(),
        "vecteur nul → vec vide ({})",
        common::backend_label()
    );
}
