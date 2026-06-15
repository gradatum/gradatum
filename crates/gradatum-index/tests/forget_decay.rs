//! Tests de régression F-44 decay — corrections audit C4 (P1+P2).
//!
//! Couvre :
//! 1. `fts_with_snippet_forgotten_degraded` (C1) — `search_fts_with_snippet` (chemin réel
//!    `vault_search`) applique le decay F-44 : note forgotten → score dégradé vs note identique
//!    non-forgotten.
//! 2. `fts_scored_filtered_sorts_after_decay` (C2) — `search_fts_scored_filtered` re-trie
//!    les résultats par score après application du decay.
//! 3. `semantic_forgotten_degraded` (P2-R4) — `search_semantic_inner` applique le decay cosine
//!    sur les notes forgotten → score cosine réduit vs note identique non-forgotten.
//! 4. `count_forgotten_global_total` (C3) — `count_forgotten_notes` retourne le count global,
//!    indépendant de la taille de page (3 forgotten, limit=1 → count=3).
//!
//! # Stratégie decay
//!
//! Le decay est `× 0.5^elapsed_days`. Pour tester l'effet réel, on insère `forgotten_at`
//! comme `now - 86_400_000 ms` (= 1 jour en arrière) via `seed_mark_forgotten_at`.
//! `elapsed_days = 1.0` → decay = 0.5^1 = 0.5.
//!
//! BM25 est négatif (valeur plus négative = meilleur match).
//! Decay ×0.5 sur une valeur négative la rapproche de 0 → score dégradé (rang pire).
//! Donc : `score_forgotten > score_live` arithmétiquement.
//!
//! Cosine est positif [0,1]. Decay ×0.5 le réduit → `score_forgotten < score_live`.

use gradatum_core::scope::VaultId;
use gradatum_core::VectorStore as _;
use gradatum_index::SqliteIndex;

// ── Test C1 : search_fts_with_snippet ─────────────────────────────────────────

/// C1 — `search_fts_with_snippet` applique le decay F-44 sur les notes forgotten.
///
/// Score BM25 d'une note forgotten il y a 1 jour (decay ×0.5) doit être dégradé
/// par rapport à une note identique non-forgotten.
#[tokio::test]
async fn fts_with_snippet_forgotten_degraded() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_live = ulid::Ulid::new().to_string();
    let id_forgotten = ulid::Ulid::new().to_string();

    idx.seed_note_with_fts_vault(
        &id_live,
        "main",
        "decisions",
        None,
        "gradatum decay forgotten regression test alpha",
    )
    .await
    .expect("seed live");

    idx.seed_note_with_fts_vault(
        &id_forgotten,
        "main",
        "decisions",
        None,
        "gradatum decay forgotten regression test alpha",
    )
    .await
    .expect("seed forgotten note");

    // Marquer forgotten il y a 1 jour → decay ×0.5
    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_mark_forgotten_at(&id_forgotten, now_ms - 86_400_000)
        .await
        .expect("seed_mark_forgotten_at");

    let results = idx
        .search_fts_with_snippet(&vault, "decay", 10, false, None, None, None)
        .await
        .expect("search_fts_with_snippet");

    assert_eq!(
        results.len(),
        2,
        "C1 : les deux notes doivent être retournées, got {}",
        results.len()
    );

    let hit_live = results
        .iter()
        .find(|h| h.note_id.to_string() == id_live)
        .expect("note live introuvable dans les résultats");
    let hit_forgotten = results
        .iter()
        .find(|h| h.note_id.to_string() == id_forgotten)
        .expect("note forgotten introuvable dans les résultats");

    // BM25 brut est identique (même texte). Decay ×0.5 sur valeur négative :
    // score_forgotten = bm25_raw × 0.5 → plus proche de 0 → arithmétiquement supérieur.
    assert!(
        hit_forgotten.bm25 > hit_live.bm25,
        "C1 : note forgotten (score {:.4}) doit être dégradée vs note live (score {:.4})",
        hit_forgotten.bm25,
        hit_live.bm25
    );

    // Vérification ordre : note live (meilleur score) doit être en premier.
    assert_eq!(
        results[0].note_id.to_string(),
        id_live,
        "C1 : note live doit être en premier (meilleur score BM25)"
    );
}

// ── Test C2 : search_fts_scored_filtered re-tri ───────────────────────────────

/// C2 — `search_fts_scored_filtered` re-trie après application du decay.
///
/// Vérifie que les résultats sont triés par score post-decay (pas score brut SQL).
#[tokio::test]
async fn fts_scored_filtered_sorts_after_decay() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();
    let vault = VaultId::new("main");

    let id_live = ulid::Ulid::new().to_string();
    let id_forgotten = ulid::Ulid::new().to_string();

    idx.seed_note_with_fts_vault(
        &id_live,
        "main",
        "decisions",
        None,
        "gradatum resort decay filtered regression test beta",
    )
    .await
    .expect("seed live");

    idx.seed_note_with_fts_vault(
        &id_forgotten,
        "main",
        "decisions",
        None,
        "gradatum resort decay filtered regression test beta",
    )
    .await
    .expect("seed forgotten");

    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_mark_forgotten_at(&id_forgotten, now_ms - 86_400_000)
        .await
        .expect("seed_mark_forgotten_at");

    let results = idx
        .search_fts_scored_filtered(
            &vault, "resort", 10, false, // include_downgraded
            None,  // section
            None,  // locus
        )
        .await
        .expect("search_fts_scored_filtered");

    assert_eq!(results.len(), 2, "C2 : deux résultats attendus");

    let (first_id, first_score, _) = &results[0];
    let (second_id, second_score, _) = &results[1];

    assert!(
        first_score <= second_score,
        "C2 : résultats doivent être triés ASC par score post-decay, \
         premier={first_score:.4} second={second_score:.4}"
    );
    assert_eq!(
        first_id.to_string(),
        id_live,
        "C2 : note live doit être en premier après re-tri"
    );
    assert_eq!(
        second_id.to_string(),
        id_forgotten,
        "C2 : note forgotten doit être en second (score dégradé)"
    );
}

// ── Test P2-R4 : search_semantic_inner decay cosine ───────────────────────────

/// P2-R4 — `search_semantic_inner` applique le decay cosine sur les notes forgotten.
///
/// Score cosine POSITIF [0,1] → decay ×0.5^1 = ×0.5 → score réduit → rang dégradé.
#[tokio::test]
async fn semantic_forgotten_degraded() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let id_live = ulid::Ulid::new();
    let id_forgotten_ulid = ulid::Ulid::new();
    let id_live_str = id_live.to_string();
    let id_forgotten_str = id_forgotten_ulid.to_string();

    let emb = vec![1.0f32, 0.0, 0.0, 0.0];

    idx.seed_note_with_fts_vault(
        &id_live_str,
        "main",
        "decisions",
        None,
        "semantic decay regression note live gamma",
    )
    .await
    .expect("seed live");
    idx.insert_note_embedding(
        &gradatum_core::identity::NoteId(id_live),
        "test-sem-decay",
        4,
        &emb,
    )
    .await
    .expect("insert embedding live");

    idx.seed_note_with_fts_vault(
        &id_forgotten_str,
        "main",
        "decisions",
        None,
        "semantic decay regression note forgotten gamma",
    )
    .await
    .expect("seed forgotten");
    idx.insert_note_embedding(
        &gradatum_core::identity::NoteId(id_forgotten_ulid),
        "test-sem-decay",
        4,
        &emb,
    )
    .await
    .expect("insert embedding forgotten");

    let now_ms = chrono::Utc::now().timestamp_millis();
    idx.seed_mark_forgotten_at(&id_forgotten_str, now_ms - 86_400_000)
        .await
        .expect("seed_mark_forgotten_at");

    let query_emb = vec![1.0f32, 0.0, 0.0, 0.0];
    let hits = idx
        .search_semantic("main", "test-sem-decay", &query_emb, 10, None)
        .await
        .expect("search_semantic");

    assert_eq!(hits.len(), 2, "P2-R4 : deux résultats attendus");

    let score_live = hits
        .iter()
        .find(|(id, _)| id.to_string() == id_live_str)
        .map(|(_, s)| *s)
        .expect("note live introuvable");
    let score_forgotten = hits
        .iter()
        .find(|(id, _)| id.to_string() == id_forgotten_str)
        .map(|(_, s)| *s)
        .expect("note forgotten introuvable");

    // Cosine brut identique. Decay ×0.5 → score_forgotten = score_live × 0.5.
    assert!(
        score_forgotten < score_live,
        "P2-R4 : cosine forgotten ({score_forgotten:.4}) doit être < cosine live ({score_live:.4})"
    );

    // Ordre décroissant : note live (meilleur cosine) doit être en premier.
    assert_eq!(
        hits[0].0.to_string(),
        id_live_str,
        "P2-R4 : note live doit être en premier (meilleur cosine)"
    );
}

// ── Test C3 : count_forgotten global ─────────────────────────────────────────

/// C3 — `count_forgotten_notes` retourne le count global, indépendant de la pagination.
///
/// 3 notes forgotten, limit=1 → `list_forgotten_notes` retourne 1, `count_forgotten_notes` = 3.
#[tokio::test]
async fn count_forgotten_global_total() {
    let idx = SqliteIndex::open_in_memory().await.unwrap();

    let id_a = ulid::Ulid::new().to_string();
    let id_b = ulid::Ulid::new().to_string();
    let id_c = ulid::Ulid::new().to_string();

    let now_ms = chrono::Utc::now().timestamp_millis();

    for (id, body) in [
        (id_a.as_str(), "forgotten count test alpha"),
        (id_b.as_str(), "forgotten count test beta"),
        (id_c.as_str(), "forgotten count test gamma"),
    ] {
        idx.seed_note_with_fts_vault(id, "main", "decisions", None, body)
            .await
            .expect("seed note");

        idx.seed_mark_forgotten_at(id, now_ms)
            .await
            .expect("seed_mark_forgotten_at");
    }

    // Pagination limit=1 : l'implémentation retourne limit+1 pour détecter next_cursor.
    // Donc avec 3 notes et limit=1, on reçoit au plus 2 résultats (1 page + 1 sentinel).
    // Le handler applique take(limit) → 1 note effective. Ce test vérifie que count est
    // bien global (3) et non la taille de la page (≤ limit+1 = 2).
    let page = idx
        .list_forgotten_notes("main", 1, None)
        .await
        .expect("list_forgotten_notes");
    // page.len() ≤ limit+1 = 2 (comportement attendu du +1 sentinel).
    assert!(
        page.len() <= 2,
        "limit=1 → au plus limit+1=2 résultats retournés (sentinel), got {}",
        page.len()
    );

    // count global → 3 (toutes les notes forgotten, indépendant de limit).
    let total = idx
        .count_forgotten_notes("main")
        .await
        .expect("count_forgotten_notes");
    assert_eq!(
        total, 3,
        "C3 : count global doit être 3 (indépendant pagination), got {total}"
    );
}
