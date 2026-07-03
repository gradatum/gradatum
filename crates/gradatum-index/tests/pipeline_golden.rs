//! Golden tests — fige le comportement des sous-pipelines search AVANT le carve 3 traits.
//!
//! # Périmètre
//!
//! Ces tests capturent l'ORDER RELATIF des résultats des trois signaux du pipeline
//! `vault_search` qui résident dans `gradatum-index` :
//!
//! 1. `search_fts_with_snippet` — signal BM25, ordre ASC (plus négatif = meilleur).
//! 2. `search_semantic` — signal cosine, ordre DESC (plus grand = meilleur).
//! 3. `get_note_created_and_indegree` — signal composite (created_ms + in_degree).
//!
//! # Ce qui n'est PAS testé ici
//!
//! La fusion RRF (`rrf_fuse`) et le scoring composite (`composite_score`, `pagerank_factor`,
//! `recency_factor`) résident dans `gradatum-search`, qui dépend de `gradatum-index` —
//! une dépendance inverse créerait un cycle Cargo. Ces fonctions sont couvertes dans
//! `gradatum-search/tests/`. Ce golden capture les signaux bruts qui les alimentent.
//!
//! # Invariant
//!
//! Si l'un de ces tests échoue après un refactoring des traits de storage, ce refactoring a modifié
//! le comportement observable — STOP, revue obligatoire avant de continuer.

use gradatum_core::{identity::NoteId, scope::VaultId};
// Nécessaire pour résoudre insert_note_embedding/search_semantic sur SqliteIndex (Étape 0.1).
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

// ── Corpus déterministe ───────────────────────────────────────────────────────
//
// ULIDs fixes (ordre lexicographique déterministe).
// Format ULID : 26 chars base32 Crockford, 10 chars timestamp + 16 chars aléatoires.
// On fixe des valeurs synthétiques valides pour que les tests soient reproductibles.

const ID_ALPHA: &str = "01AAAAAAAAAAAAAAAAAAAAAA01";
const ID_BETA: &str = "01AAAAAAAAAAAAAAAAAAAAAA02";
const ID_GAMMA: &str = "01AAAAAAAAAAAAAAAAAAAAAA03";
const ID_DELTA: &str = "01AAAAAAAAAAAAAAAAAAAAAA04";

// ── Test 1 : Signal BM25 — ordre relatif search_fts_with_snippet ─────────────

/// Fige l'ordre BM25 de `search_fts_with_snippet` sur un corpus déterministe.
///
/// Corpus : 3 notes avec des fréquences de terme différentes.
/// - ALPHA : "gradatum gradatum gradatum index storage" — 3× "gradatum"
/// - BETA  : "gradatum index" — 1× "gradatum"
/// - GAMMA : "storage layer only" — 0× "gradatum"
///
/// Requête "gradatum" → BM25 score ASC : ALPHA < BETA (meilleurs, valeurs négatives
/// plus proches de 0 pour ALPHA). GAMMA absent (pas de match).
///
/// L'assertion porte sur l'ORDRE des NoteId, pas les valeurs BM25 absolues.
#[tokio::test]
async fn golden_bm25_order_is_stable() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // seed_note_with_fts insère dans notes ET notes_fts (FTS5 content= en mémoire
    // ne se synchronise pas sans INSERT explicite dans la virtual table).
    idx.seed_note_with_fts(
        ID_ALPHA,
        "reference",
        "gradatum gradatum gradatum index storage",
    )
    .await
    .unwrap();
    idx.seed_note_with_fts(ID_BETA, "reference", "gradatum index")
        .await
        .unwrap();
    idx.seed_note_with_fts(ID_GAMMA, "reference", "storage layer only")
        .await
        .unwrap();

    let vault = VaultId::new("main");

    // Requête "gradatum" → ALPHA et BETA doivent apparaître, GAMMA absent.
    // FTS5 BM25 : score ASC → index 0 = meilleur match (plus proche de 0).
    // Avec 3× répétition dans ALPHA vs 1× dans BETA, BM25 favorise ALPHA.
    let hits = idx
        .search_fts_with_snippet(&vault, "gradatum", 10, false, None, None, None, None, None)
        .await
        .unwrap();

    // Exactement 2 résultats (GAMMA ne contient pas "gradatum").
    assert_eq!(
        hits.len(),
        2,
        "golden BM25 : attendu 2 résultats pour query 'gradatum', trouvé {}",
        hits.len()
    );

    // ALPHA doit être premier (3× "gradatum" → meilleur BM25).
    let first_id = hits[0].note_id.to_string();
    let second_id = hits[1].note_id.to_string();

    assert_eq!(
        first_id, ID_ALPHA,
        "golden BM25 : premier résultat attendu ALPHA ({ID_ALPHA}), trouvé {first_id}"
    );
    assert_eq!(
        second_id, ID_BETA,
        "golden BM25 : deuxième résultat attendu BETA ({ID_BETA}), trouvé {second_id}"
    );

    // Invariant : BM25 ASC → position 0 a le score le plus négatif (meilleur match).
    // ALPHA (3× "gradatum") a une valeur BM25 plus négative que BETA (1× "gradatum").
    assert!(
        hits[0].bm25 <= hits[1].bm25,
        "golden BM25 : score[0]={} doit être <= score[1]={} (ordre ASC — plus négatif = meilleur)",
        hits[0].bm25,
        hits[1].bm25
    );
}

// ── Test 2 : Signal sémantique — ordre cosine search_semantic ────────────────

/// Fige l'ordre cosine de `search_semantic` sur des embeddings entiers déterministes.
///
/// Corpus : 3 notes avec embeddings de dim=3 orthogonaux.
/// - ALPHA : [1.0, 0.0, 0.0] — cosine 1.0 avec query [1.0, 0.0, 0.0]
/// - BETA  : [0.0, 1.0, 0.0] — cosine 0.0 (orthogonal)
/// - GAMMA : [0.6, 0.8, 0.0] — cosine 0.6 avec query
///
/// Ordre attendu DESC : ALPHA (1.0) > GAMMA (0.6) > BETA (0.0).
///
/// Embeddings entiers = cosine calculable analytiquement = test 100% déterministe.
#[tokio::test]
async fn golden_semantic_order_is_stable() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let note_alpha = NoteId(ID_ALPHA.parse::<ulid::Ulid>().unwrap());
    let note_beta = NoteId(ID_BETA.parse::<ulid::Ulid>().unwrap());
    let note_gamma = NoteId(ID_GAMMA.parse::<ulid::Ulid>().unwrap());

    idx.seed_note(ID_ALPHA, "reference", "alpha semantic note")
        .await
        .unwrap();
    idx.seed_note(ID_BETA, "reference", "beta semantic note")
        .await
        .unwrap();
    idx.seed_note(ID_GAMMA, "reference", "gamma semantic note")
        .await
        .unwrap();

    // dim=3 suffit pour ce test déterministe — réduit l'empreinte mémoire.
    let dim: u16 = 3;

    // ALPHA : [1.0, 0.0, 0.0] — cosine 1.0 avec query identique
    idx.insert_note_embedding(&note_alpha, "golden-embedder", dim, &[1.0f32, 0.0, 0.0])
        .await
        .unwrap();

    // BETA : [0.0, 1.0, 0.0] — cosine 0.0 (orthogonal à query)
    idx.insert_note_embedding(&note_beta, "golden-embedder", dim, &[0.0f32, 1.0, 0.0])
        .await
        .unwrap();

    // GAMMA : [0.6, 0.8, 0.0] — norme ≈ 1.0, cosine 0.6 avec query
    idx.insert_note_embedding(&note_gamma, "golden-embedder", dim, &[0.6f32, 0.8, 0.0])
        .await
        .unwrap();

    // Query vers [1.0, 0.0, 0.0] → ALPHA cosine 1.0, GAMMA cosine 0.6, BETA cosine 0.0
    let results = idx
        .search_semantic("main", "golden-embedder", &[1.0f32, 0.0, 0.0], 3, None)
        .await
        .unwrap();

    assert_eq!(
        results.len(),
        3,
        "golden semantic : attendu 3 résultats, trouvé {}",
        results.len()
    );

    // Ordre relatif figé : ALPHA > GAMMA > BETA
    assert_eq!(
        results[0].0, note_alpha,
        "golden semantic : position 0 attendue ALPHA, trouvée {:?}",
        results[0].0
    );
    assert_eq!(
        results[1].0, note_gamma,
        "golden semantic : position 1 attendue GAMMA, trouvée {:?}",
        results[1].0
    );
    assert_eq!(
        results[2].0, note_beta,
        "golden semantic : position 2 attendue BETA, trouvée {:?}",
        results[2].0
    );

    // Invariant cosine décroissant (scores absolus non figés mais ordre oui).
    assert!(
        results[0].1 >= results[1].1 && results[1].1 >= results[2].1,
        "golden semantic : scores non décroissants — [{}, {}, {}]",
        results[0].1,
        results[1].1,
        results[2].1
    );

    // Borne minimale sur ALPHA (cosine proche de 1.0).
    assert!(
        (results[0].1 - 1.0f32).abs() < 1e-4,
        "golden semantic : cosine ALPHA attendu ≈ 1.0, trouvé {}",
        results[0].1
    );
}

// ── Test 3 : Signal composite — get_note_created_and_indegree ────────────────

/// Fige le comportement de `get_note_created_and_indegree`.
///
/// Ce test couvre le signal composite du pipeline : les valeurs (created_ms, in_degree)
/// alimentent `recency_factor` + `pagerank_factor` dans `composite_score` (gradatum-search).
/// Le golden ici vérifie que :
/// - Une note récente a un `created_ms` > une note ancienne (invariant recency).
/// - Le `in_degree` croît avec les backlinks ajoutés (invariant pagerank).
/// - Une note absente retourne `NoteNotFound` (invariant gracieux handler).
#[tokio::test]
async fn golden_composite_signal_is_stable() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    // Note ancienne (epoch 0) vs note récente (epoch actuel).
    let old_ms: i64 = 1_000_000; // loin dans le passé
    let new_ms: i64 = chrono::Utc::now().timestamp_millis();

    idx.seed_note_with_created(ID_ALPHA, "reference", "ancienne note", old_ms)
        .await
        .unwrap();
    idx.seed_note_with_created(ID_BETA, "reference", "note récente", new_ms)
        .await
        .unwrap();
    // GAMMA comme cible de backlinks (in_degree > 0).
    idx.seed_note_with_created(ID_GAMMA, "reference", "note liée", new_ms)
        .await
        .unwrap();
    idx.seed_note_with_created(ID_DELTA, "reference", "note source lien", new_ms)
        .await
        .unwrap();

    // Ajouter un lien DELTA → GAMMA (in_degree de GAMMA = 1).
    // upsert_link(vault_id, src, dst) : un lien sortant de DELTA vers GAMMA
    // incrémente le backlink count de GAMMA.
    idx.upsert_link("main", ID_DELTA, ID_GAMMA).await.unwrap();

    // ALPHA : note ancienne, 0 backlink
    let (created_alpha, indegree_alpha) = idx
        .get_note_created_and_indegree("main", ID_ALPHA)
        .await
        .unwrap();
    assert_eq!(
        created_alpha, old_ms,
        "golden composite : created_alpha doit être {old_ms}"
    );
    assert_eq!(
        indegree_alpha, 0,
        "golden composite : indegree ALPHA sans backlink = 0"
    );

    // BETA : note récente, 0 backlink
    let (created_beta, indegree_beta) = idx
        .get_note_created_and_indegree("main", ID_BETA)
        .await
        .unwrap();
    assert!(
        created_beta >= new_ms,
        "golden composite : created_beta {created_beta} doit être >= {new_ms}"
    );
    assert_eq!(
        indegree_beta, 0,
        "golden composite : indegree BETA sans backlink = 0"
    );

    // GAMMA : note récente, 1 backlink entrant (DELTA → GAMMA)
    let (created_gamma, indegree_gamma) = idx
        .get_note_created_and_indegree("main", ID_GAMMA)
        .await
        .unwrap();
    assert!(
        created_gamma >= new_ms,
        "golden composite : created_gamma {created_gamma} doit être >= {new_ms}"
    );
    assert_eq!(
        indegree_gamma, 1,
        "golden composite : indegree GAMMA avec 1 backlink entrant = 1"
    );

    // Invariant recency : note récente a un created_ms > note ancienne.
    assert!(
        created_beta > created_alpha,
        "golden composite : note récente ({created_beta}) doit être > note ancienne ({created_alpha})"
    );

    // Note absente → NoteNotFound (invariant handler gracieux).
    let missing_id = "01AAAAAAAAAAAAAAAAAAAAAA99";
    let result = idx.get_note_created_and_indegree("main", missing_id).await;
    assert!(
        matches!(
            result,
            Err(gradatum_core::error::GradatumError::NoteNotFound(_))
        ),
        "golden composite : note absente doit retourner NoteNotFound, trouvé {:?}",
        result
    );
}
