//! SQLite schema for gradatum-queue.
//!
//! ## Primary schema
//!
//! - `worker_leadership`: single slot for leader election
//!
//! ## Legacy schema (backward compatibility)
//!
//! Constants `CREATE_JOBS_TABLE` and `CREATE_IDX_JOBS_STATUS_LEASE`
//! are preserved for the rusqlite-based `LegacyQueue`.
//!
//! `UPDATE…RETURNING` (SQLite ≥ 3.35) is used in the atomic claim.

/// Legacy DDL — `jobs` table, rusqlite schema (backward compatibility for `LegacyQueue`).
pub const CREATE_JOBS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id           TEXT    PRIMARY KEY,
    kind         TEXT    NOT NULL,
    payload_json TEXT    NOT NULL,
    status       TEXT    NOT NULL,
    lease_until  INTEGER,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
);
";

/// Legacy index — composite `status`/`lease_until` for `claim_one` (backward compatibility).
pub const CREATE_IDX_JOBS_STATUS_LEASE: &str = "
CREATE INDEX IF NOT EXISTS idx_jobs_status_lease ON jobs(status, lease_until);
";

/// Primary DDL — schema for the leader-election slot.
///
/// The legacy `jobs_v2` queue no longer exists: the live job queue
/// lives in `gradatum_jobs`, owned by `gradatum_db_sqlite`. Only the single-slot
/// leader election remains.
///
/// One table:
/// - `worker_leadership`: single slot for leader election
pub const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS worker_leadership (
    slot       INTEGER PRIMARY KEY,
    holder     TEXT    NOT NULL,
    expires_at INTEGER NOT NULL
);
";
