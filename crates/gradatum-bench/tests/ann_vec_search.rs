//! Test de recherche vectorielle sqlite-vec (F-145 — montée rusqlite).
//!
//! Après la montée rusqlite (0.32.1 → 0.40.2, moteur SQLite 3.46.0 → 3.53.2), la carte
//! F-145 exige qu'un test de recherche vectorielle passe : l'extension `vec0` (static
//! lib `sqlite_vec0`) est liée dans le binaire et enregistrée via
//! `sqlite3_auto_extension`. Ce test enregistre l'extension PUIS ouvre l'index — c'est
//! le même ordre que `ann_recall` (bin) et `gradatum-server/src/vec_ext.rs`.
//!
//! Ce test est un test CI normal (pas `#[ignore]`) : `gradatum-bench` est la seule
//! crate du workspace sans `#![forbid(unsafe_code)]` où l'enregistrement unsafe peut
//! vivre dans un test ciblé.

use rusqlite::ffi::sqlite3_auto_extension;
use sqlite_vec::sqlite3_vec_init;

use gradatum_core::VectorStore;
use gradatum_core::identity::NoteId;
use gradatum_core::scope::{AclCheckedVaultId, VaultId};
use gradatum_index::SqliteIndex;

/// Enregistre sqlite-vec avant toute ouverture de connexion SQLite.
///
/// # Safety
///
/// `sqlite3_vec_init` a la signature `sqlite3_auto_extension` standard
/// `(sqlite3*, char**, sqlite3_api_routines*) -> int` — identique au type de pointeur
/// attendu par `sqlite3_auto_extension`. Le `transmute` convertit la déclaration C
/// conservative `extern "C" fn()` vers le type attendu ; l'ABI est identique. Même
/// justification que `gradatum-bench/src/bin/ann_recall.rs`.
fn register_sqlite_vec() {
    // SAFETY: voir doc ci-dessus — ABI identique, dédupliqué par SQLite par adresse.
    #[expect(
        clippy::missing_transmute_annotations,
        reason = "type cible dépend de l'ABI rusqlite/libsqlite3-sys interne — annoter \
                  introduirait une dépendance fragile sur les types internes (même choix \
                  que gradatum-bench/src/bin/ann_recall.rs)"
    )]
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
    }
}

/// Vec unité le long de l'axe `i` (requête exacte → cosinus ≈ 1.0).
fn axis_vec(dim: usize, i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    v[i % dim] = 1.0;
    v
}

#[tokio::test]
async fn vec_search_returns_exact_match_with_real_sqlite_vec() {
    register_sqlite_vec();

    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.set_ann_enabled(true);

    let dim = 1024usize;
    let note_id_str = "01HB0000000000000000000007";
    let ulid = ulid::Ulid::from_string(note_id_str).expect("ULID fixe — invariant test");
    let note_id = NoteId(ulid);

    idx.seed_note(note_id_str, "decisions", "corps de test")
        .await
        .expect("seed_note");
    idx.insert_note_embedding("main", &note_id, "bge-m3", dim as u16, &axis_vec(dim, 0))
        .await
        .expect("insert_note_embedding");

    let count = idx.backfill_ann_index().await.expect("backfill_ann_index");
    assert_eq!(count, 1, "1 note backfillée dans l'index ANN");

    let query = axis_vec(dim, 0);
    let results = idx
        .search_semantic(
            &AclCheckedVaultId::for_system_task(VaultId::new("main")),
            "bge-m3",
            &query,
            1,
            None,
        )
        .await
        .expect("search_semantic ANN");

    assert_eq!(results.len(), 1, "ANN doit retourner 1 résultat");
    assert!(
        results[0].1 > 0.99,
        "cosine ANN ≈ 1.0, trouvé {}",
        results[0].1
    );
}
