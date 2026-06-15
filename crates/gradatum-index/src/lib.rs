//! # gradatum-index
//!
//! Index layer: `SqliteIndex` implements `DocumentStore` + `IndexStore` + `VectorStore`
//! (crates/gradatum-core) and exposes the `Index` facade via blanket impl.
//!
//! ## Contents
//!
//! - `SqliteIndex`: SQLite + FTS5 base, 4 mandatory PRAGMAs (C12), migration `0001_phase1.sql`.
//! - `drift::scan_phase_a`: three-level helper (size → prefix-4 KB → full SHA-256).
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. See [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! SQLite schema, drift detection, and mandatory PRAGMAs (C12) are fully implemented.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// `DocumentStore` implementation for `SqliteIndex`.
pub(crate) mod document_store_impl;
pub mod drift;
/// `IndexStore` implementation for `SqliteIndex`.
pub(crate) mod index_store_impl;
/// Wikilink redirect table (slug → ULID mapping for note renames).
pub mod links;
pub mod migrations;
pub mod queries;
pub mod sqlite;
/// `VectorStore` implementation for `SqliteIndex`.
pub(crate) mod vector_store_impl;

pub use queries::{extract_h1_title, AuthorRow, Lineage, NoteRecord};
pub use sqlite::{
    fts5_quote_query, CodeSymbolMeta, DerivedNote, Freshness, IndexStatusSnapshot, SqliteIndex,
};
// SearchHitRaw / LessonHitRaw / Code* are defined in gradatum-core — re-exported here for compatibility.
pub use gradatum_core::index_store::{CodeScopeEntryRaw, CodeSelector, LessonHitRaw, SearchHitRaw};

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
