//! Erreurs du crate `gradatum-markdown`.

use thiserror::Error;

/// Erreurs de parsing/sérialisation Markdown.
#[derive(Debug, Error)]
pub enum MarkdownError {
    /// Le frontmatter YAML est manquant (le fichier ne commence pas par `---\n`).
    #[error("frontmatter manquant : le fichier doit commencer par '---\\n'")]
    MissingFrontmatter,

    /// Le frontmatter n'a pas de délimiteur fermant (`---` sur sa propre ligne).
    #[error("frontmatter non terminé : délimiteur '---' fermant introuvable")]
    UnterminatedFrontmatter,

    /// Erreur de désérialisation YAML du frontmatter.
    #[error("erreur YAML dans le frontmatter : {0}")]
    Yaml(#[from] serde_yml::Error),
}
