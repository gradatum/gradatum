//! Generic language-parser abstraction (feature `code-rust`).
//!
//! This module exposes the [`LanguageParser`] trait and the pipeline function
//! [`parse_with_language_parser`], which orchestrates the common tree-sitter pipeline:
//! creating the `Parser`, calling `set_language`, `parse`, and handling errors.
//!
//! Language-specific details (grammar, symbol extraction) are delegated to
//! implementations of the trait.
//!
//! ## Extensibility
//!
//! To add support for a new language (TypeScript, Python, Bash, ...):
//! 1. Create a `<lang>_parser.rs` file in this crate.
//! 2. Define a struct `<Lang>Parser { … }` that implements `LanguageParser`.
//! 3. Expose a `parse_<lang>_file` function in `lib.rs` that instantiates the struct
//!    and calls `parse_with_language_parser`.
//!
//! The common pipeline (this module) does not need to change.

use crate::{DerivedSymbol, IngestError};

/// Abstraction of a source-language parser producing derived symbols.
///
/// An impl of this trait encapsulates language-specific knowledge:
/// - the tree-sitter grammar to use,
/// - the symbol extraction logic from the AST.
///
/// The common pipeline (creating the `Parser`, `set_language`, `parse`, error handling)
/// is factored out in [`parse_with_language_parser`].
///
/// # Errors
/// See [`IngestError`].
pub(crate) trait LanguageParser {
    /// Returns the tree-sitter grammar for this language.
    fn ts_language(&self) -> tree_sitter::Language;

    /// Extracts symbols from a parsed tree-sitter AST.
    ///
    /// # Parameters
    /// - `tree`: the AST produced by `tree_sitter::Parser::parse`.
    /// - `source`: the source bytes (identical to those passed to the parser).
    /// - `source_path`: relative file path (used in errors and `DerivedSymbol` fields).
    fn extract_symbols(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        source_path: &str,
    ) -> Vec<DerivedSymbol>;
}

/// Runs the common pipeline: creates a tree-sitter `Parser`, calls `set_language`,
/// parses the source bytes, then delegates extraction to the provided [`LanguageParser`].
///
/// # Errors
/// - [`IngestError::ParseError`] if `set_language` fails.
/// - Returns `Ok(Vec::new())` if `parser.parse` returns `None` (file silently ignored).
pub(crate) fn parse_with_language_parser(
    parser_impl: &dyn LanguageParser,
    source_path: &str,
    content: &str,
) -> Result<Vec<DerivedSymbol>, IngestError> {
    let mut parser = tree_sitter::Parser::new();
    let language = parser_impl.ts_language();
    parser
        .set_language(&language)
        .map_err(|e| IngestError::ParseError {
            path: source_path.to_string(),
            reason: format!("tree-sitter set_language: {e}"),
        })?;

    let tree = match parser.parse(content, None) {
        Some(t) => t,
        None => {
            tracing::warn!(path = %source_path, "tree-sitter parse returned None (fichier ignoré)");
            return Ok(Vec::new());
        }
    };

    let source_bytes = content.as_bytes();
    Ok(parser_impl.extract_symbols(&tree, source_bytes, source_path))
}
