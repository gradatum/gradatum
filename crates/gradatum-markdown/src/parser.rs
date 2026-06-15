//! Markdown and YAML frontmatter parser for Gradatum notes.
//!
//! ## Expected format
//!
//! ```text
//! ---
//! <yaml frontmatter>
//! ---
//!
//! <body markdown>
//! ```
//!
//! ## Design — `ParsedNote` vs `Note`
//!
//! The parser produces a [`ParsedNote`] rather than a complete [`gradatum_core::note::Note`].
//! The on-disk Markdown file does not contain:
//! - `NoteId` — carried by the filename (`<ulid>.md`), assigned by `gradatum-vault`.
//! - `NoteVersion` — monotonic counter managed by `gradatum-vault` on each write.
//! - `IntegritySignature` — optional, provided by `gradatum-acl-auth` when enabled.
//!
//! The caller (typically `gradatum-vault`) assembles the complete `Note` by adding
//! these three fields after calling [`parse`].

use gradatum_core::{frontmatter::Frontmatter, identity::ContentHash, note::NoteBody};

use crate::error::MarkdownError;

/// Result of parsing a Gradatum Markdown file.
///
/// Contains the data extracted from the file, excluding vault-managed fields
/// (`NoteId`, `NoteVersion`, `IntegritySignature`).
///
/// The caller assembles the complete `Note`:
/// ```rust,ignore
/// let parsed = parse(raw)?;
/// let note = Note {
///     id: NoteId::from_filename(filename),
///     frontmatter: parsed.frontmatter,
///     body: parsed.body,
///     version: NoteVersion::initial(),
///     content_hash: parsed.content_hash,
///     integrity_signature: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNote {
    /// Canonical metadata deserialized from the YAML block.
    pub frontmatter: Frontmatter,

    /// Markdown body — everything after the closing `---`.
    pub body: NoteBody,

    /// JCS SHA-256 hash computed from the frontmatter and body.
    ///
    /// Pre-computed here to avoid a second parse on the vault side.
    /// Invariant: `content_hash == ContentHash::compute(&frontmatter, &body.markdown)`.
    pub content_hash: ContentHash,
}

/// Parses a Gradatum Markdown file from its string representation.
///
/// ## Algorithm
///
/// 1. Verifies that the content starts with `---\n`.
/// 2. Searches for the closing `---` line starting at position 4.
/// 3. Deserializes the YAML block between the two delimiters via `serde_yml`.
/// 4. Extracts the body: everything after the closing `---\n`.
///    Strips a leading `\n` if present (blank line between frontmatter and title).
/// 5. Computes the `ContentHash` via JCS (RFC 8785).
///
/// ## Errors
///
/// - [`MarkdownError::MissingFrontmatter`] if the content does not start with `---\n`.
/// - [`MarkdownError::UnterminatedFrontmatter`] if the closing delimiter is absent.
/// - [`MarkdownError::Yaml`] if the YAML block is invalid.
///
/// ## Example
///
/// ```
/// use gradatum_markdown::parse;
///
/// let raw = "---\nschema_version: 1\nvault_id: main\nsection: decisions\nstatus: live\ncreated: \"2026-05-04T11:00:00Z\"\n---\n\n# titre\n\nCorps.\n";
/// let parsed = parse(raw).unwrap();
/// assert_eq!(parsed.frontmatter.vault_id, "main");
/// ```
pub fn parse(raw: &str) -> Result<ParsedNote, MarkdownError> {
    // Étape 1 : vérifier le délimiteur ouvrant.
    if !raw.starts_with("---\n") {
        return Err(MarkdownError::MissingFrontmatter);
    }

    // Étape 2 : chercher le délimiteur fermant à partir de la position 4.
    // On cherche "\n---\n" après le premier "---\n" pour trouver la fin du bloc YAML.
    // La position de recherche commence après "---\n" (4 bytes).
    let search_start = 4;
    let close_marker = "\n---\n";

    let close_pos = raw[search_start..]
        .find(close_marker)
        .ok_or(MarkdownError::UnterminatedFrontmatter)?;

    // Position absolue du "\n" qui précède "---\n" fermant.
    let yaml_end = search_start + close_pos;

    // Étape 3 : extraire et parser le bloc YAML.
    let yaml_block = &raw[4..yaml_end];
    let frontmatter: Frontmatter = serde_yml::from_str(yaml_block).map_err(MarkdownError::Yaml)?;

    // Étape 4 : extraire le body (après le "\n---\n" fermant).
    let body_start = yaml_end + close_marker.len();
    let body_raw = raw.get(body_start..).unwrap_or("");

    // Strip un leading '\n' si présent (ligne vide conventionnelle entre frontmatter et body).
    let body_str = body_raw.strip_prefix('\n').unwrap_or(body_raw);

    let body = NoteBody {
        markdown: body_str.to_owned(),
    };

    // Étape 5 : calculer le ContentHash (JCS RFC 8785).
    let content_hash = ContentHash::compute(&frontmatter, &body.markdown);

    Ok(ParsedNote {
        frontmatter,
        body,
        content_hash,
    })
}
