//! Wikilink handling — regex extraction + ULID resolution + Jaro-Winkler fuzzy matching.
//!
//! Extracts `[[...]]` references from a note body and attempts to resolve
//! them by exact title match or Jaro-Winkler similarity.
//!
//! ## Format wikilink ULID-first (B5 fix)
//!
//! Les wikilinks écrits par le vault ont la forme `[[section:ULID]]` (ex :
//! `[[decisions:01KVBTMYNK4XXZJAKWMTB4AM9K]]`). La fonction
//! [`parse_ulid_target`] détecte si la cible est un ULID valide, permettant
//! au worker de court-circuiter le lookup H1 et de résoudre directement par
//! identifiant — avec vérification d'existence sur le serveur.

use once_cell::sync::Lazy;
use regex::Regex;
use ulid::Ulid;

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

/// Détecte si une cible de wikilink encode un ULID résolvable par identifiant.
///
/// Supporte deux formes :
/// - `"section:ULID"` — le ULID se trouve après le dernier `:` (ex. `"decisions:01KV..."`)
/// - `"ULID"` nu — la cible est directement un ULID Crockford
///
/// Un ULID Crockford valide = exactement 26 caractères base32 (insensible à la casse).
///
/// # Retour
///
/// - `Some(ulid)` si la partie extraite est un ULID valide au sens de [`Ulid::from_string`].
/// - `None` si la cible est un titre libre (ex. `"Mon Architecture Note"`,
///   `"example-agent Phase D"`) ou un ULID malformé.
///
/// # Exemples
///
/// ```
/// use gradatum_curator::wikilinks::parse_ulid_target;
///
/// assert!(parse_ulid_target("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K").is_some());
/// assert!(parse_ulid_target("01KVBTMYNK4XXZJAKWMTB4AM9K").is_some());
/// assert!(parse_ulid_target("Mon Titre Humain").is_none());
/// assert!(parse_ulid_target("example-agent Phase D").is_none());
/// ```
pub fn parse_ulid_target(target: &str) -> Option<Ulid> {
    // Extraire la partie candidate : tout ce qui suit le dernier `:`,
    // ou la chaîne entière si elle ne contient pas de `:`.
    let candidate = target.rsplit(':').next().unwrap_or(target);
    Ulid::from_string(candidate).ok()
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

    // ── Tests parse_ulid_target ───────────────────────────────────────────────

    /// Cible `"section:ULID"` — forme canonique des wikilinks vault.
    #[test]
    fn parse_ulid_target_section_colon_ulid_returns_some() {
        let ulid = parse_ulid_target("decisions:01KVBTMYNK4XXZJAKWMTB4AM9K");
        assert!(ulid.is_some(), "doit parser un ULID après le ':'");
        assert_eq!(
            ulid.unwrap().to_string(),
            "01KVBTMYNK4XXZJAKWMTB4AM9K",
            "ULID retourné doit correspondre à la partie après ':'"
        );
    }

    /// ULID nu sans préfixe section.
    #[test]
    fn parse_ulid_target_bare_ulid_returns_some() {
        let ulid = parse_ulid_target("01KVBTMYNK4XXZJAKWMTB4AM9K");
        assert!(ulid.is_some(), "ULID nu doit être reconnu");
    }

    /// Titre humain libre — ne doit jamais être confondu avec un ULID.
    #[test]
    fn parse_ulid_target_human_title_returns_none() {
        assert!(
            parse_ulid_target("Mon Titre Humain").is_none(),
            "titre humain n'est pas un ULID"
        );
    }

    /// Titre legacy contenant un tiret — ne doit pas être un ULID.
    #[test]
    fn parse_ulid_target_legacy_free_text_returns_none() {
        assert!(
            parse_ulid_target("example-agent Phase D").is_none(),
            "texte libre avec tiret n'est pas un ULID"
        );
    }

    /// ULID trop court (25 chars) — doit retourner None.
    #[test]
    fn parse_ulid_target_short_ulid_returns_none() {
        // 25 chars — un ULID Crockford = 26 chars
        assert!(
            parse_ulid_target("01KVBTMYNK4XXZJAKWMTB4AM9").is_none(),
            "ULID à 25 chars n'est pas valide"
        );
    }

    /// ULID trop long (27 chars) — doit retourner None.
    #[test]
    fn parse_ulid_target_long_ulid_returns_none() {
        // 27 chars — invalide
        assert!(
            parse_ulid_target("01KVBTMYNK4XXZJAKWMTB4AM9KX").is_none(),
            "ULID à 27 chars n'est pas valide"
        );
    }

    // ── Tests extract_wikilinks (existants) ──────────────────────────────────

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
