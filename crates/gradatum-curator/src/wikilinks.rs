//! Wikilink handling — regex extraction + ULID resolution + Jaro-Winkler fuzzy matching.
//!
//! Extracts `[[...]]` references from a note body and attempts to resolve
//! them by exact title match or Jaro-Winkler similarity.

use once_cell::sync::Lazy;
use regex::Regex;

/// Regex for wikilinks `[[target]]` or `[[target|alias]]`.
///
/// Group 1: the target (before the optional `|`).
static WIKILINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[([^\]\|]+)(?:\|[^\]]+)?\]\]")
        .expect("Pattern wikilink est une constante — ne peut pas échouer à la compilation")
});

/// Jaro-Winkler similarity threshold for fuzzy resolution.
pub const FUZZY_THRESHOLD: f64 = 0.88;

/// Result of a wikilink resolution attempt.
#[derive(Debug, Clone)]
pub enum WikilinkResolution {
    /// Exact match (case-insensitive) — contains the `note_id` (ULID).
    Resolved(String),
    /// Fuzzy match via Jaro-Winkler — `note_id` + similarity score.
    Fuzzy(String, f64),
    /// No match found — contains the raw target string.
    Unresolved(String),
}

/// Extracts all `[[...]]` wikilink targets from a note body.
///
/// Returns targets without the surrounding `[[...]]` or the optional `|...` alias.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    WIKILINK_RE
        .captures_iter(body)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect()
}

/// Attempts to resolve a wikilink target to a known `note_id`.
///
/// # Algorithm
/// 1. Exact match (case-insensitive) → [`WikilinkResolution::Resolved`]
/// 2. Jaro-Winkler ≥ [`FUZZY_THRESHOLD`] → [`WikilinkResolution::Fuzzy`] (best match)
/// 3. No match → [`WikilinkResolution::Unresolved`]
///
/// # Parameters
/// - `target`   : raw target extracted from the wikilink
/// - `existing` : list of `(note_id, title)` pairs for known notes
pub fn resolve(target: &str, existing: &[(String, String)]) -> WikilinkResolution {
    // Étape 1 : correspondance exacte (insensible à la casse)
    if let Some((id, _)) = existing
        .iter()
        .find(|(_, t)| t.eq_ignore_ascii_case(target))
    {
        return WikilinkResolution::Resolved(id.clone());
    }

    // Étape 2 : fuzzy Jaro-Winkler
    let mut best: Option<(String, f64)> = None;
    for (id, title) in existing {
        let sim = strsim::jaro_winkler(target, title);
        if sim >= FUZZY_THRESHOLD && best.as_ref().is_none_or(|(_, b)| sim > *b) {
            best = Some((id.clone(), sim));
        }
    }

    match best {
        Some((id, sim)) => WikilinkResolution::Fuzzy(id, sim),
        None => WikilinkResolution::Unresolved(target.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_simple_wikilink() {
        let links = extract_wikilinks("See [[Mon Note]] for details.");
        assert_eq!(links, vec!["Mon Note"]);
    }

    #[test]
    fn extract_wikilink_with_alias() {
        let links = extract_wikilinks("See [[Mon Note|alias]] here.");
        assert_eq!(links, vec!["Mon Note"]);
    }

    #[test]
    fn extract_multiple_wikilinks() {
        let links = extract_wikilinks("[[A]] and [[B]] and [[C]]");
        assert_eq!(links.len(), 3);
    }

    #[test]
    fn resolve_exact_match() {
        let existing = vec![("01ULID".to_string(), "Mon Note".to_string())];
        let r = resolve("Mon Note", &existing);
        assert!(matches!(r, WikilinkResolution::Resolved(id) if id == "01ULID"));
    }

    #[test]
    fn resolve_exact_case_insensitive() {
        let existing = vec![("01ULID".to_string(), "Mon Note".to_string())];
        let r = resolve("mon note", &existing);
        assert!(matches!(r, WikilinkResolution::Resolved(_)));
    }

    #[test]
    fn resolve_fuzzy_match() {
        let existing = vec![("01ULID".to_string(), "Mon Architecture Note".to_string())];
        // Cible très proche → Jaro-Winkler élevé
        let r = resolve("Mon Architecture Note", &existing);
        assert!(matches!(
            r,
            WikilinkResolution::Resolved(_) | WikilinkResolution::Fuzzy(_, _)
        ));
    }

    #[test]
    fn resolve_unresolved_when_no_match() {
        let existing = vec![("01ULID".to_string(), "Quelque chose".to_string())];
        let r = resolve("Note Totalement Differente XYZ", &existing);
        assert!(matches!(r, WikilinkResolution::Unresolved(_)));
    }
}
