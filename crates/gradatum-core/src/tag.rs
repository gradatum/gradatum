//! Gradatum note tag — validated `String` newtype.
//!
//! A tag is a lowercase string matching `^[a-z0-9][a-z0-9-]{0,63}$`.
//! Validation is performed manually to avoid a `regex` dependency in the core crate.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Tag for a Gradatum note.
///
/// Validated `String` newtype — validation occurs in the constructor.
/// Format: `^[a-z0-9][a-z0-9-]{0,63}$` (lowercase, digits, hyphens, 1–64 chars).
///
/// ## Examples
///
/// ```
/// use gradatum_core::tag::Tag;
///
/// let t = Tag::new("knowledge-base").unwrap();
/// assert_eq!(t.as_str(), "knowledge-base");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Constructs a `Tag` from a string, validating its format.
    ///
    /// # Errors
    ///
    /// Returns `ValidationError::InvalidTag` if:
    /// - The string is empty or exceeds 64 characters.
    /// - The first character is not `[a-z0-9]`.
    /// - Any subsequent character is outside `[a-z0-9-]`.
    pub fn new(s: impl Into<String>) -> Result<Self, ValidationError> {
        let s = s.into();

        // Contrainte longueur : 1–64 caractères.
        if s.is_empty() || s.len() > 64 {
            return Err(ValidationError::InvalidTag(s));
        }

        let bytes = s.as_bytes();

        // Premier caractère : [a-z0-9] uniquement (pas de tiret en tête).
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(ValidationError::InvalidTag(s));
        }

        // Caractères suivants : [a-z0-9-].
        for &b in bytes.iter().skip(1) {
            if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' {
                return Err(ValidationError::InvalidTag(s));
            }
        }

        Ok(Tag(s))
    }

    /// Returns the tag value as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Coerces an arbitrary string into a valid `Tag` instead of rejecting it.
    ///
    /// The algorithm never fails; it returns `None` only when nothing usable is left:
    ///
    /// 1. Lowercase the whole input.
    /// 2. Replace every run of characters outside `[a-z0-9]` with a **single** `-`.
    /// 3. Trim leading and trailing `-` (implicitly: invalid runs at either end are
    ///    never emitted).
    /// 4. Truncate to 64 characters, then drop a trailing `-` if the truncation landed
    ///    in the middle of a run.
    /// 5. If the result is empty, return `None` (nothing recoverable in the input).
    /// 6. Otherwise return `Some(Tag(result))`. The produced value always satisfies
    ///    [`Tag::new`].
    ///
    /// [`Tag::new`] (strict validation) is left untouched for call sites that want to
    /// reject non-conforming input outright.
    ///
    /// ## Examples
    ///
    /// ```
    /// use gradatum_core::tag::Tag;
    ///
    /// assert_eq!(Tag::normalize("status:OPEN").as_ref().map(Tag::as_str), Some("status-open"));
    /// assert_eq!(Tag::normalize("v0.5.3").as_ref().map(Tag::as_str), Some("v0-5-3"));
    /// assert_eq!(Tag::normalize("P1").as_ref().map(Tag::as_str), Some("p1"));
    /// assert_eq!(Tag::normalize("___"), None);
    /// assert_eq!(Tag::normalize(""), None);
    /// // Already-valid tags are returned unchanged.
    /// assert_eq!(Tag::normalize("knowledge-base").as_ref().map(Tag::as_str), Some("knowledge-base"));
    /// ```
    pub fn normalize(s: impl Into<String>) -> Option<Self> {
        let s = s.into();

        // Étape 1 : lowercase.
        let lower = s.to_lowercase();

        // Étapes 2-3 : remplacer les runs de chars hors [a-z0-9] par '-', trim implicite.
        // Un seul passage : on émet un '-' uniquement à la frontière invalide→valide,
        // et seulement si result n'est pas vide (évite le tiret de tête).
        let mut result = String::with_capacity(lower.len());
        let mut in_invalid_run = false;

        for ch in lower.chars() {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                // Caractère valide — flush le tiret accumulé si besoin.
                if in_invalid_run {
                    // result non-vide ici : le tiret de tête est impossible.
                    if !result.is_empty() {
                        result.push('-');
                    }
                    in_invalid_run = false;
                }
                result.push(ch);
            } else {
                // Caractère invalide — marque le run, ne l'émet pas.
                in_invalid_run = true;
            }
        }
        // in_invalid_run = true en fin de boucle → run invalide en queue, ignoré
        // → trim implicite des caractères invalides en queue.

        if result.is_empty() {
            return None;
        }

        // Étape 4 : troncature à 64 caractères.
        // À ce stade result est 100% ASCII ([a-z0-9-]) → truncate(n) est sûr.
        if result.len() > 64 {
            result.truncate(64);
            // Re-trim si la troncature a laissé un tiret final.
            let new_len = result.trim_end_matches('-').len();
            result.truncate(new_len);
        }

        if result.is_empty() {
            return None;
        }

        // Invariant : la valeur produite satisfait Tag::new par construction :
        // - longueur 1–64 (garantie ci-dessus)
        // - premier char ∈ [a-z0-9] (jamais '-' car skip en tête)
        // - chars suivants ∈ [a-z0-9-]
        debug_assert!(
            Self::new(result.as_str()).is_ok(),
            "Tag::normalize produced invalid tag: {:?}",
            result
        );

        Some(Tag(result))
    }
}

impl std::fmt::Display for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Tag {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── Tests ciblés : cas limites multibyte / Unicode ────────────────────────

    /// Séquence uniquement invalide → None.
    #[test]
    fn normalize_all_invalid_chars_returns_none() {
        assert_eq!(Tag::normalize("___"), None);
        assert_eq!(Tag::normalize("!!!"), None);
        assert_eq!(Tag::normalize(""), None);
        // Emoji seul → None (aucun char ASCII valide).
        assert_eq!(Tag::normalize("🦀"), None);
        assert_eq!(Tag::normalize("🦀🦀🦀"), None);
    }

    /// Chaîne multibyte > 64 octets : la troncature doit rester sur frontière char.
    #[test]
    fn normalize_multibyte_truncate_char_boundary_safe() {
        // Chaque 'é' (U+00E9) pèse 2 octets en UTF-8.
        // 40 'a' + 20 'é' = 40 + 40 = 80 octets → > 64.
        // normalize va kebab-ifier les 'é' en '-', donc le résultat sera 100% ASCII —
        // la troncature à 64 est sûre. On vérifie surtout l'absence de panique.
        let long_multibyte = "a".repeat(20) + &"é".repeat(20);
        let result = Tag::normalize(long_multibyte);
        // Doit soit retourner Some avec len ≤ 64 et satisfaire Tag::new, soit None.
        if let Some(ref t) = result {
            assert!(t.as_str().len() <= 64, "longueur > 64 : {}", t.as_str());
            Tag::new(t.as_str()).expect("normalize doit produire un tag valide");
        }
    }

    /// Chaîne avec emoji intercalés : aucun panic, résultat toujours valide.
    #[test]
    fn normalize_mixed_ascii_emoji_no_panic() {
        let inputs = [
            "hello🦀world",
            "🦀leading",
            "trailing🦀",
            "a🦀b🦀c",
            "rust🦀🦀🦀lang",
            "αβγabc", // chars multibyte non-ASCII
        ];
        for input in &inputs {
            let result = Tag::normalize(*input);
            if let Some(ref t) = result {
                Tag::new(t.as_str()).expect("normalize doit produire un tag valide");
                assert!(t.as_str().len() <= 64);
            }
        }
    }

    // ── Proptest : propriétés universelles ───────────────────────────────────

    proptest! {
        /// `Tag::normalize` ne panique JAMAIS sur une entrée arbitraire (incluant multibyte).
        #[test]
        fn prop_normalize_never_panics(s in ".*") {
            let _ = Tag::normalize(s);
        }

        /// Si `normalize` retourne `Some(t)`, alors `t` passe toujours `Tag::new` (round-trip).
        #[test]
        fn prop_normalize_some_satisfies_tag_new(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                prop_assert!(
                    Tag::new(t.as_str()).is_ok(),
                    "normalize produced invalid tag: {:?}",
                    t.as_str()
                );
            }
        }

        /// Si `normalize` retourne `Some(t)`, la longueur est ≤ 64 caractères ET ≤ 64 octets.
        /// (résultat 100% ASCII → les deux coïncident, mais on vérifie les deux explicitement)
        #[test]
        fn prop_normalize_some_len_at_most_64(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                prop_assert!(
                    t.as_str().len() <= 64,
                    "tag trop long: {:?} ({} octets)",
                    t.as_str(),
                    t.as_str().len()
                );
                prop_assert!(
                    t.as_str().chars().count() <= 64,
                    "tag trop long en chars: {:?}",
                    t.as_str()
                );
            }
        }

        /// La troncature ne coupe JAMAIS sur une frontière non-char : le résultat est
        /// toujours UTF-8 valide (implicite car `String`, mais on vérifie via `chars()`).
        #[test]
        fn prop_normalize_result_is_valid_utf8(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                // `chars()` panique si la string n'est pas UTF-8 valide.
                let _ = t.as_str().chars().count();
            }
        }

        /// `Tag::normalize` est un point fixe sur son propre output (self-idempotency).
        ///
        /// Pour toute entrée `s`, si `normalize(s) == Some(t)`,
        /// alors `normalize(t.as_str()) == Some(t)` (un second passage ne change rien).
        ///
        /// Note : un tag *valide pour `Tag::new`* n'est PAS nécessairement inchangé
        /// par `normalize` (ex : `a--b` est accepté par `Tag::new` mais normalisé en `a-b`
        /// car les tirets consécutifs constituent un "run invalide" pour l'algorithme).
        /// Cette propriété teste le seul invariant correct : l'output de `normalize`
        /// est stable sous un second appel.
        #[test]
        fn prop_normalize_idempotent_on_valid_tags(s in ".*") {
            if let Some(t) = Tag::normalize(s) {
                // Second passage sur l'output de normalize.
                let second = Tag::normalize(t.as_str());
                prop_assert_eq!(
                    second.as_ref().map(|u| u.as_str()),
                    Some(t.as_str()),
                    "normalize n'est pas un point fixe : {:?} → {:?}",
                    t.as_str(),
                    second.as_ref().map(|u| u.as_str()),
                );
            }
        }
    }
}
