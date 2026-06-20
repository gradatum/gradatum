//! SQLite schema for gradatum-queue.
//!
//! ## Primary schema
//!
//! - `jobs_v2`: main queue with expirable lease and dead-letter retry
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

/// Primary DDL — sqlx-based schema with AUTOINCREMENT, `tenant_id`, and `BLOB` payload.
///
/// Two tables:
/// - `jobs_v2`: main queue with expirable lease and dead-letter retry
/// - `worker_leadership`: single slot for leader election
///
/// Indexes:
/// - `idx_jobs_v2_pending`: fast filtering on `status='pending'` with FIFO ordering
/// - `idx_jobs_v2_lease`: harvesting expired leases
pub const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS jobs_v2 (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id    TEXT    NOT NULL DEFAULT 'main',
    kind         TEXT    NOT NULL,
    payload      BLOB    NOT NULL,
    status       TEXT    NOT NULL DEFAULT 'pending',
    attempts     INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    lease_until  INTEGER,
    leased_by    TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_error   TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_v2_pending
    ON jobs_v2 (status, created_at) WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_jobs_v2_lease
    ON jobs_v2 (lease_until) WHERE status = 'leased';

CREATE TABLE IF NOT EXISTS worker_leadership (
    slot       INTEGER PRIMARY KEY,
    holder     TEXT    NOT NULL,
    expires_at INTEGER NOT NULL
);
";
