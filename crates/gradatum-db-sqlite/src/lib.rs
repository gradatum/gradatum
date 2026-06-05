//! # gradatum-db-sqlite
//!
//! Implémentations SQLite des traits de persistance de `gradatum-core`.
//!
//! ## Contenu v0.2.0 (Phase 1.1)
//!
//! - [`SqliteQueueStore`] : implémentation du trait [`QueueStore`] via `sqlx`
//!   async (WAL mode, `UPDATE…RETURNING` atomique). Migration `006_apalis_bootstrap.sql`.
//!
//! ## Roadmap
//!
//! - `SqliteIndexStore` (impl `IndexStore`) — v0.2.0 Phase B
//! - `MarkdownStore` (impl `DocumentStore`) — v0.2.0 Phase B
//! - `SqliteVecStore` (impl `VectorStore`) — v0.4.0 Phase B sqlite-vec
//!
//! ## Architecture
//!
//! ```text
//! gradatum-core (L0) — traits QueueStore, IndexStore, DocumentStore
//!     ↑
//! gradatum-db-sqlite (L2) — implémentations SQLite (ce crate)
//!     ↑
//! gradatum-db (L3) — façade feature-gated (sélection impl)
//!     ↑
//! gradatum-worker / gradatum-server (L4)
//! ```
//!
//! ## Références
//!
//! - v81 architecture §7 L3026 (gradatum-db-sqlite crate obligatoire)
//! - v81 §10 Étape 0.2.d (SqliteQueueStore dans gradatum-db-sqlite)
//! - Migration : `migrations/006_apalis_bootstrap.sql`

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod queue_store_sqlite;

pub use queue_store_sqlite::{
    apply_sqlite_pragmas, idempotency_cleanup, idempotency_insert, idempotency_lookup,
    run_migrations, SqliteQueueStore,
};

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
