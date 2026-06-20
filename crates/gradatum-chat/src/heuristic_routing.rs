//! `HeuristicBackend` — offline section classifier by regex/keywords.
//!
//! ## Implementation note — cyclic dependency avoided
//!
//! `gradatum-curator` depends on `gradatum-chat` (for the `Chat` trait). If
//! `gradatum-chat` depended on `gradatum-curator` to call `heuristic_route`,
//! a cycle forbidden by Cargo would form.
//!
//! Chosen solution: **duplication of `heuristic_route`** (~35 lines) in this
//! module. The logic is identical to `gradatum-curator::routing::heuristic_route`
//! — both must be kept in sync when routing rules change.
//!
//! Rejected alternatives:
//! - (a) feature-gating `gradatum-curator` inside `gradatum-chat`: adds complexity
//!   with no real gain (same cycle during development).
//! - (b) a shared L0 crate `gradatum-routing-rules`: over-engineering for ~35 lines.

use std::cmp::Reverse;
use std::collections::HashMap;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::backend::{CuratorDecision, LlmBackend, LlmError};

// --- Sections canoniques ---

const SECTIONS: &[&str] = &[
    "decisions",
    "architecture",
    "debug",
    "reasoning",
    "feedback",
    "lessons-learned",
    "retrospectives",
    "experiments",
    "agent-issues",
    "reference",
    "council",
];

// --- Patterns compilés une seule fois ---

/// Keywords per section. Each entry is a regex pattern.
///
/// NOTE: keep in sync with `gradatum-curator::routing::KEYWORDS` on any change.
static KEYWORDS: Lazy<HashMap<&'static str, Vec<&'static str>>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert(
        "decisions",
        vec![r"\b(decid|GO|NOK|trade-?off|chose|picked)\b"],
    );
    m.insert(
        "architecture",
        vec![r"\b(component|pattern|crate|trait|protocol|module)\b"],
    );
    m.insert("debug", vec![r"\b(bug|crash|panic|OOM|fail|error|fix)\b"]);
    m.insert(
        "reasoning",
        vec![r"\b(consider|hypoth|tradeoff|option|why|because)\b"],
    );
    m.insert(
        "feedback",
        vec![r"\b(feedback|comment|review|critic|praise)\b"],
    );
    m.insert(
        "lessons-learned",
        vec![r"\b(lesson|learned|takeaway|avoid|always)\b"],
    );
    m.insert(
        "retrospectives",
        vec![r"\b(retro|sprint|phase|what.went.well|to.improve)\b"],
    );
    m.insert(
        "experiments",
        vec![r"\b(experiment|test|POC|spike|benchmark|hypothesis)\b"],
    );
    m.insert(
        "agent-issues",
        vec![r"\b(agent|skill.fail|pipeline.error|coord)\b"],
    );
    m.insert(
        "reference",
        vec![r"\b(reference|cheatsheet|config|port|spec)\b"],
    );
    m.insert(
        "council",
        vec![r"\[COUNCIL\]", r"(?i)\b(verdict|council|round|approved)\b"],
    );
    m
});

/// Routes a note to a section and returns a confidence score.
///
/// Algorithm: counts regex hits per section, then normalises.
/// Falls back to `"reference"` when scoring is too ambiguous.
///
/// NOTE: keep in sync with `gradatum-curator::routing::heuristic_route` on any change.
fn heuristic_route_inline(title: &str, body: &str) -> (&'static str, f32) {
    let text = format!("{title}\n{body}").to_lowercase();

    let mut scores: Vec<(&'static str, usize)> = SECTIONS
        .iter()
        .map(|s| {
            let empty: Vec<&str> = vec![];
            let kws = KEYWORDS.get(*s).unwrap_or(&empty);
            let count: usize = kws
                .iter()
                .map(|kw| {
                    Regex::new(kw)
                        .map(|r| r.find_iter(&text).count())
                        .unwrap_or(0)
                })
                .sum();
            (*s, count)
        })
        .collect();

    scores.sort_by_key(|s| Reverse(s.1));

    let total = scores.iter().map(|(_, c)| *c as f32).sum::<f32>().max(1.0);
    let (top_section, top_score) = scores[0];
    let confidence = top_score as f32 / total;

    // N'affirme la section que si le top est dominant (≥3 hits, ≥1.5× le second)
    if top_score >= 3 && top_score as f32 >= 1.5 * scores.get(1).map_or(0, |(_, c)| *c) as f32 {
        (top_section, confidence)
    } else {
        // Ambigu → fallback reference
        ("reference", confidence)
    }
}

/// Offline heuristic backend — classifies a note by regex.
///
/// Makes no network calls. Serves as the default fallback when no LLM backend
/// is configured, and as the circuit-breaker fallback when the primary LLM backend
/// is unavailable.
pub struct HeuristicBackend;

#[async_trait]
impl LlmBackend for HeuristicBackend {
    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn is_local(&self) -> bool {
        true
    }

    async fn classify(&self, _system: &str, user: &str) -> Result<CuratorDecision, LlmError> {
        // Extraction du titre et du body depuis le prompt utilisateur.
        // Format attendu : "Classify this note.\nTitle: {title}\nBody (truncated to 500 chars): {body}"
        let (title, body) = if let Some(rest) = user.strip_prefix("Classify this note.\nTitle:") {
            if let Some((title_part, body_part)) = rest.split_once("\nBody") {
                let body = body_part
                    .trim_start_matches(" (truncated to 500 chars):")
                    .trim_start_matches(" (first 500 chars):")
                    .trim_start_matches(':')
                    .trim();
                (title_part.trim(), body)
            } else {
                (rest.trim(), "")
            }
        } else {
            // Fallback : traite tout le texte comme body
            ("", user)
        };

        let (section, _confidence) = heuristic_route_inline(title, body);

        Ok(CuratorDecision {
            section: section.to_string(),
            tags: vec![],
            wikilinks: vec![],
            duplicate_hint: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_decisions_keyword() {
        let b = HeuristicBackend;
        // Le seuil exige top_score ≥ 3 ET top ≥ 1.5 × second.
        // Matches "decisions" : "chose" + "picked" + "trade-off" = 3 hits.
        // Texte en lowercase après normalisation : les patterns matchent directement.
        let result = b
            .classify(
                "system",
                "Classify this note.\nTitle: JWT TTL trade-off analysis\nBody (truncated to 500 chars): We chose Ed25519 and picked this approach after the trade-off evaluation.",
            )
            .await
            .unwrap();
        assert_eq!(result.section, "decisions");
    }

    #[tokio::test]
    async fn routes_fallback_reference_on_ambiguous() {
        let b = HeuristicBackend;
        let result = b
            .classify(
                "system",
                "Classify this note.\nTitle: Short\nBody (truncated to 500 chars): this is a short note",
            )
            .await
            .unwrap();
        assert_eq!(result.section, "reference");
    }

    /// Le préfixe [COUNCIL] dans le titre doit router vers la section `council`.
    #[tokio::test]
    async fn routes_council_prefix_in_title() {
        let b = HeuristicBackend;
        let result = b
            .classify(
                "system",
                "Classify this note.\nTitle: [COUNCIL] verdict approved\nBody (truncated to 500 chars): verdict council: GO. round 2. council approved. round verdict round.",
            )
            .await
            .unwrap();
        assert_eq!(
            result.section, "council",
            "Le préfixe [COUNCIL] doit router vers 'council'"
        );
    }

    /// Les mots-clés `verdict` + `council` + `round` + `approved` doivent router vers `council`.
    #[tokio::test]
    async fn routes_council_keywords() {
        let b = HeuristicBackend;
        let result = b
            .classify(
                "system",
                "Classify this note.\nTitle: council decision\nBody (truncated to 500 chars): verdict council round approved — council verdict round approved council",
            )
            .await
            .unwrap();
        assert_eq!(
            result.section, "council",
            "Les mots-clés council/verdict/round/approved doivent router vers 'council'"
        );
    }
}
