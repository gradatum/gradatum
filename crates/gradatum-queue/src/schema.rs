//! SQLite schema pour gradatum-queue.
//!
//! ## Schema P2.0b
//!
//! - `jobs_v2` : queue principale avec lease expirable + retry dead-letter
//! - `worker_leadership` : slot unique pour élection leader (T2 worker)
//!
//! ## Schema Phase 1 (rétrocompatibilité)
//!
//! Les constantes `CREATE_JOBS_TABLE` et `CREATE_IDX_JOBS_STATUS_LEASE`
//! sont preservées pour la [`crate::LegacyQueue`] rusqlite-based.
//!
//! `UPDATE…RETURNING` (SQLite ≥ 3.35) utilisé dans le claim atomique.

/// DDL Phase 1 — table jobs schema rusqlite (retrocompatibilite LegacyQueue).
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

/// Index Phase 1 — composite status/lease_until pour claim_one (retrocompatibilite).
pub const CREATE_IDX_JOBS_STATUS_LEASE: &str = "
CREATE INDEX IF NOT EXISTS idx_jobs_status_lease ON jobs(status, lease_until);
";

/// DDL P2.0b — schema sqlx-based avec AUTOINCREMENT, tenant_id, payload BLOB.
///
/// Deux tables :
/// - `jobs_v2` : queue principale avec lease expirable + retry dead-letter
/// - `worker_leadership` : slot unique pour election leader (T2)
///
/// Indexes :
/// - `idx_jobs_v2_pending` : filtrage rapide status='pending' + ordre FIFO
/// - `idx_jobs_v2_lease` : recolte des leases expirees
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
