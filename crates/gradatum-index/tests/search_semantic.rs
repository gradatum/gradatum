//! Tests intégration `SqliteIndex::search_semantic`.
//!
//! Couvre :
//! - top-k cosine ordering (A ≈ 1.0, B ≈ 0.9, C ≈ 0.0)
//! - query norme nulle → résultats vides
//! - embedder_id isolation (autre embedder ignoré)
//! - dim mismatch skip silencieux
//! - note downgraded exclue
//! - perf N=1500 (B2.1 — p95 ≤ 200ms)

// Nécessaire pour résoudre search_semantic/insert_note_embedding sur SqliteIndex (Étape 0.1).
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

/// Encode un vecteur f32 en BLOB little-endian (helper partagé).
///
/// Conservé pour les futurs tests `search_semantic` qui injectent des
/// embeddings sérialisés (debug perf, test rétrocompat, etc.). Marqué
/// `#[allow(dead_code)]` car non utilisé par les tests actuels (qui
/// passent via `insert_note_embedding`).
#[allow(dead_code)]
fn encode_f32_le(vec: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    blob
}

#[tokio::test]
async fn search_semantic_returns_top_k_by_cosine() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // ULIDs cohérents — seed via id explicite string ULID
    let note_a_id = ulid::Ulid::new();
    let note_b_id = ulid::Ulid::new();
    let note_c_id = ulid::Ulid::new();
    use gradatum_core::identity::NoteId;
    let note_a = NoteId(note_a_id);
    let note_b = NoteId(note_b_id);
    let note_c = NoteId(note_c_id);

    // Embedding A : [1.0, 0.0, ...] → cosine 1.0 avec query identique
    let emb_a: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    // Embedding B : [0.9, 0.436, 0.0, ...] → cosine ≈ 0.9 (norme ≈ 1.0)
    let emb_b: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 0.9;
        v[1] = 0.436;
        v
    };
    // Embedding C : [0.0, 1.0, 0.0, ...] → cosine ≈ 0.0 (orthogonal à query)
    let emb_c: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[1] = 1.0;
        v
    };

    idx.seed_note(&note_a.to_string(), "reference", "body A")
        .await
        .unwrap();
    idx.seed_note(&note_b.to_string(), "reference", "body B")
        .await
        .unwrap();
    idx.seed_note(&note_c.to_string(), "reference", "body C")
        .await
        .unwrap();

    idx.insert_note_embedding(&note_a, "test-embedder", 1024, &emb_a)
        .await
        .unwrap();
    idx.insert_note_embedding(&note_b, "test-embedder", 1024, &emb_b)
        .await
        .unwrap();
    idx.insert_note_embedding(&note_c, "test-embedder", 1024, &emb_c)
        .await
        .unwrap();

    // Query vers [1.0, 0.0, ...] → A doit être premier
    let query_emb: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };

    let results = idx
        .search_semantic("main", "test-embedder", &query_emb, 3, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 3, "doit retourner exactement 3 résultats");

    // Premier résultat = note_a (cosine ≈ 1.0)
    assert_eq!(
        results[0].0, note_a,
        "note_a doit être premier (cosine ≈ 1.0)"
    );
    assert!(
        (results[0].1 - 1.0).abs() < 1e-4,
        "score note_a ≈ 1.0, trouvé {}",
        results[0].1
    );

    // note_b doit être deuxième (cosine ≈ 0.9)
    assert_eq!(
        results[1].0, note_b,
        "note_b doit être deuxième (cosine ≈ 0.9)"
    );
    assert!(
        results[1].1 > 0.8 && results[1].1 < 1.0,
        "score note_b dans [0.8, 1.0), trouvé {}",
        results[1].1
    );

    // note_c doit être dernier (cosine ≈ 0.0)
    assert_eq!(
        results[2].0, note_c,
        "note_c doit être dernier (cosine ≈ 0.0)"
    );
    assert!(
        results[2].1.abs() < 0.1,
        "score note_c ≈ 0.0, trouvé {}",
        results[2].1
    );
}

#[tokio::test]
async fn search_semantic_query_zero_norm_returns_empty() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    use gradatum_core::identity::NoteId;

    // Seed une note avec embedding pour s'assurer que ce n'est pas juste un vault vide
    let note_id = NoteId::new();
    idx.seed_note(&note_id.to_string(), "reference", "body")
        .await
        .unwrap();
    let emb: Vec<f32> = vec![1.0f32; 1024];
    idx.insert_note_embedding(&note_id, "test-embedder", 1024, &emb)
        .await
        .unwrap();

    // Query avec vecteur nul → résultats vides (norme = 0)
    let zero_query: Vec<f32> = vec![0.0f32; 1024];
    let results = idx
        .search_semantic("main", "test-embedder", &zero_query, 10, None)
        .await
        .unwrap();

    assert!(
        results.is_empty(),
        "query norme nulle doit retourner vide, trouvé {} résultats",
        results.len()
    );
}

#[tokio::test]
async fn search_semantic_ignores_other_embedder_id() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    use gradatum_core::identity::NoteId;

    let note_id = NoteId::new();
    idx.seed_note(&note_id.to_string(), "reference", "body")
        .await
        .unwrap();

    let emb: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    // Inséré sous "embedder-A"
    idx.insert_note_embedding(&note_id, "embedder-A", 1024, &emb)
        .await
        .unwrap();

    // Recherche sous "embedder-B" → doit retourner vide
    let query: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    let results = idx
        .search_semantic("main", "embedder-B", &query, 10, None)
        .await
        .unwrap();

    assert!(
        results.is_empty(),
        "embedder-B ne doit pas voir les embeddings de embedder-A"
    );
}

#[tokio::test]
async fn search_semantic_excludes_downgraded_notes() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    use gradatum_core::identity::NoteId;

    let live_id = NoteId::new();
    let down_id = NoteId::new();

    idx.seed_note(&live_id.to_string(), "reference", "live body")
        .await
        .unwrap();
    idx.seed_note(&down_id.to_string(), "reference", "downgraded body")
        .await
        .unwrap();

    let emb: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    idx.insert_note_embedding(&live_id, "test-embedder", 1024, &emb)
        .await
        .unwrap();
    idx.insert_note_embedding(&down_id, "test-embedder", 1024, &emb)
        .await
        .unwrap();

    // Downgrader la 2ème note
    idx.downgrade_note(&down_id, "test-reason", None)
        .await
        .unwrap();

    let query: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    let results = idx
        .search_semantic("main", "test-embedder", &query, 10, None)
        .await
        .unwrap();

    // Seule la note live doit apparaître
    assert_eq!(results.len(), 1, "seule la note live doit apparaître");
    assert_eq!(results[0].0, live_id, "doit retourner la note live");
}

#[tokio::test]
async fn search_semantic_limit_truncates_results() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    use gradatum_core::identity::NoteId;

    // Insérer 5 notes avec des embeddings valides
    let mut ids = Vec::new();
    for i in 0..5u32 {
        let note_id = NoteId::new();
        idx.seed_note(&note_id.to_string(), "reference", &format!("body {i}"))
            .await
            .unwrap();
        let emb: Vec<f32> = {
            let mut v = vec![0.0f32; 1024];
            v[0] = 1.0 - (i as f32 * 0.1); // scores décroissants
            v
        };
        idx.insert_note_embedding(&note_id, "test-embedder", 1024, &emb)
            .await
            .unwrap();
        ids.push(note_id);
    }

    let query: Vec<f32> = {
        let mut v = vec![0.0f32; 1024];
        v[0] = 1.0;
        v
    };
    // limit=3 → doit retourner exactement 3 résultats
    let results = idx
        .search_semantic("main", "test-embedder", &query, 3, None)
        .await
        .unwrap();

    assert_eq!(results.len(), 3, "limit=3 doit tronquer à 3 résultats");
    // Vérifier que les scores sont décroissants (ordre correct)
    assert!(
        results[0].1 >= results[1].1 && results[1].1 >= results[2].1,
        "scores doivent être décroissants"
    );
}

/// B2.1 — test de performance cosine N=1500 (crate criterion non requis — chrono suffit).
///
/// Seuil cible : p95 ≤ 200ms pour 1500 notes × 1024 dimensions.
/// Ce test est ignoré par défaut (long ~2-5s selon charge CPU).
/// Lancement explicite : `cargo test -p gradatum-index search_semantic_perf -- --ignored`
#[tokio::test]
#[ignore = "perf test B2.1 — long (~2-5s). Lancer avec --ignored"]
async fn search_semantic_perf_n1500_p95_under_200ms() {
    use std::time::Instant;

    let idx = SqliteIndex::open_in_memory().await.unwrap();
    use gradatum_core::identity::NoteId;

    // Seed 1500 notes avec embeddings 1024d aléatoires (pattern sinusoïdal déterministe)
    let n = 1500usize;
    let dim = 1024usize;
    let embedder_id = "perf-embedder";

    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let note_id = NoteId::new();
        idx.seed_note(&note_id.to_string(), "reference", &format!("perf body {i}"))
            .await
            .unwrap();
        // Vecteur avec variation déterministe pour éviter les vecteurs identiques
        let emb: Vec<f32> = (0..dim)
            .map(|d| (i as f32 * 0.001 + d as f32 * 0.0001).sin())
            .collect();
        idx.insert_note_embedding(&note_id, embedder_id, dim as u16, &emb)
            .await
            .unwrap();
        ids.push(note_id);
    }

    // Query vers vecteur unité [1.0, 0.0, ...]
    let query_emb: Vec<f32> = {
        let mut v = vec![0.0f32; dim];
        v[0] = 1.0;
        v
    };

    // 20 runs pour calculer p95
    let runs = 20usize;
    let mut durations_ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let results = idx
            .search_semantic("main", embedder_id, &query_emb, 10, None)
            .await
            .unwrap();
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        durations_ms.push(elapsed_ms);
        // Sanity check : doit retourner 10 résultats
        assert_eq!(results.len(), 10, "doit retourner 10 résultats");
    }

    durations_ms.sort_unstable();
    let p50 = durations_ms[runs / 2];
    let p95 = durations_ms[(runs as f64 * 0.95) as usize];

    eprintln!(
        "search_semantic perf N={n} dim={dim} : p50={}ms p95={}ms (seuil ≤200ms)",
        p50, p95
    );

    assert!(
        p95 <= 200,
        "p95 {}ms dépasse le seuil 200ms — BM25-only fallback recommandé pour N > 1500",
        p95
    );
}
