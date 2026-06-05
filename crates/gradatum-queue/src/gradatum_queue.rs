//! `GradatumQueue` — façade Apalis wrappant `SqliteQueueStore` (Phase 1.1).
//!
//! Implémente [`QueueStore`] en déléguant à [`SqliteQueueStore`].
//!
//! # Architecture v81 §6 L1784-1799
//!
//! `GradatumQueue` wrap Apalis + pool sqlx pour les requêtes custom Gradatum.
//! Séparation des responsabilités :
//! - `apalis_sqlite` → polling, leases atomiques, retry Apalis natif
//! - `GradatumQueue` → `find_awaiting` DAG, `QueueEvent` broadcast, `JobClass` exclusions
//! - `pool sqlx`     → requêtes SQL custom sur les tables Apalis (lecture seule ou custom)
//!
//! # Phase 1.1 (v0.2.0)
//!
//! Délègue toutes les opérations à [`SqliteQueueStore`] (qui gère le broadcast).
//! Le stub Apalis `Backend` sera câblé en Phase 1.2 via `WorkerBuilder`.
//!
//! # Méthodes avec `todo!()`
//!
//! - `find_awaiting` → F-14 complet v0.3.0 (DAG await_jobs)
//! - `set_pending`   → F-14 complet v0.3.0 (cascade engine)

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast::Receiver;
use ulid::Ulid;

use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent, QueueStore};
use gradatum_db_sqlite::SqliteQueueStore;

/// Façade Apalis pour la queue de jobs gradatum (v0.2.0+ — ARCH-D15).
///
/// En Phase 1.1, délègue toutes les opérations à [`SqliteQueueStore`].
/// En Phase 1.2, le champ `storage` Apalis `SqliteStorage<GradatumJob>` sera
/// activé pour le polling automatique via `WorkerBuilder`.
///
/// # v81 §6 L1795-1799
///
/// ```text
/// pub struct GradatumQueue {
///     pub storage: apalis_sqlite::SqliteStorage<GradatumJob>,
///     pub pool:    sqlx::SqlitePool,
///     pub tx:      broadcast::Sender<QueueEvent>,
/// }
/// ```
///
/// Phase 1.1 : `storage` Apalis = stub (non activé). `inner` = `SqliteQueueStore`.
pub struct GradatumQueue {
    /// Implémentation SQLite déléguée (Phase 1.1).
    ///
    /// En Phase 1.2, ce champ sera complété par `apalis_sqlite::SqliteStorage<GradatumJob>`
    /// pour le polling automatique Apalis.
    inner: Arc<SqliteQueueStore>,
}

impl GradatumQueue {
    /// Crée un `GradatumQueue` depuis un [`SqliteQueueStore`].
    ///
    /// # Phase 1.1
    ///
    /// Délègue toutes les opérations à `store`. Le polling Apalis automatique
    /// sera câblé en Phase 1.2 via `apalis_sqlite::SqliteStorage`.
    #[must_use]
    pub fn new(store: SqliteQueueStore) -> Self {
        Self {
            inner: Arc::new(store),
        }
    }

    /// Référence sur le [`SqliteQueueStore`] sous-jacent.
    ///
    /// Utilisé par `gradatum-worker` pour l'injection dans `WorkerContext`.
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

    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        // Phase F-14 complet v0.3.0 — DAG await_jobs
        //
        // Requiert le cascade engine + table gradatum_job_deps.
        // v81 §6 L1838-1849 : implémentation via json_each() SQLite.
        // Référence : v81 §6 L2758 + F-14 milestone v0.3.0.
        todo!("Phase F-14 complet v0.3.0 — DAG await_jobs : cascade engine non implémenté")
    }

    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        // Phase F-14 complet v0.3.0 — cascade Waiting → Pending
        //
        // Dépend de find_awaiting (cascade engine).
        // Référence : v81 §6 L2759.
        todo!("Phase F-14 complet v0.3.0 — cascade set_pending non implémenté")
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
