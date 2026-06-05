//! Tests FTS5 — recherche plein texte via `search_fts`.

mod common;
use common::make_note;

use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

#[tokio::test]
async fn fts5_finds_note_by_keyword() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let note_rust = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "Rust est un langage système performant et sûr",
    );
    let note_python = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "Python est un langage de scripting populaire",
    );
    let note_other = make_note(
        "main",
        Section::Debug,
        NoteStatus::Live,
        "Note sans mot-clé pertinent",
    );

    idx.upsert_note(&note_rust).await.unwrap();
    idx.upsert_note(&note_python).await.unwrap();
    idx.upsert_note(&note_other).await.unwrap();

    let results = idx.search_fts(&vault, "Rust", 10).await.unwrap();
    assert_eq!(
        results.len(),
        1,
        "doit trouver exactement 1 note avec 'Rust'"
    );
    assert_eq!(results[0], note_rust.id);
}

#[tokio::test]
async fn fts5_vault_isolation() {
    // Une note d'un vault différent ne doit pas remonter
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note_main = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "gradatum architecture vault isolation test",
    );
    let note_other = make_note(
        "other",
        Section::Decisions,
        NoteStatus::Live,
        "gradatum dans un autre vault",
    );

    idx.upsert_note(&note_main).await.unwrap();
    idx.upsert_note(&note_other).await.unwrap();

    let results = idx
        .search_fts(&VaultId::new("main"), "gradatum", 10)
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "FTS doit respecter l'isolation vault : 1 résultat dans 'main'"
    );
    assert_eq!(results[0], note_main.id);
}

#[tokio::test]
async fn fts5_empty_query_result() {
    // Requête FTS qui ne correspond à rien → résultat vide
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let note = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "contenu quelconque",
    );
    idx.upsert_note(&note).await.unwrap();

    let results = idx
        .search_fts(&VaultId::new("main"), "motcleinexistantxyz", 10)
        .await
        .unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn fts5_limit_respected() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    // Insérer 5 notes contenant le mot "commun"
    for i in 0..5 {
        let note = make_note(
            "main",
            Section::Reasoning,
            NoteStatus::Live,
            &format!("note {i} avec mot commun dedans"),
        );
        idx.upsert_note(&note).await.unwrap();
    }

    let results = idx.search_fts(&vault, "commun", 3).await.unwrap();
    assert!(
        results.len() <= 3,
        "le limit=3 doit être respecté, obtenu {}",
        results.len()
    );
}

#[tokio::test]
async fn search_fts_scored_returns_bm25_ordered() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    // Note très pertinente : "rust" apparaît plusieurs fois.
    let note_high = make_note(
        "main",
        Section::Decisions,
        NoteStatus::Live,
        "rust rust rust — langage très pertinent pour ce test",
    );
    // Note moins pertinente : "rust" n'apparaît qu'une fois.
    let note_low = make_note(
        "main",
        Section::Reasoning,
        NoteStatus::Live,
        "quelque chose à propos de rust et d'autres sujets",
    );
    // Note sans "rust" : ne doit pas apparaître.
    let note_none = make_note(
        "main",
        Section::Debug,
        NoteStatus::Live,
        "note sur Python uniquement",
    );

    idx.upsert_note(&note_high).await.unwrap();
    idx.upsert_note(&note_low).await.unwrap();
    idx.upsert_note(&note_none).await.unwrap();

    let results = idx
        .search_fts_scored(&vault, "rust", 10, false)
        .await
        .expect("search_fts_scored ne doit pas échouer sur une query valide");

    // Doit trouver au moins les 2 notes contenant "rust".
    assert!(
        results.len() >= 2,
        "expected >= 2 résultats pour 'rust', obtenu {}",
        results.len()
    );

    // BM25 : score négatif, meilleur match = plus proche de 0.
    // Ordre ASC = meilleur score en premier (le plus négatif = le plus petit).
    for w in results.windows(2) {
        assert!(
            w[0].1 <= w[1].1,
            "expected BM25 ASC ordering (meilleur en premier), obtenu {:?} -> {:?}",
            w[0].1,
            w[1].1
        );
    }

    // La note "none" (pas de "rust") ne doit pas apparaître.
    let ids: Vec<_> = results.iter().map(|(id, _, _)| *id).collect();
    assert!(
        !ids.contains(&note_none.id),
        "note sans 'rust' ne doit pas apparaître dans search_fts_scored"
    );
}
