//! # gradatum-markdown
//!
//! Markdown parser and writer for Gradatum notes.
//!
//! ## Features
//!
//! - [`parse`]: parses a `.md` file (YAML frontmatter + Markdown body) into a [`ParsedNote`].
//! - [`write_parsed`]: serializes a [`ParsedNote`] to an on-disk `String`.
//! - [`write()`]: serializes a complete [`gradatum_core::note::Note`] to a `String`.
//! - [`wikilinks`]: extracts `[[target]]` or `[[target|alias]]` wikilinks from a body.
//!
//! ## On-disk format
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
//! # Title
//!
//! Note body with [[wikilinks]].
//! ```
//!
//! ## Round-trip
//!
//! `parse(write_parsed(parse(x)?)) == parse(x)` (idempotent over one cycle).
//! Equality holds on values, not on the exact textual representation
//! (`serde_yml` may reorder YAML fields).
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. See
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
