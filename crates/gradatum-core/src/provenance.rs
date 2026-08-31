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
//!
//! `compute_distill_trust` has moved to `gradatum-distill`: this module keeps
//! the static scores and the read interface, the aggregation computation lives in
//! the distillation crate.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
