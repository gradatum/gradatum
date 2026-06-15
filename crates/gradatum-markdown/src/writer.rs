//! Serialization of a `ParsedNote` to its on-disk Markdown representation.
//!
//! ## Produced format
//!
//! ```text
//! ---
//! <yaml frontmatter>
//! ---
//!
//! <body markdown>
//! ```
//!
//! ## Round-trip guarantee
//!
//! `parse(write_parsed(parse(x)?)) == parse(x)` (idempotent over one cycle).
//! Strict string equality is not guaranteed — `serde_yml` may reorder fields
//! relative to the original. The guarantee covers **values** after re-parsing,
//! not the exact textual representation.

use crate::error::MarkdownError;
use crate::parser::ParsedNote;
use gradatum_core::note::Note;

/// Serializes a `ParsedNote` to its on-disk Markdown representation.
///
/// Produces the Gradatum on-disk format:
/// ```text
/// ---\n<yaml>\n---\n\n<body>
/// ```
///
/// `serde_yml::to_string` emits YAML without a leading `---` delimiter,
/// so the delimiters are added manually before and after.
///
/// ## Errors
///
/// Returns `MarkdownError::Yaml` if the frontmatter cannot be serialized.
/// In practice this cannot occur for `Frontmatter` (no `f32::NAN` or non-YAML types).
pub fn write_parsed(note: &ParsedNote) -> Result<String, MarkdownError> {
    let yaml = serde_yml::to_string(&note.frontmatter).map_err(MarkdownError::Yaml)?;
    // serde_yml 0.9 produit "<yaml content>\n" sans délimiteur ---
    // On enveloppe manuellement.
    Ok(format!("---\n{}---\n\n{}", yaml, note.body.markdown))
}

/// Serializes a complete `Note` to its on-disk Markdown representation.
///
/// Equivalent to `write_parsed` but accepts a full `Note`.
/// The fields `id`, `version`, and `integrity_signature` are ignored —
/// they are not part of the on-disk representation (the filename carries
/// the `NoteId`, and `version` is managed by `gradatum-vault`).
///
/// ## Errors
///
/// Returns `MarkdownError::Yaml` if serialization fails.
pub fn write(note: &Note) -> Result<String, MarkdownError> {
    let parsed = ParsedNote {
        frontmatter: note.frontmatter.clone(),
        body: note.body.clone(),
        content_hash: note.content_hash,
    };
    write_parsed(&parsed)
}
