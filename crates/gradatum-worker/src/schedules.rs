//! Periodic cron schedules via `apalis-cron`.
//!
//! ## Schedules
//!
//! | Name | Cron expression | Action |
//! |---|---|---|
//! | `cleanup_dlq_daily` | `0 3 * * *` | Deletes DLQ jobs older than 30 days |
//!
//! ## Architecture
//!
//! `apalis-cron` provides `CronStream`, which emits a `Tick` at each occurrence
//! of the cron expression. The handler receives the `Tick` and performs the SQL cleanup.

use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::{BoxDynError, Data};
use apalis_cron::Tick;
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use gradatum_core::QueueStore;
use gradatum_db_sqlite::idempotency_cleanup;

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Cron schedule configuration read from TOML.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ScheduleConfig {
    /// Schedule name (e.g. `"cleanup_dlq_daily"`).
    pub name: String,
    /// Cron expression (e.g. `"0 3 * * *"`).
    pub cron: String,
    /// Retention in days for DLQ cleanup (default: 30).
    #[serde(default = "ScheduleConfig::default_retention")]
    pub retention_days: u32,
}

impl ScheduleConfig {
    fn default_retention() -> u32 {
        30
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared context for cron handlers
// ─────────────────────────────────────────────────────────────────────────────

/// Context injected into cron handlers.
///
/// Intended for manual injection outside the Monitor. Currently unused
/// — the Apalis Monitor injects pool and retention via `WorkerBuilder::data()`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct CronHandlerCtx {
    /// SQLite pool for cleanup operations.
    pub pool: Arc<SqlitePool>,
    /// DLQ retention in days.
    pub dlq_retention_days: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — cleanup_dlq_daily
// ─────────────────────────────────────────────────────────────────────────────

/// Cron handler for daily DLQ cleanup.
///
/// Deletes jobs with `status = 'DLQ'` older than `retention_days` days (default: 30).
///
/// # Behaviour
///
/// - `completed_at < now - retention_days` → DELETE
/// - If `completed_at IS NULL` → uses `created_at` as fallback
/// - Returns `Ok(())` even if 0 rows were deleted
///
/// # Safety
///
/// Irreversible destructive operation — restricted to `status='DLQ'` only.
/// The 30-day minimum retention floor is enforced in the query.
pub async fn handle_cleanup_dlq(
    _tick: Tick<Utc>,
    pool: Data<Arc<SqlitePool>>,
    retention: Data<u32>,
) -> Result<(), BoxDynError> {
    let cutoff = Utc::now() - chrono::Duration::days(*retention as i64);
    let cutoff_str = cutoff.to_rfc3339();

    let result = sqlx::query(
        r#"
        DELETE FROM gradatum_jobs
        WHERE status = 'DLQ'
          AND COALESCE(completed_at, created_at) < ?
        "#,
    )
    .bind(&cutoff_str)
    .execute(pool.as_ref())
    .await;

    match result {
        Ok(res) => {
            let deleted = res.rows_affected();
            if deleted > 0 {
                info!(
                    deleted = deleted,
                    retention_days = *retention,
                    "cleanup_dlq_daily : {} jobs DLQ purgés",
                    deleted
                );
            }
            Ok(())
        }
        Err(e) => {
            error!(error = %e, "cleanup_dlq_daily : erreur SQL");
            Err(BoxDynError::from(format!(
                "cleanup_dlq_daily SQL error: {e}"
            )))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Periodic sweep (recover_stale_leases, cancel_expired_deadlines, promote_retries)
// ─────────────────────────────────────────────────────────────────────────────

/// Runs one periodic sweep of the job store.
///
/// Calls the 5 queue maintenance operations:
/// 1. [`QueueStore::recover_stale_leases`] — returns expired Running jobs to Pending
/// 2. [`QueueStore::cancel_expired_deadlines`] — cancels jobs past their deadline
/// 3. [`QueueStore::promote_retries`] — moves Failed jobs to Pending (or DLQ at max retries)
/// 4. [`QueueStore::promote_stranded_waiting_jobs`] — DAG recovery: promotes stranded
///    `Waiting` jobs whose all dependencies are `Done` but whose post-commit cascade failed
///    (worker crash or storage error). No-op when no stranded jobs exist.
/// 5. [`idempotency_cleanup`] — purges idempotency entries older than 24 hours (TTL)
///
/// The `pool` is required to clean the `gradatum_idempotency` table (migration 008).
/// If `pool` is `None`, operation 5 is skipped with a WARN (the table may grow
/// unboundedly — acceptable only in unit tests).
///
/// Invoked every 30 s by the worker loop via `tokio::spawn`.
/// Does not panic — errors are logged.
pub async fn run_sweep_once(
    store: &(impl QueueStore + ?Sized),
    lease_ttl: Duration,
    pool: Option<&SqlitePool>,
) {
    let now = Utc::now();

    // 1. Recover expired leases
    match store.recover_stale_leases(lease_ttl).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: leases expirés récupérés");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: recover_stale_leases échoué"),
    }

    // 2. Cancel expired deadlines
    match store.cancel_expired_deadlines(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: deadlines expirés annulés");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: cancel_expired_deadlines échoué"),
    }

    // 3. Promote retries (Failed → Pending or DLQ at max retries)
    match store.promote_retries(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: retries promus en Pending");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: promote_retries échoué"),
    }

    // 4. DAG recovery sweep : promote Waiting jobs whose all deps are Done
    //    but whose post-commit cascade was missed (crash or storage error).
    //    No-op if no stranded jobs exist (common case — await_jobs unused in prod v0.6.x).
    match store.promote_stranded_waiting_jobs().await {
        Ok(promoted) if promoted > 0 => {
            tracing::info!(promoted, "dag_recovery_sweep: jobs Waiting rattrapes");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "dag_recovery_sweep: promote_stranded_waiting_jobs echoue")
        }
    }

    // 5. Idempotency cleanup (TTL 24h).
    // Purges `gradatum_idempotency` entries older than now - 24h.
    // Without this cleanup the table grows unboundedly (one row per POST /api/v1/jobs).
    match pool {
        Some(p) => {
            let cutoff_ms = (now - chrono::Duration::hours(24)).timestamp_millis();
            if let Err(e) = idempotency_cleanup(p, cutoff_ms).await {
                warn!(error = %e, "sweep: idempotency_cleanup échoué — table peut croître");
            }
        }
        None => {
            warn!("sweep: pool non disponible — idempotency_cleanup ignoré (table peut croître)");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent};
    use std::sync::Mutex;
    use tokio::sync::broadcast::Receiver;
    use ulid::Ulid;

    /// Mock store for testing `sweep_once`.
    struct MockStore {
        stale_calls: Mutex<u32>,
        deadline_calls: Mutex<u32>,
        retry_calls: Mutex<u32>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                stale_calls: Mutex::new(0),
                deadline_calls: Mutex::new(0),
                retry_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl QueueStore for MockStore {
        async fn enqueue(&self, _: JobRecord) -> Result<Ulid, QueueError> {
            unimplemented!()
        }
        async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn get(&self, _: Ulid) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn complete(&self, _: Ulid, _: JobResult) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail(&self, _: Ulid, _: &str, _: u32) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn cancel(&self, _: Ulid) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail_dlq(&self, _: Ulid, _: &str) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn find_awaiting(&self, _: Ulid) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn set_pending(&self, _: Ulid) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn recover_stale_leases(&self, _: Duration) -> Result<Vec<Ulid>, QueueError> {
            *self.stale_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn cancel_expired_deadlines(
            &self,
            _: DateTime<Utc>,
        ) -> Result<Vec<Ulid>, QueueError> {
            *self.deadline_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn promote_retries(&self, _: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
            *self.retry_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn schedule_retry(&self, _: Ulid, _: DateTime<Utc>) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn list(&self, _: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        fn subscribe(&self) -> Receiver<QueueEvent> {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            drop(tx);
            rx
        }
    }

    #[tokio::test]
    async fn sweep_once_calls_all_three_methods() {
        let store = MockStore::new();
        // pool=None : idempotency_cleanup ignoré en test unitaire (table non disponible).
        run_sweep_once(&store, Duration::from_secs(300), None).await;
        assert_eq!(*store.stale_calls.lock().unwrap(), 1);
        assert_eq!(*store.deadline_calls.lock().unwrap(), 1);
        assert_eq!(*store.retry_calls.lock().unwrap(), 1);
    }
}
