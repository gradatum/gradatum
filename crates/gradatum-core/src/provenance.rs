//! Provenance trust scores.
//!
//! Trust values are written and stored; consumption (provenance-based decay)
//! is reserved for a future release.
//!
//! ## Design
//!
//! - `TRUST_SCORES`: 4 static sources with fixed scores.
//! - `TrustLookup`: narrow synchronous trait avoiding a circular dependency
//!   `gradatum-core → gradatum-index`. Implemented by `SqliteIndex` in `gradatum-index`.
//! - `compute_distill_trust`: wired for distillation (`Job::Distill`).
//!
//! ## Deferred sources
//!
//! - `"human-validated"` (0.90) → planned for a future `Job::Validate`.

use ulid::Ulid;

/// Static provenance sources with their fixed trust scores.
///
/// `"distilled"` (0.60) is the provenance of synthesis notes produced by `Job::Distill`:
/// positioned between `agent-log` (0.50, raw agent log) and `qa-event` (0.75, interaction-
/// validated event). A distilled synthesis aggregates multiple sources, giving it higher
/// trust than a raw log without reaching the level of a directly validated event.
///
/// Deferred source: `"human-validated"` (0.90) is planned for a future `Job::Validate`.
pub const TRUST_SCORES: &[(&str, f32)] = &[
    ("human-decision", 0.95),
    ("qa-event", 0.75),
    ("distilled", 0.60),
    ("agent-log", 0.50),
    ("web-scraped", 0.35),
];

/// Minimal read-only trait for a note's trust score.
///
/// Narrow synchronous trait — avoids a circular dependency `gradatum-core → gradatum-index`.
/// Implemented by `SqliteIndex` in `gradatum-index`.
///
/// Returns `None` if the note is absent or its trust has not been set.
pub trait TrustLookup {
    /// Returns the trust score for the note identified by `id`, or `None` if unknown.
    fn get_trust(&self, id: &Ulid) -> Option<f32>;
}

/// Resolves provenance from an optional `section_hint`.
///
/// If `section_hint ∈ TRUST_SCORES` → returns `section_hint` as-is.
/// Otherwise (or if absent) → returns `"agent-log"` (conservative default).
///
/// # Examples
///
/// ```
/// use gradatum_core::provenance::resolve_provenance;
/// assert_eq!(resolve_provenance(Some("human-decision")), "human-decision");
/// assert_eq!(resolve_provenance(Some("qa-event")),       "qa-event");
/// assert_eq!(resolve_provenance(None),                   "agent-log");
/// assert_eq!(resolve_provenance(Some("unknown")),        "agent-log");
/// ```
pub fn resolve_provenance(section_hint: Option<&str>) -> &'static str {
    match section_hint {
        Some(hint) => TRUST_SCORES
            .iter()
            .find(|(k, _)| *k == hint)
            .map(|(k, _)| *k)
            .unwrap_or("agent-log"),
        None => "agent-log",
    }
}

/// Returns the static trust score for a known provenance, or `None` if unknown.
///
/// # Examples
///
/// ```
/// use gradatum_core::provenance::trust_for;
/// assert_eq!(trust_for("human-decision"), Some(0.95));
/// assert_eq!(trust_for("agent-log"),      Some(0.50));
/// assert_eq!(trust_for("unknown"),         None);
/// ```
pub fn trust_for(provenance: &str) -> Option<f32> {
    TRUST_SCORES
        .iter()
        .find(|(k, _)| *k == provenance)
        .map(|(_, v)| *v)
}

/// Distilled trust = mean(trust of known sources) × confidence, clamped to [0, 1].
///
/// Returns `0.5` (neutral) if no source is known to the index.
///
/// Wired for `Job::Distill` (distillation).
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
    fn trust_scores_known_values() {
        assert_eq!(trust_for("human-decision"), Some(0.95));
        assert_eq!(trust_for("qa-event"), Some(0.75));
        assert_eq!(trust_for("distilled"), Some(0.60)); // F-22 — synthèse distillée
        assert_eq!(trust_for("agent-log"), Some(0.50));
        assert_eq!(trust_for("web-scraped"), Some(0.35));
        assert_eq!(trust_for("unknown"), None);
        assert_eq!(trust_for("human-validated"), None); // F-43 v0.5.0 — différé
    }

    /// `"distilled"` est ordonné entre `agent-log` (0.50) et `qa-event` (0.75).
    #[test]
    fn distilled_trust_between_agent_log_and_qa_event() {
        let distilled = trust_for("distilled").expect("distilled doit exister");
        let agent_log = trust_for("agent-log").expect("agent-log doit exister");
        let qa_event = trust_for("qa-event").expect("qa-event doit exister");
        assert!(
            agent_log < distilled && distilled < qa_event,
            "distilled ({distilled}) doit être dans ]agent-log ({agent_log}), qa-event ({qa_event})["
        );
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
