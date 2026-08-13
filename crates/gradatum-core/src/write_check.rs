//! Write coherence check for vault notes.
//!
//! Pure, deterministic, zero I/O, zero LLM.
//! Detects category-title / declared-section drift in warn-only mode.
//! Can never fail a `vault_write` — the result is observability only.

/// A write-drift warning.
///
/// Produced by [`check_category_section`] when the title category is inconsistent
/// with the declared `section_hint`. The write proceeds unconditionally — this struct
/// is observability data, never a refusal.
#[derive(Debug)]
pub struct DriftWarning {
    /// Rule identifier that fired (`"category_section_coherence"`).
    pub rule: &'static str,
    /// Category extracted from the title (e.g. `"COUNCIL"`).
    pub category: String,
    /// Canonical section expected for this category (e.g. `"council"`).
    pub expected_section: &'static str,
    /// Actual declared section (`section_hint`).
    pub actual_section: Option<String>,
}

/// Mapping table: `(category, expected_section, required_tag?)`.
///
/// 13 canonical categories. Cardinality is finite and bounded — never derived
/// from a dynamic parameter (bounded metric cardinality guarantee).
const TABLE: &[(&str, &str, Option<&str>)] = &[
    ("COUNCIL", "council", None),
    ("DECISIONS", "decisions", None),
    ("TODO", "decisions", Some("todo")),
    ("RETRO", "retrospectives", None),
    ("DEBUG", "debug", None),
    ("ARCH", "architecture", None),
    ("LESSONS", "lessons-learned", None),
    ("ISSUES", "agent-issues", None),
    ("PROJECT-MAP", "project-map", None),
    ("REASONING", "reasoning", None),
    ("FEEDBACK", "feedback", None),
    ("EXP", "experiments", None),
    ("REF", "reference", None),
];

/// Checks category-title ↔ section/tags coherence. Pure, deterministic.
///
/// Returns `None` when: no recognised `[CAT]` prefix, unknown category,
/// `section_hint` absent, or title/section/tags combination is coherent.
///
/// # Arguments
///
/// - `title`: raw note title (e.g. `"[COUNCIL][gradatum] X — 2026-06-28"`).
/// - `section_hint`: section declared by the caller. `None` → rule not applicable —
///   only two declared signals are compared; routing by content belongs to the
///   curator layer.
/// - `tags`: tags declared by the caller.
///
/// # Returns
///
/// `Some(DriftWarning)` if drift detected, `None` otherwise.
///
/// # Errors
///
/// This function cannot fail — it returns `Option`, never `Result`.
pub fn check_category_section(
    title: &str,
    section_hint: Option<&str>,
    tags: &[String],
) -> Option<DriftWarning> {
    // 1. Extraire le préfixe `[CAT]` — std only, zéro regex.
    //    Forme attendue : "[CAT]..." où CAT = lettres ASCII majuscules + tirets.
    let cat = title.strip_prefix('[')?.split_once(']')?.0;
    if cat.is_empty() || !cat.bytes().all(|b| b.is_ascii_uppercase() || b == b'-') {
        return None;
    }
    // 2. P1-1 council : section_hint=None → règle inapplicable.
    //    Le curator route par contenu ; F-36 compare uniquement des signaux DÉCLARÉS.
    let section = section_hint?;
    // 3. Lookup table — catégorie inconnue → None (pas de drift détectable).
    let (_, expected, tag_req) = TABLE.iter().find(|(c, _, _)| *c == cat)?;
    // 4. Comparer section déclarée et tag requis éventuel.
    let section_ok = section == *expected;
    let tag_ok = tag_req.is_none_or(|t| tags.iter().any(|x| x == t));
    if section_ok && tag_ok {
        return None;
    }
    Some(DriftWarning {
        rule: "category_section_coherence",
        category: cat.to_string(),
        expected_section: expected,
        actual_section: Some(section.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coherent_council_no_warning() {
        assert!(
            check_category_section("[COUNCIL][gradatum] X — 2026-06-28", Some("council"), &[])
                .is_none()
        );
    }

    #[test]
    fn incoherent_council_in_reference_warns() {
        let w = check_category_section("[COUNCIL][x] Y — d", Some("reference"), &[]).unwrap();
        assert_eq!(w.rule, "category_section_coherence");
        assert_eq!(w.expected_section, "council");
    }

    #[test]
    fn todo_without_todo_tag_warns() {
        assert!(check_category_section("[TODO][x] Z — d", Some("decisions"), &[]).is_some());
    }

    #[test]
    fn todo_with_todo_tag_ok() {
        assert!(
            check_category_section("[TODO][x] Z — d", Some("decisions"), &["todo".into()])
                .is_none()
        );
    }

    #[test]
    fn no_prefix_skips() {
        assert!(check_category_section("identity/main", Some("identity"), &[]).is_none());
        assert!(check_category_section("plain title", Some("decisions"), &[]).is_none());
    }

    #[test]
    fn unknown_category_skips() {
        assert!(check_category_section("[FOO][x] T — d", Some("reference"), &[]).is_none());
    }

    #[test]
    fn none_section_skips_curator_owns_routing() {
        // P1-1 council : section_hint=None → skip (le curator gère le routing par contenu)
        assert!(check_category_section("[COUNCIL][x] T — d", None, &[]).is_none());
    }
}
