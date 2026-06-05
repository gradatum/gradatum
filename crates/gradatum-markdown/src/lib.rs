//! # gradatum-markdown
//!
//! Parser et writer Markdown pour les notes Gradatum.
//!
//! ## Fonctionnalités
//!
//! - [`parse`] : parse un fichier `.md` (frontmatter YAML + body Markdown) → [`ParsedNote`].
//! - [`write_parsed`] : sérialise un [`ParsedNote`] → `String` format on-disk.
//! - [`write`] : sérialise une [`gradatum_core::note::Note`] complète → `String`.
//! - [`wikilinks`] : extrait les wikilinks `[[target]]` ou `[[target|alias]]` d'un body.
//!
//! ## Format on-disk (spec §5.1)
//!
//! ```text
//! ---
//! schema_version: 1
//! vault_id: main
//! section: decisions
//! status: live
//! created: "2026-05-04T11:00:00Z"
//! ---
//!
//! # Titre
//!
//! Corps de la note avec [[wikilinks]].
//! ```
//!
//! ## Round-trip
//!
//! `parse(write_parsed(parse(x)?)) == parse(x)` (idempotence à 1 cycle).
//! Garantie sur les valeurs, pas sur la représentation textuelle exacte
//! (serde_yml peut réordonner les champs YAML).
//!
//! ## Stabilité
//!
//! `0.x` — pas de garantie de stabilité API. Voir
//! [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

mod error;
mod parser;
mod wikilink;
mod writer;

// Re-exports publics.
pub use error::MarkdownError;
pub use parser::{parse, ParsedNote};
pub use wikilink::{wikilinks, Wikilink};
pub use writer::{write, write_parsed};

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
