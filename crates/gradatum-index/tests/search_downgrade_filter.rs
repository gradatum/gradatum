//! Tests régression search filter downgrade.
//!
//! Vérifie le comportement de `search_fts_scored` face au param `include_downgraded` :
//! 1. Par défaut (false) : les notes downgraded sont exclues des résultats.
//! 2. Avec include_downgraded=true : les notes downgraded sont incluses.
//! 3. Score downgraded < score live (pénalité BM25 × 0.1).
//! 4. Si seules des notes downgraded existent : résultats vides par défaut.

mod common;
use common::make_note;

use gradatum_core::scope::VaultId;
use gradatum_core::section::Section;
use gradatum_core::status::NoteStatus;
use gradatum_index::SqliteIndex;

/// Crée et insère une note avec status Live, puis la downgrade si nécessaire.
/// Retourne l'id de la note insérée.
async fn seed_note(
    idx: &SqliteIndex,
    vault_id: &str,
    body: &str,
    downgraded: bool,
) -> gradatum_core::identity::NoteId {
    let note = make_note(vault_id, Section::Reference, NoteStatus::Live, body);
    let note_id = note.id;
    idx.upsert_note(&note)
        .await
        .expect("upsert_note ne doit pas échouer");
    if downgraded {
        idx.downgrade_note(&note_id, "test-downgrade", None)
            .await
            .expect("downgrade_note ne doit pas échouer");
    }
    note_id
}

/// Test : default (include_downgraded=false) exclut les downgraded.
///
/// Seed : 1 note live + 1 note downgraded, même contenu "rust programming language".
/// Attendu : 1 seul résultat (la note live).
#[tokio::test]
async fn search_default_excludes_downgraded() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let vault = VaultId::new("main");

    seed_note(&idx, "main", "rust programming language", false).await;
    seed_note(&idx, "main", "rust programming language", true).await;

    let results = idx
        .search_fts_scored(&vault, "rust", 10, false)
        .await
        .expect("search_fts_scored ne doit pas échouer");

    assert_eq!(
        results.len(),
        1,
        "default (include_downgraded=false) doit exclure les notes downgraded — obtenu {}",
        results.len()
    );
    assert_eq!(
        results[0].2, "live",
        "le seul résultat doit avoir status='live'"
    );
}

/// Test : include_downgraded=true retourne toutes les notes.
///
/// Seed : 1 note live + 1 note downgraded, même contenu "rust programming".
/// Attendu : 2 résultats.
#[tokio::test]
async fn search_include_downgraded_returns_all() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let vault = VaultId::new("main");

    seed_note(&idx, "main", "rust programming", false).await;
    seed_note(&idx, "main", "rust programming", true).await;

    let results = idx
        .search_fts_scored(&vault, "rust", 10, true)
        .await
        .expect("search_fts_scored ne doit pas échouer");

    assert_eq!(
        results.len(),
        2,
        "include_downgraded=true doit retourner les 2 notes — obtenu {}",
        results.len()
    );

    // Les deux statuts attendus doivent être présents.
    let statuses: std::collections::HashSet<&str> =
        results.iter().map(|(_, _, s)| s.as_str()).collect();
    assert!(
        statuses.contains("live"),
        "doit contenir au moins une note 'live'"
    );
    assert!(
        statuses.contains("downgraded"),
        "doit contenir au moins une note 'downgraded'"
    );
}

/// Test : score downgraded < score live (pénalité BM25 × 0.1).
///
/// BM25 retourne des valeurs négatives (plus proche de 0 = meilleur match).
/// Après multiplication par 0.1, le score downgraded devient plus négatif →
/// ORDER BY ASC le place après la note live.
/// Attendu : la note live a un score BM25 > score downgraded (moins négatif).
#[tokio::test]
async fn search_include_downgraded_penalizes_score() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let vault = VaultId::new("main");

    seed_note(&idx, "main", "rust systems programming language", false).await;
    seed_note(&idx, "main", "rust systems programming language", true).await;

    let results = idx
        .search_fts_scored(&vault, "rust", 10, true)
        .await
        .expect("search_fts_scored ne doit pas échouer");

    assert_eq!(results.len(), 2, "doit avoir 2 résultats");

    // Ordre ASC : meilleur score (BM25 moins négatif) en premier.
    // La note live doit apparaître avant la note downgraded.
    let live_score = results
        .iter()
        .find(|(_, _, s)| s == "live")
        .map(|(_, score, _)| *score)
        .expect("doit contenir une note 'live'");
    let down_score = results
        .iter()
        .find(|(_, _, s)| s == "downgraded")
        .map(|(_, score, _)| *score)
        .expect("doit contenir une note 'downgraded'");

    // BM25 négatif : live_score > down_score (live moins négatif = meilleur match).
    // Pénalité 0.1 rend down_score 10× plus négatif.
    assert!(
        live_score > down_score,
        "score live {live_score:.6} doit être > score downgraded {down_score:.6} (BM25 × 0.1 pénalité)"
    );
}

/// Test : 0 résultat si toutes les notes sont downgraded (défaut).
///
/// Seed : 1 note downgraded uniquement.
/// Attendu : résultats vides avec include_downgraded=false.
#[tokio::test]
async fn search_default_zero_results_if_only_downgraded() {
    let idx = SqliteIndex::open_in_memory().await.expect("open_in_memory");
    let vault = VaultId::new("main");

    seed_note(&idx, "main", "rust uniquement downgraded", true).await;

    let results = idx
        .search_fts_scored(&vault, "rust", 10, false)
        .await
        .expect("search_fts_scored ne doit pas échouer");

    assert!(
        results.is_empty(),
        "doit retourner 0 résultat si toutes les notes sont downgraded — obtenu {}",
        results.len()
    );
}
