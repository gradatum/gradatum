//! # gradatum-db-sqlite
//!
//! SQLite implementations of the persistence traits from `gradatum-core`.
//!
//! ## Contents
//!
//! - [`SqliteQueueStore`]: implementation of the `QueueStore` trait via async `sqlx`
//!   (WAL mode, atomic `UPDATE…RETURNING`). Migration `006_apalis_bootstrap.sql`.
//!
//! ## Architecture
//!
//! ```text
//! gradatum-core (L0) — traits QueueStore, IndexStore, DocumentStore
//!     ↑
//! gradatum-db-sqlite (L2) — SQLite implementations (this crate)
//!     ↑
//! gradatum-db (L3) — feature-gated facade (implementation selection)
//!     ↑
//! gradatum-worker / gradatum-server (L4)
//! ```
//!
//! ## References
//!
//! - Migration: `migrations/006_apalis_bootstrap.sql`

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
