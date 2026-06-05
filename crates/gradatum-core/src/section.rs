//! Les 10 sections canoniques de Gradatum.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.4.
//!
//! Ces sections constituent la hiérarchie sémantique du store. Chaque note est assignée
//! à exactement une section. Les sections sont stable-stables : pas d'ajout sans council
//! (invariant C5 gouvernance crates).

use serde::{Deserialize, Serialize};

/// Section canonique d'une note Gradatum.
///
/// 10 sections fixes représentant les catégories sémantiques du knowledge store.
/// Serialisée en `kebab-case` dans les frontmatters YAML et les APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    /// Décisions architecturales, arbitrages, choix techniques.
    Decisions,
    /// Documentation d'architecture, schémas, topologies.
    Architecture,
    /// Debug : post-mortems, root-cause analysis, traces.
    Debug,
    /// Raisonnements, chaînes de pensée, analyses structurées.
    Reasoning,
    /// Feedback : retours opérationnels, enseignements d'usage.
    Feedback,
    /// Leçons apprises : patterns récurrents, erreurs évitées.
    LessonsLearned,
    /// Rétrospectives : bilans de phase, résumés de session.
    Retrospectives,
    /// Expériences : prototypes, POCs, benchmarks exploratoires.
    Experiments,
    /// Issues agents : bugs, anomalies, dettes techniques.
    AgentIssues,
    /// Références : pointeurs vers docs externes, specs, RFCs.
    Reference,
}

impl Section {
    /// Toutes les sections canoniques, dans l'ordre déclaré.
    pub const ALL: [Section; 10] = [
        Section::Decisions,
        Section::Architecture,
        Section::Debug,
        Section::Reasoning,
        Section::Feedback,
        Section::LessonsLearned,
        Section::Retrospectives,
        Section::Experiments,
        Section::AgentIssues,
        Section::Reference,
    ];

    /// Représentation chaîne kebab-case (identique à la sérialisation serde).
    pub fn as_str(&self) -> &'static str {
        match self {
            Section::Decisions => "decisions",
            Section::Architecture => "architecture",
            Section::Debug => "debug",
            Section::Reasoning => "reasoning",
            Section::Feedback => "feedback",
            Section::LessonsLearned => "lessons-learned",
            Section::Retrospectives => "retrospectives",
            Section::Experiments => "experiments",
            Section::AgentIssues => "agent-issues",
            Section::Reference => "reference",
        }
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── CoALA scoring-only — mappings déterministes section → c_kind / doc_kind ──
//
// Spec ref : F-42 c-prime (v0.3.0 Tranche D, design spec 2026-06-01).
// Usage scoring : DIFFÉRÉ v0.4.0 (F-17). Ces fonctions capturent la métadonnée
// sans modifier aucun comportement de search/scoring (composite α/β inchangé).
//
// Source de vérité : v81 §17 + mapping c-prime (tableau §"Mapping const").
// Le backfill SQL (migration 0008) DOIT produire les mêmes valeurs pour les 10
// sections connues de l'enum — voir tests `c_kind_matches_backfill_sql` ci-dessous.
//
// NOTE drift §10a-vs-enum : `council` (11e section §10a) n'est PAS un variant de l'enum
// `Section`. Une note section_hint="council" tombe en fallback `Section::Reference`
// (dispatch.rs) → stockée `section="reference"` en DB (donc c_kind="semantic", PAS
// "episodic"). Le `WHEN 'council'` du backfill SQL (migration 0008) ne matche aucune
// ligne du pipeline normal — il ne couvre qu'une insertion SQL directe hypothétique
// hors-pipeline. Correction de fond (ajouter council à l'enum) = backlog v0.4.0.
// F-17 (scoring par c_kind, v0.4.0) doit en tenir compte.

/// Catégorie cognitive CoALA (4 catégories) dérivée déterministiquement de la section.
///
/// Retourne `"episodic"`, `"semantic"`, `"procedural"` ou `"reflective"`.
/// Fallback : `"semantic"` pour toute section inconnue (forward-compat).
///
/// # Usage
///
/// Scoring-only — usage effectif différé à F-17 (v0.4.0). Ne modifie aucun
/// comportement de search ou scoring en v0.3.0.
pub const fn section_to_c_kind(section: &Section) -> &'static str {
    match section {
        Section::Architecture => "semantic",
        Section::Decisions => "episodic",
        Section::Debug => "episodic",
        Section::Reasoning => "semantic",
        Section::Feedback => "reflective",
        Section::LessonsLearned => "semantic",
        Section::Retrospectives => "reflective",
        Section::Experiments => "semantic",
        Section::AgentIssues => "procedural",
        Section::Reference => "semantic",
    }
}

/// Axe temporel CoALA dérivé déterministiquement de la section.
///
/// Retourne `"Event"` (incident daté, point dans le temps) ou `"Static"`
/// (connaissance stable, référence pérenne). Fallback : `"Static"`.
///
/// # Usage
///
/// Scoring-only — usage effectif différé à F-17 (v0.4.0). Ne modifie aucun
/// comportement de search ou scoring en v0.3.0.
pub const fn section_to_doc_kind(section: &Section) -> &'static str {
    match section {
        Section::Debug => "Event",
        Section::AgentIssues => "Event",
        // Toutes les autres sections : connaissance stable
        Section::Architecture
        | Section::Decisions
        | Section::Reasoning
        | Section::Feedback
        | Section::LessonsLearned
        | Section::Retrospectives
        | Section::Experiments
        | Section::Reference => "Static",
    }
}

#[cfg(test)]
mod cognitive_kind_tests {
    use super::*;

    /// Vérifie que section_to_c_kind produit les valeurs attendues pour les 10 sections.
    #[test]
    fn c_kind_all_sections() {
        let cases = [
            (Section::Architecture, "semantic"),
            (Section::Decisions, "episodic"),
            (Section::Debug, "episodic"),
            (Section::Reasoning, "semantic"),
            (Section::Feedback, "reflective"),
            (Section::LessonsLearned, "semantic"),
            (Section::Retrospectives, "reflective"),
            (Section::Experiments, "semantic"),
            (Section::AgentIssues, "procedural"),
            (Section::Reference, "semantic"),
        ];
        for (section, expected) in cases {
            assert_eq!(
                section_to_c_kind(&section),
                expected,
                "section_to_c_kind({section}) attendu {expected}"
            );
        }
    }

    /// Vérifie que section_to_doc_kind produit les valeurs attendues pour les 10 sections.
    #[test]
    fn doc_kind_all_sections() {
        let cases = [
            (Section::Architecture, "Static"),
            (Section::Decisions, "Static"),
            (Section::Debug, "Event"),
            (Section::Reasoning, "Static"),
            (Section::Feedback, "Static"),
            (Section::LessonsLearned, "Static"),
            (Section::Retrospectives, "Static"),
            (Section::Experiments, "Static"),
            (Section::AgentIssues, "Event"),
            (Section::Reference, "Static"),
        ];
        for (section, expected) in cases {
            assert_eq!(
                section_to_doc_kind(&section),
                expected,
                "section_to_doc_kind({section}) attendu {expected}"
            );
        }
    }

    /// Vérifie la cohérence entre les const Rust et le backfill SQL (migration 0008).
    ///
    /// Le backfill SQL utilise une expression CASE sur la colonne `section` (string).
    /// Cette fonction simule le CASE SQL et compare le résultat avec les const Rust
    /// pour toutes les sections de l'enum (les 10 connues).
    /// Si un écart existe → les notes existantes en DB auront des valeurs différentes
    /// des nouvelles notes écrites via upsert_note.
    #[test]
    fn c_kind_matches_backfill_sql() {
        // Simulation fidèle du CASE SQL de la migration 0008 pour c_kind.
        // Source : crates/gradatum-index/migrations/0008_note_cognitive_kind.sql
        fn sql_c_kind(s: &str) -> &'static str {
            match s {
                "architecture" => "semantic",
                "decisions" => "episodic",
                "council" => "episodic",
                "debug" => "episodic",
                "reasoning" => "semantic",
                "feedback" => "reflective",
                "lessons-learned" => "semantic",
                "retrospectives" => "reflective",
                "experiments" => "semantic",
                "agent-issues" => "procedural",
                "reference" => "semantic",
                _ => "semantic",
            }
        }

        // Simulation fidèle du CASE SQL de la migration 0008 pour doc_kind.
        fn sql_doc_kind(s: &str) -> &'static str {
            match s {
                "debug" => "Event",
                "agent-issues" => "Event",
                _ => "Static",
            }
        }

        for section in Section::ALL {
            let s = section.as_str();
            assert_eq!(
                section_to_c_kind(&section),
                sql_c_kind(s),
                "DIVERGENCE c_kind pour section '{s}' : Rust={} SQL={}",
                section_to_c_kind(&section),
                sql_c_kind(s),
            );
            assert_eq!(
                section_to_doc_kind(&section),
                sql_doc_kind(s),
                "DIVERGENCE doc_kind pour section '{s}' : Rust={} SQL={}",
                section_to_doc_kind(&section),
                sql_doc_kind(s),
            );
        }
    }
}
