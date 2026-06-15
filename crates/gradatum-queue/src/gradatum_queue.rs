//! `GradatumQueue` — [`QueueStore`] facade wrapping [`SqliteQueueStore`].
//!
//! Delegates all operations to [`SqliteQueueStore`] (which manages the broadcast).
//!
//! ## Architecture
//!
//! `GradatumQueue` provides a single entry point for the job queue:
//! - `SqliteQueueStore` → polling, atomic leases, `QueueEvent` broadcast
//! - `GradatumQueue`   → `find_awaiting` DAG, `JobClass` exclusions (deferred)
//!
//! ## Unimplemented methods
//!
//! - `find_awaiting` → DAG await_jobs (deferred)
//! - `set_pending`   → cascade engine (deferred)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent, QueueStore};
use gradatum_db_sqlite::SqliteQueueStore;

/// [`QueueStore`] facade for the gradatum job queue.
///
/// Delegates all operations to [`SqliteQueueStore`].
pub struct GradatumQueue {
    /// Delegated SQLite implementation.
    inner: Arc<SqliteQueueStore>,
}

impl GradatumQueue {
    /// Creates a `GradatumQueue` from a [`SqliteQueueStore`].
    #[must_use]
    pub fn new(store: SqliteQueueStore) -> Self {
        Self {
            inner: Arc::new(store),
        }
    }

    /// Returns a reference to the underlying [`SqliteQueueStore`].
    ///
    /// Used by `gradatum-worker` for injection into `WorkerContext`.
    #[must_use]
    pub fn store(&self) -> Arc<SqliteQueueStore> {
        Arc::clone(&self.inner)
    }
}

#[async_trait]
impl QueueStore for GradatumQueue {
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError> {
        self.inner.enqueue(job).await
    }

    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
        self.inner.dequeue().await
    }

    async fn get(&self, id: Ulid) -> Result<Option<JobRecord>, QueueError> {
        self.inner.get(id).await
    }

    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError> {
        self.inner.complete(id, result).await
    }

    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError> {
        self.inner.fail(id, err, attempt).await
    }

    async fn cancel(&self, id: Ulid) -> Result<(), QueueError> {
        self.inner.cancel(id).await
    }

    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError> {
        self.inner.fail_dlq(id, err).await
    }

    async fn delete_dlq_jobs(&self, older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        self.inner.delete_dlq_jobs(older_than).await
    }

    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        // Deferred (DAG await_jobs + cascade engine) — not implemented.
        // Requires: gradatum_job_deps table + cascade engine.
        Err(QueueError::NotImplemented {
            method: "find_awaiting",
        })
    }

    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        // Deferred (cascade Waiting → Pending) — not implemented.
        // Depends on find_awaiting (cascade engine).
        Err(QueueError::NotImplemented {
            method: "set_pending",
        })
    }

    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        self.inner.recover_stale_leases(ttl).await
    }

    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        self.inner.cancel_expired_deadlines(now).await
    }

    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        self.inner.promote_retries(now).await
    }

    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError> {
        self.inner.schedule_retry(id, at).await
    }

    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        self.inner.list(filter).await
    }

    fn subscribe(&self) -> Receiver<QueueEvent> {
        self.inner.subscribe()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests unitaires
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use gradatum_db_sqlite::{apply_sqlite_pragmas, run_migrations, SqliteQueueStore};
    use sqlx::SqlitePool;
    use ulid::Ulid;

    /// Crée un `GradatumQueue` sur un pool SQLite in-memory avec migrations appliquées.
    async fn make_queue() -> GradatumQueue {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool in-memory doit créer");
        apply_sqlite_pragmas(&pool)
            .await
            .expect("pragmas WAL doivent s'appliquer");
        run_migrations(&pool)
            .await
            .expect("migrations doivent s'appliquer");
        GradatumQueue::new(SqliteQueueStore::new(pool))
    }

    /// `find_awaiting` retourne `Err(NotImplemented)` sans paniquer (A2).
    #[tokio::test]
    async fn find_awaiting_returns_not_implemented() {
        let queue = make_queue().await;
        let result = queue.find_awaiting(Ulid::new()).await;
        assert!(
            matches!(
                result,
                Err(QueueError::NotImplemented {
                    method: "find_awaiting"
                })
            ),
            "attendu NotImplemented{{find_awaiting}}, obtenu : {result:?}"
        );
    }

    /// `set_pending` retourne `Err(NotImplemented)` sans paniquer (A2).
    #[tokio::test]
    async fn set_pending_returns_not_implemented() {
        let queue = make_queue().await;
        let result = queue.set_pending(Ulid::new()).await;
        assert!(
            matches!(
                result,
                Err(QueueError::NotImplemented {
                    method: "set_pending"
                })
            ),
            "attendu NotImplemented{{set_pending}}, obtenu : {result:?}"
        );
    }
}
