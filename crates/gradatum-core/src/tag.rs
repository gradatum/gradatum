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
