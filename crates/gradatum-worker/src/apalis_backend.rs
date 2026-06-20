//! Custom Apalis backend — bridge between `SqliteQueueStore` and the Apalis API.
//!
//! Builds a `CustomBackend` (apalis_core) whose fetcher delegates to
//! `SqliteQueueStore::dequeue_by_kind`. Each stream poll performs one dequeue
//! after a 200 ms debounce; on an empty queue it yields `Ok(None)` (Idle event)
//! and returns control to the worker (canonical Apalis pattern — no internal loop,
//! see the worker-hang fix below).
//!
//! # Storage-agnostic pattern
//!
//! Apalis handles orchestration only. Storage stays in our `SqliteQueueStore`
//! via the `QueueStore` trait. `apalis-sqlite` is not used (no Apalis-side
//! storage coupling). This backend lets the Apalis `Monitor` orchestrate workers
//! without touching our persistence layer.
//!
//! # Operation
//!
//! - `fetcher`: per poll → sleep 200 ms → `dequeue_by_kind()` → `Some(record)`
//!   → emit task; `None` → yield `Ok(None)` (Idle); worker re-polls.
//! - `sink`: no-op (jobs are pushed via `GradatumQueue.enqueue()` externally).
//! - `IdType`: `RandomId` (internal Apalis identifier).
//! - Context: `()` (no Apalis-specific context — everything is carried in `GradatumJob`).
//! - The `Ulid` of `JobRecord` is injected into `task.parts.data` so that
//!   [`GradatumAcknowledger`] can retrieve it during the ack callback.
//!
//! # `Backend::ack` wired
//!
//! [`GradatumAcknowledger`] is attached via `.ack_with()` in the `Monitor`.
//! It translates `Ok(JobOutput)` → `store.complete()` and `Err(e)` → `store.fail()`.
//! [`build_gradatum_backend`] also returns a [`GradatumAcknowledger`] ready to
//! pass to `.ack_with()`.
//!
//! # TracingLayer — `.enable_tracing()` re-enabled
//!
//! The panic `make_span.rs:76 "A task must have an ID"` was caused by
//! `task.parts.task_id = None`. Fixed by injecting `TaskId::new(RandomId::default())`
//! when building the `Task` in the fetcher via `record_to_task`.
//!
//! # References
//!
//! - `docs/decisions/ARCH-D15-apalis-embedded.md`

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use apalis::prelude::{
    BackendBuilder, BoxDynError, CustomBackend, Parts, RandomId, Task, TaskBuilder, TaskId,
};
use futures::{StreamExt, sink, stream};
use tracing::{debug, error, warn};
use ulid::Ulid;

use gradatum_core::{GradatumJob, JobOutput, JobRecord, JobResult, QueueStore};

// ─────────────────────────────────────────────────────────────────────────────
// Types publics
// ─────────────────────────────────────────────────────────────────────────────

/// Custom Apalis backend type for Gradatum.
///
/// `Args = GradatumJob` — each Apalis task is a `GradatumJob`.
/// `IdType = RandomId` — internal Apalis identifier (random UUID).
/// The `Ulid` of `JobRecord` is carried in `task.parts.data` (typed extension).
pub type GradatumBackend = CustomBackend<
    GradatumJob,
    Arc<dyn QueueStore + Send + Sync>,
    futures::stream::BoxStream<
        'static,
        Result<Option<Task<GradatumJob, (), RandomId>>, BoxDynError>,
    >,
    futures::sink::Drain<Task<GradatumJob, (), RandomId>>,
    RandomId,
>;

// ─────────────────────────────────────────────────────────────────────────────
// GradatumAcknowledger — callback ack → QueueStore.complete/fail
// ─────────────────────────────────────────────────────────────────────────────

/// Apalis acknowledger for Gradatum.
///
/// Attached via `.ack_with(GradatumAcknowledger::new(store))` in the `Monitor`
/// on each `WorkerBuilder`.
///
/// # Contract
///
/// - Receives `res: &Result<JobOutput, BoxDynError>` after handler execution.
/// - Retrieves the job `Ulid` from `parts.data.get::<Ulid>()`.
/// - On `Ok` → [`QueueStore::complete`] with `JobResult { success: true, ... }`.
/// - On `Err` → [`QueueStore::fail`] with the error message + attempt count.
///
/// # Non-fatal errors
///
/// Errors from `store.complete()` / `store.fail()` are logged at `error!` but
/// not propagated — the worker continues without panicking on secondary DB errors.
#[derive(Clone)]
pub struct GradatumAcknowledger {
    store: Arc<dyn QueueStore + Send + Sync>,
}

impl GradatumAcknowledger {
    /// Creates a `GradatumAcknowledger` from a shared [`QueueStore`].
    #[must_use]
    pub fn new(store: Arc<dyn QueueStore + Send + Sync>) -> Self {
        Self { store }
    }
}

impl fmt::Debug for GradatumAcknowledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GradatumAcknowledger")
            .field("store", &"Arc<dyn QueueStore>")
            .finish()
    }
}

impl apalis::prelude::Acknowledge<JobOutput, (), RandomId> for GradatumAcknowledger {
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<(), std::convert::Infallible>> + Send>>;

    fn ack(
        &mut self,
        res: &Result<JobOutput, BoxDynError>,
        parts: &Parts<(), RandomId>,
    ) -> Self::Future {
        // Retrieve the job Ulid from the extensions injected in record_to_task.
        let maybe_job_id = parts.data.get::<Ulid>().copied();
        // Retrieve the start Instant injected in record_to_task.
        // Compute duration_ms before the move into the async closure (Instant is Copy).
        let duration_ms = parts
            .data
            .get::<Instant>()
            .map(|start| start.elapsed().as_millis() as u32)
            .unwrap_or(0);
        let attempt = parts.attempt.current() as u32;
        let store = Arc::clone(&self.store);

        let result = match res {
            Ok(output) => AckResult::Success(output.result_note_md.clone()),
            Err(e) => AckResult::Failure(format!("{e:#}"), attempt),
        };

        Box::pin(async move {
            let job_id = match maybe_job_id {
                Some(id) => id,
                None => {
                    // Should never occur if record_to_task correctly injects the Ulid.
                    error!(
                        "ack: Ulid absent des parts.data — job non retrouvable, status REST Running"
                    );
                    return Ok(());
                }
            };

            match result {
                AckResult::Success(desc) => {
                    let job_result = JobResult {
                        success: true,
                        duration_ms, // D-21 : mesure réelle via Instant injecté dans record_to_task
                        cost_usd: None,
                        result_note: None,
                        conflict_payload: None,
                    };
                    if let Err(e) = store.complete(job_id, job_result).await {
                        error!(
                            job_id = %job_id,
                            error = %e,
                            desc = %desc,
                            "ack: store.complete échoué — status restera Running"
                        );
                    } else {
                        tracing::info!(
                            job_id = %job_id,
                            "ack: job marqué Done"
                        );
                    }
                }
                AckResult::Failure(err_msg, att) => {
                    if let Err(e) = store.fail(job_id, &err_msg, att).await {
                        error!(
                            job_id = %job_id,
                            error = %e,
                            handler_error = %err_msg,
                            "ack: store.fail échoué — status restera Running"
                        );
                    } else {
                        warn!(
                            job_id = %job_id,
                            attempt = att,
                            handler_error = %err_msg,
                            "ack: job marqué Failed"
                        );
                    }
                }
            }

            Ok(())
        })
    }
}

/// Intermediate result to avoid capturing the full `res` inside the async closure.
enum AckResult {
    Success(String),
    Failure(String, u32),
}

// ─────────────────────────────────────────────────────────────────────────────
// Constructeur principal
// ─────────────────────────────────────────────────────────────────────────────

/// Builds the [`GradatumBackend`] and an associated [`GradatumAcknowledger`].
///
/// The caller must call `.ack_with(acknowledger)` on each `WorkerBuilder`
/// so that handler results are propagated to `QueueStore.complete/fail`.
///
/// # Parameter `kind`
///
/// Filters jobs fetched from the queue via [`QueueStore::dequeue_by_kind`].
/// Ensures each backend receives only jobs matching its handler
/// (e.g. `"Curate"` for the curate worker, `"Embed"` for the embed worker).
/// Eliminates the DLQ race condition where an embed-worker could fetch a Curate
/// job and return `HandlerError::UnexpectedVariant`.
///
/// # Why return both?
///
/// `GradatumAcknowledger` clones `Arc<dyn QueueStore>` — it is cheap to clone
/// per worker. The backend and acknowledger share the same store.
///
/// # Errors
///
/// Returns an error if the builder fails (should not occur with this parameter set).
pub fn build_gradatum_backend(
    store: Arc<dyn QueueStore + Send + Sync>,
    kind: &'static str,
) -> Result<(GradatumBackend, GradatumAcknowledger), anyhow::Error> {
    let store_for_fetcher = Arc::clone(&store);
    let acknowledger = GradatumAcknowledger::new(Arc::clone(&store));

    let backend = BackendBuilder::<GradatumJob, _, _, _, RandomId>::new()
        .database(store_for_fetcher)
        .fetcher(move |db, _, _| {
            let db = Arc::clone(db);
            stream::unfold(db, move |store| async move {
                // Canonical Apalis pattern: one stream poll = one dequeue. On an
                // empty queue, yield `Ok(None)` (Idle event) and return control to
                // the worker — NEVER loop internally.
                //
                // # Why no internal `loop {}` (worker-hang bug)
                //
                // An internal `loop` that only yields on `Some(record)` parks the
                // unfold future on `sleep` when the queue is empty. The Apalis
                // `CallAll::poll_next` driver gates stream polling behind the
                // `poll_ready` of the concurrency Tower limiter. The sleep wakeup
                // and the concurrency permit release race — a wakeup is lost → the
                // stream is never re-polled → the worker stops after draining a
                // micro-batch (0–2 jobs depending on timing), leaving remaining
                // Pending jobs stalled.
                //
                // Reproduced empirically by the integration test
                // `tests/worker_repoll.rs::worker_repolls_after_drain`: drain a
                // batch then re-enqueue; fails with `loop` (false), passes with
                // this pattern. The 200 ms debounce stays BEFORE the dequeue.
                tokio::time::sleep(Duration::from_millis(200)).await;
                match store.dequeue_by_kind(kind).await {
                    Ok(Some(record)) => {
                        let task = record_to_task(record);
                        Some((Ok(Some(task)), store))
                    }
                    Ok(None) => {
                        // Empty queue — yield Idle and return control to the worker.
                        debug!("gradatum_backend: file vide (Idle), re-poll dans 200ms");
                        Some((Ok(None), store))
                    }
                    Err(e) => {
                        // Storage error — emit error; the worker continues.
                        Some((Err(BoxDynError::from(format!("dequeue error: {e}"))), store))
                    }
                }
            })
            .boxed()
        })
        .sink(|_, _| {
            // No-op sink: jobs are inserted via external QueueStore::enqueue().
            // The Apalis sink is not used for results — that is the role of
            // GradatumAcknowledger (.ack_with), which notifies QueueStore.
            sink::drain()
        })
        .build()
        .map_err(|e| anyhow::anyhow!("build_gradatum_backend failed: {e:?}"))?;

    Ok((backend, acknowledger))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers internes
// ─────────────────────────────────────────────────────────────────────────────

/// Converts a [`JobRecord`] into a [`Task<GradatumJob>`] for Apalis transport.
///
/// # Injections into task.parts
///
/// - `task.parts.task_id`: `TaskId::new(RandomId::default())` — required by TracingLayer
///   (`.enable_tracing()`) to build the span without triggering
///   `expect("A task must have an ID")`.
/// - `task.parts.data[Ulid]`: `record.id` — retrieved by [`GradatumAcknowledger::ack`]
///   to call `store.complete(job_id)` / `store.fail(job_id)`.
/// - `task.parts.data[Instant]`: start timestamp — retrieved by
///   [`GradatumAcknowledger::ack`] to compute the real `duration_ms`.
fn record_to_task(record: JobRecord) -> Task<GradatumJob, (), RandomId> {
    let job_id = record.id;
    let priority = record.spec.priority.as_u8();
    let job = GradatumJob { record, priority };

    // Capture the Instant before building the task — measures elapsed time from
    // dequeue to handler completion (total worker-side execution time, including
    // Apalis dispatch).
    let started_at = Instant::now();

    TaskBuilder::new(job)
        // TaskId required for .enable_tracing() — make_span.rs:76 expect("A task must have an ID")
        .with_task_id(TaskId::new(RandomId::default()))
        // Ulid injected for GradatumAcknowledger::ack() → store.complete/fail
        .data(job_id)
        // Instant injected for real duration_ms measurement
        .data(started_at)
        .build()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
        JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, TriggerSource,
    };
    use ulid::Ulid;

    fn make_record() -> JobRecord {
        let now = Utc::now();
        let class = JobClass::Agent;
        JobRecord {
            id: Ulid::new(),
            spec: JobSpec {
                kind: Job::Curate(CurateSpec {
                    note_id: Ulid::new(),
                    tenant_id: "main".to_string(),
                    ..Default::default()
                }),
                class,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::High,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: now,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Pending,
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

    #[test]
    fn record_to_task_preserves_fields() {
        let record = make_record();
        let id = record.id;
        let task = record_to_task(record);
        // GradatumJob.record.id préservé
        assert_eq!(task.args.record.id, id);
        // priority préservée
        assert_eq!(task.args.priority, JobPriority::High.as_u8());
    }

    #[test]
    fn record_to_task_injects_task_id() {
        let record = make_record();
        let task = record_to_task(record);
        // task_id doit être Some — requis pour .enable_tracing()
        assert!(
            task.parts.task_id.is_some(),
            "task_id doit être injecté pour TracingLayer"
        );
    }

    #[test]
    fn record_to_task_injects_ulid_in_data() {
        let record = make_record();
        let expected_id = record.id;
        let task = record_to_task(record);
        // Ulid injecté dans parts.data pour GradatumAcknowledger
        let retrieved = task.parts.data.get::<Ulid>().copied();
        assert_eq!(
            retrieved,
            Some(expected_id),
            "Ulid du JobRecord doit être dans parts.data"
        );
    }

    #[test]
    fn acknowledger_is_clone() {
        // GradatumAcknowledger doit être Clone pour .ack_with() Apalis
        use std::sync::Arc;
        struct MockStore;
        #[async_trait::async_trait]
        impl QueueStore for MockStore {
            async fn enqueue(&self, _: JobRecord) -> Result<Ulid, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn dequeue(&self) -> Result<Option<JobRecord>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn get(&self, _: Ulid) -> Result<Option<JobRecord>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn complete(
                &self,
                _: Ulid,
                _: JobResult,
            ) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn fail(
                &self,
                _: Ulid,
                _: &str,
                _: u32,
            ) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn cancel(&self, _: Ulid) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn fail_dlq(&self, _: Ulid, _: &str) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn find_awaiting(
                &self,
                _: Ulid,
            ) -> Result<Vec<JobRecord>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn set_pending(&self, _: Ulid) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn recover_stale_leases(
                &self,
                _: std::time::Duration,
            ) -> Result<Vec<Ulid>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn cancel_expired_deadlines(
                &self,
                _: chrono::DateTime<chrono::Utc>,
            ) -> Result<Vec<Ulid>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn promote_retries(
                &self,
                _: chrono::DateTime<chrono::Utc>,
            ) -> Result<Vec<Ulid>, gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn schedule_retry(
                &self,
                _: Ulid,
                _: chrono::DateTime<chrono::Utc>,
            ) -> Result<(), gradatum_core::QueueError> {
                unimplemented!()
            }
            async fn list(
                &self,
                _: gradatum_core::JobFilter,
            ) -> Result<Vec<JobRecord>, gradatum_core::QueueError> {
                unimplemented!()
            }
            fn subscribe(&self) -> tokio::sync::broadcast::Receiver<gradatum_core::QueueEvent> {
                unimplemented!()
            }
        }
        let ack = GradatumAcknowledger::new(Arc::new(MockStore));
        let _ack2 = ack.clone(); // doit compiler
    }

    /// D-21 : vérifie que l'Instant est injecté dans parts.data par record_to_task.
    ///
    /// L'Instant est utilisé par GradatumAcknowledger::ack() pour calculer duration_ms réel.
    #[test]
    fn record_to_task_injects_instant_for_duration_measurement() {
        let record = make_record();
        let before = Instant::now();
        let task = record_to_task(record);
        let after = Instant::now();

        let injected = task.parts.data.get::<Instant>().copied();
        assert!(
            injected.is_some(),
            "Instant doit être injecté dans parts.data pour mesure duration_ms (D-21)"
        );

        // L'Instant injecté doit être compris entre before et after
        let start = injected.unwrap();
        assert!(
            start >= before,
            "Instant injecté doit être >= before (créé après before)"
        );
        assert!(
            start <= after,
            "Instant injecté doit être <= after (créé avant after)"
        );
    }
}
