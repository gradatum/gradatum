//! Reciprocal Rank Fusion — combines BM25 and semantic signals into a unified score.
//!
//! ## Formula
//!
//! `rrf_score(d) = Σ_i  1 / (k + rank_i(d))`
//!
//! where `k = 60` (standard constant, Cormack et al. 2009).
//! `rank_i` is the 0-based position in the sorted list of signal `i`.
//!
//! ## Signal semantics
//!
//! - **BM25**: sorted ASC (more negative = better BM25) → rank 0 = best match.
//!   BM25 lists from `search_fts_with_snippet` are already sorted ASC.
//! - **Semantic**: sorted DESC (higher cosine = better) → rank 0 = best match.
//!   Lists from `search_semantic` are already sorted DESC.
//! - **RRF output**: sorted DESC (highest score = best).
//!
//! ## Guarantees
//!
//! - Notes absent from a signal receive rank = N+1 (maximum penalty for that signal).
//! - The final sort is stable: at equal scores, the insertion order in `all_ids`
//!   is preserved (BM25 first, then semantic-only).
//! - No unnecessary allocations: `HashMap` used for O(1) lookups.

use std::collections::HashMap;

/// Fused RRF hit.
///
/// Produced by [`rrf_fuse`] and enriched by the handler (`section`, `snippet`, `title`).
#[derive(Debug, Clone)]
pub struct RrfHit {
    /// ULID identifier of the note (stored as `String` to avoid parsing overhead).
    pub note_id: String,
    /// Combined RRF score (sum of per-signal contributions).
    pub rrf_score: f64,
    /// Section of the note (populated by the handler from BM25 or semantic results).
    pub section: String,
    /// Text snippet (populated from BM25 results when available).
    pub snippet: Option<String>,
    /// H1 title of the note (when available from the `title` column).
    pub title: Option<String>,
    /// `true` if and only if this hit originates **exclusively** from the semantic/vector signal:
    /// absent from the BM25 ranking, present in the semantic ranking.
    ///
    /// Computed at RRF fusion (authoritative source) — replaces the fragile heuristic
    /// `title.is_none() && section.is_empty()` previously used by the handler to identify
    /// hits that require a batch `title`/`section` enrichment.
    pub is_semantic_only: bool,
    /// 0-based rank of the note in the BM25 list, or `None` if absent from the signal.
    ///
    /// Exposed optionally (opt-in `include_scores`) to let the UI reconstruct the lexical
    /// RRF contribution `1 / (k + bm25_rank)`. `None` means the note was absent from the
    /// BM25 ranking (penalty rank `bm25_n + 1` applied internally).
    pub bm25_rank: Option<u32>,
    /// 0-based rank of the note in the semantic list, or `None` if absent from the signal.
    ///
    /// Exposed optionally (opt-in `include_scores`) to let the UI reconstruct the semantic
    /// RRF contribution `1 / (k + sem_rank)`. `None` means the note was absent from the
    /// semantic ranking (penalty rank `sem_n + 1` applied internally).
    pub sem_rank: Option<u32>,
    /// Raw SQL status of the note (kebab-case, e.g. `"live"`, `"downgraded"`).
    ///
    /// Populated by the handler (from BM25 results or a batch `get_statuses` call for
    /// semantic-only hits). Exposed as-is in the `SearchHit.status` response field.
    /// Empty string if unresolved (semantic-only hit without batch enrichment).
    pub status: String,
    /// Temporal anchor (`temporal_index.anchor_ms`), Unix epoch milliseconds.
    ///
    /// Populated by the handler from BM25 results (`SearchHitRaw.anchor_ms`) or from
    /// the semantic batch lookup (`get_anchor_ms_batch`). `None` when absent.
    pub anchor_ms: Option<i64>,
}

/// Applies RRF fusion over BM25 and semantic results.
///
/// # Parameters
///
/// - `bm25`: BM25 results sorted ASC (best = index 0, least-negative score).
///   Each element is `(note_id, bm25_score)`.
/// - `semantic`: cosine results sorted DESC (best = index 0).
///   Each element is `(note_id, cosine_score)`.
/// - `k`: RRF constant (60.0 recommended, standard from Cormack et al. 2009).
/// - `limit`: maximum number of results returned.
///
/// # Guarantees
///
/// - Notes present only in BM25: semantic rank = `sem_n + 1`.
/// - Notes present only in semantic: BM25 rank = `bm25_n + 1`.
/// - Final sort DESC by `rrf_score` (stable: Rust's `sort_by` is stable).
/// - Tie-break: insertion order in `all_ids` is preserved
///   (BM25 first, then semantic-only).
///
/// # Panics
///
/// Never panics (no `unwrap` on critical paths).
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
        .map(|(note_id, rrf_score)| {
            // Un hit est sémantique-only SSI absent du ranking BM25 ET présent dans
            // le ranking sémantique. C'est exactement l'inverse de la présence dans
            // `bm25_rank` : les ids sémantique-only sont ceux ajoutés dans la boucle
            // `all_ids` uniquement parce qu'ils n'étaient pas dans `bm25_rank`.
            //
            // Équivalence comportementale garantie : avant ce champ, le handler utilisait
            // `hit.title.is_none() && hit.section.is_empty()` pour détecter ces hits,
            // ce qui était vrai uniquement parce que `bm25_map.get(&hit.note_id)` échouait
            // (le hit était absent de BM25). Ce champ capture la même condition à la source.
            let is_semantic_only = !bm25_rank.contains_key(note_id.as_str());
            // F-37 S1.1 — rangs réels (présence dans chaque signal), pas le rang pénalité.
            // `None` ⇒ note absente du signal ⇒ contribution `1/(k + n+1)` (max penalty).
            // Le cast usize→u32 est sûr : les listes BM25/sémantique sont bornées au
            // buffer RRF (≤ rrf_buffer, lui-même borné par le clamp limit ≤ 50 côté handler).
            let bm25_rank_val = bm25_rank
                .get(note_id.as_str())
                .map(|&r| u32::try_from(r).unwrap_or(u32::MAX));
            let sem_rank_val = sem_rank
                .get(note_id.as_str())
                .map(|&r| u32::try_from(r).unwrap_or(u32::MAX));
            RrfHit {
                note_id,
                rrf_score,
                section: String::new(), // rempli par le handler après fusion
                snippet: None,          // rempli par le handler depuis BM25 results
                title: None,            // rempli par le handler depuis BM25 results
                is_semantic_only,
                bm25_rank: bm25_rank_val,
                sem_rank: sem_rank_val,
                status: String::new(), // rempli par le handler après fusion (F-37 notes fix)
                anchor_ms: None,       // rempli par le handler (F-65 temporal)
            }
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

    #[test]
    fn rrf_fuse_is_semantic_only_bm25_hit_is_false() {
        // Un hit présent dans BM25 (même si absent de sémantique) n'est PAS semantic-only.
        let bm25 = vec![("note_lex".to_string(), -1.0f64)];
        let semantic: Vec<(String, f32)> = vec![];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_lex");
        assert!(
            !fused[0].is_semantic_only,
            "hit BM25-only ne doit pas être marqué semantic_only"
        );
    }

    #[test]
    fn rrf_fuse_is_semantic_only_semantic_hit_is_true() {
        // Un hit présent uniquement dans la liste sémantique doit être marqué semantic_only.
        let bm25: Vec<(String, f64)> = vec![];
        let semantic = vec![("note_sem".to_string(), 0.85f32)];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_sem");
        assert!(
            fused[0].is_semantic_only,
            "hit sémantique-only doit être marqué is_semantic_only=true"
        );
    }

    #[test]
    fn rrf_fuse_is_semantic_only_mixed_hit_is_false() {
        // Un hit présent dans les DEUX signaux n'est pas semantic-only.
        let bm25 = vec![("note_both".to_string(), -0.5f64)];
        let semantic = vec![("note_both".to_string(), 0.9f32)];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_both");
        assert!(
            !fused[0].is_semantic_only,
            "hit présent dans les deux signaux ne doit pas être semantic_only"
        );
    }

    #[test]
    fn rrf_fuse_is_semantic_only_mixed_list_correct_flags() {
        // Vérifie le marquage correct dans une liste mixte :
        //   note_lex  → BM25 seul → false
        //   note_sem  → sémantique seul → true
        //   note_both → les deux → false
        let bm25 = vec![
            ("note_lex".to_string(), -1.5f64),
            ("note_both".to_string(), -0.8f64),
        ];
        let semantic = vec![
            ("note_both".to_string(), 0.95f32),
            ("note_sem".to_string(), 0.80f32),
        ];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 3);

        let find = |id: &str| {
            fused
                .iter()
                .find(|h| h.note_id == id)
                .expect("note manquante dans le résultat fusionné")
        };

        assert!(
            !find("note_lex").is_semantic_only,
            "note_lex doit être false"
        );
        assert!(
            !find("note_both").is_semantic_only,
            "note_both doit être false"
        );
        assert!(find("note_sem").is_semantic_only, "note_sem doit être true");
    }

    #[test]
    fn rrf_fuse_exposes_per_signal_ranks() {
        // F-37 S1.1 — bm25_rank/sem_rank reflètent la position réelle dans chaque
        // signal (None si absente), pas le rang pénalité interne.
        //   BM25     : note_lex rank 0, note_both rank 1
        //   Semantic : note_both rank 0, note_sem rank 1
        let bm25 = vec![
            ("note_lex".to_string(), -1.5f64),
            ("note_both".to_string(), -0.8f64),
        ];
        let semantic = vec![
            ("note_both".to_string(), 0.95f32),
            ("note_sem".to_string(), 0.80f32),
        ];
        let fused = rrf_fuse(&bm25, &semantic, 60.0, 10);

        let find = |id: &str| {
            fused
                .iter()
                .find(|h| h.note_id == id)
                .expect("note manquante dans le résultat fusionné")
        };

        // note_lex : présente BM25 rank 0, absente sémantique.
        assert_eq!(find("note_lex").bm25_rank, Some(0));
        assert_eq!(find("note_lex").sem_rank, None);
        // note_both : présente dans les deux (BM25 rank 1, sémantique rank 0).
        assert_eq!(find("note_both").bm25_rank, Some(1));
        assert_eq!(find("note_both").sem_rank, Some(0));
        // note_sem : absente BM25, présente sémantique rank 1.
        assert_eq!(find("note_sem").bm25_rank, None);
        assert_eq!(find("note_sem").sem_rank, Some(1));
    }
}
