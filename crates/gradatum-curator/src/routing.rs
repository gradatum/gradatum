//! Routage de section — heuristique regex (Bayesian) + dispatch LLM.
//!
//! Préprocesse le titre et le corps d'une note pour identifier la section
//! gradatum la plus probable parmi les 11 sections canoniques.
//!
//! ## Architecture du routage
//!
//! Deux couches de détection, évaluées dans l'ordre :
//!
//! 1. **Fast-path préfixe** : si le titre commence par `[SECTION]` ou `[ABBREV]`
//!    (ex. `[DECISIONS]`, `[RETRO]`, `[ARCH]`), on retourne directement cette
//!    section avec confiance 1.0. Pas de seuil de score — le préfixe est un signal
//!    fort et non ambigu. Motif : `\b` ne matche pas entre `[` et une lettre
//!    (deux non-word chars), donc une heuristique purement keyword-based ne détecte
//!    pas les préfixes.
//!
//! 2. **Heuristique sémantique** : fréquence de mots-clés TF-IDF-like sur
//!    `titre + body`. Retombe sur `"reference"` si le signal est ambigu
//!    (score < 3 ou ratio top/second < 1.5).
//!
//! L'heuristique est exécutée offline, sans appel réseau (invariant offline-first R1).

use std::cmp::Reverse;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Les 11 sections canoniques de gradatum (ordre de priorité stable).
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

/// Patterns de préfixe canoniques — signal fort, confiance 1.0.
///
/// Matchés sur le **titre seulement** (insensible à la casse) via `is_match`.
/// Un seul match suffit à router la note sans passer par l'heuristique.
///
/// Ordre : plus spécifique (abréviation longue) avant plus court pour éviter
/// les ambiguïtés futures si les patterns évoluent.
static PREFIX_PATTERNS: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    let raw: &[(&str, &str)] = &[
        // decisions
        ("decisions", r"(?i)\[DECISIONS?\]|\[DECIS\]"),
        // council (governance decisions: architecture reviews, constitution changes, leader-overrides)
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
            let re = Regex::new(pattern).unwrap_or_else(|e| {
                panic!("Pattern préfixe invalide pour section '{section}': {e}")
            });
            (*section, re)
        })
        .collect()
});

/// Patterns heuristiques — mots-clés sémantiques sur titre + body.
///
/// Compilés une seule fois. Seuil : `top_score >= 3` ET `top_score >= 1.5 × second_score`.
static KEYWORD_PATTERNS: Lazy<Vec<(&'static str, Vec<Regex>)>> = Lazy::new(|| {
    let raw: &[(&str, &[&str])] = &[
        (
            "decisions",
            &[r"(?i)\b(decisions?|decid|GO|NOK|trade-?off|chose|picked)\b"],
        ),
        (
            "council",
            &[
                r"(?i)\b(council|art15bis|art19|art18|leader.?override|verdict|multi.?experts?|d[ée]lib[ée]ration)\b",
            ],
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
                        panic!("Pattern keyword invalide pour section '{section}': {e}")
                    })
                })
                .collect();
            (*section, regexes)
        })
        .collect()
});

/// Heuristique de routage — fast-path préfixe + heuristique sémantique.
///
/// ## Ordre d'évaluation
///
/// 1. Si le **titre** contient un préfixe canonique `[SECTION]` ou `[ABBREV]` :
///    retourner cette section avec confiance 1.0 (signal fort, non ambigu).
///
/// 2. Sinon : heuristique sémantique sur `titre + body` (fréquence de mots-clés).
///    Retombe sur `"reference"` si le signal est trop ambigu :
///    - le top score est < 3, ou
///    - le top score n'est pas ≥ 1.5× le deuxième score.
///
/// # Paramètres
/// - `title` : titre de la note
/// - `body`  : corps complet de la note
///
/// # Retour
/// `(section, confidence)` — section est une `&'static str` parmi [`SECTIONS`].
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

/// Réponse JSON structurée retournée par le prompt curator-classifier-v1.
///
/// Format attendu : `{"section":"...","tags":[...],"wikilinks":[...],"duplicate_hint":"..."|null}`
///
/// Les champs `tags`, `wikilinks` et `duplicate_hint` sont optionnels avec valeur par défaut
/// (`#[serde(default)]`) pour tolérer les LLMs qui les omettent.
///
/// ## Caveat L-02
///
/// Ce type est introduit en T6 P2.0c pour parser la sortie JSON du LLM avant de la
/// mapper vers [`gradatum_chat::backend::CuratorDecision`]. Il n'existait pas en alpha.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRoutingResponse {
    /// Section canonique parmi les 11 sections gradatum (kebab-case).
    pub section: String,
    /// Tags extraits (2-5, kebab-case). Défaut : vec vide.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Wikilinks détectés dans le body (`[[NoteTitle]]`). Défaut : vec vide.
    #[serde(default)]
    pub wikilinks: Vec<String>,
    /// Titre d'un doublon potentiel, ou `null`. Défaut : None.
    #[serde(default)]
    pub duplicate_hint: Option<String>,
}

impl LlmRoutingResponse {
    /// Mappe la réponse LLM vers un [`gradatum_chat::backend::CuratorDecision`].
    ///
    /// La section est transmise telle quelle (string kebab-case). La validation
    /// contre les 11 sections canoniques est à la charge de l'appelant.
    ///
    /// # Effets de bord
    ///
    /// Aucun — pure transformation de types.
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

    /// Vérifie que le préfixe [COUNCIL] route vers la section 'council' avec confiance 1.0.
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

    /// Vérifie que l'heuristique sémantique route un body contenant des mots-clés council.
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
}
