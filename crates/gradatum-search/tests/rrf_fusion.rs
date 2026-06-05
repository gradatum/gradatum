//! Tests intégration `rrf_fuse` — Phase 2.x.2 alpha.11.
//!
//! Couvre :
//! - fusion BM25 + semantic avec tie-break stable
//! - notes absentes d'un signal (pénalité rank N+1)
//! - limit truncation
//! - listes vides (BM25 seul, semantic seul, les deux vides)

use gradatum_search::rrf::{rrf_fuse, RrfHit};

/// Assertion helper : vérifie la présence par note_id
fn has_id(fused: &[RrfHit], id: &str) -> bool {
    fused.iter().any(|h| h.note_id == id)
}

#[test]
fn rrf_fuse_combines_bm25_and_semantic_with_deterministic_ordering() {
    // Setup déterministe :
    //   BM25     : note_A rank 0, note_B rank 1, note_C rank 2
    //   Semantic : note_B rank 0, note_A rank 1, note_C rank 2
    //
    // Avec k=60 :
    //   A = 1/(60+0) + 1/(60+1) ≈ 0.01667 + 0.01639 = 0.03306
    //   B = 1/(60+1) + 1/(60+0) ≈ 0.01639 + 0.01667 = 0.03306
    //   C = 1/(60+2) + 1/(60+2) ≈ 0.01613 + 0.01613 = 0.03226
    //
    // Tie-break A vs B : Rust sort_by stable + ordre d'insertion bm25-first (A < B).

    let bm25: Vec<(String, f64)> = vec![
        ("note_A".to_string(), -0.5),
        ("note_B".to_string(), -0.8),
        ("note_C".to_string(), -1.5),
    ];
    let semantic: Vec<(String, f32)> = vec![
        ("note_B".to_string(), 0.95),
        ("note_A".to_string(), 0.80),
        ("note_C".to_string(), 0.20),
    ];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 5);

    // 3 notes uniques attendues
    assert_eq!(fused.len(), 3, "3 notes uniques attendues");

    // Présence de toutes les notes
    assert!(has_id(&fused, "note_A"), "note_A absente");
    assert!(has_id(&fused, "note_B"), "note_B absente");
    assert!(has_id(&fused, "note_C"), "note_C absente");

    // C doit être dernier (score le plus bas)
    assert_eq!(
        fused[2].note_id, "note_C",
        "note_C doit être dernier (score le plus bas)"
    );

    // Tie-break stable : A précède B (ordre d'apparition dans bm25 first)
    let pos_a = fused.iter().position(|h| h.note_id == "note_A").unwrap();
    let pos_b = fused.iter().position(|h| h.note_id == "note_B").unwrap();
    assert!(
        pos_a < pos_b,
        "Tie-break stable : A doit précéder B (ordre d'insertion bm25), pos_a={pos_a} pos_b={pos_b}"
    );
}

#[test]
fn rrf_fuse_note_only_in_bm25_gets_semantic_penalty() {
    // note_X : BM25 rank=0, absent du semantic → rang semantic = sem_n+1 = 2
    // note_Y : BM25 rank=1, semantic rank=0 (unique dans semantic)
    //
    // sem_n = 1 (une note dans semantic)
    // X = 1/(60+0) + 1/(60+2) ≈ 0.01667 + 0.01613 = 0.03280
    // Y = 1/(60+1) + 1/(60+0) ≈ 0.01639 + 0.01667 = 0.03306
    // Y > X → Y précède X (le boost sémantique compense le rang BM25 légèrement moins bon)

    let bm25: Vec<(String, f64)> = vec![("note_X".to_string(), -0.5), ("note_Y".to_string(), -0.8)];
    let semantic: Vec<(String, f32)> = vec![("note_Y".to_string(), 0.95)];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert_eq!(fused.len(), 2, "2 notes attendues");
    assert!(has_id(&fused, "note_X"), "note_X absente");
    assert!(has_id(&fused, "note_Y"), "note_Y absente");

    // Y (score ≈ 0.03306) > X (score ≈ 0.03280) → Y en premier
    let pos_x = fused.iter().position(|h| h.note_id == "note_X").unwrap();
    let pos_y = fused.iter().position(|h| h.note_id == "note_Y").unwrap();
    assert!(
        pos_y < pos_x,
        "Y (boost semantic) doit précéder X (absent du semantic), pos_y={pos_y} pos_x={pos_x}"
    );

    // Vérifier les scores numériquement
    let score_y = fused[pos_y].rrf_score;
    let score_x = fused[pos_x].rrf_score;
    assert!(
        score_y > score_x,
        "score_y ({score_y:.6}) doit être > score_x ({score_x:.6})"
    );
}

#[test]
fn rrf_fuse_note_only_in_semantic_appears_in_output() {
    // note_Z présente uniquement dans semantic (pas dans BM25)
    // → rang BM25 = bm25_n+1 = 2
    let bm25: Vec<(String, f64)> = vec![("note_A".to_string(), -0.5)];
    let semantic: Vec<(String, f32)> = vec![
        ("note_Z".to_string(), 0.99), // Z seulement dans semantic
        ("note_A".to_string(), 0.50),
    ];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert_eq!(fused.len(), 2, "2 notes attendues (A + Z)");
    assert!(has_id(&fused, "note_A"), "note_A absente");
    assert!(has_id(&fused, "note_Z"), "note_Z absente");

    // Z : BM25 rank=2 (pénalité), semantic rank=0 → 1/(60+2) + 1/(60+0) ≈ 0.01613 + 0.01667 = 0.03280
    // A : BM25 rank=0, semantic rank=1 → 1/(60+0) + 1/(60+1) ≈ 0.01667 + 0.01639 = 0.03306
    // A doit précéder Z
    let pos_a = fused.iter().position(|h| h.note_id == "note_A").unwrap();
    let pos_z = fused.iter().position(|h| h.note_id == "note_Z").unwrap();
    assert!(
        pos_a < pos_z,
        "A (score ~0.033) doit précéder Z (score ~0.032), pos_a={pos_a} pos_z={pos_z}"
    );
}

#[test]
fn rrf_fuse_limit_truncates() {
    // 5 notes dans BM25, limit=3 → seulement 3 retournés
    let bm25: Vec<(String, f64)> = (0..5)
        .map(|i| (format!("note_{i}"), -(i as f64 * 0.5)))
        .collect();
    let semantic: Vec<(String, f32)> = vec![];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 3);

    assert_eq!(fused.len(), 3, "limit=3 doit tronquer à 3 résultats");
}

#[test]
fn rrf_fuse_empty_bm25_returns_only_semantic() {
    let bm25: Vec<(String, f64)> = vec![];
    let semantic: Vec<(String, f32)> =
        vec![("note_A".to_string(), 0.9), ("note_B".to_string(), 0.5)];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert_eq!(fused.len(), 2, "2 notes depuis semantic uniquement");
    // note_A semantic rank=0, note_B semantic rank=1
    // A score > B score → A en premier
    assert_eq!(
        fused[0].note_id, "note_A",
        "note_A (rank 0) doit être premier"
    );
    assert_eq!(
        fused[1].note_id, "note_B",
        "note_B (rank 1) doit être deuxième"
    );
}

#[test]
fn rrf_fuse_empty_semantic_returns_only_bm25_ranked() {
    let bm25: Vec<(String, f64)> = vec![
        ("note_A".to_string(), -0.3), // meilleur BM25 = rank 0
        ("note_B".to_string(), -1.5),
        ("note_C".to_string(), -3.0),
    ];
    let semantic: Vec<(String, f32)> = vec![];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert_eq!(fused.len(), 3, "3 notes depuis BM25 uniquement");
    // Avec semantic vide, tous les ranks semantic = sem_n+1 = 1.
    // Score BM25 only = 1/(60+rank_bm25) + 1/(60+1)
    // A: 1/(60+0) + 1/61 = 0.01667+0.01639 = 0.03306 (le plus haut)
    // B: 1/(60+1) + 1/61 = 0.01639+0.01639 = 0.03279
    // C: 1/(60+2) + 1/61 = 0.01613+0.01639 = 0.03252
    // Ordre A > B > C
    assert_eq!(fused[0].note_id, "note_A", "note_A doit être premier");
    assert_eq!(fused[1].note_id, "note_B", "note_B doit être deuxième");
    assert_eq!(fused[2].note_id, "note_C", "note_C doit être dernier");
}

#[test]
fn rrf_fuse_both_empty_returns_empty() {
    let bm25: Vec<(String, f64)> = vec![];
    let semantic: Vec<(String, f32)> = vec![];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert!(fused.is_empty(), "les deux vides → résultat vide");
}

#[test]
fn rrf_fuse_scores_are_monotone_descending() {
    // Vérifie que le tri final est bien décroissant
    let bm25: Vec<(String, f64)> = vec![
        ("A".to_string(), -0.1),
        ("B".to_string(), -0.5),
        ("C".to_string(), -1.0),
        ("D".to_string(), -2.0),
    ];
    let semantic: Vec<(String, f32)> = vec![
        ("A".to_string(), 0.9),
        ("C".to_string(), 0.7),
        ("B".to_string(), 0.3),
        ("D".to_string(), 0.1),
    ];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    for w in fused.windows(2) {
        assert!(
            w[0].rrf_score >= w[1].rrf_score,
            "scores doivent être décroissants : {} >= {} failed",
            w[0].rrf_score,
            w[1].rrf_score
        );
    }
}

#[test]
fn rrf_fuse_rrf_hit_section_and_snippet_initially_empty() {
    // section et snippet sont remplis par le handler — vérifier valeurs initiales
    let bm25: Vec<(String, f64)> = vec![("note_A".to_string(), -0.5)];
    let semantic: Vec<(String, f32)> = vec![];

    let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].section, "", "section initialement vide");
    assert!(fused[0].snippet.is_none(), "snippet initialement None");
    assert!(fused[0].title.is_none(), "title initialement None");
}
