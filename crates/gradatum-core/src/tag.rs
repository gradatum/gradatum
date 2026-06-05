//! Tag de note Gradatum — newtype String validé.
//!
//! Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md` §2.3.
//!
//! Un tag est une chaîne en minuscules `^[a-z0-9][a-z0-9-]{0,63}$`.
//! La validation est effectuée manuellement pour éviter la dépendance `regex` en core L0.

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Tag d'une note Gradatum.
///
/// Newtype `String` validé au constructeur.
/// Format : `^[a-z0-9][a-z0-9-]{0,63}$` (minuscules, chiffres, tiret, 1–64 chars).
///
/// ## Exemples
///
/// ```
/// use gradatum_core::tag::Tag;
///
/// let t = Tag::new("council-art19").unwrap();
/// assert_eq!(t.as_str(), "council-art19");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Construit un `Tag` depuis une chaîne, en validant le format.
    ///
    /// # Erreurs
    ///
    /// Retourne `ValidationError::InvalidTag` si :
    /// - La chaîne est vide ou dépasse 64 caractères.
    /// - Le premier caractère n'est pas `[a-z0-9]`.
    /// - Les caractères suivants contiennent autre chose que `[a-z0-9-]`.
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

    /// Retourne la valeur du tag sous forme de slice.
    pub fn as_str(&self) -> &str {
        &self.0
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
