//! Fonctions de scoring auxiliaires pour le ranking multi-facteur.
//!
//! ## Formules
//!
//! ### Recency decay
//! `recency_factor(t_note, t_now) = exp(-λ × days_old)`
//! avec λ = 0.01 (demi-vie ≈ 69 jours).
//!
//! | Âge note | Factor |
//! |---|---|
//! | 0 jours  | 1.000 |
//! | 7 jours  | 0.932 |
//! | 30 jours | 0.741 |
//! | 90 jours | 0.407 |
//! | 1 an     | 0.026 |
//!
//! ### PageRank in-degree normalisé (clampé)
//! `pagerank_factor(in_degree) = (in_degree / (in_degree + NORM_CONST)).clamp(0.0, 1.0)`
//! NORM_CONST = 5 : une note avec 5 backlinks → factor = 0.5.
//! Le clamp garantit `factor ∈ [0.0, 1.0]` même pour des valeurs extrêmes (cf. caveat B-P0-1).
//!
//! ### Score composite
//! `score = rrf_score × (1 + α × recency) × (1 + β × pagerank)`
//! α = 0.2 (recency), β = 0.1 (in-degree) — conservateurs, A/B test post-alpha.12.

/// Facteur de décroissance temporelle (recency).
///
/// Retourne 1.0 pour une note créée maintenant, décroît exponentiellement.
/// Les timestamps futurs (horloge dérivée) sont clampés à 0 jours → 1.0.
///
/// # Paramètres
/// - `note_created_ms` : timestamp de création de la note en epoch ms UTC
/// - `now_ms` : timestamp courant en epoch ms UTC
///
/// # Valeur retournée
/// `f64` dans `(0.0, 1.0]` — jamais 0.0 (`exp(x) > 0` pour tout x réel).
#[must_use]
pub fn recency_factor(note_created_ms: i64, now_ms: i64) -> f64 {
    const LAMBDA: f64 = 0.01; // demi-vie ≈ 69 jours
    const MS_PER_DAY: f64 = 86_400_000.0;
    let delta_ms = (now_ms - note_created_ms).max(0);
    let days_old = (delta_ms as f64) / MS_PER_DAY;
    (-LAMBDA * days_old).exp()
}

/// Facteur PageRank in-degree normalisé, clampé strictement dans `[0.0, 1.0]`.
///
/// `(in_degree / (in_degree + NORM_CONST)).clamp(0.0, 1.0)` → `[0.0, 1.0]`.
/// Le clamp est défensif : la formule de base est mathématiquement bornée par 1.0,
/// mais le clamp documente l'invariant API et protège contre un futur changement
/// de formule (caveat B-P0-1, council Round 1).
///
/// Notes sans backlinks → `0.0`. Notes très liées (>>100) → proche de `1.0`.
///
/// # Valeur retournée
/// `f64` dans `[0.0, 1.0]`.
///
/// # Note
///
/// La signature `u64` rend le paramètre négatif compile-impossible
/// (caveat L-rev2-1) — la borne inférieure 0.0 est garantie par construction.
#[must_use]
pub fn pagerank_factor(in_degree: u64) -> f64 {
    const NORM_CONST: f64 = 5.0;
    let deg = in_degree as f64;
    let raw = deg / (deg + NORM_CONST);
    raw.clamp(0.0, 1.0)
}

/// Score composite multi-facteur.
///
/// Formule : `rrf_score × (1 + α × recency) × (1 + β × pagerank)`
/// avec α = 0.2 (recency boost max 20%) et β = 0.1 (pagerank boost max 10%).
///
/// Si `recency = 0.0` et `pagerank = 0.0`, retourne `rrf_score` inchangé.
/// Boost maximum : `rrf_score × 1.2 × 1.1 = rrf_score × 1.32`.
#[must_use]
pub fn composite_score(rrf_score: f64, recency: f64, pagerank: f64) -> f64 {
    const ALPHA: f64 = 0.2;
    const BETA: f64 = 0.1;
    rrf_score * (1.0 + ALPHA * recency) * (1.0 + BETA * pagerank)
}

#[cfg(test)]
mod tests {
    use super::*;

    // T11-1 : Note fraîche → recency_factor = 1.0
    #[test]
    fn recency_factor_fresh_note_returns_one() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let f = recency_factor(now_ms, now_ms);
        assert!(
            (f - 1.0).abs() < 0.001,
            "note créée maintenant → factor ≈ 1.0, got {f}"
        );
    }

    // T11-2 : Note vieille de 30 jours → decay visible
    #[test]
    fn recency_factor_decays_over_30_days() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let thirty_days_ago = now_ms - 30 * 24 * 3_600_000i64;
        let f = recency_factor(thirty_days_ago, now_ms);
        // exp(-0.01 * 30) ≈ 0.7408
        assert!(f > 0.70 && f < 0.76, "30j → factor ≈ 0.74, got {f}");
    }

    // T11-3 : Note vieille d'un an → decay fort mais positif
    #[test]
    fn recency_factor_one_year_is_positive_and_small() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let one_year_ago = now_ms - 365 * 24 * 3_600_000i64;
        let f = recency_factor(one_year_ago, now_ms);
        // exp(-0.01 * 365) ≈ 0.0257
        assert!(f > 0.01 && f < 0.05, "1an → factor ≈ 0.026, got {f}");
    }

    // T11-4 : Timestamp futur (horloge dérivée) → clampé à 1.0
    #[test]
    fn recency_factor_future_timestamp_clamped_to_one() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let future = now_ms + 86_400_000i64; // +1 jour
        let f = recency_factor(future, now_ms);
        assert!(
            (f - 1.0).abs() < 0.001,
            "timestamp futur → factor = 1.0 (clampé), got {f}"
        );
    }

    // T11-5 : Monotonie — note plus ancienne = factor plus petit
    #[test]
    fn recency_factor_is_monotonically_decreasing() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let f_7d = recency_factor(now_ms - 7 * 24 * 3_600_000i64, now_ms);
        let f_30d = recency_factor(now_ms - 30 * 24 * 3_600_000i64, now_ms);
        let f_90d = recency_factor(now_ms - 90 * 24 * 3_600_000i64, now_ms);
        assert!(
            f_7d > f_30d && f_30d > f_90d,
            "monotonie décroissante requise"
        );
    }

    // T11-6 : pagerank_factor — note sans backlinks → 0.0
    #[test]
    fn pagerank_factor_zero_backlinks_returns_zero() {
        let f = pagerank_factor(0);
        assert!(
            (f - 0.0).abs() < f64::EPSILON,
            "0 backlinks → pagerank = 0.0"
        );
    }

    // T11-7 : pagerank_factor — 5 backlinks → 0.5 (normalization_constant = 5)
    #[test]
    fn pagerank_factor_five_backlinks_returns_half() {
        let f = pagerank_factor(5);
        assert!(
            (f - 0.5).abs() < 0.001,
            "5 backlinks avec norm=5 → factor = 0.5, got {f}"
        );
    }

    // T11-8 : pagerank_factor — borne supérieure < 1.0 même avec beaucoup de backlinks
    #[test]
    fn pagerank_factor_bounded_below_one() {
        let f = pagerank_factor(10_000);
        assert!(
            f < 1.0 && f > 0.99,
            "10k backlinks → factor ≈ 0.9999.., got {f}"
        );
    }

    // T11-8b : pagerank_factor clampé strictement dans [0.0, 1.0] — caveat B-P0-1
    #[test]
    fn pagerank_factor_is_strictly_in_zero_one_range() {
        // u64::MAX ne peut pas faire dépasser 1.0 grâce au clamp explicite.
        let f = pagerank_factor(u64::MAX);
        assert!(
            (0.0..=1.0).contains(&f),
            "pagerank_factor doit être ∈ [0.0, 1.0], got {f}"
        );
    }

    // T11-9 : composite_score — sans bonus → rrf_score inchangé
    #[test]
    fn composite_score_no_bonus_equals_rrf() {
        // recency=0.0, pagerank=0.0 → (1+0.2×0)×(1+0.1×0) = 1.0 → score = rrf
        let rrf = 0.0322;
        let cs = composite_score(rrf, 0.0, 0.0);
        assert!(
            (cs - rrf).abs() < 1e-9,
            "pas de bonus → composite = rrf, got {cs}"
        );
    }

    // T11-10 : composite_score — bonus maximum reasonable
    #[test]
    fn composite_score_with_full_bonus_is_bounded() {
        // recency=1.0 (note d'aujourd'hui), pagerank=1.0 (clampé)
        // composite = rrf × 1.2 × 1.1 = rrf × 1.32
        let rrf = 0.0322;
        let cs = composite_score(rrf, 1.0, 1.0);
        let expected = rrf * 1.2 * 1.1;
        assert!(
            (cs - expected).abs() < 1e-9,
            "bonus max: rrf×1.32, got {cs} vs {expected}"
        );
    }
}
