//! Error types for the `gradatum-markdown` crate.

use thiserror::Error;

/// Opaque YAML error carried by [`MarkdownError::Yaml`].
///
/// The YAML backend is an implementation detail of this crate. Wrapping its
/// error in an opaque type keeps that choice out of the public API, so the
/// backend can be replaced without a breaking SemVer change (it already was
/// once: `serde_yaml` → `serde_yml` → `serde_norway`).
///
/// The full backend diagnostic — including the line and column of the offending
/// token — is preserved verbatim through [`Display`](std::fmt::Display).
///
/// The inner error is deliberately **not** exposed through
/// [`Error::source`](std::error::Error::source): forwarding it would let a
/// caller recover the backend type via `downcast_ref` and re-establish the
/// coupling this type exists to remove.
#[derive(Debug)]
pub struct YamlError(serde_norway::Error);

impl YamlError {
    /// Wraps a backend YAML error.
    ///
    /// Crate-internal on purpose: it keeps [`MarkdownError::Yaml`] impossible to
    /// forge from outside `gradatum-markdown`, so the variant always denotes a
    /// YAML failure that actually occurred.
    pub(crate) fn new(inner: serde_norway::Error) -> Self {
        Self(inner)
    }
}

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for YamlError {}

/// Errors produced during Markdown parsing or serialization.
#[derive(Debug, Error)]
pub enum MarkdownError {
    /// Missing frontmatter: the file does not start with `---\n`.
    #[error("missing frontmatter: file must start with '---\\n'")]
    MissingFrontmatter,

    /// Unterminated frontmatter: no closing `---` delimiter found.
    #[error("unterminated frontmatter: closing '---' delimiter not found")]
    UnterminatedFrontmatter,

    /// YAML serialization or deserialization error in the frontmatter block.
    ///
    /// The message is self-contained: it embeds the backend diagnostic, so
    /// consumers that flatten the error with `to_string()` lose nothing.
    #[error("YAML error in frontmatter: {0}")]
    Yaml(#[source] YamlError),
}
