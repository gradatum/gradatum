//! # gradatum-index
//!
//! Index layer : `SqliteIndex` implémente `DocumentStore` + `IndexStore` + `VectorStore`
//! (crates/gradatum-core) et expose la façade `Index` via blanket impl.
//!
//! ## Contenu Phase 1
//!
//! - `SqliteIndex` : base SQLite + FTS5, 4 PRAGMA C12, migration `0001_phase1.sql`.
//! - `drift::scan_phase_a` : helper 3 niveaux (size → prefix-4KB → full sha256).
//!
//! ## Stabilité
//!
//! `0.x` — aucune garantie API. Voir [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! Phase 1 implémentée. Spec ref : `docs/superpowers/specs/2026-05-03-phase1-design-gradatum-core.md`
//! §5.2 (schéma SQLite) + §5.3 (drift Phase A) + §0.3 C12 (PRAGMA obligatoires).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Impl de `DocumentStore` pour `SqliteIndex` — carve Étape 0.1.
pub(crate) mod document_store_impl;
pub mod drift;
/// Impl de `IndexStore` pour `SqliteIndex` — carve Étape 0.1.
pub(crate) mod index_store_impl;
pub mod migrations;
pub mod queries;
pub mod sqlite;
/// Impl de `VectorStore` pour `SqliteIndex` — carve Étape 0.1.
pub(crate) mod vector_store_impl;

pub use queries::{AuthorRow, Lineage, NoteRecord};
pub use sqlite::SqliteIndex;
// SearchHitRaw est maintenant défini dans gradatum-core — re-exporté ici pour compat.
pub use gradatum_core::index_store::SearchHitRaw;

/// Version du crate (depuis `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
