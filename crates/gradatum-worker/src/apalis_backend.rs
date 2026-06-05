//! Backend Apalis custom — pont entre [`SqliteQueueStore`] et l'API Apalis rc.9.
//!
//! Construit un [`CustomBackend`](apalis_core::backend::custom::CustomBackend)
//! dont le fetcher délègue à [`SqliteQueueStore::dequeue_by_kind`]. Chaque poll
//! du stream effectue UN dequeue après un debounce de 200ms ; sur file vide il
//! yield `Ok(None)` (event Idle) et rend la main au worker (pattern canonique
//! Apalis — surtout PAS de boucle interne, cf. fix worker-hang ci-dessous).
//!
//! # Pattern agnostique F-24
//!
//! Apalis = orchestration uniquement. Le storage reste notre `SqliteQueueStore`
//! via le trait `QueueStore`. On n'utilise pas `apalis-sqlite` (pas de couplage
//! storage Apalis-side). Ce backend sert de passerelle pour que le `Monitor`
//! Apalis puisse orchestrer nos workers sans toucher à notre persistence.
//!
//! # Fonctionnement
//!
//! - `fetcher` : par poll → sleep 200ms → `dequeue_by_kind()` → si `Some(record)`
//!   → emit task ; si `None` → yield `Ok(None)` (Idle) ; le worker re-poll.
//! - `sink` : no-op (push via `GradatumQueue.enqueue()` externe).
//! - `IdType` : `RandomId` (identifiant Apalis interne).
//! - Context : `()` (sans contexte Apalis spécifique — on porte tout dans `GradatumJob`).
//! - Le `Ulid` du `JobRecord` est injecté dans `task.parts.data` pour que
//!   [`GradatumAcknowledger`] puisse le récupérer lors du callback ack.
//!
//! # Fix Phase 1.2 — Backend::ack câblé (D-01 résolu)
//!
//! Le [`GradatumAcknowledger`] est branché via `.ack_with()` dans le `Monitor`.
//! Il traduit `Ok(JobOutput)` → `store.complete()` et `Err(e)` → `store.fail()`.
//! La fonction [`build_gradatum_backend`] retourne aussi un [`GradatumAcknowledger`]
//! prêt à être passé à `.ack_with()`.
//!
//! # TracingLayer — `.enable_tracing()` re-activé (Phase 1.2 fix)
//!
//! Le panic `make_span.rs:76 "A task must have an ID"` était dû à `task.parts.task_id = None`.
//! Résolu en injectant `TaskId::new(RandomId::default())` lors de la construction du Task dans
//! le fetcher via [`record_to_task`].
//!
//! # Références
//!
//! - Spec §2 F-14 partiel — pattern agnostique F-24
//! - v81 §6 L1784-1799 — `GradatumQueue` architecture
//! - ARCH-D15 — `docs/decisions/ARCH-D15-apalis-embedded.md`

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use apalis::prelude::{
    BackendBuilder, BoxDynError, CustomBackend, Parts, RandomId, Task, TaskBuilder, TaskId,
};
use futures::{sink, stream, StreamExt};
use tracing::{debug, error, warn};
use ulid::Ulid;

use gradatum_core::{GradatumJob, JobOutput, JobRecord, JobResult, QueueStore};

// ─────────────────────────────────────────────────────────────────────────────
// Types publics
// ─────────────────────────────────────────────────────────────────────────────

/// Type du backend Apalis custom pour Gradatum.
///
/// `Args = GradatumJob` — chaque tâche Apalis est un `GradatumJob`.
/// `IdType = RandomId` — identifiant Apalis interne (UUID random).
/// Le `Ulid` du `JobRecord` est porté dans `task.parts.data` (extension typée).
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

/// Acknowledger Apalis pour Gradatum.
///
/// Branché via `.ack_with(GradatumAcknowledger::new(store))` dans le `Monitor`
/// sur chaque `WorkerBuilder`.
///
/// # Contrat
///
/// - Reçoit `res: &Result<JobOutput, BoxDynError>` après exécution du handler
/// - Récupère le `Ulid` du job depuis `parts.data.get::<Ulid>()`
/// - Si `Ok` → [`QueueStore::complete`] avec `JobResult { success: true, ... }`
/// - Si `Err` → [`QueueStore::fail`] avec le message d'erreur + count de tentatives
///
/// # Erreurs non-fatales
///
/// Les erreurs `store.complete()` / `store.fail()` sont loggées en `error!` mais
/// non propagées (le worker continue — pas de panique sur erreur DB secondaire).
///
/// # Références
///
/// - Spec §11 E-23 — runtime Backend::complete câblé
/// - D-21 P3 — `duration_ms` réel via `Instant` injecté dans `record_to_task`
/// - D-01 résolution `gradatum_jobs.status = Done`
#[derive(Clone)]
pub struct GradatumAcknowledger {
    store: Arc<dyn QueueStore + Send + Sync>,
}

impl GradatumAcknowledger {
    /// Crée un `GradatumAcknowledger` depuis un [`QueueStore`] partagé.
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
        // Récupérer le Ulid du job depuis les extensions injectées dans record_to_task.
        let maybe_job_id = parts.data.get::<Ulid>().copied();
        // D-21 : récupérer l'Instant de démarrage injecté dans record_to_task.
        // Calcul duration_ms avant le move dans la closure async (Instant est Copy).
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
                    // Ne devrait jamais arriver si record_to_task injecte correctement le Ulid.
                    error!("ack: Ulid absent des parts.data — job non retrouvable, status REST Running");
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

/// Résultat intermédiaire pour éviter de capturer `res` en entier dans la closure async.
enum AckResult {
    Success(String),
    Failure(String, u32),
}

// ─────────────────────────────────────────────────────────────────────────────
// Constructeur principal
// ─────────────────────────────────────────────────────────────────────────────

/// Construit le [`GradatumBackend`] + un [`GradatumAcknowledger`] associé.
///
/// Le caller doit appeler `.ack_with(acknowledger)` sur chaque `WorkerBuilder`
/// pour que les résultats des handlers soient propagés vers `QueueStore.complete/fail`.
///
/// # Paramètre `kind`
///
/// Filtre les jobs fetchés depuis la queue par [`QueueStore::dequeue_by_kind`].
/// Garantit que chaque backend ne reçoit que les jobs correspondant à son handler
/// (ex. `"Curate"` pour le curate-worker, `"Embed"` pour l'embed-worker).
/// Elimine la race condition DLQ où un embed-worker fetchait un Curate job
/// et retournait `HandlerError::UnexpectedVariant`.
///
/// # Pourquoi retourner les deux ?
///
/// `GradatumAcknowledger` clone `Arc<dyn QueueStore>` — il est léger à cloner
/// pour chaque worker. Le backend et l'acknowledger partagent le même store.
///
/// # Erreurs
///
/// Retourne une erreur si le builder échoue (ne devrait pas se produire
/// avec ce jeu de paramètres).
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
                // Pattern CANONIQUE Apalis (custom.rs doc L41-51 + poll_strategy
                // test L203-220) : UN poll du stream = UN dequeue. Sur file vide,
                // yield `Ok(None)` (event Idle) et RENDRE la main au worker — ne
                // JAMAIS boucler en interne.
                //
                // # Pourquoi pas de `loop {}` interne (bug worker-hang v0.3.x)
                //
                // Un `loop` interne qui ne yield que sur `Some(record)` garde le
                // future de l'unfold parqué sur le `sleep` quand la file est vide.
                // Le driver Apalis `CallAll::poll_next` gate le poll du stream
                // derrière `poll_ready` du limiter `concurrency` (tower). Le réveil
                // du timer `sleep` et celui de la libération d'un permit de
                // concurrence se courent — un wakeup est perdu → le stream n'est
                // plus re-pollé → le worker stoppe après avoir drainé un micro-batch
                // (0-2 jobs selon le timing), les `Pending` restants stagnent.
                //
                // Reproduit empiriquement par le test d'intégration
                // `tests/worker_repoll.rs::worker_repolls_after_drain` : drain d'un
                // batch puis ré-enqueue ; échoue avec `loop` (faux), passe avec ce
                // pattern. Le debounce 200ms reste AVANT le dequeue.
                tokio::time::sleep(Duration::from_millis(200)).await;
                match store.dequeue_by_kind(kind).await {
                    Ok(Some(record)) => {
                        let task = record_to_task(record);
                        Some((Ok(Some(task)), store))
                    }
                    Ok(None) => {
                        // File vide — yield Idle et rendre la main au worker.
                        debug!("gradatum_backend: file vide (Idle), re-poll dans 200ms");
                        Some((Ok(None), store))
                    }
                    Err(e) => {
                        // Erreur storage — emit erreur, le worker continue.
                        Some((Err(BoxDynError::from(format!("dequeue error: {e}"))), store))
                    }
                }
            })
            .boxed()
        })
        .sink(|_, _| {
            // Sink no-op : les jobs sont insérés via QueueStore::enqueue() externe.
            // Le sink Apalis n'est pas utilisé pour les résultats — c'est le rôle
            // du GradatumAcknowledger (.ack_with) de notifier QueueStore.
            sink::drain()
        })
        .build()
        .map_err(|e| anyhow::anyhow!("build_gradatum_backend failed: {e:?}"))?;

    Ok((backend, acknowledger))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers internes
// ─────────────────────────────────────────────────────────────────────────────

/// Convertit un [`JobRecord`] en [`Task<GradatumJob>`] pour le transport Apalis.
///
/// # Injections dans task.parts
///
/// - `task.parts.task_id` : `TaskId::new(RandomId::default())` — requis par TracingLayer
///   (`.enable_tracing()`) pour construire le span sans `expect("A task must have an ID")`.
/// - `task.parts.data[Ulid]` : `record.id` — récupéré par [`GradatumAcknowledger::ack`]
///   pour appeler `store.complete(job_id)` / `store.fail(job_id)`.
/// - `task.parts.data[Instant]` : horodatage de démarrage (D-21) — récupéré par
///   [`GradatumAcknowledger::ack`] pour calculer `duration_ms` réel.
fn record_to_task(record: JobRecord) -> Task<GradatumJob, (), RandomId> {
    let job_id = record.id;
    let priority = record.spec.priority.as_u8();
    let job = GradatumJob { record, priority };

    // D-21 : capture l'Instant avant la construction de la tâche — mesure
    // le temps écoulé depuis la dequeue jusqu'à la fin du handler (temps total
    // d'exécution worker-side, incluant le dispatch Apalis).
    let started_at = Instant::now();

    TaskBuilder::new(job)
        // TaskId requis pour .enable_tracing() — make_span.rs:76 expect("A task must have an ID")
        .with_task_id(TaskId::new(RandomId::default()))
        // Ulid injecté pour GradatumAcknowledger::ack() → store.complete/fail
        .data(job_id)
        // Instant injecté pour mesure duration_ms réelle (D-21)
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
