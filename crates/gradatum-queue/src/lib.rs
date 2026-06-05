//! # gradatum-queue
//!
//! Queue SQLite-backed avec sémantique de lease et claim atomique.
//!
//! ## API P2.0b (sqlx-based)
//!
//! - [`Queue`] : trait async pour les opérations de queue
//! - [`SqliteQueue`] : implémentation sqlx WAL, `UPDATE...RETURNING` atomique
//! - [`NewJob`] / [`LeasedJob`] / [`JobId`] : types de données
//! - [`QueueError`] : erreurs sqlx-based
//! - [`JobStatus`] : états d'un job (Pending/Leased/Done/Dead)
//!
//! ## API Phase 1 (rusqlite-based, rétrocompatibilité)
//!
//! - [`LegacyQueue`] : struct rusqlite synchrone (préservée pour tests Phase 1)
//! - [`LegacyQueueError`] : erreurs rusqlite-based
//!
//! ## Garanties
//!
//! - **Atomic claim** : `UPDATE…RETURNING` garantit qu'un seul consommateur
//!   obtient un job, même sous contention concurrente.
//! - **Lease recovery** : un job dont la lease a expiré redevient claimable
//!   automatiquement ; `attempts` est incrémenté à chaque re-claim.
//! - **WAL mode** : `journal_mode=WAL`, `synchronous=NORMAL`.
//!
//! ## Stabilité
//!
//! `0.x` — pas de garantie de stabilité d'API.

#![forbid(unsafe_code)]
#![warn(rust_2018_idioms)]

// Modules internes
pub mod gradatum_queue;
mod job;
pub mod queue;
mod queue_v1;
pub mod schema;

// API v0.2.0 — GradatumQueue façade Apalis (ARCH-D15)
pub use gradatum_queue::GradatumQueue;

// API P2.0b — sqlx-based (exports primaires)
#[deprecated(
    since = "0.2.0",
    note = "Remplacé par GradatumQueue (ARCH-D15). Sera retiré en v0.3.0."
)]
pub use queue::{JobId, JobInfo, JobStatus, LeasedJob, NewJob, Queue, QueueError, SqliteQueue};

// API Phase 1 — rusqlite-based (rétrocompatibilité tests legacy)
pub use job::Job;
pub use job::JobStatus as LegacyJobStatus;
pub use queue_v1::{LegacyQueue, LegacyQueueError};

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
