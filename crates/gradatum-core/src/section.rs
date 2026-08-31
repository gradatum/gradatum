//! The 14 canonical sections of Gradatum.
//!
//! These sections form the semantic hierarchy of the store. Each note is assigned
//! to exactly one section. The set is stable: adding a section requires updating
//! SQL migrations and CoALA mappings, and is subject to a governance review.
//!
//! The `Council` variant was added to align the enum with the full section registry.
//! The `ProjectMap` variant (12th) tracks traceable work units carrying a
//! typed-wikilink schema (`[[project:…]]` + `[[status:…]]` + `[[kind:…]]`).
//! The `Identity` variant (13th) stores agent soul notes (persona/governance).
//! The `Snapshot` variant (14th) stores raw session event-capture lines.

use serde::{Deserialize, Serialize};

/// Canonical section of a Gradatum note.
///
/// 14 fixed sections representing the semantic categories of the knowledge store.
/// Serialised as `kebab-case` in YAML frontmatters and APIs.
///
// Extension remains subject to a project-side governance review, but no longer
// breaks downstream consumers (`#[non_exhaustive]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
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
    // Extension remains subject to a project-side governance review, but no longer
    // breaks downstream consumers.
    ProjectMap,
    /// Identity: declarative agent soul (persona/governance) — soul notes.
    ///
    /// Notes here carry the agent persona, immutable invariants (INVARIANTS/GATES/NARRATIVE
    /// schema), and are protected from semantic forget (see [`Section::PROTECTED_FORGET`]).
    /// Write access is ACL-restricted: an agent may only write its own soul note.
    ///
    // Extension remains subject to a project-side governance review, but no longer
    // breaks downstream consumers.
    Identity,
    /// Snapshot: raw session event-capture lines, written without interpretation,
    /// destined for downstream processing by distillation.
    ///
    /// Excluded from the default search scope (see [`Section::DEFAULT_SEARCH_EXCLUDED`]).
    ///
    // Extension remains subject to a project-side governance review, but no longer
    // breaks downstream consumers.
    Snapshot,
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
    /// `AgentIssues`, `Council`, `ProjectMap` and `Identity` are excluded from
    /// every forget batch, whether triggered via the API (preview handler) or
    /// executed by the worker. This list must stay in sync with the constant below.
    pub const PROTECTED_FORGET: &'static [Section] = &[
        Section::AgentIssues,
        Section::Council,
        Section::ProjectMap,
        Section::Identity,
    ];

    /// Sections protected from **hard-delete**.
    ///
    /// Strictly wider than [`Section::PROTECTED_FORGET`]: a hard-delete is
    /// irreversible, so the governance perimeter is extended to `Decisions`
    /// (target of `gov-save-decision`) and `Reasoning` (ReasoningBank).
    /// A note in any of these sections can **never** be hard-deleted — there is
    /// **no bypass flag**; this was a deliberate maintainer decision.
    ///
    /// The guard is **system-wide**, not API-only: it is enforced at the single
    /// cascade choke point (`cascade_delete_note`), so it protects both the
    /// `vault_delete` endpoint **and** the background Purge job (which reaches
    /// the same cascade through the internal delete endpoint). A protected note
    /// that is downgraded to `garbage` is never purged. Removing such a
    /// governance note is an exceptional manual operation, out of band.
    ///
    /// # Invariant
    ///
    /// `PROTECTED_FORGET ⊆ PROTECTED_DELETE` — enforced by the unit test
    /// `protected_delete_superset_of_protected_forget`. Anything that can never
    /// be forgotten can never be hard-deleted either.
    pub const PROTECTED_DELETE: &'static [Section] = &[
        Section::AgentIssues,
        Section::Council,
        Section::ProjectMap,
        Section::Identity,
        Section::Decisions,
        Section::Reasoning,
    ];

    /// Sections excluded from the **default** search scope.
    ///
    /// Notes in these sections carry raw, uninterpreted material that search
    /// should not surface unless explicitly requested. This constant is the
    /// inventory only — applying it to the search paths is a separate piece.
    ///
    /// # Invariant
    ///
    /// `Snapshot` alone is excluded by default; any future raw-capture section
    /// must be added here.
    pub const DEFAULT_SEARCH_EXCLUDED: &'static [Section] = &[Section::Snapshot];

    /// Returns `true` if `name` (kebab-case section) can never be hard-deleted.
    ///
    /// Single source of truth for the [`Section::PROTECTED_DELETE`] membership
    /// test, shared by the `vault_delete` endpoint and the cascade choke point
    /// (`cascade_delete_note`). Unknown section names return `false` (they are
    /// not in the governance perimeter).
    #[must_use]
    pub fn is_protected_delete(name: &str) -> bool {
        Section::PROTECTED_DELETE.iter().any(|s| s.as_str() == name)
    }

    /// Sections protected from the automatic downgrade policy (graduated forgetting).
    ///
    /// The four [`Section::PROTECTED_FORGET`] sections, plus the durable-memory
    /// sections that must never lose trust just because they went quiet ("never
    /// downgrade unless factually wrong"), plus `architecture`, which feeds the
    /// regression-analysis tooling through `vault_trace`. Single source of truth —
    /// the irrelevance detector filters its candidates on this set, and configuration
    /// may only extend it, never remove a baseline entry.
    /// Kebab-case, matching [`Section::as_str`].
    pub const PROTECTED_DOWNGRADE: &'static [&'static str] = &[
        "agent-issues",
        "council",
        "project-map",
        "identity",
        "decisions",
        "lessons-learned",
        "feedback",
        "retrospectives",
        "architecture",
    ];

    /// All canonical sections, in declaration order.
    ///
    // Extension remains subject to a project-side governance review, but no longer
    // breaks downstream consumers.
    pub const ALL: [Section; 14] = [
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
        Section::Snapshot,
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
            Section::Snapshot => "snapshot",
        }
    }
}

impl std::fmt::Display for Section {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns `true` if `section` (kebab-case) is protected from automatic downgrade.
///
/// Membership test against [`Section::PROTECTED_DOWNGRADE`]. Unknown section names
/// return `false` (not in the governance perimeter).
#[must_use]
pub fn is_protected_downgrade(section: &str) -> bool {
    Section::PROTECTED_DOWNGRADE.contains(&section)
}

// ── CoALA scoring-only — deterministic section → c_kind / doc_kind mappings ──
//
// These functions capture CoALA metadata without modifying any search/scoring
// behaviour (composite α/β unchanged).
//
// The SQL backfill (migrations 0008 + 0011 + 0021 + 0024) MUST produce the same values
// for all 14 enum sections — see tests `c_kind_matches_backfill_sql` below.

/// Deterministic CoALA cognitive category (4 categories) derived from section.
///
/// Returns `"episodic"`, `"semantic"`, `"procedural"`, or `"reflective"`.
/// The match is exhaustive over all 14 [`Section`] variants — there is no runtime
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
        // Identity : gouvernance/comportement agent → procedural (F-34 v0.7.6)
        Section::Identity => "procedural",
        // Snapshot : lignes de capture d'événements de session, datées → episodic (F-246)
        Section::Snapshot => "episodic",
    }
}

/// Deterministic CoALA temporal axis derived from section.
///
/// Returns `"Event"` (dated incident, point in time) or `"Static"`
/// (stable knowledge, durable reference).
/// The match is exhaustive over all 14 [`Section`] variants — there is no runtime
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
        // Snapshot : lignes de capture d'événements de session, datées → Event (F-246)
        Section::Snapshot => "Event",
        // Toutes les autres sections : connaissance stable
        // ProjectMap : entité mutée par RMW (carte = work-status piloté), pas
        // un événement immuable → Static (spec §16 B1).
        // Identity : âme stable, mutée par RMW uniquement → Static (F-34 v0.7.6, A4).
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

/// Base per-section trust score — authority of the content, independent of age.
///
/// The trust factor of the composite score (`1 + γ·trust_decayed`) was strictly neutral
/// over the whole corpus (12,450 notes, a single value 0.5). The chosen design is to
/// **feed** the factor using the section as the base-value axis. Each value is justified
/// by the repo's documented semantics — never by analogy or a plausible default:
///
/// - **TIER A — decision & governance (0.95)**: `council` ("multi-expert governance
///   verdicts and arbitrations", `Section::Council`) · `decisions` ("architectural decisions,
///   trade-offs, and technical choices", target of `gov-save-decision`, `PROTECTED_DELETE`) ·
///   `project-map` ("source of truth for versions/todos", `Section::ProjectMap`,
///   `PROTECTED_FORGET`) · `identity` ("declarative agent soul", `Section::Identity`,
///   `PROTECTED_FORGET`). Content that DECIDES or GOVERNS the system — the top level of the
///   repo's already-calibrated, documented trust scale (`human-decision` = 0.95,
///   `provenance.rs`).
/// - **TIER B — durable memory (0.75)**: `lessons-learned`, `architecture`,
///   `retrospectives`, `feedback` (all in `PROTECTED_DOWNGRADE`: "durable-memory sections
///   that must never lose trust just because they went quiet") + `reasoning` (ReasoningBank,
///   `PROTECTED_DELETE`). Codified durable knowledge — the "interaction-validated event" level
///   of the documented scale (`qa-event` = 0.75).
/// - **TIER C — referenced records (0.60)**: `reference` ("stable fact, purely
///   informational, no narrative", curator-classifier v1) · `agent-issues` ("tracked issue
///   about agent behaviour", `Section::AgentIssues`, forget-protected but operational).
///   Records rather than decisions — the synthesis level (`distilled` = 0.60).
/// - **TIER D — raw material (0.40)**: `debug` ("post-mortems, root-cause analysis",
///   dated) · `experiments` ("prototypes, proofs of concept", exploratory). No protection
///   scope. Below the neutral 0.50 — the least authoritative content.
/// - **Unknown (0.50)**: any off-canon section falls back to 0.50, the corpus's current
///   neutral — consistent with the repo's default for unknown provenance (`agent-log` 0.50).
///
/// The decay axis (`doc_kind`, `Event` vs `Static`) is handled separately by
/// [`section_str_to_doc_kind`] and the half-lives table in `gradatum-search`.
pub const SECTION_TRUST_SCORES: &[(&str, f64)] = &[
    // PALIER A — décision & gouvernance
    ("council", 0.95),
    ("decisions", 0.95),
    ("project-map", 0.95),
    ("identity", 0.95),
    // PALIER B — mémoire durable
    ("lessons-learned", 0.75),
    ("architecture", 0.75),
    ("retrospectives", 0.75),
    ("feedback", 0.75),
    ("reasoning", 0.75),
    // PALIER C — enregistrements référencés
    ("reference", 0.60),
    ("agent-issues", 0.60),
    // PALIER D — matière première
    ("debug", 0.40),
    ("experiments", 0.40),
];

/// Sections whose trust **never** decays — tier A of [`SECTION_TRUST_SCORES`].
///
/// Doctrine of this vault: **an enacted decision is not re-judged** — an act (a `council`
/// governance verdict, a `decisions` decision, a `project-map` work unit, a declarative
/// `identity` soul) does not lose its authority as it ages.
///
/// The exemption applies to the **section**, not to `doc_kind`: on the measured corpus
/// (vault `main`, 13 sections), `doc_kind` is a deterministic function of `section` — it is
/// not a second independent lever. `council` stays `Event` (CoALA temporal axis: a dated
/// one-off verdict) but does not decay — two distinct concepts.
pub const TRUST_NON_DECAYING_SECTIONS: &[&str] =
    &["council", "decisions", "project-map", "identity"];

/// `true` if the section (kebab-case) is exempt from trust-factor decay.
///
/// Tier A of [`SECTION_TRUST_SCORES`] — see [`TRUST_NON_DECAYING_SECTIONS`] for the doctrine.
/// An unknown section → `false` (falls back to the `doc_kind` axis).
#[must_use]
pub fn is_trust_non_decaying(section: &str) -> bool {
    TRUST_NON_DECAYING_SECTIONS.contains(&section)
}

/// Base trust score of a canonical section, `0.5` (neutral) for an unknown section.
///
/// Derives the base trust-factor value from the section — see
/// [`SECTION_TRUST_SCORES`] for the justification of each value.
#[must_use]
pub const fn trust_for_section(section: &Section) -> f64 {
    match section {
        Section::Council | Section::Decisions | Section::ProjectMap | Section::Identity => 0.95,
        Section::LessonsLearned
        | Section::Architecture
        | Section::Retrospectives
        | Section::Feedback
        | Section::Reasoning => 0.75,
        Section::Reference | Section::AgentIssues => 0.60,
        Section::Debug | Section::Experiments => 0.40,
        // Snapshot : matière première brute (lignes de capture non interprétées) → palier D (F-246)
        Section::Snapshot => 0.40,
    }
}

/// Base trust score of a section by its kebab-case name, `0.5` (neutral) if unknown.
///
/// Entry point for the scoring paths that carry only a section string (`hit.section`).
/// An off-canon section (e.g. `"notes"` from the synthetic corpus, or the 21 real vault
/// sections, 8 of which are outside the enum) falls back to the neutral 0.50.
#[must_use]
pub fn trust_for_section_str(section: &str) -> f64 {
    match Section::from_canonical_str(section) {
        Some(s) => trust_for_section(&s),
        None => 0.50,
    }
}

/// `doc_kind` (CoALA temporal axis) derived from a section by its kebab-case name.
///
/// The trust-factor decay axis is `doc_kind` — `Event` decays, `Static` does not.
/// Delegates to [`section_to_doc_kind`] for canonical sections; an unknown section falls
/// back to `"Static"` (non-perishable), consistent with the repo's SQL default
/// (`COALESCE(n.doc_kind, 'Static')`, `queries.rs`).
#[must_use]
pub fn section_str_to_doc_kind(section: &str) -> &'static str {
    match Section::from_canonical_str(section) {
        Some(s) => section_to_doc_kind(&s),
        None => "Static",
    }
}

#[cfg(test)]
mod from_canonical_str_tests {
    use super::*;

    /// Toutes les 14 sections sont reconnues par from_canonical_str.
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

    /// Vérifie les propriétés canoniques de la 13e section `identity` (F-34 v0.7.6).
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
        // 14 canonical sections total.
        assert_eq!(Section::ALL.len(), 14);
        // doc_kind = "Static" (âme stable, mutée par RMW — A4 F-34).
        assert_eq!(section_to_doc_kind(&Section::Identity), "Static");
        // c_kind = "procedural" (gouvernance/comportement — F-34 v0.7.3).
        assert_eq!(section_to_c_kind(&Section::Identity), "procedural");
        // Jamais oubliée (A3 PROTECTED_FORGET).
        assert!(Section::PROTECTED_FORGET.contains(&Section::Identity));
    }

    /// Invariant F-100 : `PROTECTED_FORGET ⊆ PROTECTED_DELETE`.
    ///
    /// Toute note qui ne peut JAMAIS être oubliée ne peut JAMAIS être
    /// hard-delete non plus. Ce test verrouille l'invariant contre toute
    /// dérive future (ajout d'une section à PROTECTED_FORGET sans miroir dans
    /// PROTECTED_DELETE).
    #[test]
    fn protected_delete_superset_of_protected_forget() {
        for forget in Section::PROTECTED_FORGET {
            assert!(
                Section::PROTECTED_DELETE.contains(forget),
                "section {forget:?} protégée en forget doit l'être aussi en delete (invariant ⊃)"
            );
        }
    }

    /// F-100 1.2b — périmètre exact du refus dur hard-delete (arbitrage du mainteneur).
    ///
    /// PROTECTED_DELETE = PROTECTED_FORGET ∪ {Decisions, Reasoning}.
    #[test]
    fn protected_delete_exact_perimeter() {
        // Les 4 sections de PROTECTED_FORGET.
        assert!(Section::PROTECTED_DELETE.contains(&Section::AgentIssues));
        assert!(Section::PROTECTED_DELETE.contains(&Section::Council));
        assert!(Section::PROTECTED_DELETE.contains(&Section::ProjectMap));
        assert!(Section::PROTECTED_DELETE.contains(&Section::Identity));
        // + les 2 sections élargies (gov-save-decision + ReasoningBank).
        assert!(Section::PROTECTED_DELETE.contains(&Section::Decisions));
        assert!(Section::PROTECTED_DELETE.contains(&Section::Reasoning));
        // Exactement 6 sections, pas plus.
        assert_eq!(Section::PROTECTED_DELETE.len(), 6);
        // Une section non gouvernance N'EST PAS protégée (ex. Feedback).
        assert!(!Section::PROTECTED_DELETE.contains(&Section::Feedback));
    }

    /// La fonction partagée `is_protected_delete` reflète exactement la constante.
    #[test]
    fn is_protected_delete_matches_constant() {
        // Chaque section protégée est reconnue par son nom kebab-case.
        for sec in Section::PROTECTED_DELETE {
            assert!(Section::is_protected_delete(sec.as_str()));
        }
        // Une section hors périmètre et un nom inconnu → false.
        assert!(!Section::is_protected_delete("feedback"));
        assert!(!Section::is_protected_delete("bogus-section"));
    }

    /// F-246 — l'inventaire `DEFAULT_SEARCH_EXCLUDED` contient exactement `Snapshot`.
    ///
    /// Seule la section de capture brute est hors périmètre de recherche par défaut ;
    /// aucune autre section canonique n'y figure.
    #[test]
    fn default_search_excluded_exact_perimeter() {
        assert!(Section::DEFAULT_SEARCH_EXCLUDED.contains(&Section::Snapshot));
        assert_eq!(Section::DEFAULT_SEARCH_EXCLUDED.len(), 1);
        for sec in Section::ALL {
            assert_eq!(
                Section::DEFAULT_SEARCH_EXCLUDED.contains(&sec),
                sec == Section::Snapshot,
                "section {sec} incohérente avec DEFAULT_SEARCH_EXCLUDED"
            );
        }
    }

    // F-111 : protégées = 4 PROTECTED_FORGET + 4 mémoire durable + architecture (GO du mainteneur 2026-07-16)
    #[test]
    fn protected_downgrade_covers_forget_set_plus_durable_memory() {
        for s in [
            "agent-issues",
            "council",
            "project-map",
            "identity",
            "decisions",
            "lessons-learned",
            "feedback",
            "retrospectives",
            "architecture",
        ] {
            assert!(Section::PROTECTED_DOWNGRADE.contains(&s), "{s} manquant");
            assert!(is_protected_downgrade(s), "{s} devrait être protégée");
        }
        assert_eq!(Section::PROTECTED_DOWNGRADE.len(), 9);
        for s in ["debug", "experiments", "reference", "reasoning"] {
            assert!(
                !is_protected_downgrade(s),
                "{s} ne devrait pas être protégée"
            );
        }
    }
}

#[cfg(test)]
mod cognitive_kind_tests {
    use super::*;

    /// Vérifie que section_to_c_kind produit les valeurs attendues pour les 14 sections.
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
            (Section::Snapshot, "episodic"),
        ];
        for (section, expected) in cases {
            assert_eq!(
                section_to_c_kind(&section),
                expected,
                "section_to_c_kind({section}) attendu {expected}"
            );
        }
    }

    /// Vérifie que section_to_doc_kind produit les valeurs attendues pour les 14 sections.
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
            (Section::Snapshot, "Event"),
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

    /// Verifies consistency between the Rust constants and the SQL backfill (migrations 0008 + 0011 + 0021 + 0024).
    ///
    /// The SQL backfill uses a CASE expression on the `section` column (string).
    /// This test simulates the SQL CASE and compares the result with the Rust constants
    /// for all enum sections (the 14 canonical ones, including `Identity` added in v0.7.3
    /// and `Snapshot` added in F-246).
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
                // snapshot → episodic (pas encore de migration SQL dédiée — mapping
                // cible F-246, à refléter dans le futur backfill).
                "snapshot" => "episodic",
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
                // snapshot → Event (lignes de capture datées — F-246)
                "snapshot" => "Event",
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

    /// F-261 — les 14 sections canoniques ont toutes une valeur de confiance définie,
    /// dans [0, 1], et l'ordre des paliers est strictement décroissant du palier A au D.
    #[test]
    fn trust_for_section_all_sections_in_zero_one() {
        for section in Section::ALL {
            let t = trust_for_section(&section);
            assert!(
                (0.0..=1.0).contains(&t),
                "trust_for_section({section}) = {t} hors [0,1]"
            );
        }
        // Paliers : A > B > C > D (autorité strictement décroissante).
        let tier_a = ["council", "decisions", "project-map", "identity"];
        let tier_b = [
            "lessons-learned",
            "architecture",
            "retrospectives",
            "feedback",
            "reasoning",
        ];
        let tier_c = ["reference", "agent-issues"];
        let tier_d = ["debug", "experiments"];
        let val = |s: &str| trust_for_section_str(s);
        for a in tier_a {
            for b in tier_b {
                assert!(val(a) > val(b), "{a} ({}) doit > {b} ({})", val(a), val(b));
            }
        }
        for b in tier_b {
            for c in tier_c {
                assert!(val(b) > val(c), "{b} ({}) doit > {c} ({})", val(b), val(c));
            }
        }
        for c in tier_c {
            for d in tier_d {
                assert!(val(c) > val(d), "{c} ({}) doit > {d} ({})", val(c), val(d));
            }
        }
        // Aucune valeur ne retombe sur le neutre 0.5 (les paliers sont tous décalés).
        for s in tier_a
            .iter()
            .chain(tier_b.iter())
            .chain(tier_c.iter())
            .chain(tier_d.iter())
        {
            assert_ne!(val(s), 0.5, "{s} ne doit pas retomber sur le neutre 0.5");
        }
    }

    /// F-261 — section inconnue (hors canon, ex. `"notes"`) → neutre 0.5 et doc_kind "Static".
    #[test]
    fn trust_for_section_unknown_falls_back_to_neutral() {
        assert_eq!(trust_for_section_str("notes"), 0.5);
        assert_eq!(trust_for_section_str(""), 0.5);
        assert_eq!(trust_for_section_str("bogus-section"), 0.5);
        assert_eq!(section_str_to_doc_kind("notes"), "Static");
        assert_eq!(section_str_to_doc_kind(""), "Static");
    }

    /// F-261 — section_str_to_doc_kind est cohérente avec section_to_doc_kind sur les 14 sections.
    #[test]
    fn section_str_to_doc_kind_matches_typed() {
        for section in Section::ALL {
            assert_eq!(
                section_str_to_doc_kind(section.as_str()),
                section_to_doc_kind(&section),
                "section_str_to_doc_kind({section}) doit égaler section_to_doc_kind"
            );
        }
    }

    /// F-261 (2026-08-25) — l'exemption de décroissance couvre **exactement** le palier A.
    ///
    /// `council`, `decisions`, `project-map`, `identity` (0.95) ne décroissent jamais ;
    /// toute autre section — y compris `Event` hors palier A (`debug`, `agent-issues`) —
    /// n'est pas exemptée. Invariant : exemption ⇔ trust == 0.95 (palier A).
    #[test]
    fn trust_non_decaying_is_exactly_tier_a() {
        for s in ["council", "decisions", "project-map", "identity"] {
            assert!(
                is_trust_non_decaying(s),
                "{s} doit être exempté de décroissance"
            );
            assert_eq!(
                trust_for_section_str(s),
                0.95,
                "{s} doit être palier A (0.95)"
            );
        }
        for s in [
            "debug",
            "agent-issues",
            "reference",
            "architecture",
            "notes",
            "",
        ] {
            assert!(!is_trust_non_decaying(s), "{s} ne doit pas être exempté");
        }
        for section in Section::ALL {
            let name = section.as_str();
            assert_eq!(
                is_trust_non_decaying(name),
                trust_for_section(&section) == 0.95,
                "exemption doit coïncider avec le palier A pour {name}"
            );
        }
    }
}
