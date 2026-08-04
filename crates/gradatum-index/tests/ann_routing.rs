//! Tests de routage ANN-5 : vérification du câblage ANN vs brute-force.
//!
//! ## Ce que ces tests vérifient
//!
//! - T5.1 : `ann_is_enabled()` = false par défaut → chemin brute-force.
//! - T5.2 : `set_ann_enabled(true)` + extension absente → fallback brute-force (pas de panic).
//! - T5.3 : `set_ann_enabled(false)` → résultats identiques à `ann_is_enabled()` = false initial.
//! - T5.4 : `set_ann_ef_search` + getter round-trip.
//! - T5.5 : `ann_is_enabled()` peut être lu depuis plusieurs threads (AtomicBool).
//!
//! ## Tests nécessitant l'extension sqlite-vec
//!
//! Les tests qui nécessitent l'extension réelle sont gérés par `ann_recall.rs` (bench)
//! et gatés `#[ignore = "requiert libvec0"]`. L'extension n'est pas disponible au
//! compile-time (dépendance runtime `lib:sqlite_vec0.a`) — seuls les tests sans extension
//! peuvent être exécutés dans la CI normale.

use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Seed une note avec un embedding dans le vault "main".
async fn seed_note_with_emb(idx: &SqliteIndex, note_id_str: &str, emb: &[f32]) {
    use gradatum_core::identity::NoteId;
    let ulid = ulid::Ulid::from_string(note_id_str).expect("ULID fixe — invariant test");
    let note_id = NoteId(ulid);
    idx.seed_note(note_id_str, "decisions", "corps de test")
        .await
        .expect("seed_note");
    idx.insert_note_embedding("main", &note_id, "bge-m3", emb.len() as u16, emb)
        .await
        .expect("insert_note_embedding");
}

/// Vecteur unité le long de l'axe i.
fn axis_vec(dim: usize, i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    v[i % dim] = 1.0;
    v
}

// ── T5.1 : default = brute-force ────────────────────────────────────────────

/// T5.1 — Par défaut, `ann_is_enabled()` est false.
///
/// Vérifie que `SqliteIndex::open_in_memory()` initialise le flag ANN à false
/// (comportement brute-force byte-compat avant v0.5.3).
#[tokio::test]
async fn test_ann_brute_force_default() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Vérification directe du flag.
    assert!(
        !idx.ann_is_enabled(),
        "ann_is_enabled() doit être false par défaut (brute-force)"
    );

    // Vérification fonctionnelle : search_semantic doit retourner des résultats
    // brute-force (cosine) sans erreur.
    let dim = 1024usize;
    seed_note_with_emb(&idx, "01HB0000000000000000000001", &axis_vec(dim, 0)).await;
    seed_note_with_emb(&idx, "01HB0000000000000000000002", &axis_vec(dim, 1)).await;

    let query = axis_vec(dim, 0); // cosine 1.0 avec note 1, 0.0 avec note 2
    let results = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            "bge-m3",
            &query,
            2,
            None,
        )
        .await
        .expect("search_semantic brute-force doit réussir");

    assert_eq!(results.len(), 2, "doit retourner 2 résultats");

    // Note sur l'axe 0 doit être première (cosine 1.0 vs 0.0).
    use gradatum_core::identity::NoteId;
    let note1 = NoteId(ulid::Ulid::from_string("01HB0000000000000000000001").unwrap());
    assert_eq!(
        results[0].0, note1,
        "note sur axe 0 doit être première avec brute-force"
    );
    assert!(
        (results[0].1 - 1.0).abs() < 1e-4,
        "score cosine ≈ 1.0, trouvé {}",
        results[0].1
    );
}

// ── T5.2 : fallback brute-force quand ANN activé sans extension ─────────────

/// T5.2 — Avec `ann_enabled=true` mais extension absente → fallback brute-force.
///
/// `search_ann_inner` échoue avec "no such module: vec0" (extension non chargée).
/// `vector_store_impl.rs` doit attraper l'erreur, loguer un warn, et basculer
/// sur `search_semantic_inner`. Pas de panic, résultats non-vides si des vecteurs
/// sont seedés.
///
/// IMPORTANT : Ce test vérifie que le chemin de fallback fonctionne SANS extension.
/// Il ne vérifie pas que le chemin ANN vec0 fonctionne (ça nécessite l'extension runtime).
#[tokio::test]
async fn test_ann_fallback_brute_force_when_extension_absent() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Activer ANN alors que l'extension n'est pas chargée.
    // → search_ann_inner va échouer → fallback brute-force.
    idx.set_ann_enabled(true);
    assert!(
        idx.ann_is_enabled(),
        "ann_is_enabled() doit être true après set_ann_enabled(true)"
    );

    let dim = 1024usize;
    seed_note_with_emb(&idx, "01HB0000000000000000000003", &axis_vec(dim, 0)).await;
    seed_note_with_emb(&idx, "01HB0000000000000000000004", &axis_vec(dim, 1)).await;

    let query = axis_vec(dim, 0);

    // Sans extension sqlite-vec, search_ann_inner retourne "no such module: vec0"
    // ou "no such table: note_embeddings_ann". Le fallback brute-force doit s'activer.
    // Le résultat doit être non-vide (les embeddings sont dans note_embeddings, accessible
    // sans extension).
    let results = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            "bge-m3",
            &query,
            2,
            None,
        )
        .await
        .expect("search_semantic doit réussir même avec ann_enabled=true et extension absente");

    assert!(
        !results.is_empty(),
        "fallback brute-force doit retourner des résultats (embeddings seedés)"
    );

    // Vérification que le fallback produit le bon classement (brute-force cosine).
    use gradatum_core::identity::NoteId;
    let note3 = NoteId(ulid::Ulid::from_string("01HB0000000000000000000003").unwrap());
    assert_eq!(
        results[0].0, note3,
        "fallback brute-force doit retourner la bonne note en tête"
    );
}

// ── T5.3 : set_ann_enabled(false) = brute-force ─────────────────────────────

/// T5.3 — `set_ann_enabled(false)` après `true` → retour au chemin brute-force.
///
/// Vérifie que le flag est réversible et que `search_semantic` suit le flag.
#[tokio::test]
async fn test_ann_disable_reverts_to_brute_force() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");

    // Activer puis désactiver ANN.
    idx.set_ann_enabled(true);
    idx.set_ann_enabled(false);
    assert!(
        !idx.ann_is_enabled(),
        "ann_is_enabled() doit être false après set_ann_enabled(false)"
    );

    let dim = 1024usize;
    seed_note_with_emb(&idx, "01HB0000000000000000000005", &axis_vec(dim, 0)).await;

    let query = axis_vec(dim, 0);
    let results = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
            "bge-m3",
            &query,
            1,
            None,
        )
        .await
        .expect("search_semantic doit réussir avec ann_enabled=false");

    assert_eq!(results.len(), 1, "doit retourner 1 résultat (brute-force)");
    assert!(
        (results[0].1 - 1.0).abs() < 1e-4,
        "score cosine ≈ 1.0 (brute-force)"
    );
}

// ── T5.4 : ef_search round-trip ─────────────────────────────────────────────

/// T5.4 — `set_ann_ef_search` + getter round-trip.
///
/// Vérifie que les valeurs extrêmes et nominales sont correctement stockées.
#[test]
fn test_ann_ef_search_round_trip() {
    // Utilise un runtime inline pour éviter de créer un runtime tokio dans ce test sync.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let idx = rt
        .block_on(SqliteIndex::open_in_memory())
        .expect("open_in_memory");

    // Valeur par défaut.
    assert_eq!(idx.ann_ef_search(), 64, "défaut ef_search = 64");

    // Valeur nominale.
    idx.set_ann_ef_search(128);
    assert_eq!(idx.ann_ef_search(), 128, "ef_search doit être 128");

    // Valeur minimale.
    idx.set_ann_ef_search(1);
    assert_eq!(idx.ann_ef_search(), 1, "ef_search doit être 1");

    // Valeur maximale u32.
    idx.set_ann_ef_search(u32::MAX);
    assert_eq!(
        idx.ann_ef_search(),
        u32::MAX,
        "ef_search doit être u32::MAX"
    );

    // Reset à la valeur par défaut.
    idx.set_ann_ef_search(64);
    assert_eq!(idx.ann_ef_search(), 64, "ef_search retour à 64");
}

// ── T5.5 : AtomicBool thread-safety ─────────────────────────────────────────

/// T5.5 — `ann_is_enabled()` est lisible depuis plusieurs threads via Arc.
///
/// `SqliteIndex` contient `ann_enabled: Arc<AtomicBool>` — vérifie que
/// deux threads peuvent lire le flag en parallèle sans race condition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ann_enabled_atomic_thread_safe() {
    use std::sync::Arc;

    let idx = Arc::new(SqliteIndex::open_in_memory().await.expect("open_in_memory"));

    idx.set_ann_enabled(true);

    let idx1 = Arc::clone(&idx);
    let idx2 = Arc::clone(&idx);

    let h1 = tokio::spawn(async move {
        // Thread 1 : lit le flag 100 fois.
        for _ in 0..100 {
            assert!(idx1.ann_is_enabled(), "thread 1 : flag doit être true");
        }
    });
    let h2 = tokio::spawn(async move {
        // Thread 2 : lit le flag 100 fois.
        for _ in 0..100 {
            assert!(idx2.ann_is_enabled(), "thread 2 : flag doit être true");
        }
    });

    h1.await.expect("thread 1");
    h2.await.expect("thread 2");
}

// ── T5.6 (ignoré) : ANN avec extension réelle ───────────────────────────────

/// T5.6 — Vérifie que le chemin ANN sqlite-vec retourne des résultats valides
/// lorsque l'extension est réellement chargée.
///
/// Ignoré dans CI : nécessite que l'extension sqlite-vec soit chargée via
/// `sqlite3_auto_extension` AVANT l'ouverture de la DB (bin-only, unsafe).
/// Lancement manuel : `cargo test -p gradatum-index ann_routing -- --ignored`
#[tokio::test]
#[ignore = "requiert sqlite3_auto_extension(sqlite3_vec_init) appelé AVANT open_in_memory"]
async fn test_ann_routes_to_sqlite_vec_when_extension_loaded() {
    // Ce test ne peut passer que si le test runner a préalablement appelé
    // `sqlite3_auto_extension(sqlite3_vec_init)` (unsafe, bin-only).
    // Dans les conditions normales de CI (sans enregistrement), vec0 = absent
    // → ce test serait un faux négatif (fallback brute-force, pas de vérification ANN).
    //
    // Pour le valider manuellement, utiliser le bin `gradatum-bench --bin ann_recall`
    // qui enregistre l'extension avant d'ouvrir la DB.

    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    idx.set_ann_enabled(true);

    let dim = 1024usize;
    seed_note_with_emb(&idx, "01HB0000000000000000000007", &axis_vec(dim, 0)).await;

    // Backfill ANN.
    let count = idx.backfill_ann_index().await.expect("backfill_ann_index");
    assert_eq!(count, 1, "1 note backfillée");

    // Requête ANN.
    let query = axis_vec(dim, 0);
    let results = idx
        .search_semantic(
            &gradatum_core::scope::AclCheckedVaultId::for_system_task(
                gradatum_core::scope::VaultId::new("main"),
            ),
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
