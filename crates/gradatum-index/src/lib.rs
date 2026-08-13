//! # gradatum-index
//!
//! Index layer: `SqliteIndex` implements `DocumentStore` + `IndexStore` + `VectorStore`
//! (crates/gradatum-core) and exposes the `Index` facade via blanket impl.
//!
//! ## Contents
//!
//! - `SqliteIndex`: SQLite + FTS5 store, four mandatory PRAGMAs, schema applied by an
//!   embedded migration.
//! - `drift::scan_phase_a`: three-level change detection (size → first 4 KB → full SHA-256).
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org); backward-compatible additions only within `2.x`. See [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! SQLite schema, drift detection, and the mandatory PRAGMAs are fully implemented.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Archive registry (`archive_index`) backing note archiving and retention GC.
pub mod archive;
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
/// Approximate nearest-neighbour search over the sqlite-vec `vec0` virtual table.
///
/// Exposes [`sqlite_vec::search_ann_inner`], [`sqlite_vec::upsert_ann`] and
/// [`sqlite_vec::backfill_ann_from_conn`]. This module contains no `unsafe` code:
/// registering the SQLite extension (`sqlite3_auto_extension`) stays in the binary crates.
pub(crate) mod sqlite_vec;
/// `VectorStore` implementation for `SqliteIndex`.
pub(crate) mod vector_store_impl;

pub use archive::{ARCHIVE_LIST_MAX, ArchiveEntry, ArchiveListFilter};
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
