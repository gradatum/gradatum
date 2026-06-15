//! Tag extraction — TF-IDF top-5 + kebab-case normalisation.
//!
//! Extracts the most representative terms from a note by applying a TF-IDF
//! score filtered by frequency and IDF weight thresholds.

use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

/// Maximum number of tags returned by [`extract_tags`].
pub const MAX_TAGS: usize = 5;

/// Minimum IDF score for a term to be considered a tag candidate.
pub const MIN_IDF: f32 = 1.5;

/// Minimum TF frequency (occurrences in the document) for a candidate term.
pub const MIN_TF: usize = 2;

/// Extracts the top-[`MAX_TAGS`] TF-IDF tags from a text body.
///
/// # Parameters
/// - `body`       : body of the note to analyse
/// - `corpus_idf` : pre-computed IDF table over the corpus (term → IDF score)
///
/// # Behaviour
/// - Ignores words shorter than 3 characters and FR+EN stopwords.
/// - Filters terms with TF < [`MIN_TF`] or IDF < [`MIN_IDF`].
/// - Normalises to kebab-case before returning.
///
/// # Returns
/// Vector of kebab-case tags ordered by descending TF-IDF score.
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

/// Converts a string to kebab-case by replacing non-alphanumeric characters.
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

/// Returns `true` when a word is a stopword (FR + EN).
///
/// List kept short and stable — extend sparingly.
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
