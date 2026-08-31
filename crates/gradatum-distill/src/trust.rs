//! Distilled trust scoring.
//!
//! `compute_distill_trust` aggregates the trust of a synthesis note's sources
//! into the base trust of the synthesis: `mean(trust of known sources) × confidence`,
//! clamped to `[0, 1]`. Wired for `Job::Distill` (distillation).
//!
//! The [`TrustLookup`] read interface stays in `gradatum-core` — this crate only
//! computes, it never stores.

use gradatum_core::provenance::TrustLookup;
use ulid::Ulid;

/// Distilled trust = mean(trust of known sources) × confidence, clamped to [0, 1].
///
/// Returns `0.5` (neutral) if no source is known to the index.
///
/// # Parameters
///
/// - `sources`: ULID identifiers of the source notes.
/// - `index`: [`TrustLookup`] implementation for reading scores.
/// - `confidence`: model confidence (0.0–1.0).
///
/// # Behaviour
///
/// - If `sources` is empty or no source has a known score → returns `0.5`.
/// - Otherwise: `clamp(mean(scores) * confidence, 0.0, 1.0)`.
pub fn compute_distill_trust(sources: &[Ulid], index: &dyn TrustLookup, confidence: f32) -> f32 {
    let trusts: Vec<f32> = sources
        .iter()
        .filter_map(|id| index.get_trust(id))
        .collect();

    // Neutre 0.5 si aucune source connue de l'index — pas de multiplication par confidence
    // (0.5 représente l'absence de signal, pas un score à pondérer).
    if trusts.is_empty() {
        return 0.5;
    }

    let mean = trusts.iter().sum::<f32>() / trusts.len() as f32;
    (mean * confidence).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeIndex(HashMap<Ulid, f32>);

    impl TrustLookup for FakeIndex {
        fn get_trust(&self, id: &Ulid) -> Option<f32> {
            self.0.get(id).copied()
        }
    }

    #[test]
    fn distill_empty_sources_is_neutral() {
        let idx = FakeIndex(HashMap::new());
        assert_eq!(compute_distill_trust(&[], &idx, 0.87), 0.5);
    }

    #[test]
    fn distill_empty_sources_confidence_zero_is_neutral() {
        // Même avec confidence=0, sources vides → 0.5 (mean de l'ensemble vide).
        let idx = FakeIndex(HashMap::new());
        assert_eq!(compute_distill_trust(&[], &idx, 0.0), 0.5);
    }

    #[test]
    fn distill_mean_times_confidence() {
        let (a, b) = (Ulid::generate(), Ulid::generate());
        let mut m = HashMap::new();
        m.insert(a, 0.95);
        m.insert(b, 0.75);
        let idx = FakeIndex(m);
        let got = compute_distill_trust(&[a, b], &idx, 0.80);
        // mean(0.95, 0.75) = 0.85 ; 0.85 * 0.80 = 0.68
        assert!(
            (got - 0.85_f32 * 0.80_f32).abs() < 1e-6,
            "attendu ≈ 0.68, obtenu {got}"
        );
    }

    #[test]
    fn distill_clamp_high() {
        // confidence=1.0, score très élevé → pas de dépassement 1.0.
        let id = Ulid::generate();
        let mut m = HashMap::new();
        m.insert(id, 1.0_f32);
        let idx = FakeIndex(m);
        let got = compute_distill_trust(&[id], &idx, 1.0);
        assert!(got <= 1.0, "clamp haut violé : {got}");
        assert!((got - 1.0_f32).abs() < 1e-6);
    }

    #[test]
    fn distill_clamp_low() {
        // confidence=0.0 → tout clampé à 0.0 (sauf sources vides = 0.5 ).
        let id = Ulid::generate();
        let mut m = HashMap::new();
        m.insert(id, 0.95_f32);
        let idx = FakeIndex(m);
        let got = compute_distill_trust(&[id], &idx, 0.0);
        assert!((got - 0.0_f32).abs() < 1e-6, "clamp bas violé : {got}");
    }

    #[test]
    fn distill_unknown_sources_counted_as_missing_not_zero() {
        // Les sources sans trust connu sont ignorées (filter_map), pas comptées comme 0.
        // Donc si toutes les sources sont inconnues → neutre 0.5.
        let (a, b) = (Ulid::generate(), Ulid::generate());
        let idx = FakeIndex(HashMap::new()); // aucune source connue
        let got = compute_distill_trust(&[a, b], &idx, 0.9);
        assert_eq!(got, 0.5, "toutes sources inconnues → neutre 0.5");
    }
}
