//! Implémentation SQLite du [`QueueStore`] — `SqliteQueueStore`.
//!
//! Stocke les [`JobRecord`] dans la table `gradatum_jobs` (migration 006).
//! Utilise `sqlx` async avec WAL mode pour les opérations de queue.
//!
//! # Garanties
//!
//! - **Atomic lease** : `UPDATE … SET status='Running', lease_until=? WHERE id=?`
//!   dans une transaction exclusive évite les doubles consommations.
//! - **Sweep périodique** : `recover_stale_leases`, `cancel_expired_deadlines`,
//!   `promote_retries` sont appelés par `gradatum-worker` toutes les 30s.
//! - **Cascade** : `find_awaiting` + `set_pending` pour le chaînage `await_jobs`.
//!
//! # Limitations Phase 1.1
//!
//! - `find_awaiting` utilise un `LIKE '%"id"%'` — acceptable pour < 10k jobs actifs.
//!   Phase 1.2+ : index JSON natif ou table de jointure `gradatum_job_deps`.
//! - Pas de `LibsqlQueueStore` (F-25, Phase B opt-in).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use ulid::Ulid;

use gradatum_core::{
    job_kind_str, JobError, JobFilter, JobRecord, JobResult, JobStatus, QueueError, QueueEvent,
    QueueStore,
};

/// Canal broadcast pour les [`QueueEvent`] — capacité par défaut.
const BROADCAST_CAPACITY: usize = 256;

/// Implémentation SQLite du [`QueueStore`].
///
/// Construit depuis un [`SqlitePool`] sqlx (WAL mode requis).
/// Utiliser [`SqliteQueueStore::new`] pour créer une instance.
///
/// # Exemple
///
/// ```rust,ignore
/// let pool = SqlitePool::connect("sqlite:///path/to/gradatum.db?mode=rwc").await?;
/// let store = SqliteQueueStore::new(pool);
/// ```
pub struct SqliteQueueStore {
    /// Pool sqlx partagé (WAL mode, synchronous=NORMAL).
    pool: SqlitePool,
    /// Sender du canal broadcast pour les événements de queue.
    ///
    /// `broadcast::Sender` est `Clone + Send + Sync` — peut être cloné pour
    /// chaque méthode qui publie un événement.
    tx: broadcast::Sender<QueueEvent>,
}

impl SqliteQueueStore {
    /// Crée un nouveau `SqliteQueueStore` depuis un pool sqlx.
    ///
    /// Le pool doit avoir été configuré en mode WAL (`PRAGMA journal_mode=WAL`).
    /// Les migrations `006_apalis_bootstrap.sql` doivent avoir été appliquées.
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { pool, tx }
    }

    /// Publie un événement sur le canal broadcast.
    ///
    /// Les erreurs de publication (no receivers) sont ignorées — le broadcast
    /// est best-effort. Les consommateurs SSE/cascade s'abonnent via `subscribe()`.
    fn publish(&self, event: QueueEvent) {
        if let Err(e) = self.tx.send(event) {
            debug!("SqliteQueueStore: aucun consommateur broadcast actif ({e})");
        }
    }

    /// Sérialise un `JobRecord` en JSON pour stockage.
    fn serialize_record(record: &JobRecord) -> Result<String, QueueError> {
        serde_json::to_string(record).map_err(|e| QueueError::Serialization(e.to_string()))
    }

    /// Désérialise un `JobRecord` depuis JSON.
    fn deserialize_record(json: &str) -> Result<JobRecord, QueueError> {
        serde_json::from_str(json).map_err(|e| QueueError::Serialization(e.to_string()))
    }

    /// Convertit un `JobStatus` en sa représentation TEXT SQLite.
    fn status_to_str(status: &JobStatus) -> &'static str {
        match status {
            JobStatus::Pending => "Pending",
            JobStatus::Running => "Running",
            JobStatus::Waiting => "Waiting",
            JobStatus::Done => "Done",
            JobStatus::Failed => "Failed",
            JobStatus::DLQ => "DLQ",
            JobStatus::Cancelled => "Cancelled",
        }
    }

    /// Convertit une représentation TEXT SQLite en `JobStatus`.
    #[allow(dead_code)] // utilisé dans les tests et futures migrations de lecture
    fn str_to_status(s: &str) -> Result<JobStatus, QueueError> {
        match s {
            "Pending" => Ok(JobStatus::Pending),
            "Running" => Ok(JobStatus::Running),
            "Waiting" => Ok(JobStatus::Waiting),
            "Done" => Ok(JobStatus::Done),
            "Failed" => Ok(JobStatus::Failed),
            "DLQ" => Ok(JobStatus::DLQ),
            "Cancelled" => Ok(JobStatus::Cancelled),
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

    /// Override de [`QueueStore::dequeue_by_kind`] — filtrage SQL natif par `kind`.
    ///
    /// Exploite l'index `idx_jobs_status_kind (status, kind)` pour garantir qu'un
    /// worker `curate` ne reçoit jamais un job `Embed` ou `ReIndex`.
    /// Sans ce filtre, la race entre 10 goroutines (curate=2, embed=4, reindex=4)
    /// entraîne ~80% de DLQ via `HandlerError::UnexpectedVariant`.
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
        // Met à jour le payload avec le résultat et le statut Done
        // Note : le payload JSON est la source de vérité — on relit, on patche, on réécrit.
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
        .execute(&self.pool)
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

        // Relit le payload pour mettre à jour les erreurs
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
        .execute(&self.pool)
        .await
        .map_err(|e| QueueError::Storage(e.to_string()))?;

        self.publish(QueueEvent::JobFailed(id, attempt));
        Ok(())
    }

    async fn cancel(&self, id: Ulid) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();

        // Relit le payload pour synchroniser le statut
        let row = sqlx::query(r#"SELECT payload FROM gradatum_jobs WHERE id = ? AND status NOT IN ('Done', 'DLQ', 'Cancelled')"#)
            .bind(&id_str)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| QueueError::Storage(e.to_string()))?;

        let Some(row) = row else {
            // Job déjà terminal ou inexistant — opération idempotente
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
              AND status NOT IN ('Done', 'DLQ', 'Cancelled')
            "#,
        )
        .bind(&now_str)
        .bind(&new_payload)
        .bind(&id_str)
        .execute(&self.pool)
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
        // Phase F-14 complet v0.3.0 — DAG await_jobs
        //
        // Le chaînage déclaratif (await_jobs : Vec<JobTrigger>) est défini dans
        // JobScheduling mais l'engine de résolution des dépendances n'est pas
        // implémenté en v0.2.0. Requis : table gradatum_job_deps + cascade engine.
        //
        // Référence : v81 §6 L2383-2408 (await_jobs, TriggerCondition, cascade).
        // Milestone : F-14 complet v0.3.0.
        todo!("Phase F-14 complet v0.3.0 — DAG await_jobs : cascade engine non implémenté")
    }

    async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
        // Phase F-14 complet v0.3.0 — cascade Waiting → Pending
        //
        // Transition d'état Waiting → Pending pour le chaînage DAG.
        // Non implémenté en v0.2.0 — dépend du cascade engine (find_awaiting).
        //
        // Référence : v81 §6 L2758-2760 (set_pending).
        // Milestone : F-14 complet v0.3.0.
        todo!("Phase F-14 complet v0.3.0 — cascade set_pending non implémenté")
    }

    async fn recover_stale_leases(&self, ttl: Duration) -> Result<Vec<Ulid>, QueueError> {
        // Les jobs Running dont le lease_until est dépassé depuis > ttl
        let threshold =
            (Utc::now() - chrono::Duration::from_std(ttl).unwrap_or_default()).to_rfc3339();

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
              AND status NOT IN ('Done', 'DLQ', 'Cancelled')
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
        // Cursor-based pagination : WHERE id > cursor ORDER BY id
        let cursor_filter = filter.cursor.as_ref().map(|c| c.to_string());

        let rows = sqlx::query(
            r#"
            SELECT payload
            FROM gradatum_jobs
            WHERE (? IS NULL OR class = ?)
              AND (? IS NULL OR status = ?)
              AND (? IS NULL OR kind = ?)
              AND (? IS NULL OR created_at > ?)
              AND (? IS NULL OR id > ?)
            ORDER BY id ASC
            LIMIT ?
            "#,
        )
        .bind(&class_filter)
        .bind(&class_filter)
        .bind(&status_filter)
        .bind(&status_filter)
        .bind(&filter.kind)
        .bind(&filter.kind)
        .bind(&created_after)
        .bind(&created_after)
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

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QueueEvent> {
        self.tx.subscribe()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers privés — types non exportés
// ─────────────────────────────────────────────────────────────────────────────

/// Applique les pragmas WAL sur une connexion SQLite fraîchement ouverte.
///
/// Doit être appelé après `SqlitePool::connect` et avant toute opération.
///
/// # Effets de bord
///
/// - Active le WAL mode (performances write concurrent)
/// - Fixe `synchronous=NORMAL` (compromis durabilité/performance)
/// - Fixe `foreign_keys=ON`
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

/// Applique les migrations depuis le répertoire `migrations/`.
///
/// Wrapper autour de `sqlx::migrate!` avec le chemin de migration fixe.
/// À utiliser lors de l'initialisation du pool dans `gradatum-worker`.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

// ─────────────────────────────────────────────────────────────────────────────
// IdempotencyStore — table gradatum_idempotency (migration 008, F-16)
// ─────────────────────────────────────────────────────────────────────────────

/// Enregistre une paire `(key, job_id)` dans la table d'idempotence.
///
/// Utilise `INSERT OR IGNORE` — atomique, pas de TOCTOU.
/// Retourne `true` si la clé a été insérée (nouveau job), `false` si déjà existante.
///
/// # Effets de bord
///
/// - Écrit dans `gradatum_idempotency`.
/// - Si la clé existe déjà : no-op (INSERT OR IGNORE).
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

/// Recherche un `job_id` depuis une clé d'idempotence.
///
/// Retourne `Some(job_id)` si la clé existe, `None` sinon.
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

/// Supprime les entrées d'idempotence dont le `created_at` est antérieur à `before_ms`.
///
/// Utilisé par le job cron IdempotencyCleanup (TTL 24h).
/// Retourne le nombre d'entrées supprimées.
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
        assert_eq!(job_kind_str(&Job::Distill), "Distill");
        assert_eq!(job_kind_str(&Job::Backup), "Backup");
        assert_eq!(job_kind_str(&Job::Purge), "Purge");
        assert_eq!(job_kind_str(&Job::Summarize), "Summarize");
        assert_eq!(job_kind_str(&Job::Validate), "Validate");
        assert_eq!(job_kind_str(&Job::Audit), "Audit");
        assert_eq!(job_kind_str(&Job::Consolidate), "Consolidate");
        assert_eq!(job_kind_str(&Job::Forget), "Forget");
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
}
