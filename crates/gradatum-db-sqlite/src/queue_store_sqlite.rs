//! SQLite implementation of [`QueueStore`] — `SqliteQueueStore`.
//!
//! Stores [`JobRecord`] entries in the `gradatum_jobs` table (migration 006).
//! Uses async `sqlx` with WAL mode for queue operations.
//!
//! # Guarantees
//!
//! - **Atomic lease**: `UPDATE … SET status='Running', lease_until=? WHERE id=?`
//!   inside an exclusive transaction prevents double consumption.
//! - **Periodic sweep**: `recover_stale_leases`, `cancel_expired_deadlines`, and
//!   `promote_retries` are called by the worker every 30 seconds.
//! - **Cascade**: `find_awaiting` + `set_pending` for `await_jobs` chaining.
//!
//! # Limitations
//!
//! - `find_awaiting` uses `LIKE '%"id"%'` — acceptable for fewer than 10k active jobs.
//!   Planned improvement: native JSON index or a `gradatum_job_deps` join table.
//! - No `LibsqlQueueStore`.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use ulid::Ulid;

use gradatum_core::{
    job_kind_str, JobError, JobFilter, JobOrder, JobRecord, JobResult, JobStatus, QueueError,
    QueueEvent, QueueStore,
};

/// Broadcast channel capacity for [`QueueEvent`] — default value.
const BROADCAST_CAPACITY: usize = 256;

/// SQLite implementation of [`QueueStore`].
///
/// Constructed from a `sqlx` [`SqlitePool`] (WAL mode required).
/// Use [`SqliteQueueStore::new`] to create an instance.
///
/// # Example
///
/// ```rust,ignore
/// let pool = SqlitePool::connect("sqlite:///path/to/gradatum.db?mode=rwc").await?;
/// let store = SqliteQueueStore::new(pool);
/// ```
pub struct SqliteQueueStore {
    /// Shared `sqlx` pool (WAL mode, `synchronous=NORMAL`).
    pool: SqlitePool,
    /// Sender for the broadcast channel carrying queue events.
    ///
    /// `broadcast::Sender` is `Clone + Send + Sync` — may be cloned for
    /// each method that publishes an event.
    tx: broadcast::Sender<QueueEvent>,
}

impl SqliteQueueStore {
    /// Creates a new `SqliteQueueStore` from a `sqlx` pool.
    ///
    /// The pool must be configured in WAL mode (`PRAGMA journal_mode=WAL`).
    /// Migration `006_apalis_bootstrap.sql` must have been applied.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { pool, tx }
    }

    /// Publishes an event on the broadcast channel.
    ///
    /// Publication errors (no receivers) are silently ignored — the broadcast
    /// is best-effort. SSE and cascade consumers subscribe via `subscribe()`.
    fn publish(&self, event: QueueEvent) {
        if let Err(e) = self.tx.send(event) {
            debug!("SqliteQueueStore: aucun consommateur broadcast actif ({e})");
        }
    }

    /// Serializes a `JobRecord` to JSON for storage.
    fn serialize_record(record: &JobRecord) -> Result<String, QueueError> {
        serde_json::to_string(record).map_err(|e| QueueError::Serialization(e.to_string()))
    }

    /// Deserializes a `JobRecord` from JSON.
    fn deserialize_record(json: &str) -> Result<JobRecord, QueueError> {
        serde_json::from_str(json).map_err(|e| QueueError::Serialization(e.to_string()))
    }

    /// Converts a `JobStatus` to its SQLite TEXT representation.
    fn status_to_str(status: &JobStatus) -> &'static str {
        match status {
            JobStatus::Pending => "Pending",
            JobStatus::Running => "Running",
            JobStatus::Waiting => "Waiting",
            JobStatus::Done => "Done",
            JobStatus::Failed => "Failed",
            JobStatus::DLQ => "DLQ",
            JobStatus::Cancelled => "Cancelled",
            // Terminal state for optimistic-lock conflicts (note not overwritten).
            JobStatus::Conflict => "Conflict",
        }
    }

    /// Converts a SQLite TEXT representation to `JobStatus`.
    fn str_to_status(s: &str) -> Result<JobStatus, QueueError> {
        match s {
            "Pending" => Ok(JobStatus::Pending),
            "Running" => Ok(JobStatus::Running),
            "Waiting" => Ok(JobStatus::Waiting),
            "Done" => Ok(JobStatus::Done),
            "Failed" => Ok(JobStatus::Failed),
            "DLQ" => Ok(JobStatus::DLQ),
            "Cancelled" => Ok(JobStatus::Cancelled),
            // Optimistic-lock conflict terminal state (persisted as "Conflict").
            "Conflict" => Ok(JobStatus::Conflict),
            other => Err(QueueError::InvalidTransition(format!(
                "statut inconnu : '{other}'"
            ))),
        }
    }
}

#[async_trait]
impl QueueStore for SqliteQueueStore {
    async fn enqueue(&self, job: JobRecord) -> Result<Ulid, QueueError> {
        let id = job.id;
        let id_str = id.to_string();
        let payload = Self::serialize_record(&job)?;
        let status = Self::status_to_str(&job.lifecycle.status);
        let priority = job.spec.priority.as_u8() as i64;
        let class = format!("{:?}", job.spec.class);
        // Dénormalise le variant Job → colonne `kind` pour le filtrage SQL natif
        // (fix routing DLQ : chaque worker ne fetch que ses propres jobs via dequeue_by_kind).
        let kind = job_kind_str(&job.spec.kind);
        let created_at = job.lifecycle.created_at.to_rfc3339();
        let scheduled_at = job.scheduling.scheduled_at.to_rfc3339();
        let deadline = job.scheduling.deadline.as_ref().map(|d| d.to_rfc3339());

        // Chaînage await_jobs sérialisé en JSON array de strings ULID
        let await_jobs = if job.scheduling.await_jobs.is_empty() {
            None
        } else {
            let ids: Vec<String> = job
                .scheduling
                .await_jobs
                .iter()
                .map(|t| t.job_id.to_string())
                .collect();
            Some(
                serde_json::to_string(&ids)
                    .map_err(|e| QueueError::Serialization(e.to_string()))?,
            )
        };

        sqlx::query(
            r#"
            INSERT INTO gradatum_jobs
                (id, payload, status, priority, class, kind, created_at, scheduled_at, deadline, await_jobs)
            VALUES
                (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id_str)
        .bind(&payload)
        .bind(status)
        .bind(priority)
        .bind(&class)
        .bind(kind)
        .bind(&created_at)
        .bind(&scheduled_at)
        .bind(&deadline)
        .bind(&await_jobs)
        .execute(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        self.publish(QueueEvent::JobInserted(id));
        Ok(id)
    }

    async fn dequeue(&self) -> Result<Option<JobRecord>, QueueError> {
        // Lease atomique via transaction EXCLUSIVE
        // Sélectionne le job de plus haute priorité schedulé maintenant
        let now = Utc::now().to_rfc3339();
        let lease_until = (Utc::now() + chrono::Duration::seconds(300)).to_rfc3339();

        // `BEGIN IMMEDIATE` : voir la justification détaillée dans `dequeue_by_kind`.
        // Transaction read-then-write (SELECT lease + UPDATE) → l'upgrade read→write
        // déféré deadlocke sous contention multi-worker. IMMEDIATE prend le verrou
        // d'écriture en amont et sérialise proprement.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let row = sqlx::query(
            r#"
            SELECT id, payload
            FROM gradatum_jobs
            WHERE status = 'Pending'
              AND scheduled_at <= ?
            ORDER BY priority DESC, scheduled_at ASC
            LIMIT 1
            "#,
        )
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        let Some(row) = row else {
            tx.rollback()
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            return Ok(None);
        };

        let id_str: String = row
            .try_get("id")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let payload: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        // Mise à jour du lease (atomic dans la même transaction)
        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Running',
                lease_until = ?,
                started_at = ?,
                attempt_count = attempt_count + 1
            WHERE id = ?
            "#,
        )
        .bind(&lease_until)
        .bind(&now)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let mut record = Self::deserialize_record(&payload)?;
        // Synchronise le statut en mémoire avec l'état en base
        record.lifecycle.status = JobStatus::Running;

        Ok(Some(record))
    }

    /// Overrides [`QueueStore::dequeue_by_kind`] — native SQL filtering by `kind`.
    ///
    /// Uses the `idx_jobs_status_kind (status, kind)` index to guarantee that a
    /// `curate` worker never receives an `Embed` or `ReIndex` job.
    /// Without this filter, contention among concurrent workers (curate=2, embed=4,
    /// reindex=4) produces ~80% DLQ via `HandlerError::UnexpectedVariant`.
    async fn dequeue_by_kind(&self, kind: &str) -> Result<Option<JobRecord>, QueueError> {
        let now = Utc::now().to_rfc3339();
        let lease_until = (Utc::now() + chrono::Duration::seconds(300)).to_rfc3339();

        // `BEGIN IMMEDIATE` (et non `BEGIN` déféré, le défaut de sqlx) : ce dequeue
        // est une transaction read-then-write (SELECT puis UPDATE du lease). En
        // mode déféré, le SELECT prend un verrou de lecture partagé, puis l'UPDATE
        // tente de l'upgrader en verrou d'écriture exclusif. Sous charge multi-worker
        // concurrente sur le même fichier SQLite (curate + embed + reindex), deux
        // dequeues simultanés détiennent chacun un verrou de lecture et tentent
        // tous deux l'upgrade → deadlock mutuel → `SQLITE_BUSY` ; `busy_timeout`
        // retente mais sous contention soutenue une transaction est affamée
        // indéfiniment (un worker draine, l'autre reste à 0 — bug v0.3.x).
        // `BEGIN IMMEDIATE` prend le verrou d'écriture dès le début : les dequeues
        // se sérialisent proprement sans upgrade, plus de deadlock.
        // Reproduit + validé empiriquement par `tests/worker_multikind_concurrency.rs`.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let row = sqlx::query(
            r#"
            SELECT id, payload
            FROM gradatum_jobs
            WHERE status = 'Pending'
              AND kind = ?
              AND scheduled_at <= ?
            ORDER BY priority DESC, scheduled_at ASC
            LIMIT 1
            "#,
        )
        .bind(kind)
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        let Some(row) = row else {
            tx.rollback()
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            return Ok(None);
        };

        let id_str: String = row
            .try_get("id")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let payload: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Running',
                lease_until = ?,
                started_at = ?,
                attempt_count = attempt_count + 1
            WHERE id = ?
            "#,
        )
        .bind(&lease_until)
        .bind(&now)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let mut record = Self::deserialize_record(&payload)?;
        record.lifecycle.status = JobStatus::Running;

        Ok(Some(record))
    }

    async fn get(&self, id: Ulid) -> Result<Option<JobRecord>, QueueError> {
        // Fix E-12 : synchronise le statut du JobRecord avec les colonnes SQL autoritatives.
        //
        // Le payload BLOB contient le JobRecord sérialisé à l'enqueue. Après dequeue(),
        // le status SQL est mis à jour en Running MAIS le payload BLOB reste Pending (optimisation
        // atomicité — évite de réécrire le payload dans la transaction de lease).
        // On lit donc les colonnes SQL et on les injecte dans le record désérialisé.
        //
        // Colonnes SQL autoritatives : status, attempt_count, last_error, completed_at.
        let id_str = id.to_string();

        let row = sqlx::query(
            r#"SELECT payload, status, attempt_count, last_error, completed_at
               FROM gradatum_jobs WHERE id = ?"#,
        )
        .bind(&id_str)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        match row {
            None => Ok(None),
            Some(r) => {
                let payload: String = r
                    .try_get("payload")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;
                let sql_status: String = r
                    .try_get("status")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;
                let sql_attempts: i64 = r
                    .try_get("attempt_count")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;
                let sql_last_error: Option<String> = r
                    .try_get("last_error")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;
                let sql_completed_at: Option<String> = r
                    .try_get("completed_at")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;

                let mut record = Self::deserialize_record(&payload)?;

                // Synchronisation colonnes SQL → record (colonnes SQL font autorité).
                // Fallback sur Pending si le variant est inconnu (defensive — ne devrait pas
                // arriver en pratique, protège contre les migrations schema forward-compat).
                record.lifecycle.status =
                    Self::str_to_status(&sql_status).unwrap_or(JobStatus::Pending);
                record.retry.count = sql_attempts as u32;
                if sql_last_error.is_some() {
                    record.retry.last_error = sql_last_error;
                }
                if let Some(completed_str) = sql_completed_at {
                    if record.lifecycle.completed_at.is_none() {
                        // Réhydrate completed_at depuis SQL si le BLOB ne l'a pas encore
                        record.lifecycle.completed_at = completed_str.parse::<DateTime<Utc>>().ok();
                    }
                }

                Ok(Some(record))
            }
        }
    }

    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();

        // BEGIN IMMEDIATE : la lecture du payload et l'UPDATE sont dans la même
        // transaction exclusive — évite le double-complete concurrent (deux workers
        // lisant le même job Running avant que l'un ne l'ait marqué Done).
        // Même pattern que dequeue() — justification détaillée dans dequeue_by_kind().
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        // Met à jour le payload avec le résultat et le statut Done
        // Note : le payload JSON est la source de vérité — on relit, on patche, on réécrit.
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ?"#)
            .bind(&id_str)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?
            .ok_or(QueueError::NotFound(id))?;

        let payload_str: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let mut record = Self::deserialize_record(&payload_str)?;

        // F-41 — Garde anti-clobber Conflict→Done.
        //
        // Le handler `handle_curate` marque déjà le job `Conflict` via `mark_conflict`
        // (optimistic-lock périmé : note NON écrite) PUIS retourne `Ok(JobOutput)`.
        // L'acknowledger apalis interprète tout `Ok` comme un succès et appelle ce
        // `complete()` — qui, sans cette garde, écraserait `Conflict` par `Done`
        // (last-writer-wins séquentiel : la transaction `BEGIN IMMEDIATE` sérialise
        // les deux écritures mais ne les empêche pas, l'isolation ne protège donc pas).
        // Symptôme LIVE F-41 : l'appelant RMW voyait `Done` au lieu de `Conflict` et ne
        // pouvait pas distinguer « écrit » de « rejeté-périmé » → no-op silencieux.
        //
        // Conflict est un état TERMINAL posé délibérément par le worker : `complete()`
        // doit le respecter (idempotence sur état terminal — même esprit que la garde
        // `WHERE status NOT IN ('Done','DLQ','Cancelled','Conflict')` de `cancel()` ;
        // `fail()` n'a PAS de garde de statut et ne fait PAS partie de ce précédent).
        // On commit la transaction sans modifier la ligne (le SELECT a déjà pris le
        // verrou IMMEDIATE) puis on retourne Ok : l'ack est un no-op sur ce job.
        if record.lifecycle.status == JobStatus::Conflict {
            tx.commit()
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            tracing::debug!(
                job_id = %id,
                "complete: job déjà en état terminal Conflict (F-41) — Done ignoré"
            );
            return Ok(());
        }

        record.lifecycle.status = JobStatus::Done;
        record.lifecycle.completed_at = Some(Utc::now());
        record.lifecycle.result = Some(result.clone());
        let new_payload = Self::serialize_record(&record)?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Done',
                completed_at = ?,
                lease_until = NULL,
                payload = ?
            WHERE id = ?
            "#,
        )
        .bind(&now)
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let status = if result.success {
            JobStatus::Done
        } else {
            JobStatus::Failed
        };
        self.publish(QueueEvent::JobCompleted(id, status, result));
        Ok(())
    }

    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let err_truncated = if err.len() > 2048 { &err[..2048] } else { err };

        // BEGIN IMMEDIATE : lecture + mise à jour atomiques — évite que deux appels
        // fail() concurrents n'écrasent mutuellement leur compteur d'erreurs.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        // Relit le payload pour mettre à jour les erreurs
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ?"#)
            .bind(&id_str)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?
            .ok_or(QueueError::NotFound(id))?;

        let payload_str: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let mut record = Self::deserialize_record(&payload_str)?;
        record.lifecycle.status = JobStatus::Failed;
        record.retry.count = attempt;
        record.retry.last_error = Some(err_truncated.to_string());
        record.retry.errors.push(JobError {
            at: Utc::now(),
            message: err_truncated.to_string(),
            attempt,
        });
        let new_payload = Self::serialize_record(&record)?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Failed',
                lease_until = NULL,
                last_error = ?,
                attempt_count = ?,
                payload = ?
            WHERE id = ?
            "#,
        )
        .bind(err_truncated)
        .bind(attempt as i64)
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        self.publish(QueueEvent::JobFailed(id, attempt));
        Ok(())
    }

    async fn cancel(&self, id: Ulid) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        // BEGIN IMMEDIATE : la vérification du statut courant (SELECT NOT IN terminal)
        // et l'UPDATE sont atomiques — évite qu'un cancel concurrent ne double-écrive.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        // Relit le payload pour synchroniser le statut
        // F-41 — Conflict ajouté aux états terminaux : un job déjà Conflict (optimistic-lock
        // périmé, conflict_payload requis pour la résolution RMW) ne doit PAS être écrasé
        // en Cancelled, ce qui détruirait le payload que l'appelant attend pour retry/abandon.
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ? AND status NOT IN ('Done', 'DLQ', 'Cancelled', 'Conflict')"#)
            .bind(&id_str)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let Some(row) = row else {
            // Job déjà terminal ou inexistant — opération idempotente
            tx.rollback()
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            return Ok(());
        };

        let payload_str: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let mut record = Self::deserialize_record(&payload_str)?;
        record.lifecycle.status = JobStatus::Cancelled;
        record.lifecycle.completed_at = Some(now);
        let new_payload = Self::serialize_record(&record)?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Cancelled',
                completed_at = ?,
                lease_until = NULL,
                payload = ?
            WHERE id = ?
              AND status NOT IN ('Done', 'DLQ', 'Cancelled', 'Conflict')
            "#,
        )
        .bind(&now_str)
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        self.publish(QueueEvent::JobCancelled(id));
        Ok(())
    }

    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();
        let err_truncated = if err.len() > 2048 { &err[..2048] } else { err };

        // Relit le payload pour mettre à jour le statut DLQ
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ?"#)
            .bind(&id_str)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?
            .ok_or(QueueError::NotFound(id))?;

        let payload_str: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let mut record = Self::deserialize_record(&payload_str)?;
        record.lifecycle.status = JobStatus::DLQ;
        record.lifecycle.completed_at = Some(Utc::now());
        record.retry.last_error = Some(err_truncated.to_string());
        let new_payload = Self::serialize_record(&record)?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'DLQ',
                completed_at = ?,
                lease_until = NULL,
                last_error = ?,
                payload = ?
            WHERE id = ?
            "#,
        )
        .bind(&now)
        .bind(err_truncated)
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        warn!(
            job_id = %id,
            "job envoyé en DLQ : {err_truncated}"
        );
        Ok(())
    }

    async fn find_awaiting(&self, _job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        // Différé F-14 (DAG await_jobs + cascade engine) — non implémenté en v0.4.x.
        // Requis : table gradatum_job_deps + cascade engine (v0.5+).
        Err(QueueError::NotImplemented {
            method: "find_awaiting",
        })
    }

    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        // Différé F-14 (cascade Waiting → Pending) — non implémenté en v0.4.x.
        // Dépend de find_awaiting (cascade engine).
        Err(QueueError::NotImplemented {
            method: "set_pending",
        })
    }

    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        // Les jobs Running dont le lease_until est dépassé depuis > ttl.
        // Si le TTL est hors plage (> i64::MAX nanosecondes), on skip plutôt que
        // d'utiliser unwrap_or_default() qui retournerait Duration::ZERO et marquerait
        // TOUS les jobs Running comme stale — corruption catastrophique de la queue.
        let chrono_ttl = match chrono::Duration::from_std(ttl) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    ttl_secs = ttl.as_secs(),
                    error = %e,
                    "recover_stale_leases: TTL invalide (hors plage chrono), skip pour éviter mass-recovery"
                );
                return Ok(vec![]);
            }
        };
        let threshold = (Utc::now() - chrono_ttl).to_rfc3339();

        let rows = sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Pending',
                lease_until = NULL,
                scheduled_at = ?
            WHERE status = 'Running'
              AND lease_until < ?
            RETURNING id
            "#,
        )
        .bind(Utc::now().to_rfc3339())
        .bind(&threshold)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        let ids: Vec<Ulid> = rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<String, _>("id")
                    .ok()
                    .and_then(|s| s.parse::<Ulid>().ok())
            })
            .collect();

        if !ids.is_empty() {
            debug!(
                count = ids.len(),
                "SqliteQueueStore: leases expirés récupérés"
            );
        }
        Ok(ids)
    }

    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        let now_str = now.to_rfc3339();
        let completed_at = now.to_rfc3339();

        let rows = sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Cancelled',
                completed_at = ?
            WHERE deadline IS NOT NULL
              AND deadline < ?
              AND status NOT IN ('Done', 'DLQ', 'Cancelled', 'Conflict')
            RETURNING id
            "#,
        )
        .bind(&completed_at)
        .bind(&now_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        let ids: Vec<Ulid> = rows
            .into_iter()
            .filter_map(|row| {
                row.try_get::<String, _>("id")
                    .ok()
                    .and_then(|s| s.parse::<Ulid>().ok())
            })
            .collect();

        for &id in &ids {
            self.publish(QueueEvent::JobCancelled(id));
        }
        Ok(ids)
    }

    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        let now_str = now.to_rfc3339();

        // Sélectionne les jobs Failed dont scheduled_at <= now.
        // IMPORTANT : on sélectionne aussi `attempt_count` (colonne SQL autoritaire)
        // pour la garde DLQ. Le BLOB `payload` contient retry.count stale (valeur
        // au moment de l'enqueue, non mis à jour après chaque tentative) — utiliser
        // uniquement le BLOB ferait échouer la garde v67 (0 >= 3 = faux → loop infinie).
        let rows = sqlx::query(
            r#"
            SELECT id, payload, attempt_count
            FROM gradatum_jobs
            WHERE status = 'Failed'
              AND scheduled_at <= ?
            "#,
        )
        .bind(&now_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        let mut promoted = Vec::new();
        for row in rows {
            let id_str: String = row
                .try_get("id")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            let payload_str: String = row
                .try_get("payload")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            // Colonne SQL autoritaire — surcharge le BLOB stale pour la garde DLQ.
            let sql_attempt_count: i64 = row
                .try_get("attempt_count")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            let mut record = Self::deserialize_record(&payload_str)?;
            // Synchroniser retry.count depuis SQL avant la garde v67.
            record.retry.count = sql_attempt_count as u32;
            let id = record.id;

            // Garde v67 : si retry.count >= retry.max → fail_dlq au lieu de re-Pending.
            // retry.count est maintenant synchronisé depuis attempt_count SQL (colonne
            // autoritaire), pas depuis le BLOB stale.
            if record.retry.count >= record.retry.max {
                let err = format!(
                    "max_retries atteint ({} / {})",
                    record.retry.count, record.retry.max
                );
                self.fail_dlq(id, &err).await?;
            } else {
                sqlx::query(
                    r#"
                    UPDATE gradatum_jobs
                    SET status = 'Pending',
                        scheduled_at = ?
                    WHERE id = ?
                      AND status = 'Failed'
                    "#,
                )
                .bind(now.to_rfc3339())
                .bind(&id_str)
                .execute(&self.pool)
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?;

                promoted.push(id);
            }
        }
        Ok(promoted)
    }

    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError> {
        let id_str = id.to_string();

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Failed',
                lease_until = NULL,
                scheduled_at = ?
            WHERE id = ?
              AND status = 'Running'
            "#,
        )
        .bind(at.to_rfc3339())
        .bind(&id_str)
        .execute(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        Ok(())
    }

    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        // Phase 3 — fix E-10 : filtre `kind` natif SQL via colonne dénormalisée (migration 007).
        // La requête applique maintenant tous les filtres en SQL avec indexes — plus de filtre mémoire.
        let limit = filter.limit.clamp(1, 500) as i64;

        // Option<String> bindée comme NULL SQL désactive le filtre correspondant.
        let class_filter = filter.class.as_ref().map(|c| format!("{c:?}"));
        let status_filter = filter
            .status
            .as_ref()
            .map(|s| Self::status_to_str(s).to_string());
        let created_after = filter.created_after.as_ref().map(|d| d.to_rfc3339());
        let created_before = filter.created_before.as_ref().map(|d| d.to_rfc3339());
        // Cursor-based pagination : direction du curseur dépend de l'ordre.
        // ASC  → WHERE id > cursor ORDER BY id ASC  (comportement historique).
        // DESC → WHERE id < cursor ORDER BY id DESC (page la plus récente d'abord).
        let cursor_filter = filter.cursor.as_ref().map(|c| c.to_string());

        // Deux requêtes statiques à arité de bind IDENTIQUE — pas de SQL combinatoire.
        // Le clamp limit + filtres NULL-able restent communs.
        let query_str = match filter.order {
            JobOrder::CreatedAsc => {
                r#"
            SELECT payload
            FROM gradatum_jobs
            WHERE (? IS NULL OR class = ?)
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR kind = ?)
              AND (? IS NULL OR created_at > ?)
              AND (? IS NULL OR created_at < ?)
              AND (? IS NULL OR id > ?)
            ORDER BY id ASC
            LIMIT ?
            "#
            }
            JobOrder::CreatedDesc => {
                r#"
            SELECT payload
            FROM gradatum_jobs
            WHERE (? IS NULL OR class = ?)
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR kind = ?)
              AND (? IS NULL OR created_at > ?)
              AND (? IS NULL OR created_at < ?)
              AND (? IS NULL OR id < ?)
            ORDER BY id DESC
            LIMIT ?
            "#
            }
        };

        let rows = sqlx::query(query_str)
            .bind(&class_filter)
            .bind(&class_filter)
            .bind(&status_filter)
            .bind(&status_filter)
            .bind(&filter.kind)
            .bind(&filter.kind)
            .bind(&created_after)
            .bind(&created_after)
            .bind(&created_before)
            .bind(&created_before)
            .bind(&cursor_filter)
            .bind(&cursor_filter)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let payload: String = row
                .try_get("payload")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            let record = Self::deserialize_record(&payload)?;
            records.push(record);
        }
        Ok(records)
    }

    /// Counts jobs by status using a native `GROUP BY status` query.
    ///
    /// Performs a single query instead of N separate count queries. Unknown statuses
    /// in the column (data anomalies) are silently ignored (logged at `warn` level) —
    /// the dashboard remains tolerant. DLQ is included.
    async fn count_jobs_by_status(
        &self,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        let rows = sqlx::query("SELECT status, COUNT(*) AS n FROM gradatum_jobs GROUP BY status")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let mut out = std::collections::HashMap::new();
        for row in rows {
            let status_str: String = row
                .try_get("status")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            let n: i64 = row
                .try_get("n")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            match Self::str_to_status(&status_str) {
                Ok(st) => {
                    out.insert(st, u64::try_from(n).unwrap_or(0));
                }
                Err(_) => {
                    warn!(
                        status = %status_str,
                        "count_jobs_by_status: statut SQL hors-enum ignoré"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Permanently deletes DLQ jobs (`DELETE WHERE status = 'DLQ'`).
    ///
    /// If `older_than = Some(cutoff)`, deletes only DLQ jobs created before
    /// `cutoff`. The `created_at` column is stored as RFC3339 (`to_rfc3339()`), so
    /// a lexicographic comparison `created_at < ?` is correct as long as the format
    /// is consistent (constant `Z` offset). `cutoff.to_rfc3339()` is passed as a
    /// bound parameter.
    ///
    /// Returns the number of rows actually deleted.
    async fn delete_dlq_jobs(&self, older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        let affected = match older_than {
            Some(cutoff) => {
                sqlx::query(r#"DELETE FROM gradatum_jobs WHERE status = 'DLQ' AND created_at < ?"#)
                    .bind(cutoff.to_rfc3339())
                    .execute(&self.pool)
                    .await
                    .map_err(|e| QueueError::Storage(e.to_string()))?
                    .rows_affected()
            }
            None => sqlx::query(r#"DELETE FROM gradatum_jobs WHERE status = 'DLQ'"#)
                .execute(&self.pool)
                .await
                .map_err(|e| QueueError::Storage(e.to_string()))?
                .rows_affected(),
        };
        Ok(affected)
    }

    /// Returns a `COUNT(*)` of targeted DLQ jobs, using the same `WHERE` clause as
    /// `delete_dlq_jobs` (faithful dry-run, no `LIMIT` cap).
    async fn count_dlq_jobs(&self, older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        let n: i64 = match older_than {
            Some(cutoff) => sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM gradatum_jobs WHERE status = 'DLQ' AND created_at < ?"#,
            )
            .bind(cutoff.to_rfc3339())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?,
            None => {
                sqlx::query_scalar(r#"SELECT COUNT(*) FROM gradatum_jobs WHERE status = 'DLQ'"#)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| QueueError::Storage(e.to_string()))?
            }
        };
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Returns the **most recently created** job via `ORDER BY id DESC LIMIT 1`.
    ///
    /// `gradatum_jobs` has no tenant column: this store is single-tenant
    /// (the legacy `tenant_id` lived on `jobs_v2`, which was drained in migration 009).
    /// The `tenant` parameter is therefore ignored, in accordance with the trait contract.
    ///
    /// Because the ULID `id` is monotonic, `ORDER BY id DESC` correctly returns the
    /// most recently created job — unlike `list()`, which orders by `id ASC` for pagination.
    async fn latest_job(&self, _tenant: &str) -> Result<Option<JobRecord>, QueueError> {
        let row = sqlx::query("SELECT payload FROM gradatum_jobs ORDER BY id DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        match row {
            Some(row) => {
                let payload: String = row
                    .try_get("payload")
                    .map_err(|e| QueueError::Storage(e.to_string()))?;
                Ok(Some(Self::deserialize_record(&payload)?))
            }
            None => Ok(None),
        }
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        self.tx.subscribe()
    }

    /// Marks a job as terminal `Conflict` (optimistic-lock).
    ///
    /// Writes `status = 'Conflict'` to the SQL column and sets `lifecycle.status =
    /// Conflict` in the JSON payload. `conflict_payload` carries the `WriteConflictDto`
    /// JSON in `lifecycle.result.conflict_payload`.
    /// No retry is scheduled (terminal state).
    async fn mark_conflict(
        &self,
        id: Ulid,
        result_note_md: String,
        duration_ms: u32,
    ) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();

        // BEGIN IMMEDIATE : lecture + marquage Conflict atomiques — évite qu'un
        // complete() concurrent ne masque le conflit en marquant Done entre le
        // SELECT et l'UPDATE.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        // Relire le payload pour le patcher.
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ?"#)
            .bind(&id_str)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?
            .ok_or(QueueError::NotFound(id))?;

        let payload_str: String = row
            .try_get("payload")
            .map_err(|e| QueueError::Storage(e.to_string()))?;
        let mut record = Self::deserialize_record(&payload_str)?;

        // Parser le result_note_md comme JSON pour le stocker dans conflict_payload.
        let conflict_payload_value: Option<serde_json::Value> =
            serde_json::from_str(&result_note_md).ok();

        // Patcher le lifecycle : Conflict terminal, résultat avec conflict_payload.
        record.lifecycle.status = JobStatus::Conflict;
        record.lifecycle.completed_at = Some(Utc::now());
        record.lifecycle.result = Some(JobResult {
            success: false,
            duration_ms,
            cost_usd: None,
            result_note: None,
            conflict_payload: conflict_payload_value,
        });
        let new_payload = Self::serialize_record(&record)?;

        sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status = 'Conflict',
                completed_at = ?,
                lease_until = NULL,
                last_error = ?,
                payload = ?
            WHERE id = ?
            "#,
        )
        .bind(&now)
        .bind(result_note_md.chars().take(256).collect::<String>())
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&mut *tx)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let job_result = JobResult {
            success: false,
            duration_ms,
            cost_usd: None,
            result_note: None,
            conflict_payload: None, // Ne pas inclure dans l'événement broadcast
        };
        self.publish(QueueEvent::JobCompleted(
            id,
            JobStatus::Conflict,
            job_result,
        ));

        tracing::info!(
            job_id = %id,
            "job marqué Conflict (optimistic-lock F-41)"
        );
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers privés — types non exportés
// ─────────────────────────────────────────────────────────────────────────────

/// Applies WAL pragmas on a freshly opened SQLite connection.
///
/// Must be called after `SqlitePool::connect` and before any operation.
///
/// # Side effects
///
/// - Enables WAL mode (concurrent write performance)
/// - Sets `synchronous=NORMAL` (durability/performance trade-off)
/// - Sets `foreign_keys=ON`
pub async fn apply_sqlite_pragmas(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query("PRAGMA journal_mode=WAL;")
        .execute(pool)
        .await?;
    sqlx::query("PRAGMA synchronous=NORMAL;")
        .execute(pool)
        .await?;
    sqlx::query("PRAGMA foreign_keys=ON;").execute(pool).await?;
    Ok(())
}

/// Applies migrations from the `migrations/` directory.
///
/// Wrapper around `sqlx::migrate!` with a fixed migration path.
/// Call during pool initialization in `gradatum-worker`.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

// ─────────────────────────────────────────────────────────────────────────────
// IdempotencyStore — table gradatum_idempotency (migration 008, F-16)
// ─────────────────────────────────────────────────────────────────────────────

/// Inserts a `(key, job_id)` pair into the idempotency table.
///
/// Uses `INSERT OR IGNORE` — atomic, no TOCTOU.
/// Returns `true` if the key was inserted (new job), `false` if it already existed.
///
/// # Side effects
///
/// - Writes to `gradatum_idempotency`.
/// - If the key already exists: no-op (`INSERT OR IGNORE`).
pub async fn idempotency_insert(
    pool: &SqlitePool,
    key: &str,
    job_id: &str,
) -> Result<bool, QueueError> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rows_affected = sqlx::query(
        r#"INSERT OR IGNORE INTO gradatum_idempotency (key, job_id, created_at) VALUES (?, ?, ?)"#,
    )
    .bind(key)
    .bind(job_id)
    .bind(now_ms)
    .execute(pool)
    .await
    .map_err(|e| QueueError::Storage(e.to_string()))?
    .rows_affected();

    Ok(rows_affected > 0)
}

/// Looks up a `job_id` by idempotency key.
///
/// Returns `Some(job_id)` if the key exists, `None` otherwise.
pub async fn idempotency_lookup(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<String>, QueueError> {
    let row = sqlx::query(r#"SELECT job_id FROM gradatum_idempotency WHERE key = ?"#)
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

    match row {
        None => Ok(None),
        Some(r) => {
            let job_id: String = r
                .try_get("job_id")
                .map_err(|e| QueueError::Storage(e.to_string()))?;
            Ok(Some(job_id))
        }
    }
}

/// Deletes idempotency entries whose `created_at` is earlier than `before_ms`.
///
/// Used by the `IdempotencyCleanup` cron job (24-hour TTL).
/// Returns the number of deleted entries.
pub async fn idempotency_cleanup(pool: &SqlitePool, before_ms: i64) -> Result<u64, QueueError> {
    let rows_deleted = sqlx::query(r#"DELETE FROM gradatum_idempotency WHERE created_at < ?"#)
        .bind(before_ms)
        .execute(pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?
        .rows_affected();

    Ok(rows_deleted)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests d'intégration
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        job_kind_str, CurateSpec, EmbedSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode,
        JobPriority, JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus,
        RetryBackoff, TriggerSource,
    };
    use sqlx::SqlitePool;
    use ulid::Ulid;

    /// Crée un pool SQLite in-memory pour les tests.
    ///
    /// Applique les migrations 006 + 007 + 008 + 009 + 010 directement via `include_str!`
    /// (sqlx migrate! requiert les fichiers sur disque lors de la macro expansion,
    /// pas disponible dans les tests in-memory).
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool in-memory doit créer");
        apply_sqlite_pragmas(&pool)
            .await
            .expect("pragmas WAL doivent s'appliquer");
        sqlx::query(include_str!("../migrations/006_apalis_bootstrap.sql"))
            .execute(&pool)
            .await
            .expect("migration 006 doit s'appliquer");
        sqlx::query(include_str!("../migrations/007_jobs_kind_indexed.sql"))
            .execute(&pool)
            .await
            .expect("migration 007 doit s'appliquer");
        sqlx::query(include_str!("../migrations/008_idempotency.sql"))
            .execute(&pool)
            .await
            .expect("migration 008 doit s'appliquer");
        // Migration 009 : drain jobs_v2 pending → failed (Phase 1.2 bridge).
        // jobs_v2 n'existe pas en in-memory → ignorer l'erreur "no such table".
        let _ = sqlx::query(include_str!("../migrations/009_jobs_v2_drain.sql"))
            .execute(&pool)
            .await;
        // Migration 010 : backfill colonne `kind` pour les jobs existants.
        sqlx::query(include_str!("../migrations/010_backfill_kind.sql"))
            .execute(&pool)
            .await
            .expect("migration 010 doit s'appliquer");
        pool
    }

    fn make_record(job: Job, class: JobClass, status: JobStatus) -> JobRecord {
        let now = Utc::now();
        JobRecord {
            id: Ulid::new(),
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
            retry: JobRetry {
                count: 0,
                max: 3,
                backoff: RetryBackoff::Exponential { base: 5, max: 120 },
                last_error: None,
                errors: vec![],
            },
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

    #[tokio::test]
    async fn enqueue_and_get() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;

        let inserted_id = store.enqueue(record).await.expect("enqueue doit réussir");
        assert_eq!(inserted_id, id);

        let fetched = store.get(id).await.expect("get doit réussir");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id, id);
    }

    #[tokio::test]
    async fn dequeue_returns_highest_priority() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Insère un job System (Low=1) puis un Agent (High=3)
        let low = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let high = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let high_id = high.id;

        store.enqueue(low).await.expect("enqueue low doit réussir");
        store
            .enqueue(high)
            .await
            .expect("enqueue high doit réussir");

        let dequeued = store
            .dequeue()
            .await
            .expect("dequeue doit réussir")
            .expect("doit retourner un job");

        // Le job Agent (High) doit passer en premier
        assert_eq!(dequeued.id, high_id);
        assert_eq!(dequeued.lifecycle.status, JobStatus::Running);
    }

    #[tokio::test]
    async fn complete_job_sets_done() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue().await.expect("dequeue doit réussir");

        let result = JobResult {
            success: true,
            duration_ms: 150,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        store
            .complete(id, result)
            .await
            .expect("complete doit réussir");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Done);
    }

    /// F-41 — `complete()` ne doit PAS écraser un job déjà en état terminal `Conflict`.
    ///
    /// Reproduit le seam LIVE : `mark_conflict` pose `Conflict`, puis l'ack apalis
    /// (qui voit le `Ok` du handler) appelle `complete()`. Sans la garde anti-clobber,
    /// le status finirait `Done` → l'appelant RMW ne peut plus détecter le conflit.
    #[tokio::test]
    async fn complete_preserves_terminal_conflict() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue().await.expect("dequeue doit réussir");

        // Le worker marque le job Conflict (optimistic-lock périmé).
        let conflict_payload = serde_json::json!({
            "current_sha256": "aa".repeat(32),
            "attempted_sha256": "bb".repeat(32),
        })
        .to_string();
        store
            .mark_conflict(id, conflict_payload, 12)
            .await
            .expect("mark_conflict doit réussir");

        let after_conflict = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            after_conflict.lifecycle.status,
            JobStatus::Conflict,
            "précondition : le job doit être Conflict après mark_conflict"
        );

        // L'ack apalis appelle complete() avec un JobResult succès (le handler a
        // retourné Ok). La garde anti-clobber doit préserver Conflict.
        let ack_result = JobResult {
            success: true,
            duration_ms: 0,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        store
            .complete(id, ack_result)
            .await
            .expect("complete (ack) doit réussir sans erreur");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Conflict,
            "complete() ne doit PAS écraser un état terminal Conflict par Done (F-41)"
        );
        // Le conflict_payload posé par mark_conflict doit survivre.
        let result = fetched.lifecycle.result.expect("result doit être présent");
        assert!(
            result.conflict_payload.is_some(),
            "le conflict_payload doit survivre au complete() ignoré"
        );
    }

    #[tokio::test]
    async fn fail_and_dlq() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let mut record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        record.retry.max = 1; // max 1 retry pour le test
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue().await.expect("dequeue doit réussir");

        // Premier fail
        store
            .fail(id, "erreur test", 1)
            .await
            .expect("fail doit réussir");

        // promote_retries avec retry.count=1 >= retry.max=1 → DLQ
        store
            .schedule_retry(id, Utc::now() - chrono::Duration::seconds(1))
            .await
            .expect("schedule_retry doit réussir");

        // On force manuellement le fail_dlq pour valider le comportement
        store
            .fail_dlq(id, "max_retries atteint (1 / 1)")
            .await
            .expect("fail_dlq doit réussir");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::DLQ);
    }

    #[tokio::test]
    async fn cancel_job() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Audit, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        store.cancel(id).await.expect("cancel doit réussir");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn list_with_filter() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Insère 2 jobs Agent et 1 job System
        for _ in 0..2 {
            let r = make_record(
                Job::Curate(CurateSpec {
                    note_id: Ulid::new(),
                    tenant_id: "main".to_string(),
                    ..Default::default()
                }),
                JobClass::Agent,
                JobStatus::Pending,
            );
            store.enqueue(r).await.expect("enqueue doit réussir");
        }
        let sys = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        store.enqueue(sys).await.expect("enqueue doit réussir");

        // Filtre par Agent
        let filter = JobFilter {
            class: Some(JobClass::Agent),
            limit: 50,
            ..Default::default()
        };
        let results = store.list(filter).await.expect("list doit réussir");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.spec.class == JobClass::Agent));
    }

    /// Construit un `JobRecord` Backup dont l'`id` ULID et le `created_at` dérivent
    /// de `dt` — id monotone corrélé à la date (testable ASC/DESC + plages).
    fn make_record_at(dt: chrono::DateTime<Utc>) -> JobRecord {
        let mut r = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        r.id = Ulid::from_datetime(dt.into());
        r.lifecycle.created_at = dt;
        r
    }

    /// Enqueue 4 jobs à T+0,1,2,3 et retourne leurs ids dans l'ordre chronologique.
    async fn seed_four(store: &SqliteQueueStore) -> Vec<Ulid> {
        let base = Utc::now() - chrono::Duration::hours(1);
        let mut ids = Vec::with_capacity(4);
        for i in 0..4 {
            let r = make_record_at(base + chrono::Duration::minutes(i));
            ids.push(r.id);
            store.enqueue(r).await.expect("enqueue doit réussir");
        }
        ids
    }

    #[tokio::test]
    async fn list_order_desc_returns_newest_first() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let ids = seed_four(&store).await; // chrono : ids[0] plus ancien → ids[3] plus récent

        let filter = JobFilter {
            order: JobOrder::CreatedDesc,
            limit: 50,
            ..Default::default()
        };
        let results = store.list(filter).await.expect("list doit réussir");
        let got: Vec<Ulid> = results.iter().map(|r| r.id).collect();
        assert_eq!(
            got,
            vec![ids[3], ids[2], ids[1], ids[0]],
            "DESC = newest first"
        );
    }

    #[tokio::test]
    async fn list_order_asc_unchanged() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let ids = seed_four(&store).await;

        // Défaut = ASC (non-régression).
        let filter = JobFilter {
            limit: 50,
            ..Default::default()
        };
        let results = store.list(filter).await.expect("list doit réussir");
        let got: Vec<Ulid> = results.iter().map(|r| r.id).collect();
        assert_eq!(
            got,
            vec![ids[0], ids[1], ids[2], ids[3]],
            "ASC = oldest first"
        );
    }

    #[tokio::test]
    async fn list_desc_pagination_no_gap_no_dup() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let ids = seed_four(&store).await;

        // Page 1 DESC, limit 2 → [3, 2]
        let page1 = store
            .list(JobFilter {
                order: JobOrder::CreatedDesc,
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("list page1 doit réussir");
        let p1: Vec<Ulid> = page1.iter().map(|r| r.id).collect();
        assert_eq!(p1, vec![ids[3], ids[2]]);

        // Page 2 DESC via cursor = dernier id de page1 → [1, 0]
        let cursor = *p1.last().expect("page1 non vide");
        let page2 = store
            .list(JobFilter {
                order: JobOrder::CreatedDesc,
                limit: 2,
                cursor: Some(cursor),
                ..Default::default()
            })
            .await
            .expect("list page2 doit réussir");
        let p2: Vec<Ulid> = page2.iter().map(|r| r.id).collect();
        assert_eq!(p2, vec![ids[1], ids[0]]);

        // Union des deux pages = les 4 jobs, sans doublon ni trou.
        let mut all = p1;
        all.extend(p2);
        assert_eq!(all.len(), 4);
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), 4, "aucun doublon entre les pages DESC");
    }

    #[tokio::test]
    async fn list_created_range_isolates_window() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let ids = seed_four(&store).await; // T+0,1,2,3 minutes

        // Bornes : after = id[0].created_at, before = id[3].created_at (exclusives)
        // → ne capte que id[1] et id[2].
        let r0 = store.get(ids[0]).await.expect("get").expect("existe");
        let r3 = store.get(ids[3]).await.expect("get").expect("existe");

        let filter = JobFilter {
            created_after: Some(r0.lifecycle.created_at),
            created_before: Some(r3.lifecycle.created_at),
            limit: 50,
            ..Default::default()
        };
        let results = store.list(filter).await.expect("list doit réussir");
        let got: Vec<Ulid> = results.iter().map(|r| r.id).collect();
        assert_eq!(
            got,
            vec![ids[1], ids[2]],
            "plage exclusive isole l'intérieur"
        );
    }

    #[tokio::test]
    async fn broadcast_events_received() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let mut rx = store.subscribe();

        let record = make_record(Job::Consolidate, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        let event = rx.try_recv().expect("doit recevoir JobInserted");
        assert!(matches!(event, QueueEvent::JobInserted(eid) if eid == id));
    }

    #[tokio::test]
    async fn recover_stale_leases_restores_pending() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue().await.expect("dequeue doit réussir");

        // Simuler lease expirée en patchant directement
        sqlx::query("UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id.to_string())
            .execute(&store.pool)
            .await
            .expect("patch lease doit réussir");

        // TTL de 0 — tout lease expiré est récupéré
        let recovered = store
            .recover_stale_leases(Duration::from_secs(0))
            .await
            .expect("recover doit réussir");

        assert!(recovered.contains(&id));

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Pending);
    }

    // ── Tests régression E-12 — get() synchronise le statut SQL ─────────────

    /// Régression E-12 — enqueue → get : statut doit être Pending.
    #[tokio::test]
    async fn e12_get_after_enqueue_is_pending() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "get() après enqueue doit retourner Pending"
        );
    }

    /// Régression E-12 — enqueue → dequeue → get : statut doit être Running, pas Pending stale.
    #[tokio::test]
    async fn e12_get_after_dequeue_is_running() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        let dequeued = store
            .dequeue()
            .await
            .expect("dequeue doit réussir")
            .expect("doit retourner un job");
        assert_eq!(dequeued.lifecycle.status, JobStatus::Running);

        // C'est ici que le bug E-12 se manifestait : get() retournait Pending stale.
        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Running,
            "get() après dequeue DOIT retourner Running (fix E-12)"
        );
        assert_eq!(
            fetched.retry.count, 1,
            "attempt_count doit être synchronisé depuis SQL"
        );
    }

    /// Régression E-12 — enqueue → dequeue → complete → get : statut doit être Done.
    #[tokio::test]
    async fn e12_get_after_complete_is_done() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue().await.expect("dequeue doit réussir");

        let result = JobResult {
            success: true,
            duration_ms: 100,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        store
            .complete(id, result)
            .await
            .expect("complete doit réussir");

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Done,
            "get() après complete doit retourner Done"
        );
        assert!(
            fetched.lifecycle.completed_at.is_some(),
            "completed_at doit être réhydraté depuis SQL"
        );
    }

    // ── Tests fix routing DLQ (bug dequeue_by_kind) ──────────────────────────

    /// Test d'isolation par kind — cœur du fix routing DLQ.
    ///
    /// Enqueue 1 Curate + 1 Embed → `dequeue_by_kind("Curate")` ne rend QUE le Curate,
    /// jamais l'Embed. Même isolement dans l'autre sens.
    /// C'est LE test qui prouve que le bug de routing est résolu.
    #[tokio::test]
    async fn dequeue_by_kind_isolates_curate_from_embed() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let curate_record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let embed_record = make_record(
            Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "main".to_string(),
                force_regenerate: false,
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let curate_id = curate_record.id;
        let embed_id = embed_record.id;

        store.enqueue(curate_record).await.expect("enqueue Curate");
        store.enqueue(embed_record).await.expect("enqueue Embed");

        // Un worker curate ne doit JAMAIS recevoir un job Embed
        let got = store
            .dequeue_by_kind("Curate")
            .await
            .expect("dequeue_by_kind Curate doit réussir")
            .expect("doit retourner un job");
        assert_eq!(
            got.id, curate_id,
            "dequeue_by_kind(Curate) doit retourner le job Curate, pas l'Embed"
        );
        assert!(
            matches!(got.spec.kind, Job::Curate(_)),
            "le job retourné doit être un Curate"
        );

        // Le job Embed est encore Pending — un worker embed peut le prendre
        let got_embed = store
            .dequeue_by_kind("Embed")
            .await
            .expect("dequeue_by_kind Embed doit réussir")
            .expect("le job Embed doit être disponible");
        assert_eq!(
            got_embed.id, embed_id,
            "dequeue_by_kind(Embed) doit retourner le job Embed"
        );
        assert!(
            matches!(got_embed.spec.kind, Job::Embed(_)),
            "le job retourné doit être un Embed"
        );
    }

    /// Symétrique : `dequeue_by_kind("Embed")` ne prend pas un Curate, même si c'est
    /// le seul job disponible et qu'il est prioritaire.
    #[tokio::test]
    async fn dequeue_by_kind_embed_worker_cannot_steal_curate() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Enqueue uniquement un Curate
        let curate_record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        store.enqueue(curate_record).await.expect("enqueue Curate");

        // Un worker embed ne doit rien trouver
        let got = store
            .dequeue_by_kind("Embed")
            .await
            .expect("dequeue_by_kind Embed doit réussir");
        assert!(
            got.is_none(),
            "dequeue_by_kind(Embed) ne doit PAS retourner un job Curate"
        );
    }

    /// `enqueue()` persiste la colonne `kind` avec la valeur correcte — régression du bug racine.
    /// Sans ce test, un enqueue sans `kind` ferait silencieusement échouer le routing.
    #[tokio::test]
    async fn enqueue_persists_kind_column() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let curate_record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = curate_record.id.to_string();
        store.enqueue(curate_record).await.expect("enqueue");

        // Lire la colonne `kind` directement depuis SQLite
        let row = sqlx::query("SELECT kind FROM gradatum_jobs WHERE id = ?")
            .bind(&id)
            .fetch_one(&store.pool)
            .await
            .expect("row doit exister");
        let kind: String = row.try_get("kind").expect("colonne kind doit exister");
        assert_eq!(
            kind, "Curate",
            "la colonne kind doit valoir 'Curate' après enqueue d'un Job::Curate"
        );
    }

    /// Vérifie que la migration 010 backfille `kind` correctement depuis un payload réaliste.
    /// Simule les 41 jobs prod avec kind='' qui ont été enqueués sans la colonne.
    #[tokio::test]
    async fn migration_010_backfills_kind_from_payload() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Insérer un job avec kind='' manuellement (simule l'état pré-fix)
        let record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id_str = record.id.to_string();
        let payload =
            SqliteQueueStore::serialize_record(&record).expect("sérialisation doit réussir");
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO gradatum_jobs (id, payload, status, priority, class, kind, created_at, scheduled_at) VALUES (?, ?, 'Pending', 3, 'Agent', '', ?, ?)"
        )
        .bind(&id_str)
        .bind(&payload)
        .bind(&now)
        .bind(&now)
        .execute(&store.pool)
        .await
        .expect("insert manuel doit réussir");

        // Vérifier que kind est bien '' avant le backfill
        let row = sqlx::query("SELECT kind FROM gradatum_jobs WHERE id = ?")
            .bind(&id_str)
            .fetch_one(&store.pool)
            .await
            .expect("row doit exister");
        let kind_before: String = row.try_get("kind").expect("colonne kind");
        assert_eq!(kind_before, "", "kind doit être vide avant backfill");

        // Appliquer la migration 010
        sqlx::query(include_str!("../migrations/010_backfill_kind.sql"))
            .execute(&store.pool)
            .await
            .expect("migration 010 doit s'appliquer");

        // Vérifier que kind est maintenant rempli
        let row = sqlx::query("SELECT kind FROM gradatum_jobs WHERE id = ?")
            .bind(&id_str)
            .fetch_one(&store.pool)
            .await
            .expect("row doit exister");
        let kind_after: String = row.try_get("kind").expect("colonne kind");
        assert_eq!(
            kind_after, "Curate",
            "migration 010 doit backfiller kind='Curate' depuis le payload JSON"
        );
    }

    /// `job_kind_str` couvre tous les variants de `Job` sans wildcard.
    ///
    /// L'exhaustivité du `match` est imposée par le compilateur (pas de `_ =>`).
    /// Ce test vérifie les valeurs retournées pour les variants facilement constructibles
    /// et garantit la correspondance avec le payload JSON (`serde(tag = "type")`).
    #[test]
    fn job_kind_str_covers_all_variants() {
        use gradatum_core::ReIndexMode;

        // Variants unitaires
        assert_eq!(job_kind_str(&Job::Agent), "Agent");
        assert_eq!(job_kind_str(&Job::Pipeline), "Pipeline");
        assert_eq!(job_kind_str(&Job::Collect), "Collect");
        assert_eq!(
            job_kind_str(&Job::Distill(gradatum_core::DistillSource::default())),
            "Distill"
        );
        assert_eq!(job_kind_str(&Job::Backup), "Backup");
        assert_eq!(
            job_kind_str(&Job::Purge(gradatum_core::PurgeSpec::default())),
            "Purge"
        );
        assert_eq!(job_kind_str(&Job::Summarize), "Summarize");
        assert_eq!(job_kind_str(&Job::Validate), "Validate");
        assert_eq!(job_kind_str(&Job::Audit), "Audit");
        assert_eq!(job_kind_str(&Job::Consolidate), "Consolidate");
        assert_eq!(
            job_kind_str(&Job::Forget(gradatum_core::ForgetSpec::default())),
            "Forget"
        );
        assert_eq!(job_kind_str(&Job::Review), "Review");
        assert_eq!(job_kind_str(&Job::Classify), "Classify");
        assert_eq!(job_kind_str(&Job::Merge), "Merge");
        assert_eq!(job_kind_str(&Job::Annotate), "Annotate");

        // Variants avec données
        assert_eq!(job_kind_str(&Job::ReIndex(ReIndexMode::FtsOnly)), "ReIndex");
        assert_eq!(job_kind_str(&Job::Curate(CurateSpec::default())), "Curate");
        assert_eq!(
            job_kind_str(&Job::Embed(EmbedSpec {
                note_id: Ulid::new(),
                tenant_id: "t".into(),
                force_regenerate: false,
            })),
            "Embed"
        );
        // Migrate, Export, Notify, Ingest : construits inline pour vérifier le routing.
        // Le compilateur garantit l'exhaustivité via le match sans `_ =>` dans job_kind_str.
        assert_eq!(
            job_kind_str(&Job::Migrate(gradatum_core::MigrateSource {
                from_path: String::new(),
                mode: gradatum_core::MigrateMode::RawMarkdown,
                conflict: gradatum_core::ConflictStrategy::Skip,
                dry_run: true,
                target: gradatum_core::VaultScope::VaultWide,
            })),
            "Migrate"
        );
        assert_eq!(
            job_kind_str(&Job::Export(gradatum_core::ExportSource {
                scope: gradatum_core::VaultScope::VaultWide,
                filter: None,
                format: gradatum_core::ExportFormat::Json,
                target: String::new(),
                template: None,
            })),
            "Export"
        );
        assert_eq!(
            job_kind_str(&Job::Notify(gradatum_core::NotifySource {
                channel: gradatum_core::NotifyChannel::Nats {
                    subject: "gradatum.events".into(),
                },
                template: String::new(),
                job_ref: None,
            })),
            "Notify"
        );
        assert_eq!(
            job_kind_str(&Job::Ingest(gradatum_core::IngestSource {
                source: gradatum_core::IngestInputSource::Locus {
                    path: "/tmp".into(),
                },
                vault: "main".into(),
                locus: "rag/".into(),
                strategy: gradatum_core::IngestStrategy::Auto,
                dry_run: true,
            })),
            "Ingest"
        );
    }

    // ── Tests fix worker-hang-busy-timeout ────────────────────────────────────

    /// Régression : `promote_retries` utilisait le BLOB stale (retry.count=0) au lieu
    /// de `attempt_count` SQL pour la garde DLQ → les jobs dépassant max_retries
    /// étaient remis en Pending indéfiniment au lieu d'aller en DLQ.
    ///
    /// Ce test vérifie que `promote_retries` lit bien `attempt_count` SQL et envoie
    /// le job en DLQ quand `attempt_count >= retry.max`.
    #[tokio::test]
    async fn promote_retries_uses_sql_attempt_count_for_dlq_guard() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool.clone());

        // max=2 — après 2 tentatives le job doit partir en DLQ.
        let mut record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        record.retry.max = 2;
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");

        // Simuler 2 dequeues + fails successifs : attempt_count SQL monte à 2.
        let _ = store.dequeue().await.expect("dequeue 1");
        store.fail(id, "erreur 1", 1).await.expect("fail 1");
        // promote_retries → encore < max (1 < 2) → Pending
        store
            .schedule_retry(id, Utc::now() - chrono::Duration::seconds(1))
            .await
            .expect("schedule_retry 1");

        let promoted = store
            .promote_retries(Utc::now())
            .await
            .expect("promote_retries 1");
        assert!(
            promoted.contains(&id),
            "job doit être promu Pending après 1 tentative"
        );

        // Deuxième cycle
        let _ = store.dequeue().await.expect("dequeue 2");
        store.fail(id, "erreur 2", 2).await.expect("fail 2");
        store
            .schedule_retry(id, Utc::now() - chrono::Duration::seconds(1))
            .await
            .expect("schedule_retry 2");

        // promote_retries → attempt_count SQL = 2 >= max = 2 → DLQ (pas Pending).
        // Avant le fix, le BLOB avait retry.count=0 → 0 >= 2 = faux → Pending infini.
        let promoted2 = store
            .promote_retries(Utc::now())
            .await
            .expect("promote_retries 2");
        assert!(
            !promoted2.contains(&id),
            "job à max_retries ne doit PAS être dans la liste promoted (il est en DLQ)"
        );

        let fetched = store.get(id).await.expect("get").expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::DLQ,
            "job avec attempt_count >= max doit passer en DLQ — pas en Pending infini"
        );
    }

    /// Régression : `replay_single` remettait en Pending sans reset `attempt_count`.
    /// Un job replayed avec attempt_count >= max_retries serait aussitôt renvoyé en DLQ
    /// par le prochain sweep sans jamais être exécuté.
    ///
    /// Ce test vérifie directement que `attempt_count` et `last_error` sont remis à 0/NULL
    /// après un replay SQL (même requête que jobs_cmd::replay_single).
    #[tokio::test]
    async fn replay_dlq_resets_attempt_count() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool.clone());

        let record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        let id = record.id;
        let id_str = id.to_string();
        store.enqueue(record).await.expect("enqueue");

        // Amener le job en DLQ après déqueue + fail_dlq.
        let _ = store.dequeue().await.expect("dequeue");
        // Force attempt_count=3 via fail() avant fail_dlq.
        store.fail(id, "erreur max", 3).await.expect("fail");
        store
            .fail_dlq(id, "max_retries atteint")
            .await
            .expect("fail_dlq");

        let before = store.get(id).await.expect("get").expect("job");
        assert_eq!(before.lifecycle.status, JobStatus::DLQ);

        // Replay SQL — même requête que gradatum-admin/src/jobs_cmd.rs::replay_single.
        let rows_affected = sqlx::query(
            r#"
            UPDATE gradatum_jobs
            SET status        = 'Pending',
                lease_until   = NULL,
                scheduled_at  = datetime('now'),
                attempt_count = 0,
                last_error    = NULL
            WHERE id = ?
              AND status = 'DLQ'
            "#,
        )
        .bind(&id_str)
        .execute(&pool)
        .await
        .expect("replay SQL")
        .rows_affected();
        assert_eq!(rows_affected, 1, "replay doit affecter exactement 1 ligne");

        // Vérifier que attempt_count est bien à 0.
        let row =
            sqlx::query("SELECT attempt_count, last_error, status FROM gradatum_jobs WHERE id = ?")
                .bind(&id_str)
                .fetch_one(&pool)
                .await
                .expect("row");
        let attempt_count: i64 = row.try_get("attempt_count").expect("attempt_count");
        let last_error: Option<String> = row.try_get("last_error").expect("last_error");
        let status: String = row.try_get("status").expect("status");

        assert_eq!(attempt_count, 0, "attempt_count doit être 0 après replay");
        assert!(
            last_error.is_none(),
            "last_error doit être NULL après replay"
        );
        assert_eq!(status, "Pending", "status doit être Pending après replay");
    }

    /// Vérifie que `promote_retries` remet bien un job en Pending quand
    /// attempt_count < max_retries (chemin heureux — non régressé par le fix).
    #[tokio::test]
    async fn promote_retries_pending_when_below_max() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let mut record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        record.retry.max = 3; // max 3, on ne fait qu'1 tentative
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");

        let _ = store.dequeue().await.expect("dequeue");
        store.fail(id, "erreur 1", 1).await.expect("fail");
        store
            .schedule_retry(id, Utc::now() - chrono::Duration::seconds(1))
            .await
            .expect("schedule_retry");

        let promoted = store
            .promote_retries(Utc::now())
            .await
            .expect("promote_retries");

        assert!(
            promoted.contains(&id),
            "job en dessous de max_retries doit être promu Pending"
        );

        let fetched = store.get(id).await.expect("get").expect("job");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "job doit être Pending après promote_retries < max_retries"
        );
    }

    // ── A2 : find_awaiting / set_pending → NotImplemented (jamais de panic) ──

    /// `find_awaiting` retourne `Err(NotImplemented)` sans paniquer (A2).
    #[tokio::test]
    async fn find_awaiting_returns_not_implemented() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let result = store.find_awaiting(Ulid::new()).await;
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
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);
        let result = store.set_pending(Ulid::new()).await;
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

    // ── C1 : transitions queue atomiques — pas de double-complete ────────────

    /// C1 — Deux appels `complete` concurrents sur le même job : exactement 1
    /// doit réussir et laisser le job en Done. Le second doit aussi réussir
    /// (idempotent au niveau SQL — pas d'erreur NotFound car le job existe encore)
    /// mais le statut final doit rester Done et le résultat du premier être
    /// préservé (pas de corruption).
    ///
    /// Ce test vérifie que la transaction BEGIN IMMEDIATE sérialise correctement
    /// les deux appels sans perdre de données.
    #[tokio::test]
    async fn c1_concurrent_complete_no_double_write() {
        let pool = test_pool().await;
        let store = std::sync::Arc::new(SqliteQueueStore::new(pool));

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");

        let result_a = JobResult {
            success: true,
            duration_ms: 10,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        let result_b = JobResult {
            success: true,
            duration_ms: 20,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };

        let store_a = store.clone();
        let store_b = store.clone();
        // Lancer les deux complete() en parallèle.
        let (r_a, r_b) = tokio::join!(
            store_a.complete(id, result_a),
            store_b.complete(id, result_b),
        );

        // Les deux ne doivent pas paniquer — ils peuvent tous deux réussir (idempotent)
        // ou l'un retourner une erreur Storage (SQLITE_BUSY sous contention extrême).
        // L'important : le statut final est Done, pas corrompu.
        let _ = r_a;
        let _ = r_b;

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Done,
            "statut final doit être Done après double complete()"
        );
    }

    /// C1 — Deux appels `fail` concurrents sur le même job : le statut final
    /// doit être Failed (pas Pending ou autre état corrompu).
    #[tokio::test]
    async fn c1_concurrent_fail_no_corruption() {
        let pool = test_pool().await;
        let store = std::sync::Arc::new(SqliteQueueStore::new(pool));

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");

        let store_a = store.clone();
        let store_b = store.clone();
        let (r_a, r_b) = tokio::join!(
            store_a.fail(id, "erreur concurrent A", 1),
            store_b.fail(id, "erreur concurrent B", 1),
        );
        let _ = r_a;
        let _ = r_b;

        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Failed,
            "statut final doit être Failed après double fail()"
        );
    }

    // ── C3 : recover_stale_leases — TTL invalide ne doit pas mass-recover ────

    /// C3 — TTL = Duration::MAX (hors plage chrono) → 0 job récupéré, pas de
    /// panic, pas de mass-recovery catastrophique.
    #[tokio::test]
    async fn c3_recover_stale_leases_invalid_ttl_returns_empty() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Enqueuer + déqueuer un job (il passe Running avec lease_until proche).
        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");

        // TTL invalide = Duration::MAX (> i64::MAX ns → hors plage chrono::Duration).
        // Avant le fix, unwrap_or_default() → Duration::ZERO → threshold = now → le job
        // Running avec lease_until futur aurait pu être faussement récupéré.
        // Après le fix : Ok(vec![]) retourné sans toucher aucun job.
        let recovered = store
            .recover_stale_leases(Duration::MAX)
            .await
            .expect("recover_stale_leases doit réussir même avec TTL invalide");

        assert!(
            recovered.is_empty(),
            "TTL invalide doit retourner 0 job récupéré, pas de mass-recovery"
        );

        // Vérifier que le job Running est intact (pas faussement remis en Pending).
        let fetched = store
            .get(id)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Running,
            "le job Running ne doit PAS être remis en Pending par un TTL invalide"
        );
    }

    /// C3 — TTL valide (0s) continue de fonctionner normalement (non-régression).
    #[tokio::test]
    async fn c3_recover_stale_leases_valid_ttl_works() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");

        // Forcer la lease dans le passé.
        sqlx::query("UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id.to_string())
            .execute(&store.pool)
            .await
            .expect("patch lease");

        let recovered = store
            .recover_stale_leases(Duration::from_secs(0))
            .await
            .expect("recover doit réussir");

        assert!(
            recovered.contains(&id),
            "TTL valide (0s) doit récupérer le job avec lease expirée"
        );
    }

    /// Régression dashboard `last_job` — `latest_job()` doit renvoyer le job le
    /// PLUS RÉCENT (ORDER BY id DESC), jamais le plus ancien.
    ///
    /// Bug d'origine : le dashboard appelait `list(JobFilter{limit:1})` qui ordonne
    /// `id ASC` → renvoyait le plus vieux job (ex. 314h) au lieu du job du jour,
    /// donnant l'illusion d'un worker mort.
    ///
    /// Test déterministe : 3 jobs avec des ULID à timestamps croissants explicites
    /// (pas de dépendance à la monotonie de `Ulid::new()` ni à des sleeps).
    #[tokio::test]
    async fn latest_job_returns_most_recent_not_oldest() {
        use std::time::{Duration as StdDuration, UNIX_EPOCH};

        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // Base epoch + offsets croissants → 3 ULID strictement ordonnés.
        let base = UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
        let id_old = Ulid::from_datetime(base);
        let id_mid = Ulid::from_datetime(base + StdDuration::from_secs(3600));
        let id_new = Ulid::from_datetime(base + StdDuration::from_secs(7200));
        assert!(
            id_old < id_mid && id_mid < id_new,
            "ULID doivent être ordonnés"
        );

        // Enqueue dans le DÉSORDRE pour prouver que le tri SQL (pas l'ordre d'insert)
        // est seul responsable du résultat.
        for id in [id_mid, id_old, id_new] {
            let mut record = make_record(Job::Consolidate, JobClass::System, JobStatus::Pending);
            record.id = id;
            store.enqueue(record).await.expect("enqueue doit réussir");
        }

        let latest = store
            .latest_job("main")
            .await
            .expect("latest_job doit réussir")
            .expect("la file n'est pas vide → un job attendu");

        assert_eq!(
            latest.id, id_new,
            "latest_job doit renvoyer le job le plus RÉCENT (id le plus grand), pas le plus ancien"
        );
        assert_ne!(
            latest.id, id_old,
            "latest_job ne doit JAMAIS renvoyer le job le plus ancien (bug dashboard d'origine)"
        );
    }

    /// `latest_job()` sur une file vide dégrade proprement en `None` (le dashboard
    /// affiche alors « pas de last_job » sans erreur).
    #[tokio::test]
    async fn latest_job_empty_returns_none() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        let latest = store
            .latest_job("main")
            .await
            .expect("latest_job doit réussir même sur file vide");

        assert!(latest.is_none(), "file vide → None, pas d'erreur");
    }

    // ── D1.3 — prune DLQ ──────────────────────────────────────────────────────

    /// Helper : enqueue un job puis le force en DLQ.
    async fn seed_dlq(store: &SqliteQueueStore) -> Ulid {
        let record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");
        store.fail_dlq(id, "test prune").await.expect("fail_dlq");
        id
    }

    /// Helper : enqueue un job en DLQ avec un `created_at` arbitraire.
    ///
    /// `fail_dlq` ne change pas `created_at` (seul le statut), donc on contrôle
    /// l'ancienneté via le record initial — utile pour tester `--older-than`.
    async fn seed_dlq_at(store: &SqliteQueueStore, created_at: DateTime<Utc>) -> Ulid {
        let mut record = make_record(Job::Validate, JobClass::System, JobStatus::Pending);
        record.lifecycle.created_at = created_at;
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue().await.expect("dequeue");
        store.fail_dlq(id, "test prune").await.expect("fail_dlq");
        id
    }

    /// P2 audit — `count_dlq_jobs(None)` compte TOUS les DLQ, sans borne `LIMIT`.
    ///
    /// Régression du bug `list(limit: 200)` : avec > 200 jobs DLQ, l'ancien dry-run
    /// sous-comptait. Ce test seede 205 jobs DLQ et exige un compte exact de 205.
    #[tokio::test]
    async fn count_dlq_jobs_exact_above_200() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        for _ in 0..205 {
            seed_dlq(&store).await;
        }

        let counted = store.count_dlq_jobs(None).await.expect("count_dlq_jobs");
        assert_eq!(
            counted, 205,
            "count_dlq_jobs doit compter les 205 DLQ (pas de cap à 200)"
        );

        // Le compte doit correspondre EXACTEMENT au DELETE (même WHERE).
        let deleted = store.delete_dlq_jobs(None).await.expect("delete");
        assert_eq!(deleted, counted, "count == delete (même clause WHERE)");
    }

    /// P2 audit — `--older-than` cible des jobs anciens hors de la fenêtre des 200
    /// premiers : l'ancien `list(limit: 200)` pouvait early-return « rien à
    /// supprimer ». Le `COUNT(*)` dédié les voit → prune s'exécute.
    #[tokio::test]
    async fn count_dlq_jobs_older_than_beyond_200() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        // 210 jobs DLQ récents (créés ~now) — hors fenêtre du cutoff.
        for _ in 0..210 {
            seed_dlq(&store).await;
        }
        // 5 jobs DLQ anciens (créés il y a 100 jours) — dans la fenêtre du cutoff.
        let old_created = Utc::now() - chrono::Duration::days(100);
        for _ in 0..5 {
            seed_dlq_at(&store, old_created).await;
        }

        let cutoff = Utc::now() - chrono::Duration::days(30);
        let counted = store
            .count_dlq_jobs(Some(cutoff))
            .await
            .expect("count older_than");
        assert_eq!(
            counted, 5,
            "seuls les 5 jobs DLQ > 30j doivent être comptés (vus malgré 210 récents)"
        );

        // Le prune réel doit supprimer exactement ces 5 jobs.
        let deleted = store
            .delete_dlq_jobs(Some(cutoff))
            .await
            .expect("delete older_than");
        assert_eq!(
            deleted, 5,
            "le DELETE doit supprimer exactement les 5 anciens"
        );
    }

    /// `delete_dlq_jobs(None)` supprime tous les jobs DLQ → 0 restant.
    #[tokio::test]
    async fn delete_dlq_jobs_prunes_all() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        for _ in 0..3 {
            seed_dlq(&store).await;
        }

        // 3 jobs en DLQ avant prune.
        let before = store
            .list(JobFilter {
                status: Some(JobStatus::DLQ),
                limit: 200,
                ..Default::default()
            })
            .await
            .expect("list before");
        assert_eq!(before.len(), 3, "3 jobs DLQ attendus avant prune");

        let deleted = store.delete_dlq_jobs(None).await.expect("delete_dlq_jobs");
        assert_eq!(deleted, 3, "les 3 jobs DLQ doivent être supprimés");

        let after = store
            .list(JobFilter {
                status: Some(JobStatus::DLQ),
                limit: 200,
                ..Default::default()
            })
            .await
            .expect("list after");
        assert_eq!(after.len(), 0, "0 job DLQ après prune total");
    }

    /// `delete_dlq_jobs(Some(cutoff))` respecte la fenêtre d'ancienneté :
    /// un cutoff dans le passé ne supprime rien (jobs créés à `now`), un cutoff
    /// dans le futur supprime tout.
    #[tokio::test]
    async fn delete_dlq_jobs_respects_older_than() {
        let pool = test_pool().await;
        let store = SqliteQueueStore::new(pool);

        for _ in 0..2 {
            seed_dlq(&store).await;
        }

        // Cutoff dans le passé (jobs créés à ~now) → aucun ciblé.
        let past = Utc::now() - chrono::Duration::days(1);
        let deleted_past = store
            .delete_dlq_jobs(Some(past))
            .await
            .expect("delete past");
        assert_eq!(deleted_past, 0, "cutoff passé → aucun job supprimé");

        // Cutoff dans le futur → tous ciblés.
        let future = Utc::now() + chrono::Duration::days(1);
        let deleted_future = store
            .delete_dlq_jobs(Some(future))
            .await
            .expect("delete future");
        assert_eq!(
            deleted_future, 2,
            "cutoff futur → tous les jobs DLQ supprimés"
        );
    }
}
