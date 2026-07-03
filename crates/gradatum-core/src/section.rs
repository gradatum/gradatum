//! The 13 canonical sections of Gradatum.
//!
//! These sections form the semantic hierarchy of the store. Each note is assigned
//! to exactly one section. The set is stable: adding a section requires updating
//! SQL migrations and CoALA mappings, and is subject to a governance review.
//!
//! The `Council` variant was added to align the enum with the full section registry.
//! The `ProjectMap` variant (12th) tracks traceable work units carrying a
//! typed-wikilink schema (`[[project:…]]` + `[[status:…]]` + `[[kind:…]]`).
//! The `Identity` variant (13th) stores agent soul notes (persona/governance, since v0.7.3).

use serde::{Deserialize, Serialize};

/// Canonical section of a Gradatum note.
///
/// 13 fixed sections representing the semantic categories of the knowledge store.
/// Serialised as `kebab-case` in YAML frontmatters and APIs.
///
// Extension requires a governance review (anti-incremental-drift).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    /// Architectural decisions, trade-offs, and technical choices.
    Decisions,
    /// Architecture documentation, diagrams, and topologies.
    Architecture,
    /// Debugging: post-mortems, root-cause analysis, traces.
    Debug,
    /// Reasoning: thought chains, structured analyses.
    Reasoning,
    /// Feedback: operational observations, usage learnings.
    Feedback,
    /// Lessons learned: recurring patterns, avoided mistakes.
    LessonsLearned,
    /// Retrospectives: phase reviews, session summaries.
    Retrospectives,
    /// Experiments: prototypes, proofs of concept, exploratory benchmarks.
    Experiments,
    /// Agent issues: bugs, anomalies, technical debt.
    AgentIssues,
    /// References: pointers to external docs, specs, standards.
    Reference,
    /// Council: multi-expert governance verdicts and arbitrations.
    ///
    /// Notes in this section are protected from semantic forget
    /// (see `PROTECTED_FORGET`). Before this variant was added, notes in
    /// this section fell back to `Section::Reference`.
    Council,
    /// Project map: traceable work units (feature/fix/task) carrying a typed
    /// wikilink schema (`[[project:…]]` + `[[status:…]]` + `[[kind:…]]`).
    ///
    /// The schema is enforced at write time by a **dedicated** validator
    /// ([`gradatum_core::project_map`](crate::project_map)), distinct from the
    /// general-purpose schema-registry. Notes here form the project backbone
    /// (source of truth for versions/todos) and are protected from semantic
    /// forget (see [`Section::PROTECTED_FORGET`]).
    ///
    // Extension requires a governance review (anti-incremental-drift).
    ProjectMap,
    /// Identity: declarative agent soul (persona/governance) — soul notes, since v0.7.3.
    ///
    /// Notes here carry the agent persona, immutable invariants (INVARIANTS/GATES/NARRATIVE
    /// schema), and are protected from semantic forget (see [`Section::PROTECTED_FORGET`]).
    /// Write access is ACL-restricted: an agent may only write its own soul note.
    ///
    // Extension requires a governance review (anti-incremental-drift).
    Identity,
}

impl Section {
    /// Sections protected from semantic forget.
    ///
    /// Notes in these sections (governance identity) can never be forgotten.
    /// Single source of truth: imported by `gradatum-server::api_v1::forget`
    /// and `gradatum-worker::apalis_handlers` to guarantee consistency.
    ///
    /// # Invariant
    ///
    /// `AgentIssues` and `Council` are excluded from every forget batch,
    /// whether triggered via the API (preview handler) or executed by the worker.
    pub const PROTECTED_FORGET: &'static [Section] = &[
        Section::AgentIssues,
        Section::Council,
        Section::ProjectMap,
        Section::Identity,
    ];

    /// All canonical sections, in declaration order.
    ///
    // Extension requires a governance review (anti-incremental-drift).
    pub const ALL: [Section; 13] = [
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
        Section::Council,
        Section::ProjectMap,
        Section::Identity,
    ];

    /// Parse a kebab-case string into a `Section`.
    ///
    /// Iterates over [`Section::ALL`] and matches on [`Section::as_str`], so
    /// any new variant automatically becomes parseable without updating a
    /// secondary match arm.
    ///
    /// Returns `None` for unknown strings (callers map to the appropriate
    /// error type — e.g. HTTP 400 in persist handlers).
    ///
    /// # Examples
    ///
    /// ```
    /// use gradatum_core::section::Section;
    ///
    /// assert_eq!(Section::from_canonical_str("project-map"), Some(Section::ProjectMap));
    /// assert_eq!(Section::from_canonical_str("decisions"), Some(Section::Decisions));
    /// assert_eq!(Section::from_canonical_str("bogus"), None);
    /// ```
    pub fn from_canonical_str(s: &str) -> Option<Self> {
        Section::ALL.iter().find(|sec| sec.as_str() == s).copied()
    }

    /// Kebab-case string representation (identical to the serde serialisation).
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
            Section::Council => "council",
            Section::ProjectMap => "project-map",
            Section::Identity => "identity",
        }
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── CoALA scoring-only — deterministic section → c_kind / doc_kind mappings ──
//
// These functions capture CoALA metadata without modifying any search/scoring
// behaviour (composite α/β unchanged).
//
// The SQL backfill (migrations 0008 + 0011 + 0021) MUST produce the same values
// for all 12 enum sections — see tests `c_kind_matches_backfill_sql` below.

/// Deterministic CoALA cognitive category (4 categories) derived from section.
///
/// Returns `"episodic"`, `"semantic"`, `"procedural"`, or `"reflective"`.
/// The match is exhaustive over all 13 [`Section`] variants — there is no runtime
/// fallback. Adding a new variant requires updating this function (the compiler
/// will enforce it via an exhaustiveness error).
///
/// # Usage
///
/// Scoring metadata only. Does not modify any search or scoring behaviour.
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
        // Council : verdicts datés d'une délibération → episodic
        Section::Council => "episodic",
        // ProjectMap : unité de travail/process → procedural (spec §16 B1)
        Section::ProjectMap => "procedural",
        // Identity : gouvernance/comportement agent → procedural (F-34 v0.7.3)
        Section::Identity => "procedural",
    }
}

/// Deterministic CoALA temporal axis derived from section.
///
/// Returns `"Event"` (dated incident, point in time) or `"Static"`
/// (stable knowledge, durable reference).
/// The match is exhaustive over all 13 [`Section`] variants — there is no runtime
/// fallback. Adding a new variant requires updating this function (the compiler
/// will enforce it via an exhaustiveness error).
///
/// # Usage
///
/// Scoring metadata only. Does not modify any search or scoring behaviour.
pub const fn section_to_doc_kind(section: &Section) -> &'static str {
    match section {
        Section::Debug => "Event",
        Section::AgentIssues => "Event",
        // Toutes les autres sections : connaissance stable
        // ProjectMap : entité mutée par RMW (carte = work-status piloté), pas
        // un événement immuable → Static (spec §16 B1).
        // Identity : âme stable, mutée par RMW uniquement → Static (F-34 v0.7.3, A4).
        Section::Architecture
        | Section::Decisions
        | Section::Reasoning
        | Section::Feedback
        | Section::LessonsLearned
        | Section::Retrospectives
        | Section::Experiments
        | Section::Reference
        | Section::ProjectMap
        | Section::Identity => "Static",
        // Council : verdict ponctuel daté → Event (cohérent avec Debug/AgentIssues)
        Section::Council => "Event",
    }
}

#[cfg(test)]
mod from_canonical_str_tests {
    use super::*;

    /// Toutes les 13 sections sont reconnues par from_canonical_str.
    #[test]
    fn accepts_all_canonical_sections() {
        for section in Section::ALL {
            let s = section.as_str();
            assert_eq!(
                Section::from_canonical_str(s),
                Some(section),
                "from_canonical_str({s:?}) attendu Some({section:?})"
            );
        }
    }

    /// project-map (12ᵉ section) est accepté — anti-régression stage1.
    #[test]
    fn accepts_project_map() {
        assert_eq!(
            Section::from_canonical_str("project-map"),
            Some(Section::ProjectMap)
        );
    }

    /// Sections d'origine (11 premières) inchangées.
    #[test]
    fn accepts_original_eleven_sections() {
        let cases = [
            ("decisions", Section::Decisions),
            ("architecture", Section::Architecture),
            ("debug", Section::Debug),
            ("reasoning", Section::Reasoning),
            ("feedback", Section::Feedback),
            ("lessons-learned", Section::LessonsLearned),
            ("retrospectives", Section::Retrospectives),
            ("experiments", Section::Experiments),
            ("agent-issues", Section::AgentIssues),
            ("reference", Section::Reference),
            ("council", Section::Council),
        ];
        for (s, expected) in cases {
            assert_eq!(
                Section::from_canonical_str(s),
                Some(expected),
                "section '{s}'"
            );
        }
    }

    /// Chaînes inconnues retournent None.
    #[test]
    fn rejects_unknown_strings() {
        for bogus in ["bogus", "", "DECISIONS", "project_map", "ProjectMap"] {
            assert_eq!(
                Section::from_canonical_str(bogus),
                None,
                "from_canonical_str({bogus:?}) attendu None"
            );
        }
    }

    /// Vérifie les propriétés canoniques de la 13e section `identity` (F-34 v0.7.3).
    ///
    /// Invariants vérifiés :
    /// - round-trip parse (`from_canonical_str` / `as_str`)
    /// - `ALL.len() == 13`
    /// - `doc_kind = "Static"` (âme stable, mutée par RMW)
    /// - `c_kind = "procedural"` (gouvernance/comportement)
    /// - présence dans `PROTECTED_FORGET` (jamais oubliée)
    #[test]
    fn identity_section_is_canonical_static_protected() {
        // Round-trip parse.
        assert_eq!(
            Section::from_canonical_str("identity"),
            Some(Section::Identity)
        );
        assert_eq!(Section::Identity.as_str(), "identity");
        // 13 canonical sections total.
        assert_eq!(Section::ALL.len(), 13);
        // doc_kind = "Static" (âme stable, mutée par RMW — A4 F-34).
        assert_eq!(section_to_doc_kind(&Section::Identity), "Static");
        // c_kind = "procedural" (gouvernance/comportement — F-34 v0.7.3).
        assert_eq!(section_to_c_kind(&Section::Identity), "procedural");
        // Jamais oubliée (A3 PROTECTED_FORGET).
        assert!(Section::PROTECTED_FORGET.contains(&Section::Identity));
    }
}

#[cfg(test)]
mod cognitive_kind_tests {
    use super::*;

    /// Vérifie que section_to_c_kind produit les valeurs attendues pour les 13 sections.
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
            (Section::Council, "episodic"),
            (Section::ProjectMap, "procedural"),
            (Section::Identity, "procedural"),
        ];
        for (section, expected) in cases {
            assert_eq!(
                section_to_c_kind(&section),
                expected,
                "section_to_c_kind({section}) attendu {expected}"
            );
        }
    }

    /// Vérifie que section_to_doc_kind produit les valeurs attendues pour les 13 sections.
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
            (Section::Council, "Event"),
            (Section::ProjectMap, "Static"),
            (Section::Identity, "Static"),
        ];
        for (section, expected) in cases {
            assert_eq!(
                section_to_doc_kind(&section),
                expected,
                "section_to_doc_kind({section}) attendu {expected}"
            );
        }
    }

    /// Reads the **real** migration 0024 SQL file and asserts that it uses the correct
    /// case for `doc_kind` and `c_kind` for the `identity` section.
    ///
    /// This test is the authoritative guard against casing regressions in the SQL
    /// backfill: the simulation in [`c_kind_matches_backfill_sql`] hardcodes expected
    /// values, so it would not detect a wrong-case literal in the actual `.sql` file.
    ///
    /// Finding P1-C1 (reviewer v0.7.3): `doc_kind = 'static'` (lowercase) was written
    /// instead of `'Static'`.  The filter `doc_kind IN ('Static','Event')` would silently
    /// miss all backfilled notes.  Any future drift in casing will now turn this test red.
    #[test]
    fn identity_backfill_sql_0024_correct_case() {
        // Path is relative to the source file location
        // (crates/gradatum-core/src/ → crates/gradatum-index/migrations/).
        const SQL: &str =
            include_str!("../../gradatum-index/migrations/0024_identity_section_backfill.sql");

        // doc_kind must be 'Static' (capital S), not 'static'.
        assert!(
            SQL.contains("doc_kind = 'Static'"),
            "migration 0024 must use doc_kind = 'Static' (capital S) — \
             found: {}",
            SQL.lines()
                .find(|l| l.contains("doc_kind"))
                .unwrap_or("<no doc_kind line found>")
        );

        // c_kind must be 'procedural' (all lowercase — correct canonical value).
        assert!(
            SQL.contains("c_kind = 'procedural'"),
            "migration 0024 must use c_kind = 'procedural'"
        );

        // Negative guard: 'static' (lowercase) must NOT appear on a SET line
        // (would indicate the casing regression is back).
        let has_lowercase_static = SQL
            .lines()
            .filter(|l| l.trim_start().starts_with("UPDATE"))
            .any(|l| l.contains("'static'"));
        assert!(
            !has_lowercase_static,
            "migration 0024 must NOT contain lowercase 'static' on an UPDATE line \
             (use 'Static' to match the canonical doc_kind value)"
        );
    }

    /// Verifies consistency between the Rust constants and the SQL backfill (migrations 0008 + 0011).
    ///
    /// The SQL backfill uses a CASE expression on the `section` column (string).
    /// This test simulates the SQL CASE and compares the result with the Rust constants
    /// for all enum sections (the 13 canonical ones, including `Identity` added in v0.7.3).
    /// Any mismatch means existing DB rows will have different values than new rows
    /// written via `upsert_note`.
    #[test]
    fn c_kind_matches_backfill_sql() {
        // Simulation fidèle du CASE SQL des migrations 0008 + 0011 + 0021 + 0024 pour c_kind.
        // Sources :
        //   crates/gradatum-index/migrations/0008_note_cognitive_kind.sql
        //   crates/gradatum-index/migrations/0011_council_section_backfill.sql
        //   crates/gradatum-index/migrations/0021_project_map_section_backfill.sql
        //   crates/gradatum-index/migrations/0024_identity_section_backfill.sql
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
                "project-map" => "procedural",
                // identity → procedural (migration 0024, F-34 v0.7.3)
                "identity" => "procedural",
                _ => "semantic",
            }
        }

        // Simulation fidèle du CASE SQL des migrations 0008 + 0011 + 0021 + 0024 pour doc_kind.
        // "identity" tombe dans le défaut _ => "Static" (cohérent avec section_to_doc_kind).
        fn sql_doc_kind(s: &str) -> &'static str {
            match s {
                "debug" => "Event",
                "agent-issues" => "Event",
                "council" => "Event",
                "project-map" => "Static",
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
