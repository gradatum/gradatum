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
/// ANN search via sqlite-vec `vec0` virtual table (v0.5.3 ANN-1).
///
/// Expose [`sqlite_vec::search_ann_inner`], [`sqlite_vec::upsert_ann`],
/// et [`sqlite_vec::backfill_ann_from_conn`].
/// Aucun `unsafe` dans ce module — l'enregistrement de l'extension
/// (`sqlite3_auto_extension`) reste dans les bin crates.
pub(crate) mod sqlite_vec;
/// `VectorStore` implementation for `SqliteIndex`.
pub(crate) mod vector_store_impl;

pub use queries::{AuthorRow, Lineage, NoteRecord, extract_h1_title};
pub use sqlite::{
    CodeSymbolMeta, DerivedNote, Freshness, IndexStatusSnapshot, SqliteIndex, fts5_quote_query,
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
