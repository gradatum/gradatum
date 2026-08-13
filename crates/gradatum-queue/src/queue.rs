//! `Queue` async trait and `SqliteQueue` implementation with `UPDATE...RETURNING` atomic lease.
//!
//! ## Guarantees
//!
//! - **Atomic claim**: atomic SQLite `UPDATE...RETURNING` ensures only one
//!   worker obtains a job even under multi-process contention (WAL mode).
//! - **Lease recovery**: a job whose lease has expired (`lease_until < now`) is
//!   re-claimable automatically; `attempts` is incremented on each re-claim.
//! - **Dead-letter**: when `attempts >= max_attempts`, the job transitions to `dead`.
//! - **WAL mode**: sqlx pool of 8 connections, WAL journal, synchronous NORMAL.
//! - **Millisecond timestamps**: sub-second precision for tests and production.
//!
//! ## Differences from [`crate::LegacyQueue`] (rusqlite)
//!
//! - ID: `i64` AUTOINCREMENT (vs ULID TEXT)
//! - Table: `jobs_v2` (coexists with the legacy `jobs` table)
//! - Payload: opaque `BLOB` (vs TEXT JSON)
//! - API: async trait with multi-kind filter (vs synchronous struct with single kind)
//! - Timestamps: milliseconds (sub-second precision, compatible with `Duration::from_millis`)

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::schema::SCHEMA_V1;

/// Errors returned by sqlx-based queue operations.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Underlying sqlx error (SQLite, pool, query).
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Invalid system clock (before `UNIX_EPOCH`).
    #[error("system time: {0}")]
    Time(#[from] std::time::SystemTimeError),
    /// Unknown or corrupted job status string found in the database.
    ///
    /// Masking this as `Pending` would silently re-queue jobs that may be in
    /// an unknown terminal state, risking duplicate processing or infinite retries.
    /// Surfacing a hard error lets the caller detect and alert on data corruption.
    #[error("corrupted job status in database: {0:?}")]
    CorruptedStatus(String),
    /// Job is not in `leased` state or the lease has expired — the caller
    /// attempted to `complete()` or `fail()` a job it does not hold a valid
    /// lease on. This is a correctness guard against stale-lease processing
    /// (multi-tenant isolation, P0 #5).
    #[error("job {0} is not leased or lease expired — cannot complete/fail")]
    NotLeased(JobId),
}

/// Opaque job identifier (AUTOINCREMENT `i64`, stable in the database).
pub type JobId = i64;

/// Data required to enqueue a new job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewJob {
    /// Isolated tenant. Carries the **resolved** tenant, not a constant: the sole
    /// production construction site (`gradatum-admin::backfill_embeddings`) propagates
    /// the tenant resolved upstream. Only tests hardcode `"main"`.
    pub tenant_id: String,
    /// Job type, e.g. `"curate"`, `"embed"`.
    pub kind: String,
    /// Opaque payload bytes, encoded by the caller. The sole live caller
    /// (`gradatum-admin::backfill_embeddings`) serialises via `serde_json`.
    pub payload: Vec<u8>,
    /// Maximum number of attempts before transitioning to `dead`.
    pub max_attempts: i32,
}

/// Job with an active lease returned by [`Queue::lease`].
#[derive(Debug, Clone)]
pub struct LeasedJob {
    /// Job identifier in `jobs_v2`.
    pub id: JobId,
    /// Job tenant.
    pub tenant_id: String,
    /// Job type.
    pub kind: String,
    /// Opaque binary payload.
    pub payload: Vec<u8>,
    /// Number of attempts (incremented on each lease, including re-lease).
    pub attempts: i32,
}

/// Read-only view of a job without claiming it (returned by [`Queue::get`]).
///
/// Distinct from [`LeasedJob`]: no `payload` (metadata read only),
/// no database mutation. Used by `GET /api/v1/jobs/:id`.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub status: JobStatus,
    pub attempts: i32,
    pub last_error: Option<String>,
}

/// State of a job in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// Waiting to be processed.
    Pending,
    /// Active lease — a worker is currently processing this job.
    Leased,
    /// Processing completed successfully.
    Done,
    /// `attempts >= max_attempts` — dead-letter, no automatic retry.
    Dead,
}

impl JobStatus {
    /// Returns the SQLite string representation of the status (`"pending"`, `"leased"`, etc.).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::Done => "done",
            Self::Dead => "dead",
        }
    }

    /// Reconstructs a `JobStatus` from a SQLite string. Returns `None` if unknown.
    /// Custom `Option<Self>` signature — not a public parse, no `FromStr` trait required.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "leased" => Some(Self::Leased),
            "done" => Some(Self::Done),
            "dead" => Some(Self::Dead),
            _ => None,
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn job_status_as_str_round_trip() {
        assert_eq!(JobStatus::Pending.as_str(), "pending");
        assert_eq!(JobStatus::Leased.as_str(), "leased");
        assert_eq!(JobStatus::Done.as_str(), "done");
        assert_eq!(JobStatus::Dead.as_str(), "dead");
    }

    #[test]
    fn job_status_from_str_round_trip() {
        assert_eq!(JobStatus::from_str("pending"), Some(JobStatus::Pending));
        assert_eq!(JobStatus::from_str("leased"), Some(JobStatus::Leased));
        assert_eq!(JobStatus::from_str("done"), Some(JobStatus::Done));
        assert_eq!(JobStatus::from_str("dead"), Some(JobStatus::Dead));
        assert_eq!(JobStatus::from_str("unknown"), None);
    }
}

#[cfg(test)]
mod get_tests {
    use super::*;
    use std::time::Duration;

    fn make_job(kind: &str) -> NewJob {
        NewJob {
            tenant_id: "main".to_string(),
            kind: kind.to_string(),
            payload: vec![1, 2, 3],
            max_attempts: 3,
        }
    }

    #[tokio::test]
    async fn get_existing_returns_some() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");
        let info = q.get(id).await.expect("get").expect("Some");
        assert_eq!(info.id, id);
        assert_eq!(info.status, JobStatus::Pending);
        assert_eq!(info.attempts, 0);
        assert_eq!(info.last_error, None);
    }

    #[tokio::test]
    async fn get_unknown_returns_none() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let info = q.get(99999).await.expect("get");
        assert!(info.is_none());
    }

    #[tokio::test]
    async fn get_after_lease_reflects_leased() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");
        let _leased = q
            .lease(&["curate"], Duration::from_secs(60))
            .await
            .expect("lease")
            .expect("Some");
        let info = q.get(id).await.expect("get").expect("Some");
        assert_eq!(info.status, JobStatus::Leased);
        assert_eq!(info.attempts, 1);
    }

    #[tokio::test]
    async fn get_after_complete_reflects_done() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");
        let _ = q
            .lease(&["curate"], Duration::from_secs(60))
            .await
            .expect("lease")
            .expect("Some");
        q.complete(id).await.expect("complete");
        let info = q.get(id).await.expect("get").expect("Some");
        assert_eq!(info.status, JobStatus::Done);
    }

    /// Injection d'un statut corrompu directement en DB → `get` retourne `QueueError::CorruptedStatus`.
    ///
    /// Vérifie que la corruption DB est détectée et remontée explicitement
    /// plutôt que silencieusement masquée comme `Pending`.
    #[tokio::test]
    async fn get_corrupted_status_returns_error() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");

        // Injection directe d'un statut inconnu pour simuler une corruption DB.
        sqlx::query("UPDATE jobs_v2 SET status = 'zombie' WHERE id = ?")
            .bind(id)
            .execute(&q.pool)
            .await
            .expect("injection statut corrompu");

        let result = q.get(id).await;
        assert!(
            result.is_err(),
            "get sur statut corrompu doit retourner Err, obtenu Ok"
        );
        match result.unwrap_err() {
            QueueError::CorruptedStatus(s) => {
                assert_eq!(
                    s, "zombie",
                    "la chaîne corrompue doit être préservée dans l'erreur"
                );
            }
            other => panic!("attendu CorruptedStatus, obtenu {:?}", other),
        }
    }
}

#[cfg(test)]
mod stale_lease_tests {
    use super::*;
    use std::time::Duration;

    fn make_job(kind: &str) -> NewJob {
        NewJob {
            tenant_id: "main".to_string(),
            kind: kind.to_string(),
            payload: vec![1, 2, 3],
            max_attempts: 3,
        }
    }

    /// `complete()` sur un job non-leased (encore `pending`) → `NotLeased` (P0 #5).
    #[tokio::test]
    async fn complete_rejects_non_leased_job() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");

        let result = q.complete(id).await;
        assert!(
            result.is_err(),
            "complete sur un job pending doit échouer, obtenu Ok"
        );
        match result.unwrap_err() {
            QueueError::NotLeased(jid) => assert_eq!(jid, id),
            other => panic!("attendu NotLeased, obtenu {:?}", other),
        }
    }

    /// `fail()` sur un job non-leased → `NotLeased` (P0 #5).
    #[tokio::test]
    async fn fail_rejects_non_leased_job() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");

        let result = q.fail(id, "erreur test").await;
        assert!(result.is_err(), "fail sur un job pending doit échouer");
        match result.unwrap_err() {
            QueueError::NotLeased(jid) => assert_eq!(jid, id),
            other => panic!("attendu NotLeased, obtenu {:?}", other),
        }
    }

    /// `complete()` après un lease + `complete` déjà fait → `NotLeased` (idempotence guarded).
    #[tokio::test]
    async fn complete_rejects_already_done_job() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");
        let _leased = q
            .lease(&["curate"], Duration::from_secs(60))
            .await
            .expect("lease")
            .expect("Some");
        q.complete(id).await.expect("premier complete OK");

        // Deuxième complete → le job n'est plus `leased` → NotLeased.
        let result = q.complete(id).await;
        assert!(result.is_err(), "deuxième complete doit échouer");
        match result.unwrap_err() {
            QueueError::NotLeased(_) => {}
            other => panic!("attendu NotLeased, obtenu {:?}", other),
        }
    }

    /// `complete()` avec un lease expiré (0 ms) → `NotLeased`.
    #[tokio::test]
    async fn complete_rejects_expired_lease() {
        let q = SqliteQueue::in_memory().await.expect("queue");
        let id = q.enqueue(make_job("curate")).await.expect("enqueue");
        // Lease de 1 ms pour que le lease expire quasi-instantanément.
        let _leased = q
            .lease(&["curate"], Duration::from_millis(1))
            .await
            .expect("lease")
            .expect("Some");

        // Attendre que le lease expire.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = q.complete(id).await;
        assert!(result.is_err(), "complete sur lease expiré doit échouer");
        match result.unwrap_err() {
            QueueError::NotLeased(_) => {}
            other => panic!("attendu NotLeased, obtenu {:?}", other),
        }
    }
}

/// Async trait for the gradatum job queue.
///
/// All methods are async and `Send + Sync + 'static` for use
/// in multi-threaded Axum/tokio handlers.
#[async_trait]
pub trait Queue: Send + Sync + 'static {
    /// Reads the state of a job without claiming it. Returns `None` if the id does not exist.
    ///
    /// Used by `GET /api/v1/jobs/:id` (status polling).
    async fn get(&self, id: JobId) -> Result<Option<JobInfo>, QueueError>;

    /// Inserts a new `pending` job and returns its `JobId`.
    async fn enqueue(&self, job: NewJob) -> Result<JobId, QueueError>;

    /// Atomically claims the first available job (pending or expired lease).
    ///
    /// Filters on `kinds` (OR); returns `None` if the queue is empty.
    /// `duration` defines the lease validity period (millisecond precision).
    async fn lease(
        &self,
        kinds: &[&str],
        duration: Duration,
    ) -> Result<Option<LeasedJob>, QueueError>;

    /// Marks a job as `done` (terminal — cannot be re-claimed).
    async fn complete(&self, id: JobId) -> Result<(), QueueError>;

    /// Marks a job as failed with an error message.
    ///
    /// If `attempts < max_attempts`: resets to `pending` for retry.
    /// If `attempts >= max_attempts`: transitions to `dead`.
    async fn fail(&self, id: JobId, err: &str) -> Result<(), QueueError>;

    /// Extends the lease of an active job.
    async fn extend_lease(&self, id: JobId, dur: Duration) -> Result<(), QueueError>;

    /// Returns the number of jobs in `pending` state.
    async fn depth(&self) -> Result<u64, QueueError>;

    /// Returns the age of the oldest `pending` job in seconds (0 if the queue is empty).
    async fn oldest_age_secs(&self) -> Result<u64, QueueError>;
}

/// sqlx-based implementation of [`Queue`] backed by SQLite WAL.
///
/// Pool of up to 8 connections; `UPDATE...RETURNING` for atomic claim.
/// Target table: `jobs_v2` (coexists with the legacy rusqlite `jobs` table).
/// Unix millisecond timestamps for sub-second precision.
pub struct SqliteQueue {
    pool: SqlitePool,
}

impl SqliteQueue {
    /// Opens (or creates) a SQLite database at the given `db_path`.
    ///
    /// Creates the `jobs_v2` + `worker_leadership` schema if absent (idempotent).
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::Sqlx`] if opening, the pool, or the migration fails.
    pub async fn new(db_path: &Path) -> Result<Self, QueueError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // 5s busy_timeout: without this setting SQLite returns SQLITE_BUSY
            // immediately when another writer holds the WAL lock. With busy_timeout,
            // SQLite retries for up to 5s before failing — prevents the instant-ack
            // error that left jobs stuck in Running.
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        // Execute the full DDL (idempotent via IF NOT EXISTS).
        // SCHEMA_V1 contains multiple statements separated by `;`.
        sqlx::query(SCHEMA_V1).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Opens an in-memory SQLite database (`:memory:`).
    ///
    /// Single connection (`max_connections=1`) — the database is destroyed when the pool closes.
    /// WAL is disabled on `:memory:` (no effect on behavior).
    ///
    /// # Usage
    ///
    /// ```rust,no_run
    /// use gradatum_queue::SqliteQueue;
    /// // In an async tokio context:
    /// // let queue = SqliteQueue::in_memory().await.expect("in-memory queue");
    /// ```
    pub async fn in_memory() -> Result<Self, QueueError> {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Memory);
        // max_connections=1: single connection required for sqlx :memory: databases.
        // Multiple connections open separate (non-shared) in-memory databases.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::query(SCHEMA_V1).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Returns the current timestamp in Unix milliseconds.
    ///
    /// Uses milliseconds (not seconds) to guarantee the sub-second precision
    /// required for short-lease tests (< 1 second).
    fn now_ms() -> Result<i64, QueueError> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64)
    }
}

#[async_trait]
impl Queue for SqliteQueue {
    async fn get(&self, id: JobId) -> Result<Option<JobInfo>, QueueError> {
        let row = sqlx::query("SELECT id, status, attempts, last_error FROM jobs_v2 WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        row.map(|r| {
            let status_str: String = r.get("status");
            let status = JobStatus::from_str(&status_str)
                .ok_or_else(|| QueueError::CorruptedStatus(status_str))?;
            Ok(JobInfo {
                id: r.get("id"),
                status,
                attempts: r.get("attempts"),
                last_error: r.get("last_error"),
            })
        })
        .transpose()
    }

    async fn enqueue(&self, job: NewJob) -> Result<JobId, QueueError> {
        let now = Self::now_ms()?;
        let row = sqlx::query(
            "INSERT INTO jobs_v2 (tenant_id, kind, payload, max_attempts, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(&job.tenant_id)
        .bind(&job.kind)
        .bind(&job.payload)
        .bind(job.max_attempts)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("id"))
    }

    async fn lease(
        &self,
        kinds: &[&str],
        duration: Duration,
    ) -> Result<Option<LeasedJob>, QueueError> {
        let now = Self::now_ms()?;
        // Convert the duration to milliseconds for sub-second precision.
        let lease_until = now + duration.as_millis() as i64;
        let leased_by = ulid::Ulid::generate().to_string();

        // Build placeholders for the IN filter dynamically.
        // `kinds` comes from the calling code (never from external input),
        // so there is no injection risk; values are bound separately.
        let placeholders = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        let q = format!(
            "UPDATE jobs_v2
             SET status     = 'leased',
                 lease_until = ?,
                 leased_by   = ?,
                 attempts    = attempts + 1,
                 updated_at  = ?
             WHERE id = (
                 SELECT id FROM jobs_v2
                 WHERE (status = 'pending'
                    OR  (status = 'leased' AND lease_until < ?))
                   AND kind IN ({placeholders})
                 ORDER BY created_at ASC
                 LIMIT 1
             )
             RETURNING id, tenant_id, kind, payload, attempts"
        );

        // Bind in order: lease_until, leased_by, now (UPDATE SET), now (WHERE expiry),
        // then the kinds for the IN filter.
        let mut query = sqlx::query(&q)
            .bind(lease_until)
            .bind(&leased_by)
            .bind(now)
            .bind(now);
        for k in kinds {
            query = query.bind(k);
        }

        let row = query.fetch_optional(&self.pool).await?;
        Ok(row.map(|r| LeasedJob {
            id: r.get("id"),
            tenant_id: r.get("tenant_id"),
            kind: r.get("kind"),
            payload: r.get("payload"),
            attempts: r.get("attempts"),
        }))
    }

    async fn complete(&self, id: JobId) -> Result<(), QueueError> {
        let now = Self::now_ms()?;
        // Garde stale-lease (P0 #5) : refuse complete si le job n'est pas leased
        // ou si le lease a expiré. Sans cette garde, un worker peut marquer `done`
        // un job dont le lease a expiré et qu'un autre worker a déjà repris.
        let result = sqlx::query(
            "UPDATE jobs_v2
             SET status = 'done', lease_until = NULL, leased_by = NULL, updated_at = ?
             WHERE id = ?
               AND status = 'leased'
               AND lease_until > ?",
        )
        .bind(now)
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(QueueError::NotLeased(id));
        }
        Ok(())
    }

    async fn fail(&self, id: JobId, err: &str) -> Result<(), QueueError> {
        let now = Self::now_ms()?;
        // If attempts >= max_attempts -> dead; otherwise back to pending for retry.
        // Garde stale-lease (P0 #5) : refuse fail si le job n'est pas leased
        // ou si le lease a expiré.
        let result = sqlx::query(
            "UPDATE jobs_v2
             SET status     = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'pending' END,
                 last_error = ?,
                 updated_at  = ?,
                 lease_until = NULL,
                 leased_by   = NULL
             WHERE id = ?
               AND status = 'leased'
               AND lease_until > ?",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(QueueError::NotLeased(id));
        }
        Ok(())
    }

    async fn extend_lease(&self, id: JobId, dur: Duration) -> Result<(), QueueError> {
        let now = Self::now_ms()?;
        let new_until = now + dur.as_millis() as i64;
        sqlx::query(
            "UPDATE jobs_v2
             SET lease_until = ?, updated_at = ?
             WHERE id = ? AND status = 'leased'",
        )
        .bind(new_until)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn depth(&self) -> Result<u64, QueueError> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM jobs_v2 WHERE status = 'pending'")
                .fetch_one(&self.pool)
                .await?;
        Ok(count as u64)
    }

    async fn oldest_age_secs(&self) -> Result<u64, QueueError> {
        let now = Self::now_ms()?;
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT created_at FROM jobs_v2 WHERE status = 'pending'
             ORDER BY created_at ASC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        // created_at is in ms; convert the difference to seconds.
        Ok(row
            .map(|(c,)| ((now - c).max(0) as u64) / 1000)
            .unwrap_or(0))
    }
}
