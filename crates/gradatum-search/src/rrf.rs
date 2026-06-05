//! Reciprocal Rank Fusion — combine BM25 et sémantique en un score unifié.
//!
//! ## Formule
//!
//! `rrf_score(d) = Σ_i  1 / (k + rank_i(d))`
//!
//! où `k = 60` (constante standard, Cormack et al. 2009).
//! `rank_i` est la position 0-indexée dans la liste triée du signal `i`.
//!
//! ## Sémantique des signaux
//!
//! - **BM25** : trié ASC (plus négatif = meilleur BM25) → rank 0 = meilleur match.
//!   Les listes BM25 de `search_fts_with_snippet` sont déjà triées ASC.
//! - **Semantic** : trié DESC (plus grand cosine = meilleur) → rank 0 = meilleur match.
//!   Les listes de `search_semantic` sont déjà triées DESC.
//! - **RRF output** : trié DESC (score le plus élevé = meilleur).
//!
//! ## Garanties
//!
//! - Notes absentes d'un signal → rang = N+1 (pénalité maximale pour ce signal).
//! - Le tri final est stable : à score égal, l'ordre d'insertion dans `all_ids`
//!   est préservé (BM25 first, puis semantic-only).
//! - Pas d'allocations inutiles : `HashMap` utilisé pour les lookups O(1).

use std::collections::HashMap;

/// Hit fusionné RRF.
///
/// Produit par [`rrf_fuse`], enrichi par le handler (`section`, `snippet`, `title`).
#[derive(Debug, Clone)]
pub struct RrfHit {
    /// Identifiant ULID de la note (String pour éviter le coût de parsing).
    pub note_id: String,
    /// Score RRF combiné (somme des contributions par signal).
    pub rrf_score: f64,
    /// Section de la note (remplie par le handler depuis BM25 ou semantic results).
    pub section: String,
    /// Snippet textuel (rempli depuis les résultats BM25 si disponible).
    pub snippet: Option<String>,
    /// Titre H1 de la note (si disponible depuis la colonne title).
    pub title: Option<String>,
}

/// Applique la fusion RRF sur les résultats BM25 et sémantique.
///
/// # Paramètres
///
/// - `bm25` : résultats BM25 triés ASC (meilleur = index 0, score le moins négatif).
///   Chaque élément est `(note_id, bm25_score)`.
/// - `semantic` : résultats cosine triés DESC (meilleur = index 0).
///   Chaque élément est `(note_id, cosine_score)`.
/// - `k` : constante RRF (60.0 recommandé, standard Cormack et al. 2009).
/// - `limit` : nombre maximum de résultats retournés.
///
/// # Garanties
///
/// - Notes présentes uniquement dans BM25 : rang sémantique = `sem_n + 1`.
/// - Notes présentes uniquement dans semantic : rang BM25 = `bm25_n + 1`.
/// - Tri final DESC par `rrf_score` (stable : `sort_by` Rust est stable).
/// - Tie-break : l'ordre d'apparition dans `all_ids` est préservé
///   (BM25 d'abord, semantic-only ensuite).
///
/// # Panics
///
/// Cette fonction ne panique jamais (pas de `unwrap` sur path critique).
#[must_use]
pub fn rrf_fuse(
    bm25: &[(String, f64)],
    semantic: &[(String, f32)],
    k: f64,
    limit: usize,
) -> Vec<RrfHit> {
    let bm25_n = bm25.len();
    let sem_n = semantic.len();

    // Map note_id → rang dans chaque signal (O(1) lookup).
    let bm25_rank: HashMap<&str, usize> = bm25
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();

    let sem_rank: HashMap<&str, usize> = semantic
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();

    // Union des note_ids : BM25 d'abord (ordre préservé), puis semantic-only.
    // Cet ordre détermine le tie-break stable final.
    let mut all_ids: Vec<&str> = bm25.iter().map(|(id, _)| id.as_str()).collect();
    for (id, _) in semantic {
        if !bm25_rank.contains_key(id.as_str()) {
            all_ids.push(id.as_str());
        }
    }

    // Calcul du score RRF pour chaque note candidate.
    let mut hits: Vec<(String, f64)> = all_ids
        .iter()
        .map(|&id| {
            // Rang BM25 : position dans la liste BM25, ou bm25_n+1 si absent.
            let r_bm25 = *bm25_rank.get(id).unwrap_or(&(bm25_n + 1));
            // Rang sémantique : position dans la liste semantic, ou sem_n+1 si absent.
            let r_sem = *sem_rank.get(id).unwrap_or(&(sem_n + 1));
            let score = 1.0 / (k + r_bm25 as f64) + 1.0 / (k + r_sem as f64);
            (id.to_string(), score)
        })
        .collect();

    // Tri décroissant stable : à score égal, l'ordre d'insertion (BM25 first) est préservé.
    // `sort_by` Rust est stable — `Equal` ne réordonne pas les éléments à score identique.
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);

    hits.into_iter()
        .map(|(note_id, rrf_score)| RrfHit {
            note_id,
            rrf_score,
            section: String::new(), // rempli par le handler après fusion
            snippet: None,          // rempli par le handler depuis BM25 results
            title: None,            // rempli par le handler depuis BM25 results
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_fuse_single_hit_bm25_only() {
        let bm25 = vec![("note_A".to_string(), -0.5f64)];
        let semantic: Vec<(String, f32)> = vec![];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_A");
        // score = 1/(60+0) + 1/(60+1) ≈ 0.03306
        assert!(fused[0].rrf_score > 0.03 && fused[0].rrf_score < 0.04);
    }

    #[test]
    fn rrf_fuse_single_hit_semantic_only() {
        let bm25: Vec<(String, f64)> = vec![];
        let semantic = vec![("note_B".to_string(), 0.9f32)];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_B");
        // score = 1/(60+1) + 1/(60+0) ≈ 0.03306
        assert!(fused[0].rrf_score > 0.03 && fused[0].rrf_score < 0.04);
    }

    #[test]
    fn rrf_fuse_k_parameter_scales_scores() {
        // Avec k=1 (très petit), les scores sont plus élevés
        let bm25 = vec![("note_A".to_string(), -0.5f64)];
        let semantic: Vec<(String, f32)> = vec![];
        let fused_k1 = rrf_fuse(&bm25, &semantic, 1.0, 10);
        let fused_k60 = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert!(
            fused_k1[0].rrf_score > fused_k60[0].rrf_score,
            "k=1 doit produire des scores plus élevés que k=60"
        );
    }
}
