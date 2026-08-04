//! Section routing — regex heuristic (Bayesian) + LLM dispatch.
//!
//! Preprocesses the title and body of a note to identify the most probable
//! gradatum section among the 11 canonical sections.
//!
//! ## Routing architecture
//!
//! Two detection layers evaluated in order:
//!
//! 1. **Prefix fast-path**: when the title starts with `[SECTION]` or `[ABBREV]`
//!    (e.g. `[DECISIONS]`, `[RETRO]`, `[ARCH]`), returns that section with
//!    confidence 1.0 immediately. No score threshold — the prefix is a strong,
//!    unambiguous signal. Rationale: `\b` does not match between `[` and a letter
//!    (two non-word chars), so a purely keyword-based heuristic misses prefixes.
//!
//! 2. **Semantic heuristic**: TF-IDF-like keyword frequency over `title + body`.
//!    Falls back to `"reference"` when the signal is ambiguous
//!    (score < 3 or top/second ratio < 1.5).
//!
//! The heuristic runs offline with no network calls (offline-first invariant).
//!
//! ## Hint validation vs heuristic sections
//!
//! [`SECTIONS`] contains only the 11 **auto-classifiable** sections (those the
//! heuristic can produce). It is intentionally distinct from [`is_valid_hint_section`],
//! which delegates to [`gradatum_core::section::Section::ALL`] (13 sections).
//!
//! This separation preserves the **hint-only invariant** for `project-map`: the
//! heuristic can never produce `project-map`, but an explicit `section_hint` can.

use std::cmp::Reverse;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// The 11 auto-classifiable gradatum sections (stable priority order).
///
/// This list covers only sections that the heuristic and LLM can **produce**
/// from note content. It is intentionally a subset of the 13 canonical sections:
/// `"project-map"` is absent because it is **hint-only** — it can only be
/// assigned via an explicit `section_hint`, never by content classification.
///
/// For hint validation (accepting or rejecting a `section_hint`), use
/// [`is_valid_hint_section`] instead, which derives its answer from
/// [`gradatum_core::section::Section::ALL`] (all 13 sections).
pub const SECTIONS: &[&str] = &[
    "decisions",
    "council",
    "architecture",
    "debug",
    "reasoning",
    "feedback",
    "lessons-learned",
    "retrospectives",
    "experiments",
    "agent-issues",
    "reference",
];

/// Returns `true` if `s` is a valid canonical section that may appear in a
/// `section_hint`.
///
/// Derives the answer from [`gradatum_core::section::Section::ALL`] — the single
/// source of truth for the 13 canonical sections — so this function stays correct
/// automatically when new sections are added to [`gradatum_core::section::Section::ALL`].
///
/// This function is **distinct from [`SECTIONS`]**: `SECTIONS` only lists the 11
/// sections that the heuristic can produce. `is_valid_hint_section` also accepts
/// `"project-map"` (hint-only, never auto-classified).
///
/// # Invariant
///
/// `project-map` must never appear in [`SECTIONS`] (would allow heuristic to
/// produce it). It MUST appear in the validation performed by this function
/// (callers can request it explicitly via `section_hint`).
#[must_use]
pub fn is_valid_hint_section(s: &str) -> bool {
    gradatum_core::section::Section::ALL
        .iter()
        .any(|sec| sec.as_str() == s)
}

/// Canonical prefix patterns — strong signal, confidence 1.0.
///
/// Matched against the **title only** (case-insensitive) via `is_match`.
/// A single match routes the note directly without invoking the heuristic.
///
/// Order: more specific (longer abbreviation) before shorter, to avoid future
/// ambiguity if patterns evolve.
static PREFIX_PATTERNS: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    let raw: &[(&str, &str)] = &[
        // decisions
        ("decisions", r"(?i)\[DECISIONS?\]|\[DECIS\]"),
        // council (governance decisions: architecture reviews, policy changes, overrides)
        ("council", r"(?i)\[COUNCIL\]"),
        // architecture
        ("architecture", r"(?i)\[ARCH(?:ITECTURE)?\]"),
        // debug
        ("debug", r"(?i)\[DEBUG\]"),
        // reasoning
        ("reasoning", r"(?i)\[REASON(?:ING)?\]"),
        // feedback
        ("feedback", r"(?i)\[FEEDBACK\]"),
        // lessons-learned (abbrev : LESSONS, LESSON, LESSONS-LEARNED)
        ("lessons-learned", r"(?i)\[LESSONS?(?:-LEARNED)?\]"),
        // retrospectives (abbrev : RETRO, RETROS, RETROSPECTIVE, RETROSPECTIVES)
        ("retrospectives", r"(?i)\[RETROS?(?:PECTIVES?)?\]"),
        // experiments (abbrev : EXP, EXPE, EXPERIMENTS)
        ("experiments", r"(?i)\[EXP(?:ERIMENTS?)?\]|\[EXPE\]"),
        // agent-issues (abbrev : ISSUES, ISSUE, AGENT-ISSUE, AGENT-ISSUES)
        ("agent-issues", r"(?i)\[AGENT-ISSUES?\]|\[ISSUES?\]"),
        // reference (abbrev : REF, REFERENCE)
        ("reference", r"(?i)\[REF(?:ERENCE)?\]"),
    ];
    raw.iter()
        .map(|(section, pattern)| {
            let re = Regex::new(pattern)
                .unwrap_or_else(|e| panic!("invalid prefix pattern for section '{section}': {e}"));
            (*section, re)
        })
        .collect()
});

/// Heuristic patterns — semantic keywords matched over title + body.
///
/// Compiled once. Threshold: `top_score >= 3` AND `top_score >= 1.5 × second_score`.
static KEYWORD_PATTERNS: Lazy<Vec<(&'static str, Vec<Regex>)>> = Lazy::new(|| {
    let raw: &[(&str, &[&str])] = &[
        (
            "decisions",
            &[r"(?i)\b(decisions?|decid|GO|NOK|trade-?off|chose|picked)\b"],
        ),
        (
            "council",
            &[r"(?i)\b(council|verdict|multi.?experts?|d[e\u{e9}]lib[e\u{e9}]ration)\b"],
        ),
        (
            "architecture",
            &[r"(?i)\b(architecture|component|pattern|crate|trait|protocol|module)\b"],
        ),
        (
            "debug",
            &[r"(?i)\b(debug|bug|crash|panic|OOM|fail|error|fix)\b"],
        ),
        (
            "reasoning",
            &[r"(?i)\b(reasoning(?:-pattern)?|consider|hypoth|tradeoff|option|why|because)\b"],
        ),
        (
            "feedback",
            &[r"(?i)\b(feedback|comment|review|critic|praise)\b"],
        ),
        (
            "lessons-learned",
            &[r"(?i)\b(lessons?[\s-]learned|lesson|learned|takeaway|avoid|always)\b"],
        ),
        (
            "retrospectives",
            &[r"(?i)\b(retrospectives?|retro|sprint|phase|what.went.well|to.improve)\b"],
        ),
        (
            "experiments",
            &[r"(?i)\b(experiments?|experiment|POC|spike|benchmark|hypothesis)\b"],
        ),
        (
            "agent-issues",
            &[r"(?i)\b(agent[\s-]issues?|agent|skill.fail|pipeline.error|coord)\b"],
        ),
        (
            "reference",
            &[r"(?i)\b(reference|cheatsheet|config|port|spec)\b"],
        ),
    ];
    raw.iter()
        .map(|(section, patterns)| {
            let regexes: Vec<Regex> = patterns
                .iter()
                .map(|p| {
                    Regex::new(p).unwrap_or_else(|e| {
                        panic!("invalid keyword pattern for section '{section}': {e}")
                    })
                })
                .collect();
            (*section, regexes)
        })
        .collect()
});

/// Routes a note to a section — prefix fast-path then semantic heuristic.
///
/// ## Evaluation order
///
/// 1. When the **title** contains a canonical prefix `[SECTION]` or `[ABBREV]`:
///    returns that section with confidence 1.0 (strong, unambiguous signal).
///
/// 2. Otherwise: semantic heuristic over `title + body` (keyword frequency).
///    Falls back to `"reference"` when the signal is too ambiguous:
///    - top score < 3, or
///    - top score < 1.5× the second score.
///
/// # Parameters
/// - `title`: title of the note
/// - `body`: full body of the note
///
/// # Returns
/// `(section, confidence)` — section is a `&'static str` from [`SECTIONS`].
pub fn heuristic_route(title: &str, body: &str) -> (&'static str, f32) {
    // ── 1. Fast-path : préfixe canonique dans le titre ────────────────────────
    for (section, re) in PREFIX_PATTERNS.iter() {
        if re.is_match(title) {
            return (section, 1.0);
        }
    }

    // ── 2. Heuristique sémantique sur titre + body ────────────────────────────
    let text = format!("{title}\n{body}").to_lowercase();
    let mut scores: Vec<(&'static str, usize)> = KEYWORD_PATTERNS
        .iter()
        .map(|(section, regexes)| {
            let count: usize = regexes.iter().map(|r| r.find_iter(&text).count()).sum();
            (*section, count)
        })
        .collect();

    scores.sort_by_key(|s| Reverse(s.1));

    let total = scores.iter().map(|(_, c)| *c as f32).sum::<f32>().max(1.0);

    let (top_section, top_score) = scores[0];
    let second_score = scores.get(1).map_or(0, |(_, c)| *c);
    let confidence = top_score as f32 / total;

    if top_score >= 3 && top_score as f32 >= 1.5 * second_score as f32 {
        (top_section, confidence)
    } else {
        ("reference", confidence)
    }
}

// ── Type LlmRoutingResponse (L-02) ─────────────────────────────────────

/// Structured JSON response returned by the curator-classifier prompt.
///
/// Expected format: `{"section":"...","tags":[...],"wikilinks":[...],"duplicate_hint":"..."|null}`
///
/// The `tags`, `wikilinks`, and `duplicate_hint` fields are optional with defaults
/// (`#[serde(default)]`) to tolerate LLMs that omit them.
///
/// Used to parse LLM JSON output before mapping it to
/// [`gradatum_chat::backend::CuratorDecision`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRoutingResponse {
    /// Canonical section among the 11 gradatum sections (kebab-case).
    pub section: String,
    /// Extracted tags (2-5, kebab-case). Default: empty vec.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Wikilinks detected in the body (`[[NoteTitle]]`). Default: empty vec.
    #[serde(default)]
    pub wikilinks: Vec<String>,
    /// Title of a potential duplicate, or `null`. Default: `None`.
    #[serde(default)]
    pub duplicate_hint: Option<String>,
}

impl LlmRoutingResponse {
    /// Maps the LLM response to a [`gradatum_chat::backend::CuratorDecision`].
    ///
    /// The section is passed through as-is (kebab-case string). Validation
    /// against the 11 canonical sections is the caller's responsibility.
    ///
    /// # Side effects
    ///
    /// None — pure type transformation.
    pub fn into_curator_decision(self) -> gradatum_chat::backend::CuratorDecision {
        gradatum_chat::backend::CuratorDecision {
            section: self.section,
            tags: self.tags,
            wikilinks: self.wikilinks,
            duplicate_hint: self.duplicate_hint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug 1 regression : `[DECISIONS]` ne matchait pas avec `\b` seul
    /// car `[` et `D` sont deux non-word chars — word-boundary inefficace.
    /// Fix : fast-path préfixe détecte `[DECISIONS]` avant l'heuristique scorée.
    #[test]
    fn route_with_decisions_prefix() {
        let (section, conf) = heuristic_route(
            "[DECISIONS][gradatum] T10 audit JSONL DONE",
            "Some body content.",
        );
        assert_eq!(
            section, "decisions",
            "Le préfixe [DECISIONS] doit router vers la section 'decisions'"
        );
        assert_eq!(conf, 1.0, "Confiance doit être 1.0 sur préfixe canonique");
    }

    #[test]
    fn route_with_retro_prefix() {
        let (section, conf) = heuristic_route(
            "[RETRO][my-project] Sprint 5 Étape 4 DONE",
            "Sprint review.",
        );
        assert_eq!(
            section, "retrospectives",
            "Le préfixe [RETRO] doit router vers la section 'retrospectives'"
        );
        assert_eq!(conf, 1.0);
    }

    #[test]
    fn route_with_lessons_prefix() {
        let (section, conf) = heuristic_route(
            "[LESSONS][infra] postfix smtp port 25 blocked",
            "Lesson learned from incident.",
        );
        assert_eq!(
            section, "lessons-learned",
            "Le préfixe [LESSONS] doit router vers la section 'lessons-learned'"
        );
        assert_eq!(conf, 1.0);
    }

    /// Vérifie que le routage sémantique sans préfixe fonctionne toujours.
    #[test]
    fn route_semantic_without_prefix() {
        let (section, _conf) = heuristic_route(
            "Crash OOM lors de l'ingest frwiki",
            "bug identifié: OOM dans le pipeline, fix en cours, panic sur unwrap.",
        );
        assert_eq!(
            section, "debug",
            "Les mots-clés sémantiques sans préfixe doivent continuer à router correctement"
        );
    }

    /// Vérifie le pattern [FEEDBACK] explicite.
    #[test]
    fn route_with_feedback_prefix() {
        let (section, conf) = heuristic_route(
            "[FEEDBACK][example-app] latence élevée sur search",
            "review: la latence dépasse 500ms.",
        );
        assert_eq!(section, "feedback");
        assert_eq!(conf, 1.0);
    }

    /// Vérifie le pattern [ARCH] abrégé.
    #[test]
    fn route_with_arch_prefix() {
        let (section, conf) = heuristic_route(
            "[ARCH][gradatum] design crate gradatum-server",
            "module principal, protocol HTTP, trait Handler.",
        );
        assert_eq!(section, "architecture");
        assert_eq!(conf, 1.0);
    }

    /// Le préfixe `[COUNCIL]` déclenche un routage direct vers la section dédiée, confiance 1.0.
    #[test]
    fn route_with_council_prefix() {
        let (section, conf) = heuristic_route(
            "[COUNCIL][my-project] verdict GO section council — 2026-05-09",
            "Verdict council : GO. Trois experts consultés.",
        );
        assert_eq!(
            section, "council",
            "Le préfixe [COUNCIL] doit router vers la section 'council'"
        );
        assert_eq!(conf, 1.0, "Confiance doit être 1.0 sur préfixe canonique");
    }

    /// Les mots-clés de section sémantiques sans préfixe `[...]` déclenchent le routage heuristique.
    #[test]
    fn route_with_council_keyword() {
        let (section, _conf) = heuristic_route(
            "Délibération experts project",
            "council verdict multi-experts délibération council members",
        );
        assert_eq!(
            section, "council",
            "Les mots-clés 'council verdict multi-experts délibération' doivent router vers 'council'"
        );
    }

    // ── Tests is_valid_hint_section (invariant hint-only project-map) ────────────

    /// `is_valid_hint_section` accepte les 13 sections canoniques, y compris "project-map" et "identity".
    #[test]
    fn is_valid_hint_accepts_all_12_canonical_sections() {
        let all_canonical = [
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
            "project-map",
        ];
        for section in all_canonical {
            assert!(
                is_valid_hint_section(section),
                "is_valid_hint_section('{section}') doit retourner true pour une section canonique"
            );
        }
    }

    /// `is_valid_hint_section` rejette les valeurs invalides.
    #[test]
    fn is_valid_hint_rejects_invalid_values() {
        let invalids = ["", "foo", "project_map", "PROJECT-MAP", "decision", "refs"];
        for val in invalids {
            assert!(
                !is_valid_hint_section(val),
                "is_valid_hint_section('{val}') doit retourner false"
            );
        }
    }

    /// `SECTIONS` (heuristique) ne contient PAS "project-map" — invariant hint-only.
    ///
    /// Ce test gèle le contrat : l'heuristique ne peut jamais produire "project-map"
    /// par contenu. Si SECTIONS contenait "project-map", l'invariant serait violé.
    #[test]
    fn sections_does_not_contain_project_map() {
        assert!(
            !SECTIONS.contains(&"project-map"),
            "SECTIONS (heuristique) ne doit PAS contenir 'project-map' (hint-only invariant)"
        );
    }
}
