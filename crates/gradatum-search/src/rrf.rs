//! Fusion module — combines BM25 and semantic signals into a unified score.
//!
//! Two fusions coexist:
//!
//! - **Rank fusion (RRF)** — [`rrf_fuse`]: `rrf_score(d) = Σ_i  1 / (k + rank_i(d))`,
//!   with `k = 60` (Cormack et al. 2009). Pure rank fusion, magnitude discarded by
//!   construction. Kept for the Noop-embedder path (bit-identical retro-compat).
//! - **Weighted normalised-magnitude fusion** — [`hybrid_fuse_weighted`] /
//!   [`rrf_fuse_short_circuit`]: single arm → the responding arm's normalised score
//!   (criterion 6); two arms → `0.5·normalize_bm25 + 0.5·normalize_semantic`
//!   (criterion 10). The magnitude is kept in the nominal case.
//!
//! ## Signal semantics
//!
//! - **BM25**: sorted ASC (more negative = better BM25) → rank 0 = best match.
//!   BM25 lists from `search_fts_with_snippet` are already sorted ASC.
//! - **Semantic**: sorted DESC (higher cosine = better) → rank 0 = best match.
//!   Lists from `search_semantic` are already sorted DESC.
//! - **Output**: sorted DESC (highest score = best).
//!
//! ## Guarantees
//!
//! - Notes absent from a signal receive rank = N+1 (maximum penalty) in [`rrf_fuse`],
//!   or a `0.0` magnitude contribution in [`hybrid_fuse_weighted`].
//! - The final sort is stable: at equal scores, the insertion order in `all_ids`
//!   is preserved (BM25 first, then semantic-only).
//! - No unnecessary allocations: `HashMap` used for O(1) lookups.

use crate::scoring::{normalize_bm25, normalize_semantic, weighted_fusion_score};
use std::collections::HashMap;

/// Fused RRF hit.
///
/// Produced by [`rrf_fuse`] and enriched by the handler (`section`, `snippet`, `title`).
#[derive(Debug, Clone)]
pub struct RrfHit {
    /// ULID identifier of the note (stored as `String` to avoid parsing overhead).
    pub note_id: String,
    /// Combined score: RRF sum of per-signal rank contributions ([`rrf_fuse`]),
    /// or weighted normalised magnitude ([`hybrid_fuse_weighted`] / single-arm
    /// short-circuit) — the field name is kept for wire/API stability.
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

/// Fuses BM25 and semantic signals by weighted normalised magnitudes when both
/// arms respond (criterion 10), short-circuiting the rank fusion that the
/// single-arm cases were already bypassing.
///
/// When **exactly one arm** responds, the rank fusion is short-circuited and the
/// **normalized score of the responding arm is authoritative** (criterion 6).
///
/// When **both arms** respond, [`hybrid_fuse_weighted`] replaces the rank-only
/// RRF fusion: each note scores `0.5 × normalize_bm25(bm25) + 0.5 × normalize_semantic(cosine)`
/// (see [`weighted_fusion_score`]). The magnitude is no longer discarded in the
/// nominal case — a note matching both signals strongly outranks a note matching
/// one weakly, which is exactly the discriminator the RRF ceiling (~0.04) and the
/// ~6–14 % top-to-10th spread were hiding.
///
/// # Single-arm scores
///
/// - BM25 only: `rrf_score = normalize_bm25(bm25_score)` (`1/(1 + |bm25|)`).
/// - Semantic only: `rrf_score = normalize_semantic(cosine)` (cosine clamped to `[0,1]`).
///
/// # `k` parameter
///
/// Retained for signature compatibility with [`rrf_fuse`]; it has **no effect**
/// anymore — the two-arm case no longer ranks, so no `k` enters the score.
///
/// # Guarantees
///
/// Same `RrfHit` shape as [`rrf_fuse`] (`bm25_rank` / `sem_rank` /
/// `is_semantic_only`), so the downstream enrichment and composite scoring are
/// unchanged. The single-arm lists are already ordered best-first (BM25 ASC,
/// semantic DESC) and the normalizations are monotonic — the ordering is preserved.
///
/// # Panics
///
/// Never panics (no `unwrap` on critical paths).
#[must_use]
pub fn rrf_fuse_short_circuit(
    bm25: &[(String, f64)],
    semantic: &[(String, f32)],
    _k: f64,
    limit: usize,
) -> Vec<RrfHit> {
    // F-162 critère 6 — court-circuit à bras unique ; critère 10 — pondération à deux bras.
    //
    // Cas 0 : aucun bras ne répond → rien à fusionner.
    if bm25.is_empty() && semantic.is_empty() {
        return Vec::new();
    }
    // Cas 1 : seul le bras sémantique répond → son score normalisé fait foi.
    if bm25.is_empty() {
        let mut hits: Vec<RrfHit> = semantic
            .iter()
            .enumerate()
            .map(|(i, (id, score))| {
                let sem_rank = u32::try_from(i).unwrap_or(u32::MAX);
                RrfHit {
                    note_id: id.clone(),
                    // Magnitude du bras sémantique (cosine), pas une contribution de rang.
                    rrf_score: normalize_semantic(*score),
                    section: String::new(),
                    snippet: None,
                    title: None,
                    is_semantic_only: true,
                    bm25_rank: None,
                    sem_rank: Some(sem_rank),
                    status: String::new(),
                    anchor_ms: None,
                }
            })
            .collect();
        hits.truncate(limit);
        return hits;
    }
    // Cas 2 : seul le bras lexical répond → son score normalisé fait foi.
    if semantic.is_empty() {
        let mut hits: Vec<RrfHit> = bm25
            .iter()
            .enumerate()
            .map(|(i, (id, score))| {
                let bm25_rank = u32::try_from(i).unwrap_or(u32::MAX);
                RrfHit {
                    note_id: id.clone(),
                    // Magnitude du bras BM25 (négatif, meilleur ≈ 0), pas une contribution de rang.
                    rrf_score: normalize_bm25(*score),
                    section: String::new(),
                    snippet: None,
                    title: None,
                    is_semantic_only: false,
                    bm25_rank: Some(bm25_rank),
                    sem_rank: None,
                    status: String::new(),
                    anchor_ms: None,
                }
            })
            .collect();
        hits.truncate(limit);
        return hits;
    }
    // Cas 3 : les deux bras répondent → fusion pondérée sur scores normalisés.
    // La fusion par rang (RRF) jetait la magnitude par construction ; le critère 10
    // la remplace par la fusion pondérée — la magnitude cesse d'être jetée au nominal.
    hybrid_fuse_weighted(bm25, semantic, limit)
}

/// Weighted normalised-magnitude fusion for the **two-arm** case (criterion 10).
///
/// Each candidate scores `weighted_fusion_score(bm25, cosine)` =
/// `0.5 × normalize_bm25(bm25) + 0.5 × normalize_semantic(cosine)`, with a `0.0`
/// contribution for the arm the note is absent from. The score is **intrinsic**
/// to the note (no pool-size dependence): the rank-pool sensitivity of `rrf_fuse`
/// disappears — a top-K at `limit=L` is a prefix of the top-K at `limit=L+1`.
///
/// Tie-break: stable `sort_by` preserves insertion order (`all_ids` = BM25 first,
/// then semantic-only) exactly as in [`rrf_fuse`].
///
/// # Panics
///
/// Never panics (no `unwrap` on critical paths).
#[must_use]
pub fn hybrid_fuse_weighted(
    bm25: &[(String, f64)],
    semantic: &[(String, f32)],
    limit: usize,
) -> Vec<RrfHit> {
    // Maps de rang (O(1) lookup) pour exposer bm25_rank/sem_rank et l'absence.
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
    // Maps de scores bruts pour la fusion pondérée (l'absence → None → contribution 0).
    let bm25_score: HashMap<&str, f64> = bm25.iter().map(|(id, s)| (id.as_str(), *s)).collect();
    let sem_score: HashMap<&str, f32> = semantic.iter().map(|(id, s)| (id.as_str(), *s)).collect();

    // Union des note_ids : BM25 d'abord (ordre préservé), puis semantic-only.
    // Cet ordre détermine le tie-break stable final.
    let mut all_ids: Vec<&str> = bm25.iter().map(|(id, _)| id.as_str()).collect();
    for (id, _) in semantic {
        if !bm25_rank.contains_key(id.as_str()) {
            all_ids.push(id.as_str());
        }
    }

    // Score pondéré de chaque note candidate (magnitudes normalisées, pas de rang).
    let mut hits: Vec<(String, f64)> = all_ids
        .iter()
        .map(|&id| {
            let score =
                weighted_fusion_score(bm25_score.get(id).copied(), sem_score.get(id).copied());
            (id.to_string(), score)
        })
        .collect();

    // Tri décroissant stable : à score égal, l'ordre d'insertion (BM25 first) est préservé.
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);

    hits.into_iter()
        .map(|(note_id, rrf_score)| {
            // Même marquage que rrf_fuse : sémantique-only SSI absent du ranking BM25.
            let is_semantic_only = !bm25_rank.contains_key(note_id.as_str());
            let bm25_rank_val = bm25_rank
                .get(note_id.as_str())
                .map(|&r| u32::try_from(r).unwrap_or(u32::MAX));
            let sem_rank_val = sem_rank
                .get(note_id.as_str())
                .map(|&r| u32::try_from(r).unwrap_or(u32::MAX));
            RrfHit {
                note_id,
                rrf_score,
                section: String::new(),
                snippet: None,
                title: None,
                is_semantic_only,
                bm25_rank: bm25_rank_val,
                sem_rank: sem_rank_val,
                status: String::new(),
                anchor_ms: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── F-162 critère 6 — court-circuit à bras unique ────────────────────────

    #[test]
    fn single_arm_bm25_only_uses_normalized_bm25_score() {
        let bm25 = vec![("note_A".to_string(), -0.5f64)];
        let semantic: Vec<(String, f32)> = vec![];
        let fused = rrf_fuse_short_circuit(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_A");
        // score = 1/(1+|−0.5|) = 1/1.5 ≈ 0.6667 — la magnitude du bras fait foi.
        assert!(
            (fused[0].rrf_score - (1.0 / 1.5)).abs() < 1e-9,
            "BM25 seul : score normalisé 1/(1+|bm25|) ≈ 0.6667 attendu, got {}",
            fused[0].rrf_score
        );
    }

    #[test]
    fn single_arm_semantic_only_uses_normalized_cosine_score() {
        let bm25: Vec<(String, f64)> = vec![];
        let semantic = vec![("note_B".to_string(), 0.9f32)];
        let fused = rrf_fuse_short_circuit(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].note_id, "note_B");
        assert!(
            (fused[0].rrf_score - (0.9_f32 as f64)).abs() < 1e-12,
            "sémantique seule : score normalisé = cosine 0.9 attendu, got {}",
            fused[0].rrf_score
        );
    }

    #[test]
    fn single_arm_bm25_score_reflects_magnitude_not_rank() {
        // Deux hits BM25 : fort (−0.5) et faible (−20.0). La fusion par rang rendrait
        // 1/60+1/62 ≈ 0.0328 et 1/61+1/62 ≈ 0.0325 — la magnitude est jetée. Le
        // court-circuit doit discriminer : 0.667 vs 0.0476.
        let bm25 = vec![
            ("note_strong".to_string(), -0.5f64),
            ("note_weak".to_string(), -20.0f64),
        ];
        let semantic: Vec<(String, f32)> = vec![];
        let fused = rrf_fuse_short_circuit(&bm25, &semantic, 60.0, 10);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].note_id, "note_strong");
        assert_eq!(fused[1].note_id, "note_weak");
        assert!(
            fused[0].rrf_score > fused[1].rrf_score,
            "la magnitude doit discriminer : strong={} weak={}",
            fused[0].rrf_score,
            fused[1].rrf_score
        );
        assert!(
            (fused[0].rrf_score - (1.0 / 1.5)).abs() < 1e-9,
            "strong = 1/(1+0.5) = 0.6667, got {}",
            fused[0].rrf_score
        );
    }

    #[test]
    fn short_circuit_uses_weighted_fusion_when_both_arms_respond() {
        // Deux bras répondent → la fusion pondérée remplace le RRF pur (critère 10).
        let bm25 = vec![("note_A".to_string(), -0.5f64)];
        let semantic = vec![
            ("note_A".to_string(), 0.9f32),
            ("note_B".to_string(), 0.8f32),
        ];
        let sc = rrf_fuse_short_circuit(&bm25, &semantic, 60.0, 10);
        assert_eq!(sc.len(), 2);
        // note_A : 0.5×normalize_bm25(-0.5) + 0.5×0.9 = 0.5×(1/1.5) + 0.45 = 0.7833…
        // note_B : 0.5×0.8 = 0.4 (bras BM25 absent → contribution 0).
        let a = sc.iter().find(|h| h.note_id == "note_A").unwrap();
        let b = sc.iter().find(|h| h.note_id == "note_B").unwrap();
        // L'attendu utilise la conversion f32 → f64 réelle (0.9f32 as f64 ≈ 0.9000000).
        let expect_a = 0.5 * (1.0 / 1.5) + 0.5 * (0.9f32 as f64);
        let expect_b = 0.5 * (0.8f32 as f64);
        assert!(
            (a.rrf_score - expect_a).abs() < 1e-9,
            "fusion pondérée note_A attendue {expect_a}, got {}",
            a.rrf_score
        );
        assert!(
            (b.rrf_score - expect_b).abs() < 1e-9,
            "fusion pondérée note_B attendue {expect_b}, got {}",
            b.rrf_score
        );
        assert!(
            a.rrf_score > b.rrf_score,
            "les deux bras doivent discriminer"
        );
        // Ordre : note_A (les deux bras) devant note_B (sémantique seule).
        assert_eq!(sc[0].note_id, "note_A");
        assert_eq!(sc[1].note_id, "note_B");
        // Marquage : note_B est sémantique-only (absente de BM25).
        assert!(a.bm25_rank.is_some() && a.sem_rank.is_some());
        assert!(b.bm25_rank.is_none() && b.sem_rank == Some(1));
    }

    #[test]
    fn weighted_fusion_score_is_intrinsic_no_pool_dependence() {
        // Le score d'une note ne dépend PAS du pool : le top-1 à limit=1 est le
        // même que le top-1 à limit=3 (le RRF, lui, re-note selon la taille du pool).
        let bm25 = vec![
            ("note_A".to_string(), -0.5f64),
            ("note_B".to_string(), -2.0f64),
        ];
        let semantic = vec![
            ("note_A".to_string(), 0.9f32),
            ("note_B".to_string(), 0.4f32),
        ];
        let top1 = hybrid_fuse_weighted(&bm25, &semantic, 1);
        let top3 = hybrid_fuse_weighted(&bm25, &semantic, 3);
        assert_eq!(top1[0].note_id, top3[0].note_id, "top-1 invariant du pool");
        assert_eq!(top1[0].rrf_score.to_bits(), top3[0].rrf_score.to_bits());
    }

    #[test]
    fn weighted_fusion_magnitude_discriminates_where_rrf_was_flat() {
        // Deux notes présentes dans les deux bras : l'une forte des deux côtés,
        // l'autre faible des deux côtés. Le RRF pur (k=60) les aurait classées
        // 1/(60+r)+… quasi ex-æquo ; la fusion pondérée les sépare franchement.
        let bm25 = vec![
            ("strong".to_string(), -0.5f64),
            ("weak".to_string(), -20.0f64),
        ];
        let semantic = vec![
            ("strong".to_string(), 0.95f32),
            ("weak".to_string(), 0.10f32),
        ];
        let fused = hybrid_fuse_weighted(&bm25, &semantic, 10);
        assert_eq!(fused[0].note_id, "strong");
        assert_eq!(fused[1].note_id, "weak");
        // Écart relatif franc (le RRF donnait ~2 % ; ici la magnitude domine).
        let spread = (fused[0].rrf_score - fused[1].rrf_score) / fused[0].rrf_score;
        assert!(
            spread > 0.5,
            "la magnitude doit discriminer franchement (spread={spread:.3}), strong={} weak={}",
            fused[0].rrf_score,
            fused[1].rrf_score
        );
    }

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
