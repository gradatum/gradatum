//! Error types for the `gradatum-markdown` crate.

use thiserror::Error;

/// Errors produced during Markdown parsing or serialization.
#[derive(Debug, Error)]
pub enum MarkdownError {
    /// Missing frontmatter: the file does not start with `---\n`.
    #[error("frontmatter manquant : le fichier doit commencer par '---\\n'")]
    MissingFrontmatter,

    /// Unterminated frontmatter: no closing `---` delimiter found.
    #[error("frontmatter non terminé : délimiteur '---' fermant introuvable")]
    UnterminatedFrontmatter,

    /// YAML deserialization error in the frontmatter block.
    #[error("erreur YAML dans le frontmatter : {0}")]
    Yaml(#[from] serde_yml::Error),
}
