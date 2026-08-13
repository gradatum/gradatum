//! # gradatum-queue
//!
//! SQLite-backed queue with lease semantics and atomic claim.
//!
//! ## sqlx-based API
//!
//! - [`Queue`]: async trait for queue operations
//! - [`SqliteQueue`]: sqlx WAL implementation, atomic `UPDATE...RETURNING`
//! - [`NewJob`] / [`LeasedJob`] / [`JobId`]: data types
//! - [`QueueError`]: sqlx-based errors
//! - [`JobStatus`]: job states (`Pending`/`Leased`/`Done`/`Dead`)
//!
//! ## Legacy API (rusqlite-based, backward compatibility)
//!
//! - [`LegacyQueue`]: synchronous rusqlite struct (preserved for backward compatibility)
//! - [`LegacyQueueError`]: rusqlite-based errors
//!
//! ## Guarantees
//!
//! - **Atomic claim**: `UPDATE…RETURNING` ensures only one consumer obtains a job,
//!   even under concurrent contention.
//! - **Lease recovery**: a job whose lease has expired becomes claimable again
//!   automatically; `attempts` is incremented on each re-claim.
//! - **WAL mode**: `journal_mode=WAL`, `synchronous=NORMAL`.
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible additions
//! only within `2.x`, breaking changes deferred to the next major. See
//! [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

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
    note = "Replaced by GradatumQueue (ARCH-D15). Scheduled for removal in a future major version."
)]
pub use queue::{JobId, JobInfo, JobStatus, LeasedJob, NewJob, Queue, QueueError, SqliteQueue};

// API legacy — rusqlite-based (rétrocompatibilité)
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
