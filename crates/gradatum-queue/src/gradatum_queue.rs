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
//! ## Full delegation
//!
//! Every method, `find_awaiting` and `set_pending` included, delegates to
//! [`SqliteQueueStore`] — there is no `NotImplemented` stub here.

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
    /// **No caller in the workspace.** `gradatum-worker` does not use it, and no
    /// `WorkerContext` type exists anywhere — the accessor is kept for external consumers
    /// that need the concrete store behind the queue.
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

    async fn dequeue(&self, tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        self.inner.dequeue(tenant_filter).await
    }

    /// Override the default trait impl — forwards to `SqliteQueueStore::dequeue_by_kind`
    /// with native SQL `kind` filtering.
    ///
    /// The default trait impl (`dequeue(tenant_filter)`) ignores `kind`, which would
    /// cause all workers to fetch any job regardless of kind — breaking the DLQ
    /// routing fix. This override preserves the native `kind` filter in the SQL
    /// `WHERE` clause, ensuring each worker fetches only jobs matching its handler.
    async fn dequeue_by_kind(
        &self,
        kind: &str,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        self.inner.dequeue_by_kind(kind, tenant_filter).await
    }

    async fn get(
        &self,
        id: Ulid,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        self.inner.get(id, tenant_filter).await
    }

    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError> {
        self.inner.complete(id, result).await
    }

    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError> {
        self.inner.fail(id, err, attempt).await
    }

    async fn cancel(&self, id: Ulid, tenant_filter: Option<&str>) -> Result<(), QueueError> {
        self.inner.cancel(id, tenant_filter).await
    }

    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError> {
        self.inner.fail_dlq(id, err).await
    }

    async fn delete_dlq_jobs(&self, older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        self.inner.delete_dlq_jobs(older_than).await
    }

    async fn find_awaiting(&self, job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        self.inner.find_awaiting(job_id).await
    }

    async fn set_pending(&self, id: Ulid) -> Result<(), QueueError> {
        self.inner.set_pending(id).await
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
    use gradatum_core::*;
    use gradatum_db_sqlite::{QueueDb, SqliteQueueStore, apply_sqlite_pragmas, run_migrations};
    use ulid::Ulid;

    /// Crée un `GradatumQueue` sur une base SQLite in-memory avec migrations appliquées.
    async fn make_queue() -> GradatumQueue {
        let db = QueueDb::open_in_memory()
            .await
            .expect("db in-memory doit créer");
        apply_sqlite_pragmas(&db)
            .await
            .expect("pragmas WAL doivent s'appliquer");
        run_migrations(&db)
            .await
            .expect("migrations doivent s'appliquer");
        GradatumQueue::new(SqliteQueueStore::new(db))
    }

    /// `find_awaiting` délègue à `SqliteQueueStore` : retourne `[]` si aucun job
    /// Waiting ne référence le ULID fourni (pas de NotImplemented).
    #[tokio::test]
    async fn find_awaiting_delegates_and_returns_empty_when_no_dependents() {
        let queue = make_queue().await;
        let result = queue
            .find_awaiting(Ulid::generate())
            .await
            .expect("find_awaiting doit déléguer sans erreur");
        assert!(
            result.is_empty(),
            "attendu vec vide pour ULID sans dépendants, obtenu : {result:?}"
        );
    }

    /// `set_pending` délègue à `SqliteQueueStore` : retourne `Ok(())` (no-op
    /// idempotent) si le job n'existe pas — confirme la délégation (pas de NotImplemented).
    #[tokio::test]
    async fn set_pending_delegates_and_returns_ok_for_unknown_id() {
        let queue = make_queue().await;
        let result = queue.set_pending(Ulid::generate()).await;
        assert!(
            result.is_ok(),
            "attendu Ok(()) (no-op idempotent) pour ULID inconnu, obtenu : {result:?}"
        );
    }

    /// `dequeue_by_kind` délègue à `SqliteQueueStore` avec le filtre `kind` natif SQL.
    ///
    /// Sans cet override, le défaut du trait ignore `kind` et appelle
    /// `dequeue(tenant_filter)` — un worker embed pourrait recevoir un job Curate,
    /// ce qui casse le fix routing DLQ (P1-5).
    #[tokio::test]
    async fn dequeue_by_kind_forwards_kind_filter() {
        use gradatum_core::*;
        let queue = make_queue().await;

        // Enqueue 1 Curate + 1 Embed.
        let curate = make_record_for_test(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let embed = make_record_for_test(
            Job::Embed(EmbedSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let curate_id = curate.id;
        let embed_id = embed.id;
        queue.enqueue(curate).await.expect("enqueue Curate");
        queue.enqueue(embed).await.expect("enqueue Embed");

        // dequeue_by_kind("Curate") ne retourne QUE le Curate.
        let got = queue
            .dequeue_by_kind("Curate", None)
            .await
            .expect("dequeue_by_kind Curate")
            .expect("doit retourner un job");
        assert_eq!(
            got.id, curate_id,
            "dequeue_by_kind(Curate) doit retourner le job Curate"
        );

        // dequeue_by_kind("Embed") retourne l'Embed restant.
        let got_embed = queue
            .dequeue_by_kind("Embed", None)
            .await
            .expect("dequeue_by_kind Embed")
            .expect("doit retourner un job");
        assert_eq!(
            got_embed.id, embed_id,
            "dequeue_by_kind(Embed) doit retourner le job Embed"
        );
    }

    fn make_record_for_test(job: Job, class: JobClass, status: JobStatus) -> JobRecord {
        let now = chrono::Utc::now();
        JobRecord {
            id: Ulid::generate(),
            spec: JobSpec {
                kind: job,
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::default_for(&class),
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status,
                created_at: now,
                started_at: None,
                completed_at: None,
                lease_until: None,
                result: None,
            },
            retry: JobRetry::default(),
            lineage: JobLineage {
                triggered_by: None,
                parent_job: None,
                pipeline_id: None,
                pipeline_step: None,
                children: vec![],
                cost_usd: None,
            },
        }
    }
}
