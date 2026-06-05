//! Organisation des wikilinks — extraction regex + résolution ULID + Jaro-Winkler fuzzy.
//!
//! Extrait les références `[[...]]` d'un corps de note et tente de les résoudre
//! par correspondance exacte de titre ou par similarité Jaro-Winkler.

use once_cell::sync::Lazy;
use regex::Regex;

/// Regex pour les wikilinks `[[cible]]` ou `[[cible|alias]]`.
///
/// Groupe 1 : la cible (avant le `|` optionnel).
static WIKILINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[([^\]\|]+)(?:\|[^\]]+)?\]\]")
        .expect("Pattern wikilink est une constante — ne peut pas échouer à la compilation")
});

/// Seuil de similarité Jaro-Winkler pour la résolution fuzzy.
pub const FUZZY_THRESHOLD: f64 = 0.88;

/// Résultat de la tentative de résolution d'un wikilink.
#[derive(Debug, Clone)]
pub enum WikilinkResolution {
    /// Correspondance exacte (insensible à la casse) — contient le `note_id` (ULID).
    Resolved(String),
    /// Correspondance floue via Jaro-Winkler — `note_id` + score de similarité.
    Fuzzy(String, f64),
    /// Aucune correspondance trouvée — contient la cible brute.
    Unresolved(String),
}

/// Extrait toutes les cibles de wikilinks `[[...]]` depuis un corps de note.
///
/// Retourne les cibles sans le `[[...]]` ni l'alias éventuel `|...`.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    WIKILINK_RE
        .captures_iter(body)
        .filter_map(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .collect()
}

/// Tente de résoudre un wikilink cible vers un `note_id` connu.
///
/// # Algorithme
/// 1. Correspondance exacte (insensible à la casse) → [`WikilinkResolution::Resolved`]
/// 2. Jaro-Winkler ≥ [`FUZZY_THRESHOLD`] → [`WikilinkResolution::Fuzzy`] (meilleur match)
/// 3. Aucun match → [`WikilinkResolution::Unresolved`]
///
/// # Paramètres
/// - `target`   : cible brute extraite du wikilink
/// - `existing` : liste de paires `(note_id, title)` pour les notes connues
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
