//! Classification par tags — TF-IDF top-5 + normalisation kebab-case.
//!
//! Extrait les termes les plus représentatifs d'une note en appliquant
//! un score TF-IDF filtré par des seuils de fréquence et de pondération IDF.

use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

/// Nombre maximum de tags retournés par [`extract_tags`].
pub const MAX_TAGS: usize = 5;

/// Score IDF minimum pour qu'un terme soit considéré comme tag candidat.
pub const MIN_IDF: f32 = 1.5;

/// Fréquence TF minimum (occurrences dans le document) pour un terme candidat.
pub const MIN_TF: usize = 2;

/// Extrait les tags TF-IDF top-[`MAX_TAGS`] d'un corps de texte.
///
/// # Paramètres
/// - `body`       : corps de la note à analyser
/// - `corpus_idf` : table IDF pré-calculée sur le corpus (terme → score IDF)
///
/// # Comportement
/// - Ignore les mots < 3 caractères et les stopwords FR+EN
/// - Filtre les termes dont TF < [`MIN_TF`] ou IDF < [`MIN_IDF`]
/// - Normalise en kebab-case avant de retourner
///
/// # Retour
/// Vecteur de tags kebab-case ordonnés par score TF-IDF décroissant.
pub fn extract_tags(body: &str, corpus_idf: &HashMap<String, f32>) -> Vec<String> {
    let mut tf: HashMap<String, usize> = HashMap::new();
    for word in body.unicode_words() {
        let normalized = word.to_lowercase();
        if normalized.len() < 3 || is_stopword(&normalized) {
            continue;
        }
        *tf.entry(normalized).or_insert(0) += 1;
    }

    let mut scored: Vec<(String, f32)> = tf
        .iter()
        .filter(|(_, c)| **c >= MIN_TF)
        .filter_map(|(w, c)| {
            corpus_idf
                .get(w)
                .filter(|&&idf| idf >= MIN_IDF)
                .map(|idf| (w.clone(), (*c as f32) * idf))
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    scored
        .into_iter()
        .take(MAX_TAGS)
        .map(|(w, _)| kebab_case(&w))
        .collect()
}

/// Convertit une chaîne en kebab-case en remplaçant les caractères non-alphanumériques.
fn kebab_case(s: &str) -> String {
    let raw: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    raw.trim_matches('-').to_string()
}

/// Vérifie si un mot est un stopword (FR + EN).
///
/// Liste conservée courte et stable — augmenter avec parcimonie.
fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "and"
            | "for"
            | "with"
            | "this"
            | "that"
            | "have"
            | "from"
            | "is"
            | "are"
            | "was"
            | "were"
            | "a"
            | "an"
            | "of"
            | "to"
            | "in"
            | "on"
            | "at"
            // Stopwords FR
            | "le"
            | "la"
            | "les"
            | "un"
            | "une"
            | "des"
            | "du"
            | "de"
            | "et"
            | "en"
            | "au"
            | "aux"
            | "par"
            | "sur"
            | "dans"
            | "qui"
            | "que"
            | "ou"
            | "est"
            | "son"
            | "sa"
            | "ses"
            | "il"
            | "elle"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_case_replaces_spaces() {
        assert_eq!(kebab_case("hello world"), "hello-world");
    }

    #[test]
    fn kebab_case_trims_dashes() {
        assert_eq!(kebab_case(" rust "), "rust");
    }

    #[test]
    fn extract_tags_empty_without_idf() {
        // Sans table IDF, aucun tag retourné
        let tags = extract_tags("rust rust rust architecture", &HashMap::new());
        assert!(tags.is_empty());
    }

    #[test]
    fn extract_tags_respects_min_tf() {
        let mut idf = HashMap::new();
        idf.insert("rust".to_string(), 2.0_f32);
        // "rust" apparaît 1 fois seulement → MIN_TF=2 → rejeté
        let tags = extract_tags("rust architecture", &idf);
        assert!(tags.is_empty());
    }

    #[test]
    fn extract_tags_returns_top_tags() {
        let mut idf = HashMap::new();
        idf.insert("rust".to_string(), 2.0_f32);
        idf.insert("architecture".to_string(), 2.0_f32);
        // "rust" × 3 + "architecture" × 2 → les deux passent MIN_TF=2
        let tags = extract_tags("rust rust rust architecture architecture", &idf);
        assert!(!tags.is_empty());
        assert!(tags.contains(&"rust".to_string()));
    }
}
