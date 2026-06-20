//! Tests TDD — carve 3 traits (DocumentStore/IndexStore/VectorStore).
//!
//! Chaque test vérifie qu'un `Arc<dyn Trait>` peut être créé depuis `Arc<SqliteIndex>`
//! et qu'un round-trip basique fonctionne. Ces tests compilent et passent IFF :
//! - le trait est défini dans `gradatum-core`,
//! - `SqliteIndex` l'implémente dans `gradatum-index`.
//!
//! Les golden tests (`pipeline_golden.rs`) restent la preuve de non-régression
//! comportementale. Ces tests vérifient l'object-safety et le câblage des impls.

use std::sync::Arc;

use gradatum_core::{
    DocumentStore, IndexStore, VectorStore,
    identity::NoteId,
    scope::{OverrideScope, VaultId},
    status::NoteStatus,
};
use gradatum_index::SqliteIndex;

// ── Task 1 : DocumentStore ────────────────────────────────────────────────────

/// Vérifie que `SqliteIndex` est object-safe et utilisable via `Arc<dyn DocumentStore>`.
/// Round-trip : seed → write_note (via upsert inhérent) → get_note → Some.
#[tokio::test]
async fn sqlite_index_is_document_store_via_dyn() {
    let idx = Arc::new(SqliteIndex::open_in_memory().await.unwrap());

    // Vérification de l'object-safety : coerce vers Arc<dyn DocumentStore>
    let store: Arc<dyn DocumentStore> = idx.clone();

    // ID ULID fixe déterministe pour le test
    let note_id_str = "01HZZZZZZZZZZZZZZZZZZZZZZA";

    // Seed via méthode concrète de SqliteIndex (populate la BDD)
    idx.seed_note(note_id_str, "decisions", "test document store content")
        .await
        .unwrap();

    // get_note via le trait dyn (test du câblage)
    let record = store
        .get_note("main", note_id_str)
        .await
        .expect("get_note via dyn DocumentStore ne doit pas échouer");

    assert!(
        record.is_some(),
        "get_note via dyn DocumentStore doit retourner Some pour une note seedée"
    );
    let record = record.unwrap();
    assert_eq!(record.id, note_id_str);
    assert_eq!(record.section, "decisions");

    // get_content_hash via le trait dyn — seed_note stocke X'00' (1 byte stub),
    // get_content_hash retourne donc Err(Storage("content_hash trop court")).
    // On teste que la méthode est câblée (pas de panic, pas de "method not found").
    let ulid = note_id_str.parse::<ulid::Ulid>().unwrap();
    let hash_result = store.get_content_hash(NoteId(ulid)).await;
    // Résultat acceptable : Ok(Some(_)) avec 32 bytes OU Err(Storage) si stub 1-byte.
    // Les deux prouvent que la méthode de trait est résolue et dispatche correctement.
    assert!(
        hash_result.is_ok()
            || matches!(
                &hash_result,
                Err(gradatum_core::error::GradatumError::Storage(_))
            ),
        "get_content_hash via dyn DocumentStore doit retourner Ok ou Storage (pas de panic)"
    );

    // list_by_status via le trait dyn
    let vault = VaultId::new("main");
    let ids = store
        .list_by_status(&vault, NoteStatus::Live)
        .await
        .expect("list_by_status via dyn DocumentStore ne doit pas échouer");
    assert!(
        ids.iter().any(|id| id.to_string() == note_id_str),
        "list_by_status doit contenir la note seedée"
    );
}

// ── Task 2 : VectorStore ──────────────────────────────────────────────────────

/// Vérifie que `SqliteIndex` est object-safe et utilisable via `Arc<dyn VectorStore>`.
/// Round-trip : insert_note_embedding → get_note_embedding → Some + search_semantic.
#[tokio::test]
async fn sqlite_index_is_vector_store_via_dyn() {
    let idx = Arc::new(SqliteIndex::open_in_memory().await.unwrap());

    // Object-safety : coerce vers Arc<dyn VectorStore>
    let vstore: Arc<dyn VectorStore> = idx.clone();

    let note_id_str = "01HZZZZZZZZZZZZZZZZZZZZZZA";
    let ulid = note_id_str.parse::<ulid::Ulid>().unwrap();
    let note_id = NoteId(ulid);

    // Seed la note dans notes (nécessaire pour le JOIN dans search_semantic)
    idx.seed_note(note_id_str, "reference", "vector store test")
        .await
        .unwrap();

    // insert_note_embedding via dyn VectorStore
    let dim: u16 = 3;
    vstore
        .insert_note_embedding(&note_id, "test-embedder", dim, &[1.0f32, 0.0, 0.0])
        .await
        .expect("insert_note_embedding via dyn VectorStore ne doit pas échouer");

    // get_note_embedding via dyn VectorStore
    let emb = vstore
        .get_note_embedding(&note_id, "test-embedder")
        .await
        .expect("get_note_embedding via dyn VectorStore ne doit pas échouer");
    assert!(
        emb.is_some(),
        "get_note_embedding doit retourner Some après insert"
    );
    let vec = emb.unwrap();
    assert_eq!(vec.len(), 3);
    assert!(
        (vec[0] - 1.0f32).abs() < 1e-6,
        "composante [0] doit être 1.0"
    );

    // search_semantic via dyn VectorStore
    let results = vstore
        .search_semantic("main", "test-embedder", &[1.0f32, 0.0, 0.0], 5, None)
        .await
        .expect("search_semantic via dyn VectorStore ne doit pas échouer");
    assert_eq!(results.len(), 1, "1 note avec embedding attendu");
    assert_eq!(results[0].0, note_id);
    // cosine([1,0,0], [1,0,0]) = 1.0
    assert!((results[0].1 - 1.0f32).abs() < 1e-5, "cosine attendu ≈ 1.0");
}

// ── Task 3 : IndexStore ───────────────────────────────────────────────────────

/// Vérifie que `SqliteIndex` est object-safe et utilisable via `Arc<dyn IndexStore>`.
/// Round-trip : search_fts_scored + get_note_created_and_indegree.
#[tokio::test]
async fn sqlite_index_is_index_store_via_dyn() {
    let idx = Arc::new(SqliteIndex::open_in_memory().await.unwrap());

    // Object-safety : coerce vers Arc<dyn IndexStore>
    let istore: Arc<dyn IndexStore> = idx.clone();

    let note_id_str = "01HZZZZZZZZZZZZZZZZZZZZZZA";
    let vault = VaultId::new("main");

    // Seed avec FTS pour search_fts_scored
    idx.seed_note_with_fts(
        note_id_str,
        "reference",
        "index store FTS test content gradatum",
    )
    .await
    .unwrap();

    // search_fts via dyn IndexStore
    let ids = istore
        .search_fts(&vault, "gradatum", 10)
        .await
        .expect("search_fts via dyn IndexStore ne doit pas échouer");
    assert!(
        ids.iter().any(|id| id.to_string() == note_id_str),
        "search_fts doit trouver la note seedée"
    );

    // search_fts_scored via dyn IndexStore
    let scored = istore
        .search_fts_scored(&vault, "gradatum", 10, false)
        .await
        .expect("search_fts_scored via dyn IndexStore ne doit pas échouer");
    assert!(
        scored
            .iter()
            .any(|(id, _, _)| id.to_string() == note_id_str),
        "search_fts_scored doit trouver la note seedée"
    );

    // get_note_created_and_indegree via dyn IndexStore
    let (created_ms, in_degree) = istore
        .get_note_created_and_indegree("main", note_id_str)
        .await
        .expect("get_note_created_and_indegree via dyn IndexStore ne doit pas échouer");
    assert!(created_ms > 0, "created_ms doit être positif");
    assert_eq!(in_degree, 0, "aucun backlink attendu pour une note isolée");

    // upsert_override_raw / get_override_raw via dyn IndexStore
    let note_ulid = note_id_str.parse::<ulid::Ulid>().unwrap();
    let note_id = NoteId(note_ulid);
    let scope = OverrideScope::Vault(vault.clone());

    istore
        .upsert_override_raw(note_id, &scope, "test-override", 1, "key = \"value\"")
        .await
        .expect("upsert_override_raw via dyn IndexStore ne doit pas échouer");

    let raw = istore
        .get_override_raw(note_id, &scope, "test-override")
        .await
        .expect("get_override_raw via dyn IndexStore ne doit pas échouer");
    assert!(raw.is_some(), "override doit être présent après upsert");
    let (sv, payload) = raw.unwrap();
    assert_eq!(sv, 1);
    assert!(payload.contains("value"));
}
