//! Tests `SqliteIndex::insert_note_embedding` / `get_note_embedding`.
//!
//! Couvre :
//! - persistance du vecteur (insert + re-lecture via get_note_embedding)
//! - idempotence UPSERT (deuxième insert sur même clé → remplace)
//! - rejet dim mismatch (invariant sécurité runtime)
//!
//! Note : `note_embeddings.note_id` est une FK `REFERENCES notes(id) ON DELETE CASCADE`.
//! Chaque test doit donc insérer une note via `upsert_note` avant d'insérer l'embedding.

mod common;
use common::make_note;

use gradatum_core::identity::NoteId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
// Nécessaire pour résoudre insert_note_embedding/get_note_embedding sur SqliteIndex (Étape 0.1).
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

#[tokio::test]
async fn insert_note_embedding_persists_vector() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Insérer la note parente pour satisfaire la FK.
    let note = make_note("main", Section::Decisions, NoteStatus::Live, "test embed");
    idx.upsert_note(&note).await.expect("upsert_note ok");
    let note_id = note.id;

    let vec_in: Vec<f32> = (0..384).map(|i| i as f32 * 0.001).collect();

    idx.insert_note_embedding(&note_id, "bge-small-en-v1.5", 384, &vec_in)
        .await
        .expect("insert ok");

    let vec_out = idx
        .get_note_embedding(&note_id, "bge-small-en-v1.5")
        .await
        .expect("get ok")
        .expect("vecteur doit être présent après insert");

    assert_eq!(vec_out.len(), 384, "doit contenir exactement 384 f32");
    assert!(
        (vec_out[0] - vec_in[0]).abs() < 1e-6,
        "premier élément incorrect : {} vs {}",
        vec_out[0],
        vec_in[0]
    );
    assert!(
        (vec_out[383] - vec_in[383]).abs() < 1e-6,
        "dernier élément incorrect : {} vs {}",
        vec_out[383],
        vec_in[383]
    );
}

#[tokio::test]
async fn insert_note_embedding_replaces_on_conflict() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Insérer la note parente pour satisfaire la FK.
    let note = make_note("main", Section::Decisions, NoteStatus::Live, "test upsert");
    idx.upsert_note(&note).await.expect("upsert_note ok");
    let note_id = note.id;
    let embedder_id = "bge-small-en-v1.5";

    let vec1 = vec![0.1f32; 384];
    let vec2 = vec![0.5f32; 384];

    idx.insert_note_embedding(&note_id, embedder_id, 384, &vec1)
        .await
        .expect("insert 1 ok");

    idx.insert_note_embedding(&note_id, embedder_id, 384, &vec2)
        .await
        .expect("insert 2 (upsert) ok");

    let vec_out = idx
        .get_note_embedding(&note_id, embedder_id)
        .await
        .expect("get ok")
        .expect("vecteur doit être présent après upsert");

    assert_eq!(vec_out.len(), 384);
    // Le vecteur stocké doit être vec2 (0.5), pas vec1 (0.1).
    assert!(
        (vec_out[0] - 0.5f32).abs() < 1e-6,
        "après upsert, le vecteur doit être vec2 (0.5), trouvé {}",
        vec_out[0]
    );
    // Vérifier qu'un embedder différent retourne None (isolation entre embedders).
    let other = idx
        .get_note_embedding(&note_id, "autre-embedder")
        .await
        .expect("get ok");
    assert!(
        other.is_none(),
        "un embedder_id différent ne doit pas retourner de résultat"
    );
}

#[tokio::test]
async fn insert_note_embedding_rejects_dim_mismatch() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    // Pas besoin de note parente : l'erreur est retournée avant la requête SQL.
    let note_id = NoteId::new();

    // 100 éléments mais on annonce dim=384 → doit retourner Err.
    let vec = vec![0.1f32; 100];
    let res = idx
        .insert_note_embedding(&note_id, "bge-small-en-v1.5", 384, &vec)
        .await;

    assert!(
        res.is_err(),
        "insert_note_embedding doit rejeter vector.len()=100 != dim=384"
    );
    let err_msg = res.unwrap_err().to_string();
    assert!(
        err_msg.contains("100") && err_msg.contains("384"),
        "le message d'erreur doit mentionner les deux tailles, trouvé : {err_msg:?}"
    );
}
