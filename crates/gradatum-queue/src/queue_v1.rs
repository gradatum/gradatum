//! [`LegacyQueue`] — SQLite-backed with atomic claim via `UPDATE…RETURNING`.
//!
//! Synchronous rusqlite implementation preserved for backward compatibility.
//! For current code, use [`crate::SqliteQueue`] (sqlx-based, async).
//!
//! ## Guarantees
//!
//! - Single connection per `LegacyQueue`, protected by a `tokio::sync::Mutex`.
//!   Two concurrent `claim_one` calls are serialized: the second sees an empty queue.
//! - Expired lease = job is re-claimable. `attempts` is incremented on each claim.
//! - `failed` is terminal (no automatic retry).
//! - `claim_one` uses `UPDATE…RETURNING` (SQLite ≥ 3.35, bundled ≥ 3.47).
//!
//! ## SQLite PRAGMAs applied at open
//!
//! ```sql
//! PRAGMA journal_mode  = WAL;
//! PRAGMA synchronous   = NORMAL;
//! PRAGMA busy_timeout  = 5000;
//! PRAGMA foreign_keys  = ON;
//! ```

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::job::{Job, JobStatus};
use crate::schema::{CREATE_IDX_JOBS_STATUS_LEASE, CREATE_JOBS_TABLE};

/// Errors returned by [`LegacyQueue`] (rusqlite-based).
#[derive(Debug, thiserror::Error)]
pub enum LegacyQueueError {
    /// Underlying SQLite error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// I/O error (file open, permissions).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Failed to decode a ULID stored in the database.
    #[error("ulid parse: {0}")]
    UlidParse(#[from] ulid::DecodeError),
}

/// SQLite-backed queue (rusqlite, sync, ULID-based) — preserved for backward compatibility.
///
/// Use [`LegacyQueue::open`] for a persistent database or
/// [`LegacyQueue::open_in_memory`] for tests.
///
/// Prefer [`crate::SqliteQueue`] (sqlx-based, async) for current code.
pub struct LegacyQueue {
    /// Single connection serialized by a tokio Mutex.
    /// Hold the lock for the shortest time possible (never across an `.await`).
    conn: Mutex<Connection>,
}

impl LegacyQueue {
    /// Opens (or creates) a persistent SQLite database at the given `path`.
    ///
    /// Applies the required SQLite PRAGMAs and creates the schema if absent.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`] if opening or the PRAGMAs fail.
    pub async fn open(path: &Path) -> Result<Self, LegacyQueueError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Opens an in-memory database (useful for tests).
    ///
    /// Note: WAL is not supported on `:memory:`; SQLite falls back to `MEMORY`.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`] if initialization fails.
    pub async fn open_in_memory() -> Result<Self, LegacyQueueError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Initializes the connection: required SQLite PRAGMAs + schema DDL.
    fn init(conn: Connection) -> Result<Self, LegacyQueueError> {
        // PRAGMAs — required, applied in this exact order.
        conn.execute_batch(
            "PRAGMA journal_mode  = WAL;
             PRAGMA synchronous   = NORMAL;
             PRAGMA busy_timeout  = 5000;
             PRAGMA foreign_keys  = ON;",
        )?;

        // DDL — idempotent via IF NOT EXISTS.
        conn.execute_batch(CREATE_JOBS_TABLE)?;
        conn.execute_batch(CREATE_IDX_JOBS_STATUS_LEASE)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Inserts a new `pending` job into the queue.
    ///
    /// Returns the [`Ulid`] of the created job.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`] if the insert fails.
    pub async fn enqueue(&self, kind: &str, payload_json: &str) -> Result<Ulid, LegacyQueueError> {
        let id = Ulid::generate();
        let now_ms = now_ms();

        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "INSERT INTO jobs (id, kind, payload_json, status, lease_until, created_at, updated_at, attempts, last_error)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, 0, NULL)",
        )?
        .execute(params![
            id.to_string(),
            kind,
            payload_json,
            JobStatus::Pending,
            now_ms,
        ])?;

        Ok(id)
    }

    /// Atomically claims the first available job (pending or expired lease).
    ///
    /// Uses `UPDATE…RETURNING` to ensure only one concurrent caller
    /// obtains the job, even without an explicit application-level lock.
    ///
    /// `lease_duration_ms` defines the claim validity duration in milliseconds
    /// (300,000 ms = 5 min recommended).
    ///
    /// Returns `Ok(None)` if no job is available.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`] or [`LegacyQueueError::UlidParse`].
    pub async fn claim_one(&self, lease_duration_ms: i64) -> Result<Option<Job>, LegacyQueueError> {
        let now_ms = now_ms();
        let lease_until = now_ms + lease_duration_ms;

        let conn = self.conn.lock().await;
        // Atomic UPDATE…RETURNING — SQLite ≥ 3.35 (bundled ≥ 3.47).
        // The sub-SELECT identifies the first eligible job:
        //   - pending: never claimed
        //   - claimed: expired lease (re-claimable)
        // ORDER BY created_at guarantees FIFO.
        let mut stmt = conn.prepare_cached(
            "UPDATE jobs
             SET status = 'claimed',
                 lease_until = ?1,
                 updated_at  = ?2,
                 attempts    = attempts + 1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE status = 'pending'
                    OR (status = 'claimed' AND lease_until < ?2)
                 ORDER BY created_at ASC
                 LIMIT 1
             )
             RETURNING id, kind, payload_json, status, lease_until,
                       created_at, updated_at, attempts, last_error",
        )?;

        let job = stmt
            .query_row(params![lease_until, now_ms], row_to_job)
            .optional()?;

        Ok(job)
    }

    /// Marks a job as `done`.
    ///
    /// Clears `lease_until` and `last_error`. Idempotent if called multiple times
    /// on the same job (only `updated_at` changes).
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`].
    pub async fn complete(&self, id: Ulid) -> Result<(), LegacyQueueError> {
        let now_ms = now_ms();
        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "UPDATE jobs
             SET status = 'done', lease_until = NULL, updated_at = ?1
             WHERE id = ?2",
        )?
        .execute(params![now_ms, id.to_string()])?;
        Ok(())
    }

    /// Marks a job as `failed` with an error message.
    ///
    /// `failed` is terminal (no automatic retry). Clears `lease_until`.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`].
    pub async fn fail(&self, id: Ulid, reason: &str) -> Result<(), LegacyQueueError> {
        let now_ms = now_ms();
        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "UPDATE jobs
             SET status = 'failed', lease_until = NULL,
                 updated_at = ?1, last_error = ?2
             WHERE id = ?3",
        )?
        .execute(params![now_ms, reason, id.to_string()])?;
        Ok(())
    }

    /// Reads a SQLite PRAGMA value by name.
    ///
    /// Used primarily in tests to verify the configured SQLite PRAGMAs.
    ///
    /// # Errors
    ///
    /// Returns [`LegacyQueueError::Sqlite`] if the PRAGMA does not exist or
    /// if the return type `T` does not match.
    pub async fn pragma_value<T: rusqlite::types::FromSql>(
        &self,
        name: &str,
    ) -> Result<T, LegacyQueueError> {
        let conn = self.conn.lock().await;
        // Build the PRAGMA query dynamically.
        // Only alphanumeric + underscore names are accepted in practice.
        // No injection risk: name always comes from calling code, never from external input.
        let sql = format!("PRAGMA {name}");
        let value = conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(value)
    }
}

/// Returns the current timestamp in Unix milliseconds.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // The system clock cannot be before UNIX_EPOCH in a standard production environment.
        .expect("system clock earlier than UNIX_EPOCH — invalid environment")
        .as_millis() as i64
}

/// Maps a SQLite row to a [`Job`].
///
/// Column order MUST match exactly the RETURNING SELECT and read queries.
fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let id_str: String = row.get(0)?;
    let id = id_str.parse::<Ulid>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(Job {
        id,
        kind: row.get(1)?,
        payload_json: row.get(2)?,
        status: row.get(3)?,
        lease_until: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        attempts: row.get::<_, i64>(7)? as u32,
        last_error: row.get(8)?,
    })
}
