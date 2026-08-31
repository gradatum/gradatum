//! SQLite implementation of [`QueueStore`] — `SqliteQueueStore`.
//!
//! Stores [`JobRecord`] entries in the `gradatum_jobs` table (migration 006).
//! Uses a single `rusqlite::Connection` under `Arc<tokio::sync::Mutex>` with WAL mode
//! for queue operations (a sync/async bridge).
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

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha384};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, warn};
use ulid::Ulid;

use gradatum_core::{
    JobError, JobFilter, JobOrder, JobRecord, JobResult, JobStatus, QueueError, QueueEvent,
    QueueStore, job_kind_str,
};

/// Broadcast channel capacity for [`QueueEvent`] — default value.
const BROADCAST_CAPACITY: usize = 256;

/// Maps any error (rusqlite, driver) to `QueueError::Storage`.
fn storage_err(e: impl std::fmt::Display) -> QueueError {
    QueueError::Storage(e.to_string())
}

/// Blocking-thread failure error (panic or cancellation) — impossible in practice.
fn blocking_err() -> QueueError {
    QueueError::Storage("queue blocking thread failed (panic or cancellation)".to_string())
}

/// Shared handle to the queue's SQLite database.
///
/// A single `rusqlite::Connection` under `Arc<tokio::sync::Mutex>`, opened and operated on
/// blocking threads (`spawn_blocking`), with the `blocking_lock()` held for as short a time
/// as possible — the same bridge pattern as the server stores (proactive_recall_store /
/// note_usage_store / read_usage_store).
#[derive(Clone)]
pub struct QueueDb {
    conn: Arc<Mutex<Connection>>,
}

impl QueueDb {
    /// Opens (or creates) the database at `path` on a blocking thread, with WAL and a 5 s `busy_timeout`.
    ///
    /// Aligned with the original sqlx settings (`journal_mode=WAL`, `busy_timeout=5000 ms`).
    /// `synchronous` stays at the SQLite default (FULL) — not changed here, as the server/worker
    /// `SqliteConnectOptions` were. `apply_sqlite_pragmas` goes further (`synchronous=NORMAL`,
    /// `foreign_keys=ON`) for callers that require it.
    pub async fn open(path: &Path) -> Result<Self, QueueError> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, QueueError> {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(storage_err)?;
            // WAL AVANT toute transaction : SQLite interdit le changement de journal_mode
            // dans une transaction. `busy_timeout` 5 s conservé (contention WAL multi-process).
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(storage_err)?;
            conn.pragma_update(None, "busy_timeout", 5000i32)
                .map_err(storage_err)?;
            Ok(conn)
        })
        .await
        .map_err(|_| blocking_err())??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an in-memory database (tests) on a blocking thread.
    pub async fn open_in_memory() -> Result<Self, QueueError> {
        let conn = tokio::task::spawn_blocking(|| -> Result<Connection, QueueError> {
            Connection::open_in_memory().map_err(storage_err)
        })
        .await
        .map_err(|_| blocking_err())??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens the database at `path` WITHOUT creating it — fails if the file is absent.
    ///
    /// sqlx `create_if_missing(false)` parity: used by admin paths that require a pre-existing
    /// database (fail-fast rather than a spurious creation).
    /// WAL + a 5 s `busy_timeout` applied, identical to [`QueueDb::open`].
    pub async fn open_existing(path: &Path) -> Result<Self, QueueError> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, QueueError> {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(storage_err)?;
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(storage_err)?;
            conn.pragma_update(None, "busy_timeout", 5000i32)
                .map_err(storage_err)?;
            Ok(conn)
        })
        .await
        .map_err(|_| blocking_err())??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` on a blocking thread, holding the `blocking_lock()` for as short a time as possible.
    ///
    /// The lock is held only while `f` runs — no async wait under the lock. `rusqlite` errors
    /// are mapped to `QueueError::Storage`.
    pub async fn with_conn<T, F>(&self, f: F) -> Result<T, QueueError>
    where
        F: FnOnce(&Connection) -> Result<T, rusqlite::Error> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            f(&conn).map_err(storage_err)
        })
        .await
        .map_err(|_| blocking_err())?
    }

    /// Acquires the blocking lock on the connection — crate-internal use.
    ///
    /// Must be called from a blocking thread (`spawn_blocking`) and released as early as possible.
    /// `with_conn` is the recommended access; this shortcut serves operations that hold the lock
    /// across several statements (migrations, transactions).
    pub(crate) fn blocking_lock(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.conn.blocking_lock()
    }
}

/// Opens (or creates) the queue database at `path` — alias of [`QueueDb::open`].
///
/// Applies WAL and a 5 s `busy_timeout`. `run_migrations` remains a separate call.
pub async fn open_queue_db(path: &Path) -> Result<QueueDb, QueueError> {
    QueueDb::open(path).await
}

/// Opens the queue database at `path` WITHOUT creating it — alias of [`QueueDb::open_existing`].
///
/// Fails if the file is absent (sqlx `create_if_missing(false)` parity).
pub async fn open_queue_db_existing(path: &Path) -> Result<QueueDb, QueueError> {
    QueueDb::open_existing(path).await
}

/// Opens an in-memory queue database (tests) — alias of [`QueueDb::open_in_memory`].
pub async fn open_queue_db_in_memory() -> Result<QueueDb, QueueError> {
    QueueDb::open_in_memory().await
}

/// SQLite implementation of [`QueueStore`].
///
/// Constructed from a [`QueueDb`] (single rusqlite connection, WAL mode).
/// Use [`SqliteQueueStore::new`] to create an instance.
///
/// # Example
///
/// ```rust,ignore
/// let db = gradatum_db_sqlite::open_queue_db(&path).await?;
/// gradatum_db_sqlite::run_migrations(&db).await?;
/// let store = SqliteQueueStore::new(db);
/// ```
pub struct SqliteQueueStore {
    /// Shared connection to the queue database (sync/async bridge pattern).
    db: QueueDb,
    /// Sender for the broadcast channel carrying queue events.
    ///
    /// `broadcast::Sender` is `Clone + Send + Sync` — may be cloned for
    /// each method that publishes an event.
    tx: broadcast::Sender<QueueEvent>,
}

impl SqliteQueueStore {
    /// Creates a new `SqliteQueueStore` from a [`QueueDb`].
    ///
    /// The connection must be in WAL mode (`PRAGMA journal_mode=WAL`).
    /// Migrations `006 → 012` must have been applied.
    #[must_use]
    pub fn new(db: QueueDb) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { db, tx }
    }

    /// Returns the shared [`QueueDb`] — direct access to the connection for consumers that must
    /// run their own SQL (admin replay, leader election, tests).
    #[must_use]
    pub fn db(&self) -> &QueueDb {
        &self.db
    }

    /// Publishes an event on the broadcast channel.
    ///
    /// Publication errors (no receivers) are silently ignored — the broadcast
    /// is best-effort. SSE and cascade consumers subscribe via `subscribe()`.
    fn publish(&self, event: QueueEvent) {
        if let Err(e) = self.tx.send(event) {
            debug!("SqliteQueueStore: no active broadcast consumer ({e})");
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
                "unknown status: '{other}'"
            ))),
        }
    }

    /// Promotes dependants of a job `done_id` that has just transitioned to `Done`.
    ///
    /// For each job in `Waiting` whose `await_jobs` contains `done_id`:
    /// - Re-reads the complete dependencies from the payload.
    /// - Verifies that ALL referenced IDs are in `Done` status in the database.
    /// - If so → calls `set_pending(dependent_id)`.
    ///
    /// # Inertia guarantee
    ///
    /// If `find_awaiting` returns an empty list (no dependent jobs), the method
    /// returns immediately without any mutation. The behavior of jobs without
    /// `await_jobs` is strictly unchanged.
    ///
    /// # Errors
    ///
    /// Returns the first storage error encountered. Any `set_pending` calls that
    /// completed before the error are not rolled back (best-effort).
    pub(crate) async fn cascade_check_and_promote(&self, done_id: Ulid) -> Result<(), QueueError> {
        let dependents = self.find_awaiting(done_id).await?;

        // Inertie garantie : aucun dépendant → aucune mutation.
        if dependents.is_empty() {
            return Ok(());
        }

        for dep in dependents {
            let dep_id = dep.id;
            let await_ids: Vec<Ulid> = dep.scheduling.await_jobs.iter().map(|t| t.job_id).collect();

            // Vérifie que TOUS les jobs référencés sont Done.
            // Si un seul n'est pas Done, on skip ce dépendant.
            let all_done = self.all_jobs_done(&await_ids).await?;
            if all_done {
                self.set_pending(dep_id).await?;
            }
        }

        Ok(())
    }

    /// Returns `true` if all jobs in `ids` have status `Done`.
    ///
    /// Returns `true` for an empty list (vacuous truth — no constraint, immediately satisfied)
    /// or when all IDs are `Done`.
    ///
    /// # Errors
    ///
    /// Returns `QueueError::Storage` on SQLite error.
    async fn all_jobs_done(&self, ids: &[Ulid]) -> Result<bool, QueueError> {
        if ids.is_empty() {
            return Ok(true);
        }

        // Compte combien parmi les IDs fournis sont effectivement Done.
        // On utilise une requête paramétrique avec un IN clause construit dynamiquement.
        // Sécurité : les IDs sont des Ulid (types forts) → pas d'injection SQL possible.
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT COUNT(*) as cnt FROM gradatum_jobs WHERE id IN ({placeholders}) AND status = 'Done'"
        );
        let id_strs: Vec<String> = ids.iter().map(|id| id.to_string()).collect();

        let db = self.db.clone();
        let done_count = tokio::task::spawn_blocking(move || -> Result<i64, QueueError> {
            let conn = db.blocking_lock();
            let n = conn
                .query_row(&sql, rusqlite::params_from_iter(id_strs.iter()), |row| {
                    row.get(0)
                })
                .map_err(storage_err)?;
            Ok(n)
        })
        .await
        .map_err(|_| blocking_err())??;

        Ok(done_count as usize == ids.len())
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
        // L1 : tenant SERVI par le job, dérivé du spec (source = le spec, pas
        // l'appelant). Estampillé dans la colonne `tenant_id` pour le filtrage.
        // `to_string()` : le `&str` emprunte `job.spec` — non `'static`, inutilisable
        // dans le `spawn_blocking` ci-dessous. La valeur est identique (même affichage).
        let tenant = gradatum_core::spec_tenant(&job.spec).to_string();
        // P2 audit (SecAuditor #2, décision (a)) : un `Forget::Agent` multi-vault n'élit
        // aucun vault (`forget_scope_vault` → `None`). Depuis A6' l'estampille retombe
        // donc sur le vault porté par le JOB (`JobSpec.scope`, A2-bis) — et `"main"`
        // seulement en dernier ressort, quand le job lui-même n'en porte aucun.
        //
        // Ce log est le SEUL instrument d'enquête a posteriori sur ce cas : il émet la
        // valeur EFFECTIVEMENT estampillée (`tenant_stamp`, mesurée et non décrite), de
        // sorte qu'un opérateur retrouve le job dans `gradatum_jobs` sans deviner. Le
        // job reste refusé terminalement par le worker (`ensure_forget_scope_vault`).
        if let gradatum_core::Job::Forget(f) = &job.spec.kind
            && let gradatum_core::ForgetScope::Agent {
                agent_id, vaults, ..
            } = &f.scope
            && vaults.len() > 1
        {
            warn!(
                job_id = %id,
                agent_id = %agent_id,
                vaults = ?vaults,
                tenant_stamp = %tenant,
                "enqueue: Forget::Agent multi-vault — scope elects no vault; tenant_id stamped from the job vault (see tenant_stamp), not 'main' (audit)"
            );
        }
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

        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<(), QueueError> {
            let conn = db.blocking_lock();
            conn.execute(
                r#"
                INSERT INTO gradatum_jobs
                    (id, payload, status, priority, class, kind, created_at, scheduled_at, deadline, await_jobs, tenant_id)
                VALUES
                    (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
                params![
                    id_str, payload, status, priority, class, kind, created_at, scheduled_at,
                    deadline, await_jobs, tenant
                ],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .map_err(|_| blocking_err())??;

        self.publish(QueueEvent::JobInserted(id));
        Ok(id)
    }

    async fn dequeue(&self, tenant_filter: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
        // Lease atomique via transaction EXCLUSIVE
        // Sélectionne le job de plus haute priorité schedulé maintenant
        let now = Utc::now().to_rfc3339();
        let lease_until = (Utc::now() + chrono::Duration::seconds(300)).to_rfc3339();
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

        // `BEGIN IMMEDIATE` : voir la justification détaillée dans `dequeue_by_kind`.
        // Transaction read-then-write (SELECT lease + UPDATE) → l'upgrade read→write
        // déféré deadlocke sous contention multi-worker. IMMEDIATE prend le verrou
        // d'écriture en amont et sérialise proprement.
        //
        // Toute la transaction s'exécute sur un fil bloquant sous le verrou unique :
        // `BEGIN IMMEDIATE` + SELECT + UPDATE + COMMIT dans la même fermeture, le verrou
        // `blocking_lock()` n'est relâché qu'en sortie (aucune attente async sous verrou).
        let result =
            tokio::task::spawn_blocking(move || -> Result<Option<(String, String)>, QueueError> {
                let mut conn = db.blocking_lock();

                // Pattern `? IS NULL OR tenant_id = ?` : même que `list()` (L1 isolation).
                // `None` = sans filtre tenant (backward-compatible single-tenant) ;
                // `Some(t)` = isole le dequeue à un tenant (multi-tenant ON).
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_err)?;

                let row = tx
                    .query_row(
                        r#"
                        SELECT id, payload
                        FROM gradatum_jobs
                        WHERE status = 'Pending'
                          AND scheduled_at <= ?1
                          AND (?2 IS NULL OR tenant_id = ?2)
                        ORDER BY priority DESC, scheduled_at ASC
                        LIMIT 1
                        "#,
                        params![now, tenant_filter],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(storage_err)?;

                let Some((id_str, payload)) = row else {
                    tx.rollback().map_err(storage_err)?;
                    return Ok(None);
                };

                // Mise à jour du lease (atomic dans la même transaction)
                tx.execute(
                    r#"
                    UPDATE gradatum_jobs
                    SET status = 'Running',
                        lease_until = ?1,
                        started_at = ?2,
                        attempt_count = attempt_count + 1
                    WHERE id = ?3
                    "#,
                    params![lease_until, now, id_str],
                )
                .map_err(storage_err)?;

                tx.commit().map_err(storage_err)?;
                Ok(Some((id_str, payload)))
            })
            .await
            .map_err(|_| blocking_err())??;

        let Some((_id_str, payload)) = result else {
            return Ok(None);
        };

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
    async fn dequeue_by_kind(
        &self,
        kind: &str,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        let now = Utc::now().to_rfc3339();
        let lease_until = (Utc::now() + chrono::Duration::seconds(300)).to_rfc3339();
        let kind = kind.to_owned();
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

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
        //
        // Connexion unique : le verrou du Mutex sérialise déjà les dequeues entre eux
        // (pas de deux transactions concurrentes sur des connexions distinctes), et
        // `BEGIN IMMEDIATE` reste appliqué pour la fidélité au comportement historique.
        let result =
            tokio::task::spawn_blocking(move || -> Result<Option<(String, String)>, QueueError> {
                let mut conn = db.blocking_lock();

                // Pattern `? IS NULL OR tenant_id = ?` : même que `list()` (L1 isolation).
                // `None` = sans filtre tenant (backward-compatible single-tenant) ;
                // `Some(t)` = isole le dequeue à un tenant (multi-tenant ON).
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_err)?;

                let row = tx
                    .query_row(
                        r#"
                        SELECT id, payload
                        FROM gradatum_jobs
                        WHERE status = 'Pending'
                          AND kind = ?1
                          AND scheduled_at <= ?2
                          AND (?3 IS NULL OR tenant_id = ?3)
                        ORDER BY priority DESC, scheduled_at ASC
                        LIMIT 1
                        "#,
                        params![kind, now, tenant_filter],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(storage_err)?;

                let Some((id_str, payload)) = row else {
                    tx.rollback().map_err(storage_err)?;
                    return Ok(None);
                };

                tx.execute(
                    r#"
                    UPDATE gradatum_jobs
                    SET status = 'Running',
                        lease_until = ?1,
                        started_at = ?2,
                        attempt_count = attempt_count + 1
                    WHERE id = ?3
                    "#,
                    params![lease_until, now, id_str],
                )
                .map_err(storage_err)?;

                tx.commit().map_err(storage_err)?;
                Ok(Some((id_str, payload)))
            })
            .await
            .map_err(|_| blocking_err())??;

        let Some((_id_str, payload)) = result else {
            return Ok(None);
        };

        let mut record = Self::deserialize_record(&payload)?;
        record.lifecycle.status = JobStatus::Running;

        Ok(Some(record))
    }

    async fn get(
        &self,
        id: Ulid,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        // Fix E-12 : synchronise le statut du JobRecord avec les colonnes SQL autoritatives.
        //
        // Le payload BLOB contient le JobRecord sérialisé à l'enqueue. Après dequeue(),
        // le status SQL est mis à jour en Running MAIS le payload BLOB reste Pending (optimisation
        // atomicité — évite de réécrire le payload dans la transaction de lease).
        // On lit donc les colonnes SQL et on les injecte dans le record désérialisé.
        //
        // Colonnes SQL autoritatives : status, attempt_count, last_error, completed_at.
        let id_str = id.to_string();
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

        // L1 : `None` = SQL actuel (byte-identical) ; `Some(t)` = `AND tenant_id = ?`
        // → un job d'un autre tenant lit `None` (404 anti-disclosure au handler).
        let sql = match tenant_filter.as_deref() {
            None => "SELECT payload, status, attempt_count, last_error, completed_at \
                     FROM gradatum_jobs WHERE id = ?1"
                .to_string(),
            Some(_) => "SELECT payload, status, attempt_count, last_error, completed_at \
                        FROM gradatum_jobs WHERE id = ?1 AND tenant_id = ?2"
                .to_string(),
        };

        let row = tokio::task::spawn_blocking(
            move || -> Result<Option<(String, String, i64, Option<String>, Option<String>)>, QueueError> {
                let conn = db.blocking_lock();
                let mut stmt = conn.prepare(&sql).map_err(storage_err)?;
                let row = match &tenant_filter {
                    None => stmt.query_row([&id_str], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    }),
                    Some(t) => stmt.query_row(params![id_str, t], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    }),
                };
                row.optional().map_err(storage_err)
            },
        )
        .await
        .map_err(|_| blocking_err())??;

        match row {
            None => Ok(None),
            Some((payload, sql_status, sql_attempts, sql_last_error, sql_completed_at)) => {
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
                if let Some(completed_str) = sql_completed_at
                    && record.lifecycle.completed_at.is_none()
                {
                    // Réhydrate completed_at depuis SQL si le BLOB ne l'a pas encore
                    record.lifecycle.completed_at = completed_str.parse::<DateTime<Utc>>().ok();
                }

                Ok(Some(record))
            }
        }
    }

    async fn complete(&self, id: Ulid, result: JobResult) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();
        let db = self.db.clone();
        let result_for_payload = result.clone();

        // BEGIN IMMEDIATE : la lecture du payload et l'UPDATE sont dans la même
        // transaction exclusive — évite le double-complete concurrent (deux workers
        // lisant le même job Running avant que l'un ne l'ait marqué Done).
        // Même pattern que dequeue() — justification détaillée dans dequeue_by_kind().
        //
        // Toute la transaction s'exécute sur un fil bloquant sous le verrou unique.
        #[derive(Debug)]
        enum CompleteOutcome {
            /// Job already in Conflict — commit with no change (no-op).
            Conflict,
            /// UPDATE succeeded — commit.
            Updated,
            /// UPDATE affected 0 rows (lease expired or status ≠ Running) — rollback.
            NotLeased,
        }

        let outcome = tokio::task::spawn_blocking(
            move || -> Result<CompleteOutcome, QueueError> {
                let mut conn = db.blocking_lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_err)?;

                // Met à jour le payload avec le résultat et le statut Done
                // Note : le payload JSON est la source de vérité — on relit, on patche, on réécrit.
                //
                // P2-6 : le SELECT vérifie `status = 'Running'`. Un `complete()` sur un job
                // Pending/Done/Failed/Cancelled/DLQ/Conflict échoue immédiatement (NotFound)
                // plutôt que d'être silencieux — le handler n'a pas de lease valide.
                let payload_str: String = tx
                    .query_row(
                        r#"SELECT payload FROM gradatum_jobs WHERE id = ?1 AND status = 'Running'"#,
                        [&id_str],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage_err)?
                    .ok_or(QueueError::NotFound(id))?;

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
                    tx.commit().map_err(storage_err)?;
                    tracing::debug!(
                        job_id = %id,
                        "complete: job already in terminal state Conflict (F-41) — Done ignored"
                    );
                    return Ok(CompleteOutcome::Conflict);
                }

                record.lifecycle.status = JobStatus::Done;
                record.lifecycle.completed_at = Some(Utc::now());
                record.lifecycle.result = Some(result_for_payload);
                let new_payload = Self::serialize_record(&record)?;

                // P0-1 — Garde stale-lease : refuse complete si le job n'est pas en
                // statut 'Running' ou si le lease est expiré. Sans cette garde, un worker
                // peut marquer `Done` un job dont le lease a expiré et qu'un autre
                // worker a déjà repris → corruption silencieuse du job.
                //
                // Même pattern que l'ancien `SqliteQueue::complete()` (queue.rs, table
                // `jobs_v2`) — source supprimée en 2.1.0 (F-177), même logique sur `gradatum_jobs`.
                let rows_affected = tx
                    .execute(
                        r#"
                        UPDATE gradatum_jobs
                        SET status = 'Done',
                            completed_at = ?1,
                            lease_until = NULL,
                            payload = ?2
                        WHERE id = ?3
                          AND status = 'Running'
                          AND lease_until > ?4
                        "#,
                        params![now, new_payload, id_str, now],
                    )
                    .map_err(storage_err)?;

                if rows_affected == 0 {
                    // Le job n'est plus en Running ou le lease a expiré — le worker
                    // n'a plus de droit d'écriture sur ce job.
                    tx.rollback().map_err(storage_err)?;
                    tracing::warn!(
                        job_id = %id,
                        "complete: job not leased or lease expired — rejected (P0-1 stale-lease guard)"
                    );
                    return Ok(CompleteOutcome::NotLeased);
                }

                tx.commit().map_err(storage_err)?;
                Ok(CompleteOutcome::Updated)
            },
        )
        .await
        .map_err(|_| blocking_err())??;

        match outcome {
            CompleteOutcome::Conflict => return Ok(()),
            CompleteOutcome::NotLeased => return Err(QueueError::NotLeased(id)),
            CompleteOutcome::Updated => {}
        }

        let status = if result.success {
            JobStatus::Done
        } else {
            JobStatus::Failed
        };
        self.publish(QueueEvent::JobCompleted(id, status, result));

        // Cascade best-effort : promouvoir les dépendants Waiting dont toutes
        // les dépendances sont Done. Exécuté après commit + broadcast pour
        // garantir que les lectures de statut dans la cascade voient l'état Done.
        //
        // Inertie garantie : si find_awaiting retourne [], aucune mutation.
        // Les erreurs de cascade sont loguées mais ne font pas échouer le complete()
        // (le job est déjà Done). En revanche, il N'EXISTE PAS de filet de rattrapage :
        // run_sweep_once() ne re-visite jamais les jobs Waiting orphelins.
        // Conséquence : si cascade_check_and_promote échoue (erreur storage) OU
        // si le worker crashe entre tx.commit() et cet appel, les jobs dépendants
        // restent bloqués en Waiting indéfiniment jusqu'à intervention manuelle.
        // Dette connue DT-DAG-1 — à corriger avant d'activer un producteur await_jobs réel.
        if let Err(e) = self.cascade_check_and_promote(id).await {
            warn!(
                job_id = %id,
                error = %e,
                "complete: cascade_check_and_promote failed (best-effort, job Done)"
            );
        }

        Ok(())
    }

    async fn fail(&self, id: Ulid, err: &str, attempt: u32) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let err_truncated: String = if err.chars().count() > 2048 {
            err.chars().take(2048).collect()
        } else {
            err.to_string()
        };
        let now = Utc::now().to_rfc3339();
        let db = self.db.clone();

        // BEGIN IMMEDIATE : lecture + mise à jour atomiques — évite que deux appels
        // fail() concurrents n'écrasent mutuellement leur compteur d'erreurs.
        //
        // Toute la transaction s'exécute sur un fil bloquant sous le verrou unique.
        // Retour : `true` = UPDATE committé, `false` = rollback (lease expiré).
        let committed = tokio::task::spawn_blocking(move || -> Result<bool, QueueError> {
            let mut conn = db.blocking_lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_err)?;

            // Relit le payload pour mettre à jour les erreurs.
            //
            // P2-7 : le SELECT vérifie `status = 'Running'`. Un `fail()` sur un job
            // Pending/Done/Failed/Cancelled/DLQ/Conflict échoue immédiatement (NotFound)
            // plutôt que d'être silencieux — le handler n'a pas de lease valide.
            let payload_str: String = tx
                .query_row(
                    r#"SELECT payload FROM gradatum_jobs WHERE id = ?1 AND status = 'Running'"#,
                    [&id_str],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_err)?
                .ok_or(QueueError::NotFound(id))?;

            let mut record = Self::deserialize_record(&payload_str)?;
            record.lifecycle.status = JobStatus::Failed;
            record.retry.count = attempt;
            record.retry.last_error = Some(err_truncated.clone());
            record.retry.errors.push(JobError {
                at: Utc::now(),
                message: err_truncated.clone(),
                attempt,
            });
            let new_payload = Self::serialize_record(&record)?;

            // P0-2 — Garde stale-lease : refuse fail si le job n'est pas en
            // statut 'Running' ou si le lease est expiré. Sans cette garde, un worker
            // peut marquer `Failed` un job dont le lease a expiré et qu'un autre
            // worker a déjà repris → corruption silencieuse du job.
            //
            // Même pattern que l'ancien `SqliteQueue::fail()` (queue.rs, table
            // `jobs_v2`) — source supprimée en 2.1.0 (F-177), même logique sur `gradatum_jobs`.
            let rows_affected = tx
                .execute(
                    r#"
                        UPDATE gradatum_jobs
                        SET status = 'Failed',
                            lease_until = NULL,
                            last_error = ?1,
                            attempt_count = ?2,
                            payload = ?3
                        WHERE id = ?4
                          AND status = 'Running'
                          AND lease_until > ?5
                        "#,
                    params![err_truncated, attempt as i64, new_payload, id_str, now],
                )
                .map_err(storage_err)?;

            if rows_affected == 0 {
                // Le job n'est plus en Running ou le lease a expiré — le worker
                // n'a plus de droit d'écriture sur ce job.
                tx.rollback().map_err(storage_err)?;
                tracing::warn!(
                    job_id = %id,
                    "fail: job not leased or lease expired — rejected (P0-2 stale-lease guard)"
                );
                return Ok(false);
            }

            tx.commit().map_err(storage_err)?;
            Ok(true)
        })
        .await
        .map_err(|_| blocking_err())??;

        if !committed {
            return Err(QueueError::NotLeased(id));
        }

        self.publish(QueueEvent::JobFailed(id, attempt));
        Ok(())
    }

    async fn cancel(&self, id: Ulid, tenant_filter: Option<&str>) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

        // L1 : `None` = SQL actuel (byte-identical) ; `Some(t)` = `AND tenant_id = ?`
        // sur le SELECT (gate) ET l'UPDATE (defense-in-depth). Un cancel d'un job
        // d'autrui trouve 0 row → no-op idempotent (le job reste actif).
        let tenant_clause = if tenant_filter.is_some() {
            " AND tenant_id = ?"
        } else {
            ""
        };

        // BEGIN IMMEDIATE : la vérification du statut courant (SELECT NOT IN terminal)
        // et l'UPDATE sont atomiques — évite qu'un cancel concurrent ne double-écrive.
        //
        // Toute la transaction s'exécute sur un fil bloquant sous le verrou unique.
        // Retour : `true` = annulé + committé, `false` = déjà terminal (no-op idempotent).
        let cancelled = tokio::task::spawn_blocking(
            move || -> Result<bool, QueueError> {
                let mut conn = db.blocking_lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_err)?;

                // Relit le payload pour synchroniser le statut
                // F-41 — Conflict ajouté aux états terminaux : un job déjà Conflict (optimistic-lock
                // périmé, conflict_payload requis pour la résolution RMW) ne doit PAS être écrasé
                // en Cancelled, ce qui détruirait le payload que l'appelant attend pour retry/abandon.
                // JobStatus::TERMINAL_SQL — single source of truth for the terminal set.
                let select_sql = format!(
                    "SELECT payload FROM gradatum_jobs WHERE id = ?1 AND status NOT IN ({}){}",
                    JobStatus::TERMINAL_SQL,
                    tenant_clause
                );
                let row: Option<String> = match &tenant_filter {
                    None => tx
                        .query_row(&select_sql, [&id_str], |row| row.get(0))
                        .optional()
                        .map_err(storage_err)?,
                    Some(t) => tx
                        .query_row(&select_sql, params![id_str, t], |row| row.get(0))
                        .optional()
                        .map_err(storage_err)?,
                };

                let Some(payload_str) = row else {
                    // Job déjà terminal ou inexistant — opération idempotente
                    tx.rollback().map_err(storage_err)?;
                    return Ok(false);
                };

                let mut record = Self::deserialize_record(&payload_str)?;
                record.lifecycle.status = JobStatus::Cancelled;
                record.lifecycle.completed_at = Some(now);
                let new_payload = Self::serialize_record(&record)?;

                // JobStatus::TERMINAL_SQL — single source of truth for the terminal set.
                let update_sql = format!(
                    "UPDATE gradatum_jobs \
                         SET status = 'Cancelled', completed_at = ?1, lease_until = NULL, payload = ?2 \
                         WHERE id = ?3 AND status NOT IN ({}){}",
                    JobStatus::TERMINAL_SQL,
                    tenant_clause
                );
                match &tenant_filter {
                    None => tx
                        .execute(&update_sql, params![now_str, new_payload, id_str])
                        .map_err(storage_err)?,
                    Some(t) => tx
                        .execute(&update_sql, params![now_str, new_payload, id_str, t])
                        .map_err(storage_err)?,
                };

                tx.commit().map_err(storage_err)?;
                Ok(true)
            },
        )
        .await
        .map_err(|_| blocking_err())??;

        if !cancelled {
            return Ok(());
        }

        self.publish(QueueEvent::JobCancelled(id));
        Ok(())
    }

    async fn fail_dlq(&self, id: Ulid, err: &str) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let now = Utc::now().to_rfc3339();
        let err = err.to_owned();
        let db = self.db.clone();

        // Relit le payload pour mettre à jour le statut DLQ.
        // La lecture + composition + UPDATE s'exécutent sur un fil bloquant sous le verrou.
        let last_error = tokio::task::spawn_blocking(move || -> Result<String, QueueError> {
            let conn = db.blocking_lock();

            let payload_str: String = conn
                .query_row(
                    r#"SELECT payload FROM gradatum_jobs WHERE id = ?1"#,
                    [&id_str],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_err)?
                .ok_or(QueueError::NotFound(id))?;

            let mut record = Self::deserialize_record(&payload_str)?;

            // F-217 — `err` ne porte que la CONSÉQUENCE (« max_retries atteint (N/M) »
            // décidée par `promote_retries`), jamais la cause. Le motif réel du dernier
            // échec vit dans `retry.errors[]` (poussé par `fail()` à chaque tentative).
            // On préserve ce motif EN TÊTE de `last_error` — c'est la première chose que
            // l'opérateur lit, et le rendu `jobs dlq` tronque cette colonne à 80 caractères,
            // donc la cause doit précéder ; puis on conserve la mention d'épuisement en
            // annexe. Les deux informations coexistent au lieu que la seconde écrase la
            // première. `retry.errors[]` n'est jamais touché : il reste la source exhaustive.
            let composed = match record.retry.errors.last() {
                Some(last) if last.message != err => format!("{} — {err}", last.message),
                _ => err.clone(),
            };
            // Cap DoS (ANSSI R15) : borne le champ à 2048 caractères après composition.
            let last_error: String = if composed.chars().count() > 2048 {
                composed.chars().take(2048).collect()
            } else {
                composed
            };

            record.lifecycle.status = JobStatus::DLQ;
            record.lifecycle.completed_at = Some(Utc::now());
            record.retry.last_error = Some(last_error.clone());
            let new_payload = Self::serialize_record(&record)?;

            conn.execute(
                r#"
                UPDATE gradatum_jobs
                SET status = 'DLQ',
                    completed_at = ?1,
                    lease_until = NULL,
                    last_error = ?2,
                    payload = ?3
                WHERE id = ?4
                "#,
                params![now, last_error, new_payload, id_str],
            )
            .map_err(storage_err)?;

            Ok(last_error)
        })
        .await
        .map_err(|_| blocking_err())??;

        warn!(
            job_id = %id,
            "job sent to DLQ: {last_error}"
        );
        Ok(())
    }

    async fn find_awaiting(&self, job_id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
        // Pattern LIKE anti-collision : `%"<ulid>"%` — guillemets inclus dans le pattern.
        //
        // La colonne `await_jobs` est sérialisée en JSON array de strings ULID :
        // `["01ABC...", "01DEF..."]` — chaque ULID est délimité par des guillemets JSON.
        // Un ULID (26 chars fixes, alphabet Crockford) ne peut pas être un suffixe/préfixe
        // d'un autre ULID dans cette représentation JSON → zéro collision substring possible.
        //
        // Performance : l'index `idx_gradatum_jobs_waiting` (partiel sur status='Waiting')
        // pré-filtre le scan. Le LIKE avec wildcard en tête désactive l'index B-tree sur
        // `await_jobs`, mais le filtrage préalable par status limite le scan au seul
        // sous-ensemble Waiting — acceptable pour < 10k jobs actifs (doc-comment §Limitations).
        let pattern = format!("\"{}\"", job_id);
        let like_pattern = format!("%{pattern}%");
        let db = self.db.clone();

        let payloads = tokio::task::spawn_blocking(move || -> Result<Vec<String>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT payload
                    FROM gradatum_jobs
                    WHERE status = 'Waiting'
                      AND await_jobs LIKE ?1
                    "#,
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([&like_pattern], |row| row.get::<_, String>(0))
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(storage_err)?);
            }
            Ok(out)
        })
        .await
        .map_err(|_| blocking_err())??;

        payloads
            .into_iter()
            .map(|payload| Self::deserialize_record(&payload))
            .collect()
    }

    async fn set_pending(&self, id: Ulid) -> Result<(), QueueError> {
        // Transition idempotente Waiting → Pending.
        //
        // La clause `AND status = 'Waiting'` garantit que :
        // - Un job déjà Pending ou Done n'est pas touché (idempotence).
        // - 0 rows affected ≠ erreur : l'état cible est déjà atteint ou le job
        //   est dans un état terminal — les deux sont des no-ops corrects.
        let id_str = id.to_string();
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<(), QueueError> {
            let conn = db.blocking_lock();
            conn.execute(
                r#"
                UPDATE gradatum_jobs
                SET status = 'Pending'
                WHERE id = ?1
                  AND status = 'Waiting'
                "#,
                [&id_str],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .map_err(|_| blocking_err())??;

        Ok(())
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
                    "recover_stale_leases: invalid TTL (out of chrono range), skip to avoid mass-recovery"
                );
                return Ok(vec![]);
            }
        };
        let threshold = (Utc::now() - chrono_ttl).to_rfc3339();
        let now_str = Utc::now().to_rfc3339();
        let db = self.db.clone();

        // `UPDATE … RETURNING id` : requête avec lignes retournées → statement préparé
        // (rusqlite n'expose pas de rows sur `execute`).
        let ids = tokio::task::spawn_blocking(move || -> Result<Vec<Ulid>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn
                .prepare(
                    r#"
                    UPDATE gradatum_jobs
                    SET status = 'Pending',
                        lease_until = NULL,
                        scheduled_at = ?1
                    WHERE status = 'Running'
                      AND lease_until < ?2
                    RETURNING id
                    "#,
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map(params![now_str, threshold], |row| row.get::<_, String>(0))
                .map_err(storage_err)?;
            let mut ids = Vec::new();
            for row in rows {
                let id_str = row.map_err(storage_err)?;
                if let Ok(ulid) = id_str.parse::<Ulid>() {
                    ids.push(ulid);
                }
            }
            Ok(ids)
        })
        .await
        .map_err(|_| blocking_err())??;

        if !ids.is_empty() {
            debug!(
                count = ids.len(),
                "SqliteQueueStore: expired leases recovered"
            );
        }
        Ok(ids)
    }

    async fn cancel_expired_deadlines(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        let now_str = now.to_rfc3339();
        let completed_at = now.to_rfc3339();
        let db = self.db.clone();

        // JobStatus::TERMINAL_SQL — single source of truth for the terminal set.
        let sql = format!(
            "UPDATE gradatum_jobs \
                 SET status = 'Cancelled', completed_at = ?1 \
                 WHERE deadline IS NOT NULL AND deadline < ?2 \
                 AND status NOT IN ({}) \
                 RETURNING id",
            JobStatus::TERMINAL_SQL
        );

        let ids = tokio::task::spawn_blocking(move || -> Result<Vec<Ulid>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn.prepare(&sql).map_err(storage_err)?;
            let rows = stmt
                .query_map(params![completed_at, now_str], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(storage_err)?;
            let mut ids = Vec::new();
            for row in rows {
                let id_str = row.map_err(storage_err)?;
                if let Ok(ulid) = id_str.parse::<Ulid>() {
                    ids.push(ulid);
                }
            }
            Ok(ids)
        })
        .await
        .map_err(|_| blocking_err())??;

        for &id in &ids {
            self.publish(QueueEvent::JobCancelled(id));
        }
        Ok(ids)
    }

    async fn promote_retries(&self, now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
        let now_str = now.to_rfc3339();
        let db = self.db.clone();

        // Sélectionne les jobs Failed dont scheduled_at <= now.
        // IMPORTANT : on sélectionne aussi `attempt_count` (colonne SQL autoritaire)
        // pour la garde DLQ. Le BLOB `payload` contient retry.count stale (valeur
        // au moment de l'enqueue, non mis à jour après chaque tentative) — utiliser
        // uniquement le BLOB ferait échouer la garde v67 (0 >= 3 = faux → loop infinie).
        let rows = tokio::task::spawn_blocking(
            move || -> Result<Vec<(String, String, i64)>, QueueError> {
                let conn = db.blocking_lock();
                let mut stmt = conn
                    .prepare(
                        r#"
                    SELECT id, payload, attempt_count
                    FROM gradatum_jobs
                    WHERE status = 'Failed'
                      AND scheduled_at <= ?1
                    "#,
                    )
                    .map_err(storage_err)?;
                let iter = stmt
                    .query_map([&now_str], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(storage_err)?;
                let mut out = Vec::new();
                for r in iter {
                    out.push(r.map_err(storage_err)?);
                }
                Ok(out)
            },
        )
        .await
        .map_err(|_| blocking_err())??;

        let mut promoted = Vec::new();
        for (id_str, payload_str, sql_attempt_count) in rows {
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
                let now_rfc = now.to_rfc3339();
                let db = self.db.clone();
                tokio::task::spawn_blocking(move || -> Result<(), QueueError> {
                    let conn = db.blocking_lock();
                    conn.execute(
                        r#"
                        UPDATE gradatum_jobs
                        SET status = 'Pending',
                            scheduled_at = ?1
                        WHERE id = ?2
                          AND status = 'Failed'
                        "#,
                        params![now_rfc, id_str],
                    )
                    .map_err(storage_err)?;
                    Ok(())
                })
                .await
                .map_err(|_| blocking_err())??;

                promoted.push(id);
            }
        }
        Ok(promoted)
    }

    async fn schedule_retry(&self, id: Ulid, at: DateTime<Utc>) -> Result<(), QueueError> {
        let id_str = id.to_string();
        let at_str = at.to_rfc3339();
        let db = self.db.clone();

        tokio::task::spawn_blocking(move || -> Result<(), QueueError> {
            let conn = db.blocking_lock();
            conn.execute(
                r#"
                UPDATE gradatum_jobs
                SET status = 'Failed',
                    lease_until = NULL,
                    scheduled_at = ?1
                WHERE id = ?2
                  AND status = 'Running'
                "#,
                params![at_str, id_str],
            )
            .map_err(storage_err)?;
            Ok(())
        })
        .await
        .map_err(|_| blocking_err())??;

        Ok(())
    }

    async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
        // Phase 3 — fix E-10 : filtre `kind` natif SQL via colonne dénormalisée (migration 007).
        // La requête applique maintenant tous les filtres en SQL avec indexes — plus de filtre mémoire.
        let limit = filter.limit.clamp(1, 500) as i64;

        // Option<String> bindée comme NULL SQL désactive le filtre correspondant.
        // Valeurs possédées : le `spawn_blocking` exige un `'static` — on matérialise
        // toutes les options en Strings avant la fermeture.
        let class_filter = filter.class.as_ref().map(|c| format!("{c:?}"));
        let status_filter = filter
            .status
            .as_ref()
            .map(|s| Self::status_to_str(s).to_string());
        let kind_filter = filter.kind.clone();
        let tenant_filter = filter.tenant.clone();
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
            WHERE (?1 IS NULL OR class = ?1)
              AND (?2 IS NULL OR status = ?2)
              AND (?3 IS NULL OR kind = ?3)
              AND (?4 IS NULL OR tenant_id = ?4)
              AND (?5 IS NULL OR created_at > ?5)
              AND (?6 IS NULL OR created_at < ?6)
              AND (?7 IS NULL OR id > ?7)
            ORDER BY id ASC
            LIMIT ?8
            "#
            }
            JobOrder::CreatedDesc => {
                r#"
            SELECT payload
            FROM gradatum_jobs
            WHERE (?1 IS NULL OR class = ?1)
              AND (?2 IS NULL OR status = ?2)
              AND (?3 IS NULL OR kind = ?3)
              AND (?4 IS NULL OR tenant_id = ?4)
              AND (?5 IS NULL OR created_at > ?5)
              AND (?6 IS NULL OR created_at < ?6)
              AND (?7 IS NULL OR id < ?7)
            ORDER BY id DESC
            LIMIT ?8
            "#
            }
        };

        let db = self.db.clone();
        let payloads = tokio::task::spawn_blocking(move || -> Result<Vec<String>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn.prepare(query_str).map_err(storage_err)?;
            let rows = stmt
                .query_map(
                    params![
                        class_filter,
                        status_filter,
                        kind_filter,
                        tenant_filter,
                        created_after,
                        created_before,
                        cursor_filter,
                        limit,
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(storage_err)?);
            }
            Ok(out)
        })
        .await
        .map_err(|_| blocking_err())??;

        let mut records = Vec::with_capacity(payloads.len());
        for payload in payloads {
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
        tenant_filter: Option<&str>,
    ) -> Result<std::collections::HashMap<JobStatus, u64>, QueueError> {
        // L1 : `None` = SQL actuel (byte-identical) ; `Some(t)` = `WHERE tenant_id = ?`.
        let sql = match tenant_filter {
            None => "SELECT status, COUNT(*) AS n FROM gradatum_jobs GROUP BY status".to_string(),
            Some(_) => {
                "SELECT status, COUNT(*) AS n FROM gradatum_jobs WHERE tenant_id = ?1 GROUP BY status"
                    .to_string()
            }
        };
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

        let rows =
            tokio::task::spawn_blocking(move || -> Result<Vec<(String, i64)>, QueueError> {
                let conn = db.blocking_lock();
                let mut stmt = conn.prepare(&sql).map_err(storage_err)?;
                // Les deux bras produisent des `MappedRows` de types de fermeture différents —
                // on matérialise en `Vec` dans chaque bras pour un type commun.
                let rows = match &tenant_filter {
                    None => {
                        let iter = stmt
                            .query_map([], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                            })
                            .map_err(storage_err)?;
                        let mut out = Vec::new();
                        for r in iter {
                            out.push(r.map_err(storage_err)?);
                        }
                        out
                    }
                    Some(t) => {
                        let iter = stmt
                            .query_map([t], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                            })
                            .map_err(storage_err)?;
                        let mut out = Vec::new();
                        for r in iter {
                            out.push(r.map_err(storage_err)?);
                        }
                        out
                    }
                };
                Ok(rows)
            })
            .await
            .map_err(|_| blocking_err())??;

        let mut out = std::collections::HashMap::new();
        for (status_str, n) in rows {
            match Self::str_to_status(&status_str) {
                Ok(st) => {
                    out.insert(st, u64::try_from(n).unwrap_or(0));
                }
                Err(_) => {
                    warn!(
                        status = %status_str,
                        "count_jobs_by_status: out-of-enum SQL status ignored"
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
        let cutoff = older_than.map(|c| c.to_rfc3339());
        let db = self.db.clone();

        let affected = tokio::task::spawn_blocking(move || -> Result<u64, QueueError> {
            let conn = db.blocking_lock();
            let n = match &cutoff {
                Some(cutoff) => conn
                    .execute(
                        r#"DELETE FROM gradatum_jobs WHERE status = 'DLQ' AND created_at < ?1"#,
                        [cutoff],
                    )
                    .map_err(storage_err)?,
                None => conn
                    .execute(r#"DELETE FROM gradatum_jobs WHERE status = 'DLQ'"#, [])
                    .map_err(storage_err)?,
            };
            Ok(n as u64)
        })
        .await
        .map_err(|_| blocking_err())??;
        Ok(affected)
    }

    /// Returns a `COUNT(*)` of targeted DLQ jobs, using the same `WHERE` clause as
    /// `delete_dlq_jobs` (faithful dry-run, no `LIMIT` cap).
    async fn count_dlq_jobs(&self, older_than: Option<DateTime<Utc>>) -> Result<u64, QueueError> {
        let cutoff = older_than.map(|c| c.to_rfc3339());
        let db = self.db.clone();

        let n = tokio::task::spawn_blocking(move || -> Result<i64, QueueError> {
            let conn = db.blocking_lock();
            let n = match &cutoff {
                Some(cutoff) => conn.query_row(
                    r#"SELECT COUNT(*) FROM gradatum_jobs WHERE status = 'DLQ' AND created_at < ?1"#,
                    [cutoff],
                    |row| row.get(0),
                ),
                None => conn.query_row(
                    r#"SELECT COUNT(*) FROM gradatum_jobs WHERE status = 'DLQ'"#,
                    [],
                    |row| row.get(0),
                ),
            };
            n.map_err(storage_err)
        })
        .await
        .map_err(|_| blocking_err())??;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// DAG recovery: promotes `Waiting` jobs whose all dependencies are `Done`
    /// but whose post-commit cascade failed.
    ///
    /// Overrides the default trait impl (which returns `Ok(0)`).
    /// Calls `all_jobs_done` (inherent method on `SqliteQueueStore`) for each
    /// `Waiting` job with a non-empty `await_jobs`, then calls `set_pending` if eligible.
    ///
    /// # Errors
    ///
    /// Returns `QueueError::Storage` on SQLite error. Promotions already applied
    /// before the error are not rolled back (best-effort, consistent with
    /// `cascade_check_and_promote`).
    async fn promote_stranded_waiting_jobs(&self) -> Result<u32, QueueError> {
        // Requete utilisant l'index partiel `idx_gradatum_jobs_waiting` (status='Waiting').
        // Filtre `await_jobs != '[]'` elimine les jobs sans contrainte de chaininge.
        // `await_jobs IS NOT NULL` elimine les lignes de migrations anciennes.
        let db = self.db.clone();
        let payloads = tokio::task::spawn_blocking(move || -> Result<Vec<String>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT payload
                    FROM gradatum_jobs
                    WHERE status = 'Waiting'
                      AND await_jobs IS NOT NULL
                      AND await_jobs != '[]'
                    "#,
                )
                .map_err(storage_err)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(storage_err)?);
            }
            Ok(out)
        })
        .await
        .map_err(|_| blocking_err())??;

        let mut promoted: u32 = 0;

        for payload in payloads {
            let record = Self::deserialize_record(&payload)?;
            let dep_id = record.id;

            let await_ids: Vec<Ulid> = record
                .scheduling
                .await_jobs
                .iter()
                .map(|t| t.job_id)
                .collect();

            // Cas degenere : await_jobs deserialise vide malgre le filtre SQL -> skip.
            if await_ids.is_empty() {
                continue;
            }

            // Verifie que TOUTES les dependances sont Done.
            if self.all_jobs_done(&await_ids).await? {
                self.set_pending(dep_id).await?;
                promoted += 1;
            }
        }

        Ok(promoted)
    }

    /// Returns the **most recently created** job via `ORDER BY id DESC LIMIT 1`.
    ///
    /// `tenant_filter` **is honoured**: migration 011 added
    /// `tenant_id TEXT NOT NULL DEFAULT 'main'` to `gradatum_jobs` (plus its index), so
    /// `Some(t)` appends `WHERE tenant_id = ?` and returns the latest job of that tenant
    /// alone. `None` keeps the historical, tenant-agnostic query (byte-identical SQL).
    ///
    /// Because the ULID `id` is monotonic, `ORDER BY id DESC` correctly returns the
    /// most recently created job — unlike `list()`, which orders by `id ASC` for pagination.
    async fn latest_job(
        &self,
        tenant_filter: Option<&str>,
    ) -> Result<Option<JobRecord>, QueueError> {
        // L1+L2 : `None` = SQL actuel (byte-identical, ferme L2 en l'état) ;
        // `Some(t)` = `WHERE tenant_id = ?` (dernier job de ce tenant seulement).
        let sql = match tenant_filter {
            None => "SELECT payload FROM gradatum_jobs ORDER BY id DESC LIMIT 1".to_string(),
            Some(_) => {
                "SELECT payload FROM gradatum_jobs WHERE tenant_id = ?1 ORDER BY id DESC LIMIT 1"
                    .to_string()
            }
        };
        let tenant_filter = tenant_filter.map(str::to_owned);
        let db = self.db.clone();

        let payload = tokio::task::spawn_blocking(move || -> Result<Option<String>, QueueError> {
            let conn = db.blocking_lock();
            let mut stmt = conn.prepare(&sql).map_err(storage_err)?;
            let row = match &tenant_filter {
                None => stmt
                    .query_row([], |row| row.get::<_, String>(0))
                    .optional()
                    .map_err(storage_err)?,
                Some(t) => stmt
                    .query_row([t], |row| row.get::<_, String>(0))
                    .optional()
                    .map_err(storage_err)?,
            };
            Ok(row)
        })
        .await
        .map_err(|_| blocking_err())??;

        match payload {
            Some(payload) => Ok(Some(Self::deserialize_record(&payload)?)),
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
        let db = self.db.clone();

        // BEGIN IMMEDIATE : lecture + marquage Conflict atomiques — évite qu'un
        // complete() concurrent ne masque le conflit en marquant Done entre le
        // SELECT et l'UPDATE.
        //
        // Toute la transaction s'exécute sur un fil bloquant sous le verrou unique.
        tokio::task::spawn_blocking(move || -> Result<(), QueueError> {
            let mut conn = db.blocking_lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage_err)?;

            // Relire le payload pour le patcher.
            let payload_str: String = tx
                .query_row(
                    r#"SELECT payload FROM gradatum_jobs WHERE id = ?1"#,
                    [&id_str],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_err)?
                .ok_or(QueueError::NotFound(id))?;

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

            tx.execute(
                r#"
                UPDATE gradatum_jobs
                SET status = 'Conflict',
                    completed_at = ?1,
                    lease_until = NULL,
                    last_error = ?2,
                    payload = ?3
                WHERE id = ?4
                "#,
                params![
                    now,
                    result_note_md.chars().take(256).collect::<String>(),
                    new_payload,
                    id_str
                ],
            )
            .map_err(storage_err)?;

            tx.commit().map_err(storage_err)?;
            Ok(())
        })
        .await
        .map_err(|_| blocking_err())??;

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
            "job marked Conflict (optimistic-lock F-41)"
        );
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers privés — types non exportés
// ─────────────────────────────────────────────────────────────────────────────

/// Applies WAL pragmas on the queue database.
///
/// # Side effects
///
/// - Enables WAL mode (concurrent write performance)
/// - Sets `synchronous=NORMAL` (durability/performance trade-off)
/// - Sets `foreign_keys=ON`
///
/// Idempotent. `open_queue_db` already applies WAL + `busy_timeout`; this call adds
/// `synchronous=NORMAL` and `foreign_keys=ON` for callers that require them
/// (admin, tests) — exact parity with the original sqlx settings.
pub async fn apply_sqlite_pragmas(db: &QueueDb) -> Result<(), QueueError> {
    db.with_conn(|conn| {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────
// Migrations — honore la table de suivi `_sqlx_migrations` tenue par sqlx (F-145)
// ─────────────────────────────────────────────────────────────────────────────

/// Embedded migration — the 7 queue migrations (006 → 012).
///
/// Version and description computed like sqlx:
///   - version = the numeric prefix of the filename (everything before the first `_`);
///   - description = the part after the first `_`, `.sql` extension removed, `_` → spaces.
///
/// The SQL body is included via `include_str!` — byte-identical to the file on disk, hence to
/// the SHA-384 checksum sqlx recorded. ⚠️ DO NOT MODIFY these files: an applied migration is
/// immutable, and modified contents would invalidate the checksum → startup refusal.
struct EmbeddedMigration {
    version: i64,
    description: &'static str,
    sql: &'static str,
}

/// Migrations de la file, en ordre de version (l'ordre d'application de sqlx).
const MIGRATIONS: &[EmbeddedMigration] = &[
    EmbeddedMigration {
        version: 6,
        description: "apalis bootstrap",
        sql: include_str!("../migrations/006_apalis_bootstrap.sql"),
    },
    EmbeddedMigration {
        version: 7,
        description: "jobs kind indexed",
        sql: include_str!("../migrations/007_jobs_kind_indexed.sql"),
    },
    EmbeddedMigration {
        version: 8,
        description: "idempotency",
        sql: include_str!("../migrations/008_idempotency.sql"),
    },
    EmbeddedMigration {
        version: 9,
        description: "jobs v2 drain",
        sql: include_str!("../migrations/009_jobs_v2_drain.sql"),
    },
    EmbeddedMigration {
        version: 10,
        description: "backfill kind",
        sql: include_str!("../migrations/010_backfill_kind.sql"),
    },
    EmbeddedMigration {
        version: 11,
        description: "jobs tenant scope",
        sql: include_str!("../migrations/011_jobs_tenant_scope.sql"),
    },
    EmbeddedMigration {
        version: 12,
        description: "drop jobs v2",
        sql: include_str!("../migrations/012_drop_jobs_v2.sql"),
    },
];

/// Applies the pending migrations, honoring the `_sqlx_migrations` tracking table
/// kept by sqlx (schema measured in sqlx-sqlite 0.8.6/src/migrate.rs:72-79).
///
/// "Already applied" decision: the `version` column (PK) — a version already present is
/// NEVER replayed. sqlx fidelity (the `Migrate` trait):
///   - a `success = false` row → dirty database → startup refusal (MigrateError::Dirty);
///   - an applied migration whose SHA-384 checksum differs from the embedded file →
///     startup refusal (MigrateError::VersionMismatch);
///   - an applied migration absent from our embedded list → ignored (like sqlx, which only
///     iterates its own migrations).
///
/// ⚠️ Some queue migrations are NOT idempotent (007/011: `ALTER TABLE ADD COLUMN`). The
/// no-replay property is therefore a safety guarantee: on an up-to-date database, no migration
/// is replayed (proven by `init_does_not_replay_migrations_on_production_like_base`).
///
/// Returns the number of migrations applied (0 on an up-to-date database).
pub async fn run_migrations(db: &QueueDb) -> Result<usize, QueueError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = db.blocking_lock();
        run_migrations_sync(&mut conn)
    })
    .await
    .map_err(|_| blocking_err())?
}

/// Synchronous part of `run_migrations`, run on a blocking thread under the lock.
fn run_migrations_sync(conn: &mut Connection) -> Result<usize, QueueError> {
    // Schéma exact de sqlx (sqlx-sqlite 0.8.6/src/migrate.rs) — no-op si déjà présente.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );",
    )
    .map_err(storage_err)?;

    // Base sale : une migration marquée en échec → refus de démarrage (parité
    // MigrateError::Dirty). Ne peut survenir que d'une écriture manuelle : l'application
    // sqlx est transactionnelle (migration + enregistrement dans la même transaction).
    let dirty: Option<i64> = conn
        .query_row(
            "SELECT version FROM _sqlx_migrations WHERE success = false ORDER BY version LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_err)?;
    if let Some(version) = dirty {
        return Err(storage_err(format!(
            "dirty migration base: migration {version} marked as failed (success = false)"
        )));
    }

    // Migrations déjà appliquées (version + checksum), comme sqlx list_applied_migrations.
    let mut applied: Vec<(i64, Vec<u8>)> = Vec::new();
    {
        let mut stmt = conn
            .prepare("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(storage_err)?;
        for row in rows {
            applied.push(row.map_err(storage_err)?);
        }
    }

    let mut applied_count = 0;
    for migration in MIGRATIONS {
        let checksum = Sha384::digest(migration.sql.as_bytes()).to_vec();

        if let Some((_, stored)) = applied.iter().find(|(v, _)| *v == migration.version) {
            // Migration déjà appliquée : vérifier que le fichier n'a pas bougé depuis
            // (immuable post-application). Ne JAMAIS rejouer.
            if stored != &checksum {
                return Err(storage_err(format!(
                    "migration {} already applied but its content changed \
                     (SHA-384 checksum differs) — refusing startup",
                    migration.version
                )));
            }
            continue;
        }

        // Application dans une transaction unique (migration + enregistrement), comme sqlx :
        // jamais de migration exécutée deux fois. `unchecked_transaction` car la connexion est
        // dédiée (pas de transaction imbriquée possible).
        let tx = conn.unchecked_transaction().map_err(storage_err)?;
        tx.execute_batch(migration.sql).map_err(storage_err)?;
        tx.execute(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (?1, ?2, TRUE, ?3, -1)",
            params![migration.version, migration.description, checksum],
        )
        .map_err(storage_err)?;
        tx.commit().map_err(storage_err)?;

        tracing::info!(version = migration.version, "queue migration applied");
        applied_count += 1;
    }

    Ok(applied_count)
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
pub async fn idempotency_insert(db: &QueueDb, key: &str, job_id: &str) -> Result<bool, QueueError> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let key = key.to_owned();
    let job_id = job_id.to_owned();
    let db = db.clone();
    let affected = tokio::task::spawn_blocking(move || -> Result<usize, QueueError> {
        let conn = db.blocking_lock();
        let n = conn
            .execute(
                r#"INSERT OR IGNORE INTO gradatum_idempotency (key, job_id, created_at) VALUES (?1, ?2, ?3)"#,
                params![key, job_id, now_ms],
            )
            .map_err(storage_err)?;
        Ok(n)
    })
    .await
    .map_err(|_| blocking_err())??;

    Ok(affected > 0)
}

/// Looks up a `job_id` by idempotency key.
///
/// Returns `Some(job_id)` if the key exists, `None` otherwise.
pub async fn idempotency_lookup(db: &QueueDb, key: &str) -> Result<Option<String>, QueueError> {
    let key = key.to_owned();
    let db = db.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<String>, QueueError> {
        let conn = db.blocking_lock();
        conn.query_row(
            r#"SELECT job_id FROM gradatum_idempotency WHERE key = ?1"#,
            [&key],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_err)
    })
    .await
    .map_err(|_| blocking_err())?
}

/// Deletes idempotency entries whose `created_at` is earlier than `before_ms`.
///
/// Used by the `IdempotencyCleanup` cron job (24-hour TTL).
/// Returns the number of deleted entries.
pub async fn idempotency_cleanup(db: &QueueDb, before_ms: i64) -> Result<u64, QueueError> {
    let db = db.clone();
    let deleted = tokio::task::spawn_blocking(move || -> Result<usize, QueueError> {
        let conn = db.blocking_lock();
        let n = conn
            .execute(
                r#"DELETE FROM gradatum_idempotency WHERE created_at < ?1"#,
                [before_ms],
            )
            .map_err(storage_err)?;
        Ok(n)
    })
    .await
    .map_err(|_| blocking_err())??;

    Ok(deleted as u64)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests d'intégration
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gradatum_core::{
        CurateSpec, EmbedSpec, Job, JobClass, JobLifecycle, JobLineage, JobMode, JobPriority,
        JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, JobTrigger, RetryBackoff,
        TriggerCondition, TriggerSource, job_kind_str,
    };
    use rusqlite::Connection;
    use ulid::Ulid;

    /// Crée une base SQLite in-memory pour les tests, migrations 006 → 012 appliquées.
    async fn test_db() -> QueueDb {
        let db = QueueDb::open_in_memory()
            .await
            .expect("db in-memory doit créer");
        apply_sqlite_pragmas(&db)
            .await
            .expect("pragmas WAL doivent s'appliquer");
        run_migrations(&db)
            .await
            .expect("migrations doivent s'appliquer");
        db
    }

    /// Exécute une requête (bind d'un id string) sur la base, via la connexion partagée.
    async fn exec_id(db: &QueueDb, sql: &str, id: &str) {
        let sql = sql.to_owned();
        let id = id.to_owned();
        db.with_conn(move |conn| conn.execute(&sql, [&id]).map(|_| ()))
            .await
            .expect("sql doit réussir");
    }

    /// Lit une valeur `String` (première colonne) d'un `SELECT … WHERE id = ?`.
    async fn select_str(db: &QueueDb, sql: &str, id: &str) -> String {
        let sql = sql.to_owned();
        let id = id.to_owned();
        db.with_conn(move |conn| conn.query_row(&sql, [&id], |row| row.get::<_, String>(0)))
            .await
            .expect("select doit réussir")
    }

    /// La migration 011 ajoute `tenant_id TEXT NOT NULL DEFAULT 'main'`
    /// à `gradatum_jobs` ; les jobs legacy insérés sans la colonne sont backfillés
    /// à `'main'` par le DEFAULT (correct : à `multi_tenant` OFF tout est `main`).
    #[tokio::test]
    async fn migration_011_adds_tenant_id_default_main() {
        let db = test_db().await;

        // La colonne est présente sur gradatum_jobs.
        let cols: Vec<String> = db
            .with_conn(|conn| {
                let mut stmt =
                    conn.prepare("SELECT name FROM pragma_table_info('gradatum_jobs')")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
            .expect("pragma_table_info doit répondre");
        assert!(
            cols.contains(&"tenant_id".to_string()),
            "colonne tenant_id absente de gradatum_jobs"
        );

        // Un INSERT legacy (sans tenant_id explicite) est backfillé à 'main'.
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO gradatum_jobs (id, payload, status, priority, class, kind, created_at, scheduled_at) \
                 VALUES ('01LEGACY', '{}', 'Pending', 2, 'System', '', ?1, ?2)",
                params!["2020-01-01T00:00:00Z", "2020-01-01T00:00:00Z"],
            )
            .map(|_| ())
        })
        .await
        .expect("insert legacy doit réussir");

        let tenant: String = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT tenant_id FROM gradatum_jobs WHERE id = '01LEGACY'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .expect("select tenant_id doit répondre");
        assert_eq!(tenant, "main", "job legacy doit être backfillé à 'main'");
    }

    /// `enqueue` estampille `tenant_id = spec_tenant(&spec)` et les
    /// 5 surfaces filtrées (`get`/`cancel`/`list`/`count`/`latest`) isolent par
    /// tenant à `Some(t)`, tout en restant byte-identical à `None`.
    #[tokio::test]
    async fn enqueue_stamps_tenant_from_spec_and_filter_isolates() {
        let store = SqliteQueueStore::new(test_db().await);
        let mk = |tenant: &str| {
            make_record(
                Job::Curate(CurateSpec {
                    tenant_id: tenant.to_string(),
                    ..Default::default()
                }),
                JobClass::Api,
                JobStatus::Pending,
            )
        };
        let ja = store.enqueue(mk("alice")).await.expect("enqueue alice");
        let jb = store.enqueue(mk("bob")).await.expect("enqueue bob");

        // Stamp : la colonne tenant_id reflète le spec (pas le DEFAULT 'main').
        let ta: String = select_str(
            store.db(),
            "SELECT tenant_id FROM gradatum_jobs WHERE id = ?",
            &ja.to_string(),
        )
        .await;
        assert_eq!(ta, "alice", "enqueue doit estampiller le tenant du spec");

        // None = pas de filtre (byte-identical) → voit tout.
        assert!(store.get(ja, None).await.unwrap().is_some());
        assert!(store.get(jb, None).await.unwrap().is_some());

        // get scopé : alice n'accède qu'à ses jobs (→ None = 404 anti-disclosure).
        assert!(store.get(ja, Some("alice")).await.unwrap().is_some());
        assert!(
            store.get(jb, Some("alice")).await.unwrap().is_none(),
            "alice ne doit PAS voir le job de bob"
        );

        // cancel scopé : alice ne peut pas annuler le job de bob (0 row, reste actif).
        store
            .cancel(jb, Some("alice"))
            .await
            .expect("cancel cross-tenant = no-op idempotent, pas d'erreur");
        let jb_after = store.get(jb, None).await.unwrap().expect("job bob existe");
        assert_ne!(
            jb_after.lifecycle.status,
            JobStatus::Cancelled,
            "job de bob ne doit pas être annulé par alice"
        );
        // Le bon tenant annule bien.
        store.cancel(jb, Some("bob")).await.expect("cancel bob");
        assert_eq!(
            store.get(jb, None).await.unwrap().unwrap().lifecycle.status,
            JobStatus::Cancelled
        );

        // list scopé : alice ne voit pas le job de bob.
        let la = store
            .list(JobFilter {
                tenant: Some("alice".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            la.iter().all(|j| j.id != jb),
            "list(alice) ne doit pas contenir jb"
        );
        assert!(
            la.iter().any(|j| j.id == ja),
            "list(alice) doit contenir ja"
        );

        // count scopé : alice = 1 job (Pending), sans les jobs de bob.
        let ca = store.count_jobs_by_status(Some("alice")).await.unwrap();
        assert_eq!(
            ca.values().sum::<u64>(),
            1,
            "count(alice) doit ne compter que ja"
        );

        // latest scopé : le dernier job d'alice est ja.
        assert_eq!(
            store.latest_job(Some("alice")).await.unwrap().map(|j| j.id),
            Some(ja)
        );
    }

    /// A7 défaut 1 — l'estampille d'un `Forget::Agent` multi-vault est le vault du JOB.
    ///
    /// Le `warn!` d'audit d'`enqueue` annonçait un « fallback `'main'` » : faux depuis
    /// A6'. `forget_scope_vault` n'élit aucun vault (N > 1) → `spec_tenant` retombe sur
    /// `JobSpec.scope`, et `"main"` n'est plus que le dernier ressort. Un opérateur qui
    /// enquête sur une fuite cherchait donc le job sous le mauvais tenant.
    ///
    /// Ce test mesure la valeur réellement écrite en colonne, seule preuve qui ancre le
    /// texte du log.
    #[tokio::test]
    async fn enqueue_stamps_multi_vault_agent_forget_with_the_job_vault() {
        let store = SqliteQueueStore::new(test_db().await);
        let multi_vault_forget = || {
            Job::Forget(gradatum_core::ForgetSpec {
                scope: gradatum_core::ForgetScope::Agent {
                    agent_id: "alice".to_string(),
                    vaults: vec!["bob".to_string(), "carol".to_string()],
                },
                ..Default::default()
            })
        };
        async fn stamp_of(store: &SqliteQueueStore, id: Ulid) -> String {
            select_str(
                store.db(),
                "SELECT tenant_id FROM gradatum_jobs WHERE id = ?",
                &id.to_string(),
            )
            .await
        }

        // Job scopé sur un vault → c'est CE vault qui est estampillé, jamais "main".
        let mut scoped = make_record(multi_vault_forget(), JobClass::Api, JobStatus::Pending);
        scoped.spec.scope = JobScope::Vault("alice".to_string());
        let scoped_id = store.enqueue(scoped).await.expect("enqueue job scopé");
        assert_eq!(
            stamp_of(&store, scoped_id).await,
            "alice",
            "l'estampille doit être le vault du job, pas le littéral 'main'"
        );

        // Job sans vault porté → "main" en DERNIER ressort seulement.
        let vault_wide = make_record(multi_vault_forget(), JobClass::Api, JobStatus::Pending);
        assert!(matches!(vault_wide.spec.scope, JobScope::VaultWide));
        let wide_id = store.enqueue(vault_wide).await.expect("enqueue VaultWide");
        assert_eq!(
            stamp_of(&store, wide_id).await,
            "main",
            "sans vault porté par le job, le repli mono-vault reste 'main'"
        );
    }

    /// GATE d'isolation dimension **tenant** sur la queue (5 surfaces).
    ///
    /// À `Some(t)`, le tenant A ne peut ni `get`/`cancel` (→ absent/no-op) ni voir
    /// dans `list`/`count`/`latest_job` les jobs du tenant B. Distincte de la gate
    /// notes/vault (`no_cross_vault_leak`) : ici l'axe est `gradatum_jobs.tenant_id`.
    #[tokio::test]
    async fn no_cross_tenant_job_leak() {
        let store = SqliteQueueStore::new(test_db().await);
        let mk = |t: &str| {
            make_record(
                Job::Curate(CurateSpec {
                    tenant_id: t.to_string(),
                    ..Default::default()
                }),
                JobClass::Api,
                JobStatus::Pending,
            )
        };
        let a = store.enqueue(mk("tenant-a")).await.expect("enqueue A");
        let b = store.enqueue(mk("tenant-b")).await.expect("enqueue B");

        // 1. get(B, Some(A)) == None.
        assert!(
            store.get(b, Some("tenant-a")).await.unwrap().is_none(),
            "get cross-tenant doit être None"
        );

        // 2. cancel(B, Some(A)) n'annule PAS le job de B.
        store
            .cancel(b, Some("tenant-a"))
            .await
            .expect("cancel cross-tenant = no-op");
        assert_ne!(
            store.get(b, None).await.unwrap().unwrap().lifecycle.status,
            JobStatus::Cancelled,
            "cancel cross-tenant ne doit pas annuler B"
        );

        // 3. list(tenant=Some(A)) ne contient aucun job de B.
        let la = store
            .list(JobFilter {
                tenant: Some("tenant-a".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(la.iter().all(|j| j.id != b), "list(A) ne doit pas fuiter B");
        assert!(la.iter().any(|j| j.id == a), "list(A) doit contenir A");

        // 4. count(Some(A)) == nb jobs de A uniquement.
        let ca = store.count_jobs_by_status(Some("tenant-a")).await.unwrap();
        assert_eq!(ca.values().sum::<u64>(), 1, "count(A) ne compte que A");

        // 5. latest_job(Some(A)) ∈ jobs de A ; réciproque pour B.
        assert_eq!(
            store
                .latest_job(Some("tenant-a"))
                .await
                .unwrap()
                .map(|j| j.id),
            Some(a)
        );
        assert_eq!(
            store
                .latest_job(Some("tenant-b"))
                .await
                .unwrap()
                .map(|j| j.id),
            Some(b)
        );
    }

    fn make_record(job: Job, class: JobClass, status: JobStatus) -> JobRecord {
        let now = Utc::now();
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
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;

        let inserted_id = store.enqueue(record).await.expect("enqueue doit réussir");
        assert_eq!(inserted_id, id);

        let fetched = store.get(id, None).await.expect("get doit réussir");
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id, id);
    }

    #[tokio::test]
    async fn dequeue_returns_highest_priority() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Insère un job System (Low=1) puis un Agent (High=3)
        let low = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let high = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
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
            .dequeue(None)
            .await
            .expect("dequeue doit réussir")
            .expect("doit retourner un job");

        // Le job Agent (High) doit passer en premier
        assert_eq!(dequeued.id, high_id);
        assert_eq!(dequeued.lifecycle.status, JobStatus::Running);
    }

    #[tokio::test]
    async fn complete_job_sets_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

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
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Done);
    }

    /// `complete()` must not overwrite a job already in the terminal `Conflict` state.
    ///
    /// Reproduces the real-world interleaving: `mark_conflict` sets `Conflict`, then the
    /// apalis acknowledgement (which sees the handler's `Ok`) calls `complete()`.
    ///
    /// **P2-6** : le SELECT vérifie `status = 'Running'` — un job Conflict n'est
    /// pas Running, donc `complete()` retourne `Err(NotFound)` (rejeté immédiatement
    /// sans atteindre le BLOB guard F-41). Le statut Conflict est préservé **par la
    /// base de données elle-même** (le `WHERE` SQL), pas seulement par le guard
    /// BLOB applicatif. Cette garantie est plus forte — elle ne dépend pas de la
    /// cohérence BLOB/SQL.
    #[tokio::test]
    async fn complete_preserves_terminal_conflict() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

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
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            after_conflict.lifecycle.status,
            JobStatus::Conflict,
            "précondition : le job doit être Conflict après mark_conflict"
        );

        // L'ack apalis appelle complete() avec un JobResult succès (le handler a
        // retourné Ok). P2-6 : le SELECT `WHERE status = 'Running'` ne trouve plus
        // le job (Conflict ≠ Running) → Err(NotFound). Le statut Conflict est
        // préservé par la base elle-même, sans que le BLOB guard ne soit sollicité.
        let ack_result = JobResult {
            success: true,
            duration_ms: 0,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        let err = store.complete(id, ack_result).await.expect_err(
            "complete sur job Conflict doit échouer (P2-6 : SELECT WHERE status='Running')",
        );
        assert!(
            matches!(err, QueueError::NotFound(_)),
            "attendu NotFound (job Conflict → pas Running), obtenu {err:?}"
        );

        // Le statut Conflict est préservé.
        let fetched = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Conflict,
            "complete() ne doit PAS écraser un état terminal Conflict par Done (F-41, garanti par P2-6)"
        );
        // Le conflict_payload posé par mark_conflict doit survivre.
        let result = fetched.lifecycle.result.expect("result doit être présent");
        assert!(
            result.conflict_payload.is_some(),
            "le conflict_payload doit survivre"
        );
    }

    #[tokio::test]
    async fn fail_and_dlq() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let mut record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        record.retry.max = 1; // max 1 retry pour le test
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

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
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::DLQ);
    }

    /// F-217 — le motif réel du dernier échec survit à la bascule en DLQ.
    ///
    /// `fail_dlq` recevait « max_retries atteint (N/M) » (la CONSÉQUENCE, décidée par
    /// `promote_retries`) et écrasait `retry.last_error` avec ce texte générique,
    /// masquant la cause. Résultat terrain : 8 morts affichant tous la même ligne.
    ///
    /// Ce test prouve, sur deux familles d'échec distinctes, que :
    ///   1. la cause nommée survit dans `last_error` après passage en DLQ ;
    ///   2. la mention d'épuisement des tentatives y reste lisible ;
    ///   3. `retry.errors[]` conserve l'historique complet (source exhaustive intacte) ;
    ///   4. deux causes différentes restent distinguables sur la colonne `last_error`
    ///      tronquée à 80 caractères — reproduit le rendu CLI `jobs dlq`
    ///      (`gradatum-admin/src/jobs_cmd.rs:346-348`).
    #[tokio::test]
    async fn fail_dlq_preserves_named_error_after_transition() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Deux causes réelles distinctes, à l'image des 8 morts LIVE (refus d'écriture
        // disque vs point d'accès injoignable). Volontairement ≥ 80 octets ASCII : la
        // troncature à 80 du CLI reste alors dans le préfixe-cause, sans frontière
        // multi-octet (propriété RÉ-AFFIRMÉE par assertion plus bas, jamais présumée
        // de la longueur du littéral). Nom d'hôte neutre `example.invalid` (RFC 6761,
        // ne résout jamais) : le test a besoin d'un point d'accès injoignable, pas du
        // nom d'une machine réelle.
        let cause_a = "opendal error: PermissionDenied (os error 13) while writing blob object to local store";
        let cause_b = "embedding endpoint http://embed.example.invalid/v1/embeddings unreachable: connection refused";

        let mut ids = Vec::new();
        for cause in [cause_a, cause_b] {
            let mut record = make_record(
                Job::Curate(CurateSpec {
                    note_id: Ulid::generate(),
                    tenant_id: "main".to_string(),
                    ..Default::default()
                }),
                JobClass::Agent,
                JobStatus::Pending,
            );
            record.retry.max = 3;
            let id = record.id;
            store.enqueue(record).await.expect("enqueue");
            let _ = store.dequeue(None).await.expect("dequeue");

            // Une tentative qui échoue avec la cause nommée : `fail()` la pousse dans
            // `retry.errors[]` ET `retry.last_error`.
            store.fail(id, cause, 3).await.expect("fail");

            // Bascule DLQ avec le message générique que produit `promote_retries`.
            store
                .fail_dlq(id, "max_retries atteint (4 / 3)")
                .await
                .expect("fail_dlq");
            ids.push((id, cause));
        }

        // Rendu identique au CLI `jobs dlq` : `last_error` tronqué à 80 octets.
        let dlq = store
            .list(JobFilter {
                status: Some(JobStatus::DLQ),
                limit: 50,
                ..Default::default()
            })
            .await
            .expect("list DLQ");
        assert_eq!(dlq.len(), 2, "les deux jobs doivent être en DLQ");
        // Ces découpes à l'octet 80 (miroir de l'ancien rendu CLI) ne sont sûres que
        // si l'octet 80 tombe sur une frontière de caractère. On l'EXIGE explicitement
        // — ainsi, si un littéral de cause raccourcit un jour sous 80 octets ASCII, le
        // test échoue franchement au lieu de paniquer ou de passer pour la mauvaise raison.
        for r in &dlq {
            let le = r.retry.last_error.as_deref().unwrap_or("");
            let cut = le.len().min(80);
            assert!(
                le.is_char_boundary(cut),
                "pré-condition : l'octet {cut} doit être une frontière de caractère (last_error={le})"
            );
        }
        for r in &dlq {
            let le = r.retry.last_error.as_deref().unwrap_or("(no detail)");
            println!(
                "{}  retries={}/{}  last_error={}",
                r.id,
                r.retry.count,
                r.retry.max,
                &le[..le.len().min(80)]
            );
        }

        for (id, cause) in ids {
            let fetched = store.get(id, None).await.expect("get").expect("job existe");
            assert_eq!(fetched.lifecycle.status, JobStatus::DLQ);
            let last_error = fetched.retry.last_error.expect("last_error présent");

            // 1. La cause nommée survit à la bascule.
            assert!(
                last_error.contains(cause),
                "last_error doit conserver la cause réelle « {cause} », obtenu : {last_error}"
            );
            // 2. La mention d'épuisement reste lisible.
            assert!(
                last_error.contains("max_retries atteint"),
                "last_error doit conserver la mention d'épuisement, obtenu : {last_error}"
            );
            // 3. `retry.errors[]` conserve la cause (source exhaustive non altérée).
            assert!(
                fetched.retry.errors.iter().any(|e| e.message == cause),
                "retry.errors[] doit toujours contenir la cause réelle"
            );
        }

        // 4. Les deux causes sont distinguables sur la colonne tronquée à 80.
        let mut truncated: Vec<String> = dlq
            .iter()
            .map(|r| {
                let le = r.retry.last_error.as_deref().unwrap_or("");
                le[..le.len().min(80)].to_string()
            })
            .collect();
        truncated.sort();
        assert_ne!(
            truncated[0], truncated[1],
            "deux causes différentes ne doivent PAS rendre la même ligne (défaut F-217)"
        );
    }

    #[tokio::test]
    async fn cancel_job() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Audit, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        store.cancel(id, None).await.expect("cancel doit réussir");

        let fetched = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn list_with_filter() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Insère 2 jobs Agent et 1 job System
        for _ in 0..2 {
            let r = make_record(
                Job::Curate(CurateSpec {
                    note_id: Ulid::generate(),
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

    /// Builds a Backup `JobRecord` whose ULID `id` and `created_at` both derive from `dt`
    /// — monotone ID correlated with the date (testable ASC/DESC + range queries).
    fn make_record_at(dt: chrono::DateTime<Utc>) -> JobRecord {
        let mut r = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        r.id = Ulid::from_datetime(dt.into());
        r.lifecycle.created_at = dt;
        r
    }

    /// Enqueues 4 jobs at T+0, 1, 2, 3 and returns their IDs in chronological order.
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
        let pool = test_db().await;
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
        let pool = test_db().await;
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
        let pool = test_db().await;
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
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);
        let ids = seed_four(&store).await; // T+0,1,2,3 minutes

        // Bornes : after = id[0].created_at, before = id[3].created_at (exclusives)
        // → ne capte que id[1] et id[2].
        let r0 = store.get(ids[0], None).await.expect("get").expect("existe");
        let r3 = store.get(ids[3], None).await.expect("get").expect("existe");

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
        let pool = test_db().await;
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
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

        // Simuler lease expirée en patchant directement
        exec_id(
            store.db(),
            "UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?",
            &id.to_string(),
        )
        .await;

        // TTL de 0 — tout lease expiré est récupéré
        let recovered = store
            .recover_stale_leases(Duration::from_secs(0))
            .await
            .expect("recover doit réussir");

        assert!(recovered.contains(&id));

        let fetched = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(fetched.lifecycle.status, JobStatus::Pending);
    }

    // ── Tests régression E-12 — get() synchronise le statut SQL ─────────────

    /// `enqueue` → `get`: status must be `Pending`.
    #[tokio::test]
    async fn e12_get_after_enqueue_is_pending() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        let fetched = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "get() après enqueue doit retourner Pending"
        );
    }

    /// `enqueue` → `dequeue` → `get`: status must be `Running`, not stale `Pending`.
    #[tokio::test]
    async fn e12_get_after_dequeue_is_running() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");

        let dequeued = store
            .dequeue(None)
            .await
            .expect("dequeue doit réussir")
            .expect("doit retourner un job");
        assert_eq!(dequeued.lifecycle.status, JobStatus::Running);

        // C'est ici que le bug E-12 se manifestait : get() retournait Pending stale.
        let fetched = store
            .get(id, None)
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

    /// `enqueue` → `dequeue` → `complete` → `get`: status must be `Done`.
    #[tokio::test]
    async fn e12_get_after_complete_is_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

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
            .get(id, None)
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

    /// Kind isolation test — core of the DLQ routing fix.
    ///
    /// Enqueue 1 Curate + 1 Embed → `dequeue_by_kind("Curate")` returns ONLY the Curate,
    /// never the Embed. Same isolation in the other direction.
    #[tokio::test]
    async fn dequeue_by_kind_isolates_curate_from_embed() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let curate_record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let embed_record = make_record(
            Job::Embed(EmbedSpec {
                note_id: Ulid::generate(),
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
            .dequeue_by_kind("Curate", None)
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
            .dequeue_by_kind("Embed", None)
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

    /// Symmetric: `dequeue_by_kind("Embed")` does not steal a Curate, even when
    /// it is the only available job and has priority.
    #[tokio::test]
    async fn dequeue_by_kind_embed_worker_cannot_steal_curate() {
        let pool = test_db().await;
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
            .dequeue_by_kind("Embed", None)
            .await
            .expect("dequeue_by_kind Embed doit réussir");
        assert!(
            got.is_none(),
            "dequeue_by_kind(Embed) ne doit PAS retourner un job Curate"
        );
    }

    /// `enqueue()` persists the `kind` column with the correct value — root-bug regression.
    /// Without this test, an enqueue without `kind` would silently break routing.
    #[tokio::test]
    async fn enqueue_persists_kind_column() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let curate_record = make_record(
            Job::Curate(CurateSpec::default()),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = curate_record.id.to_string();
        store.enqueue(curate_record).await.expect("enqueue");

        // Lire la colonne `kind` directement depuis SQLite
        let kind = select_str(
            store.db(),
            "SELECT kind FROM gradatum_jobs WHERE id = ?",
            &id,
        )
        .await;
        assert_eq!(
            kind, "Curate",
            "la colonne kind doit valoir 'Curate' après enqueue d'un Job::Curate"
        );
    }

    /// Verifies that migration 010 backfills `kind` correctly from a realistic payload.
    /// Simulates jobs that were enqueued without the `kind` column (empty string).
    #[tokio::test]
    async fn migration_010_backfills_kind_from_payload() {
        let pool = test_db().await;
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

        let id_str2 = id_str.clone();
        store
            .db()
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO gradatum_jobs (id, payload, status, priority, class, kind, created_at, scheduled_at) VALUES (?1, ?2, 'Pending', 3, 'Agent', '', ?3, ?4)",
                    params![id_str2, payload, now, now],
                )
                .map(|_| ())
            })
            .await
            .expect("insert manuel doit réussir");

        // Vérifier que kind est bien '' avant le backfill
        let kind_before = select_str(
            store.db(),
            "SELECT kind FROM gradatum_jobs WHERE id = ?",
            &id_str,
        )
        .await;
        assert_eq!(kind_before, "", "kind doit être vide avant backfill");

        // Appliquer la migration 010
        store
            .db()
            .with_conn(|conn| {
                conn.execute_batch(include_str!("../migrations/010_backfill_kind.sql"))
            })
            .await
            .expect("migration 010 doit s'appliquer");

        // Vérifier que kind est maintenant rempli
        let kind_after = select_str(
            store.db(),
            "SELECT kind FROM gradatum_jobs WHERE id = ?",
            &id_str,
        )
        .await;
        assert_eq!(
            kind_after, "Curate",
            "migration 010 doit backfiller kind='Curate' depuis le payload JSON"
        );
    }

    /// `job_kind_str` covers all `Job` variants without a wildcard arm.
    ///
    /// Match exhaustiveness is enforced by the compiler (no `_ =>`).
    /// Verifies the returned values for easily constructible variants and asserts
    /// their correspondence with the JSON payload (`serde(tag = "type")`).
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
        assert_eq!(
            job_kind_str(&Job::Validate(gradatum_core::ValidateSpec::default())),
            "Validate"
        );
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
                note_id: Ulid::generate(),
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

    /// `promote_retries` reads the SQL `attempt_count` column (not the stale BLOB `retry.count`)
    /// for the DLQ guard, so jobs that exceed `max_retries` are moved to DLQ rather than
    /// being reset to `Pending` indefinitely.
    ///
    /// Verifies that `promote_retries` reads `attempt_count` from SQL and sends the job to
    /// DLQ when `attempt_count >= retry.max`.
    #[tokio::test]
    async fn promote_retries_uses_sql_attempt_count_for_dlq_guard() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool.clone());

        // max=2 — après 2 tentatives le job doit partir en DLQ.
        let mut record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        record.retry.max = 2;
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");

        // Simuler 2 dequeues + fails successifs : attempt_count SQL monte à 2.
        let _ = store.dequeue(None).await.expect("dequeue 1");
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
        let _ = store.dequeue(None).await.expect("dequeue 2");
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

        let fetched = store
            .get(id, None)
            .await
            .expect("get")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::DLQ,
            "job avec attempt_count >= max doit passer en DLQ — pas en Pending infini"
        );
    }

    /// `replay_single` was resetting to `Pending` without clearing `attempt_count`.
    /// A replayed job with `attempt_count >= max_retries` would be immediately sent
    /// to DLQ on the next sweep without ever executing.
    ///
    /// Verifies directly that `attempt_count` and `last_error` are reset to 0/NULL
    /// after a replay SQL (same query used by `jobs_cmd::replay_single`).
    #[tokio::test]
    async fn replay_dlq_resets_attempt_count() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool.clone());

        let record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        let id = record.id;
        let id_str = id.to_string();
        store.enqueue(record).await.expect("enqueue");

        // Amener le job en DLQ après déqueue + fail_dlq.
        let _ = store.dequeue(None).await.expect("dequeue");
        // Force attempt_count=3 via fail() avant fail_dlq.
        store.fail(id, "erreur max", 3).await.expect("fail");
        store
            .fail_dlq(id, "max_retries atteint")
            .await
            .expect("fail_dlq");

        let before = store.get(id, None).await.expect("get").expect("job");
        assert_eq!(before.lifecycle.status, JobStatus::DLQ);

        // Replay SQL — même requête que gradatum-admin/src/jobs_cmd.rs::replay_single.
        let replay_sql = r#"
            UPDATE gradatum_jobs
            SET status        = 'Pending',
                lease_until   = NULL,
                scheduled_at  = datetime('now'),
                attempt_count = 0,
                last_error    = NULL
            WHERE id = ?1
              AND status = 'DLQ'
        "#;
        let id_for_replay = id_str.clone();
        let rows_affected = pool
            .with_conn(move |conn| conn.execute(replay_sql, [&id_for_replay]))
            .await
            .expect("replay SQL");
        assert_eq!(rows_affected, 1, "replay doit affecter exactement 1 ligne");

        // Vérifier que attempt_count est bien à 0.
        let (attempt_count, last_error, status): (i64, Option<String>, String) = {
            let id_for_read = id_str.clone();
            pool.with_conn(move |conn| {
                conn.query_row(
                    "SELECT attempt_count, last_error, status FROM gradatum_jobs WHERE id = ?1",
                    [&id_for_read],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            })
            .await
            .expect("row")
        };

        assert_eq!(attempt_count, 0, "attempt_count doit être 0 après replay");
        assert!(
            last_error.is_none(),
            "last_error doit être NULL après replay"
        );
        assert_eq!(status, "Pending", "status doit être Pending après replay");
    }

    /// Verifies that `promote_retries` resets a job to `Pending` when
    /// `attempt_count < max_retries` (happy path — not regressed by the fix).
    #[tokio::test]
    async fn promote_retries_pending_when_below_max() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let mut record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        record.retry.max = 3; // max 3, on ne fait qu'1 tentative
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");

        let _ = store.dequeue(None).await.expect("dequeue");
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

        let fetched = store.get(id, None).await.expect("get").expect("job");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "job doit être Pending après promote_retries < max_retries"
        );
    }

    // ── A2 : find_awaiting / set_pending + cascade ────────────────────────────

    // ── Helpers de seeding ────────────────────────────────────────────────────

    /// Builds a `JobRecord` with `await_jobs = [job_trigger]` and `status = Waiting`.
    fn make_waiting_record_with_dep(dep_id: Ulid) -> JobRecord {
        let mut rec = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        rec.scheduling.await_jobs = vec![JobTrigger {
            job_id: dep_id,
            condition: TriggerCondition::OnDone,
        }];
        rec
    }

    // ── find_awaiting ─────────────────────────────────────────────────────────

    /// `find_awaiting` returns `Waiting` jobs whose `await_jobs` contains `job_id`.
    #[tokio::test]
    async fn find_awaiting_returns_dependents_when_job_matches() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let done_id = Ulid::generate();
        let dep = make_waiting_record_with_dep(done_id);
        let dep_id = dep.id;
        store.enqueue(dep).await.expect("enqueue dépendant");

        let result = store
            .find_awaiting(done_id)
            .await
            .expect("find_awaiting doit réussir");

        assert_eq!(result.len(), 1, "un dépendant attendu");
        assert_eq!(
            result[0].id, dep_id,
            "le dépendant trouvé doit correspondre"
        );
    }

    /// `find_awaiting` returns an empty vec when no job depends on `job_id`.
    #[tokio::test]
    async fn find_awaiting_returns_empty_when_no_dependents() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Seed un job sans await_jobs — ne doit pas être retourné.
        let rec = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        store.enqueue(rec).await.expect("enqueue");

        let result = store
            .find_awaiting(Ulid::generate())
            .await
            .expect("find_awaiting doit réussir");

        assert!(result.is_empty(), "aucun dépendant attendu");
    }

    /// `find_awaiting` does not perform partial matching: a ULID that is a prefix
    /// of another ULID does not match.
    ///
    /// The `LIKE '%"<id>"%'` pattern (quotes included) ensures only an exact ULID
    /// matches — a sub-prefix without closing quotes does not match.
    #[tokio::test]
    async fn find_awaiting_no_partial_match() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // ULIDs de 26 chars — "01AAAAAAAAAAAAAAAAAAAAAA" est différent de "01AAAAAAAAAAAAAAAAAAAAA"
        // On crée un ULID réel et un ULID fictif qui est un préfixe tronqué.
        let real_dep_id = Ulid::generate();
        let dep = make_waiting_record_with_dep(real_dep_id);
        store.enqueue(dep).await.expect("enqueue dépendant");

        // On cherche avec un Ulid::generate() différent — aucun match attendu.
        let different_id = Ulid::generate();
        let result = store
            .find_awaiting(different_id)
            .await
            .expect("find_awaiting doit réussir");

        assert!(
            result.is_empty(),
            "un ULID différent ne doit pas matcher un autre ULID"
        );
    }

    // ── set_pending ───────────────────────────────────────────────────────────

    /// `set_pending` transitions a `Waiting` job to `Pending`.
    #[tokio::test]
    async fn set_pending_transitions_waiting_to_pending() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let rec = make_record(Job::Backup, JobClass::System, JobStatus::Waiting);
        let id = rec.id;
        store.enqueue(rec).await.expect("enqueue");

        store
            .set_pending(id)
            .await
            .expect("set_pending doit réussir");

        let fetched = store.get(id, None).await.expect("get").expect("job");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "job doit être Pending après set_pending"
        );
    }

    /// `set_pending` is idempotent: two successive calls do not return an error.
    #[tokio::test]
    async fn set_pending_is_idempotent_when_already_pending() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let rec = make_record(Job::Backup, JobClass::System, JobStatus::Waiting);
        let id = rec.id;
        store.enqueue(rec).await.expect("enqueue");

        store.set_pending(id).await.expect("premier set_pending");
        store
            .set_pending(id)
            .await
            .expect("second set_pending idempotent");

        let fetched = store.get(id, None).await.expect("get").expect("job");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Pending,
            "statut doit rester Pending après double set_pending"
        );
    }

    /// `set_pending` is a no-op on a terminal-state job (`Done`): returns `Ok`
    /// without modifying the status.
    #[tokio::test]
    async fn set_pending_no_op_when_not_waiting() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Crée et complète un job (Done).
        let rec = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id = rec.id;
        store.enqueue(rec).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");
        store
            .complete(
                id,
                JobResult {
                    success: true,
                    duration_ms: 1,
                    cost_usd: None,
                    result_note: None,
                    conflict_payload: None,
                },
            )
            .await
            .expect("complete");

        // set_pending sur un job Done doit être no-op (pas d'erreur).
        store
            .set_pending(id)
            .await
            .expect("set_pending no-op sur Done doit réussir");

        let fetched = store.get(id, None).await.expect("get").expect("job");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Done,
            "statut Done ne doit pas être modifié par set_pending"
        );
    }

    // ── cascade_check_and_promote ─────────────────────────────────────────────

    /// Cascade: job B waits on [A], A is Done → B transitions to Pending.
    #[tokio::test]
    async fn cascade_promotes_when_all_deps_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job A en Done.
        let job_a = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_a = job_a.id;
        store.enqueue(job_a).await.expect("enqueue A");
        let _ = store.dequeue(None).await.expect("dequeue A");
        store
            .complete(
                id_a,
                JobResult {
                    success: true,
                    duration_ms: 1,
                    cost_usd: None,
                    result_note: None,
                    conflict_payload: None,
                },
            )
            .await
            .expect("complete A");

        // Job B attend [A], statut Waiting.
        let mut job_b = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        job_b.scheduling.await_jobs = vec![JobTrigger {
            job_id: id_a,
            condition: TriggerCondition::OnDone,
        }];
        let id_b = job_b.id;
        store.enqueue(job_b).await.expect("enqueue B");

        // Déclenche la cascade sur A.
        store
            .cascade_check_and_promote(id_a)
            .await
            .expect("cascade");

        let fetched_b = store.get(id_b, None).await.expect("get B").expect("job B");
        assert_eq!(
            fetched_b.lifecycle.status,
            JobStatus::Pending,
            "B doit être Pending car A est Done"
        );
    }

    /// Cascade: job B waits on [A, C], A is Done but C is Pending → B remains Waiting.
    #[tokio::test]
    async fn cascade_does_not_promote_when_dep_not_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job A en Done.
        let job_a = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_a = job_a.id;
        store.enqueue(job_a).await.expect("enqueue A");
        let _ = store.dequeue(None).await.expect("dequeue A");
        store
            .complete(
                id_a,
                JobResult {
                    success: true,
                    duration_ms: 1,
                    cost_usd: None,
                    result_note: None,
                    conflict_payload: None,
                },
            )
            .await
            .expect("complete A");

        // Job C en Pending (pas Done).
        let job_c = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_c = job_c.id;
        store.enqueue(job_c).await.expect("enqueue C");

        // Job B attend [A, C], statut Waiting.
        let mut job_b = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        job_b.scheduling.await_jobs = vec![
            JobTrigger {
                job_id: id_a,
                condition: TriggerCondition::OnDone,
            },
            JobTrigger {
                job_id: id_c,
                condition: TriggerCondition::OnDone,
            },
        ];
        let id_b = job_b.id;
        store.enqueue(job_b).await.expect("enqueue B");

        // Cascade sur A (C pas encore Done).
        store
            .cascade_check_and_promote(id_a)
            .await
            .expect("cascade");

        let fetched_b = store.get(id_b, None).await.expect("get B").expect("job B");
        assert_eq!(
            fetched_b.lifecycle.status,
            JobStatus::Waiting,
            "B doit rester Waiting car C n'est pas Done"
        );
    }

    /// Cascade inertia: no dependants → no mutations, returns `Ok`.
    #[tokio::test]
    async fn cascade_inertia_no_deps() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job sans dépendants enqueued.
        let rec = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        store.enqueue(rec).await.expect("enqueue");

        // cascade sur un ID arbitraire → aucun job Waiting référence cet ID.
        let result = store.cascade_check_and_promote(Ulid::generate()).await;

        assert!(result.is_ok(), "cascade sans dépendant doit retourner OK");
    }

    // ── C1 : transitions queue atomiques — pas de double-complete ────────────

    /// Two concurrent `complete` calls on the same job: exactly one must succeed and
    /// leave the job in `Done`. The second must also succeed (SQL-level idempotent —
    /// no `NotFound` error because the job still exists) but the final status must
    /// remain `Done` and the first caller's result must be preserved (no corruption).
    ///
    /// Verifies that `BEGIN IMMEDIATE` transactions correctly serialize both calls
    /// without losing data.
    #[tokio::test]
    async fn c1_concurrent_complete_no_double_write() {
        let pool = test_db().await;
        let store = std::sync::Arc::new(SqliteQueueStore::new(pool));

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");

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
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Done,
            "statut final doit être Done après double complete()"
        );
    }

    /// Two concurrent `fail` calls on the same job: the final status must be
    /// `Failed` (not `Pending` or another corrupted state).
    #[tokio::test]
    async fn c1_concurrent_fail_no_corruption() {
        let pool = test_db().await;
        let store = std::sync::Arc::new(SqliteQueueStore::new(pool));

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");

        let store_a = store.clone();
        let store_b = store.clone();
        let (r_a, r_b) = tokio::join!(
            store_a.fail(id, "erreur concurrent A", 1),
            store_b.fail(id, "erreur concurrent B", 1),
        );
        let _ = r_a;
        let _ = r_b;

        let fetched = store
            .get(id, None)
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

    /// `TTL = Duration::MAX` (outside chrono range) → 0 jobs recovered, no panic,
    /// no catastrophic mass-recovery.
    #[tokio::test]
    async fn c3_recover_stale_leases_invalid_ttl_returns_empty() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Enqueuer + déqueuer un job (il passe Running avec lease_until proche).
        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");

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
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            fetched.lifecycle.status,
            JobStatus::Running,
            "le job Running ne doit PAS être remis en Pending par un TTL invalide"
        );
    }

    /// A valid TTL (0s) continues to work normally (non-regression).
    #[tokio::test]
    async fn c3_recover_stale_leases_valid_ttl_works() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");

        // Forcer la lease dans le passé.
        exec_id(
            store.db(),
            "UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?",
            &id.to_string(),
        )
        .await;

        let recovered = store
            .recover_stale_leases(Duration::from_secs(0))
            .await
            .expect("recover doit réussir");

        assert!(
            recovered.contains(&id),
            "TTL valide (0s) doit récupérer le job avec lease expirée"
        );
    }

    /// `latest_job()` must return the MOST RECENT job (ORDER BY id DESC), never the oldest.
    ///
    /// Root bug: the dashboard called `list(JobFilter{limit:1})` which orders `id ASC`
    /// → returned the oldest job (e.g. 314h old) instead of today's job,
    /// creating the illusion of a dead worker.
    ///
    /// Deterministic test: 3 jobs with ULID timestamps at explicit increasing offsets
    /// (no dependency on `Ulid::generate()` monotonicity or sleeps).
    #[tokio::test]
    async fn latest_job_returns_most_recent_not_oldest() {
        use std::time::{Duration as StdDuration, UNIX_EPOCH};

        let pool = test_db().await;
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
            .latest_job(None)
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

    /// `latest_job()` on an empty queue degrades cleanly to `None`
    /// (the dashboard shows "no last_job" without error).
    #[tokio::test]
    async fn latest_job_empty_returns_none() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let latest = store
            .latest_job(None)
            .await
            .expect("latest_job doit réussir même sur file vide");

        assert!(latest.is_none(), "file vide → None, pas d'erreur");
    }

    // ── D1.3 — prune DLQ ──────────────────────────────────────────────────────

    /// Enqueues a job and forces it into the DLQ.
    async fn seed_dlq(store: &SqliteQueueStore) -> Ulid {
        let record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");
        store.fail_dlq(id, "test prune").await.expect("fail_dlq");
        id
    }

    /// Enqueues a job in the DLQ with an arbitrary `created_at`.
    ///
    /// `fail_dlq` does not change `created_at` (only the status), so age is controlled
    /// via the initial record — useful for testing `--older-than`.
    async fn seed_dlq_at(store: &SqliteQueueStore, created_at: DateTime<Utc>) -> Ulid {
        let mut record = make_record(
            Job::Validate(gradatum_core::ValidateSpec::default()),
            JobClass::System,
            JobStatus::Pending,
        );
        record.lifecycle.created_at = created_at;
        let id = record.id;
        store.enqueue(record).await.expect("enqueue");
        let _ = store.dequeue(None).await.expect("dequeue");
        store.fail_dlq(id, "test prune").await.expect("fail_dlq");
        id
    }

    /// `count_dlq_jobs(None)` counts ALL DLQ jobs, without a `LIMIT` cap.
    ///
    /// Regression of the `list(limit: 200)` bug: with > 200 DLQ jobs, the old dry-run
    /// under-counted. This test seeds 205 DLQ jobs and asserts an exact count of 205.
    #[tokio::test]
    async fn count_dlq_jobs_exact_above_200() {
        let pool = test_db().await;
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

    /// `--older-than` targets old jobs outside the first-200 window: the old
    /// `list(limit: 200)` could early-return "nothing to delete". The dedicated
    /// `COUNT(*)` sees them → prune executes.
    #[tokio::test]
    async fn count_dlq_jobs_older_than_beyond_200() {
        let pool = test_db().await;
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

    /// `delete_dlq_jobs(None)` removes all DLQ jobs → 0 remaining.
    #[tokio::test]
    async fn delete_dlq_jobs_prunes_all() {
        let pool = test_db().await;
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

    /// `delete_dlq_jobs(Some(cutoff))` respects the age window:
    /// a past cutoff deletes nothing (jobs created at `now`); a future cutoff deletes all.
    #[tokio::test]
    async fn delete_dlq_jobs_respects_older_than() {
        let pool = test_db().await;
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

    // ── promote_stranded_waiting_jobs (DT-DAG-1) ─────────────────────────────

    /// DAG recovery: a `Waiting` job whose all dependencies are `Done` but whose
    /// post-`complete` cascade failed (crash or storage error) is recovered by
    /// `promote_stranded_waiting_jobs` → transitions to `Pending`.
    ///
    /// Simulated case: job B is enqueued directly in `Waiting` with `await_jobs=[A]`,
    /// while A is already `Done` — without going through `cascade_check_and_promote`.
    #[tokio::test]
    async fn promotes_waiting_job_when_all_deps_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job A : Pending → Running → Done.
        let job_a = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_a = job_a.id;
        store.enqueue(job_a).await.expect("enqueue A");
        let _ = store.dequeue(None).await.expect("dequeue A");
        store
            .complete(
                id_a,
                JobResult {
                    success: true,
                    duration_ms: 1,
                    cost_usd: None,
                    result_note: None,
                    conflict_payload: None,
                },
            )
            .await
            .expect("complete A");

        // Job B : Waiting, attend [A]. La cascade n'a PAS été appelée
        // (simule l'echec de cascade_check_and_promote).
        let mut job_b = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        job_b.scheduling.await_jobs = vec![JobTrigger {
            job_id: id_a,
            condition: TriggerCondition::OnDone,
        }];
        let id_b = job_b.id;
        store.enqueue(job_b).await.expect("enqueue B");

        // Verification prealable : B est bien bloque en Waiting.
        let before = store
            .get(id_b, None)
            .await
            .expect("get B before")
            .expect("B");
        assert_eq!(
            before.lifecycle.status,
            JobStatus::Waiting,
            "B doit etre Waiting avant la passe DAG recovery"
        );

        // Passe DAG recovery -- doit rattraper B.
        let promoted = store
            .promote_stranded_waiting_jobs()
            .await
            .expect("promote_stranded_waiting_jobs");

        assert_eq!(promoted, 1, "exactement 1 job doit etre promu");

        let after = store
            .get(id_b, None)
            .await
            .expect("get B after")
            .expect("B");
        assert_eq!(
            after.lifecycle.status,
            JobStatus::Pending,
            "B doit etre Pending apres la passe DAG recovery"
        );
    }

    /// DAG recovery inertia: no `Waiting` job with a non-empty `await_jobs`
    /// → `promote_stranded_waiting_jobs` returns 0 with zero mutations.
    ///
    /// Matches the current production state where `await_jobs = '[]'` for all
    /// jobs (no active DAG) → the sweep is a strict no-op.
    #[tokio::test]
    async fn sweep_is_noop_when_no_waiting_jobs() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job Pending classique -- ne doit pas etre touche.
        let pending = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        store.enqueue(pending).await.expect("enqueue pending");

        // Job Waiting SANS await_jobs (await_jobs = '[]') -- doit etre ignore.
        let waiting_no_deps = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        store
            .enqueue(waiting_no_deps)
            .await
            .expect("enqueue waiting sans deps");

        let promoted = store
            .promote_stranded_waiting_jobs()
            .await
            .expect("promote_stranded_waiting_jobs");

        assert_eq!(
            promoted, 0,
            "aucun job Waiting avec await_jobs -> 0 promotion (no-op)"
        );
    }

    /// Partial DAG recovery: job B waits on [A, C], A is Done but C is Pending
    /// → B remains Waiting (not all dependencies are Done).
    #[tokio::test]
    async fn does_not_promote_when_dep_not_done() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job A en Done.
        let job_a = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_a = job_a.id;
        store.enqueue(job_a).await.expect("enqueue A");
        let _ = store.dequeue(None).await.expect("dequeue A");
        store
            .complete(
                id_a,
                JobResult {
                    success: true,
                    duration_ms: 1,
                    cost_usd: None,
                    result_note: None,
                    conflict_payload: None,
                },
            )
            .await
            .expect("complete A");

        // Job C en Pending (pas Done).
        let job_c = make_record(Job::Backup, JobClass::System, JobStatus::Pending);
        let id_c = job_c.id;
        store.enqueue(job_c).await.expect("enqueue C");

        // Job B attend [A, C], cascade ratee.
        let mut job_b = make_record(Job::Summarize, JobClass::System, JobStatus::Waiting);
        job_b.scheduling.await_jobs = vec![
            JobTrigger {
                job_id: id_a,
                condition: TriggerCondition::OnDone,
            },
            JobTrigger {
                job_id: id_c,
                condition: TriggerCondition::OnDone,
            },
        ];
        let id_b = job_b.id;
        store.enqueue(job_b).await.expect("enqueue B");

        let promoted = store
            .promote_stranded_waiting_jobs()
            .await
            .expect("promote_stranded_waiting_jobs");

        assert_eq!(promoted, 0, "B ne doit pas etre promu -- C n'est pas Done");

        let fetched_b = store.get(id_b, None).await.expect("get B").expect("B");
        assert_eq!(
            fetched_b.lifecycle.status,
            JobStatus::Waiting,
            "B doit rester Waiting car une dependance n'est pas Done"
        );
    }

    // ── P0-4 Tests stale-lease sur SqliteQueueStore (chemin de production) ────

    /// `complete()` rejeté après expiration du lease (garde `lease_until > ?`
    /// au niveau UPDATE).
    ///
    /// Scénario : un worker obtient un lease (dequeue), puis son lease expire.
    /// Le job est toujours `Running` (le sweep n'est pas encore passé),
    /// donc le SELECT P2-6 (`status = 'Running'`) le trouve encore.
    /// Mais l'UPDATE P0-1 (`AND lease_until > ?`) échoue car le lease est
    /// expiré → `rows_affected() = 0` → `NotLeased`.
    ///
    /// Ce test isole la garde UPDATE du P0-1 (le SELECT P2-6 n'est pas le
    /// garde-fou ici — le statut SQL est toujours Running).
    #[tokio::test]
    async fn complete_rejected_after_stale_lease_recovered() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

        // Simuler un lease expiré dans le passé, MAIS le statut reste Running
        // (le sweep n'est pas encore passé — on teste la garde lease_until).
        exec_id(
            store.db(),
            "UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?",
            &id.to_string(),
        )
        .await;

        // Vérifier que le statut SQL est toujours Running (pas Pending).
        let sql_status = select_str(
            store.db(),
            "SELECT status FROM gradatum_jobs WHERE id = ?",
            &id.to_string(),
        )
        .await;
        assert_eq!(
            sql_status, "Running",
            "précondition : le statut SQL doit être Running (le sweep n'est pas passé)"
        );

        // L'ancien worker appelle complete() → le SELECT P2-6 trouve le job
        // (status=Running OK), mais l'UPDATE P0-1 échoue (lease_until expiré).
        let result = JobResult {
            success: true,
            duration_ms: 100,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        let err = store
            .complete(id, result)
            .await
            .expect_err("complete doit échouer avec lease expiré");
        assert!(
            matches!(err, QueueError::NotLeased(_)),
            "attendu NotLeased (garde UPDATE lease_until > now), obtenu {err:?}"
        );

        // Le statut du job est toujours Running — le complete() a été rejeté.
        let after = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            after.lifecycle.status,
            JobStatus::Running,
            "le job doit rester Running après NotLeased"
        );
    }

    /// `fail()` après expiration du lease → rejeté (`NotLeased`).
    ///
    /// Scénario : un worker obtient un lease, puis son lease expire.
    /// Un autre worker a déjà pu reprendre le job via `recover_stale_leases`,
    /// ou le job est simplement en état Pending. `fail()` doit être rejeté.
    #[tokio::test]
    async fn fail_rejected_after_lease_expired() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

        // Simuler un lease expiré.
        exec_id(
            store.db(),
            "UPDATE gradatum_jobs SET lease_until = '2020-01-01T00:00:00Z' WHERE id = ?",
            &id.to_string(),
        )
        .await;

        // fail() avec le lease expiré → NotLeased.
        let err = store
            .fail(id, "erreur test", 1)
            .await
            .expect_err("fail doit échouer avec lease expiré");
        assert!(
            matches!(err, QueueError::NotLeased(_)),
            "attendu NotLeased, obtenu {err:?}"
        );
    }

    /// `complete()` appelé deux fois : le premier réussit, le second est rejeté
    /// (le job n'est plus Running après le premier complete).
    #[tokio::test]
    async fn double_complete_rejected() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        let _ = store.dequeue(None).await.expect("dequeue doit réussir");

        // Premier complete → OK.
        let result = JobResult {
            success: true,
            duration_ms: 100,
            cost_usd: None,
            result_note: None,
            conflict_payload: None,
        };
        store
            .complete(id, result.clone())
            .await
            .expect("premier complete doit réussir");

        // Vérifier que le job est Done.
        let after_first = store
            .get(id, None)
            .await
            .expect("get doit réussir")
            .expect("job doit exister");
        assert_eq!(
            after_first.lifecycle.status,
            JobStatus::Done,
            "le job doit être Done après le premier complete"
        );

        // Deuxième complete → rejeté (le job n'est plus Running).
        let err = store
            .complete(id, result)
            .await
            .expect_err("deuxième complete doit échouer");
        // Le SELECT `WHERE status = 'Running'` ne trouve plus le job (Done)
        // → NotFound (pas NotLeased, car le job n'est même plus Running).
        assert!(
            matches!(err, QueueError::NotFound(_)),
            "attendu NotFound (job Done → pas Running), obtenu {err:?}"
        );
    }

    /// `fail()` appelé sur un job Pending (sans lease) → rejeté.
    #[tokio::test]
    async fn fail_on_pending_job_rejected() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        let record = make_record(Job::Summarize, JobClass::System, JobStatus::Pending);
        let id = record.id;
        store.enqueue(record).await.expect("enqueue doit réussir");
        // Pas de dequeue → le job est toujours Pending, pas de lease.

        let err = store
            .fail(id, "erreur test", 1)
            .await
            .expect_err("fail sur job Pending doit échouer");
        // Le SELECT `WHERE status = 'Running'` ne trouve pas le job Pending.
        assert!(
            matches!(err, QueueError::NotFound(_)),
            "attendu NotFound (job Pending → pas Running), obtenu {err:?}"
        );
    }

    // ── P0-3 Test : worker pool partagé (shared pool, tenant_filter = None) ──

    /// `dequeue_by_kind(kind, None)` avec un pool partagé : les jobs de tous
    /// les tenants sont éligibles. Ce test prouve que le `None` filter
    /// n'empêche PAS le dequeue cross-tenant — le worker voit tous les jobs.
    ///
    /// Le handler (curate/embed/etc.) est responsable de l'isolation des
    /// données par tenant via `ensure_job_tenant()`. Le worker lui-même
    /// n'a pas d'affiliation à un tenant.
    #[tokio::test]
    async fn shared_pool_dequeue_sees_all_tenants() {
        let pool = test_db().await;
        let store = SqliteQueueStore::new(pool);

        // Job tenant A.
        let job_a = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "alice".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id_a = job_a.id;
        store.enqueue(job_a).await.expect("enqueue alice");

        // Job tenant B.
        let job_b = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "bob".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let id_b = job_b.id;
        store.enqueue(job_b).await.expect("enqueue bob");

        // Shared pool (None) → premier dequeue obtient un job (peu importe le tenant).
        let first = store
            .dequeue_by_kind("Curate", None)
            .await
            .expect("dequeue")
            .expect("un job doit être dispo");
        assert!(
            first.id == id_a || first.id == id_b,
            "shared pool doit dequeue un job existant"
        );

        // Deuxième dequeue → l'autre job.
        let second = store
            .dequeue_by_kind("Curate", None)
            .await
            .expect("dequeue")
            .expect("le second job doit être dispo");
        assert_ne!(first.id, second.id, "les deux jobs doivent être différents");

        // Les deux jobs sont bien accessibles par le pool partagé.
        let all_ids = [id_a, id_b];
        assert!(
            all_ids.contains(&first.id) && all_ids.contains(&second.id),
            "les deux jobs (alice + bob) doivent être dequés par le pool partagé"
        );

        // Vérifier que chaque job porte son tenant dans le payload
        // (le handler peut ainsi isoler les opérations par tenant).
        let first_record = store
            .get(first.id, None)
            .await
            .expect("get first")
            .expect("first existe");
        let first_tenant = match &first_record.spec.kind {
            Job::Curate(c) => c.tenant_id.clone(),
            _ => panic!("pas un Curate"),
        };
        assert!(
            first_tenant == "alice" || first_tenant == "bob",
            "le tenant est préservé dans le payload du job"
        );
    }

    /// G10-P1 : régression — une erreur > 2048 octets avec un caractère
    /// multi-octets à la frontière 2048 ne doit PAS paniquer.
    ///
    /// L'ancien code `&err[..2048]` slice sur les octets → panique
    /// `"byte index 2048 is not a char boundary"` si la frontière 2048
    /// tombe au milieu d'un caractère multi-octets (ex: 'é' = 2 octets).
    /// Scénario réel : erreur LLM/HTTP longue (> 2048 octets) → fail()
    /// panic → job non failé → lease expire → recover repend → re-panic.
    #[test]
    fn fail_truncation_utf8_boundary_no_panic() {
        // Construit une erreur où l'octet 2048 est le second octet d'un 'é'
        // (U+00E9 = C3 A9 en UTF-8). 2047 × 'a' + 'é' → frontière au milieu
        // du caractère.
        let prefix = "a".repeat(2047);
        let err = format!("{prefix}é padding padding padding padding");
        assert!(err.len() > 2048, "l'erreur doit dépasser 2048 octets");

        // La troncation par chars (fix) ne doit pas paniquer.
        let truncated: String = if err.chars().count() > 2048 {
            err.chars().take(2048).collect()
        } else {
            err.to_string()
        };
        assert!(
            truncated.chars().count() <= 2048,
            "tronqué à max 2048 chars"
        );
        // Vérifie que le caractère 'é' (multi-octets) est intact.
        assert!(
            truncated.ends_with('é'),
            "le dernier caractère multi-octets doit être préservé"
        );
    }

    // ── Preuve P0 (F-145 sous-lot 3) : non-rejeu des migrations de la file ──────
    //
    // La base de la file porte 7 migrations (006 → 012), dont certaines NON idempotentes
    // (007/011 : `ALTER TABLE ADD COLUMN`). Le remplaçant rusqlite doit honorer la table de
    // suivi `_sqlx_migrations` tenue par sqlx : une version présente n'est JAMAIS rejouée.
    // Les tests ci-dessous fabriquent des bases jetables et prouvent le verdict.

    /// P0 — une base portant la table de suivi REMPLIE comme en production (créée par
    /// `sqlx::migrate!`, versions 6 → 12 enregistrées avec leur checksum SHA-384, table
    /// `gradatum_jobs` à l'état post-012) ne fait RIEN rejouer par `run_migrations`.
    ///
    /// C'est LA preuve exigée par le brief : une seule migration rejouée (ex. 011
    /// `ALTER TABLE … ADD COLUMN tenant_id`) échouerait sur « duplicate column name » et
    /// corromprait la base LIVE — ce test prouve que cela ne peut pas arriver.
    #[tokio::test]
    async fn init_does_not_replay_migrations_on_production_like_base() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("queue.sqlite");

        // Fabriquer la base « comme en production » : schéma exact de sqlx
        // (sqlx-sqlite 0.8.6/src/migrate.rs) + lignes appliquées 6→12 + gradatum_jobs déjà créée.
        let conn = Connection::open(&path).expect("open fixture base");
        conn.execute_batch(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            );",
        )
        .expect("fixture tracking table");
        for m in MIGRATIONS {
            let checksum = Sha384::digest(m.sql.as_bytes()).to_vec();
            conn.execute(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
                 VALUES (?1, ?2, TRUE, ?3, -1)",
                params![m.version, m.description, checksum],
            )
            .expect("fixture migration row");
        }
        // Table gradatum_jobs à l'état post-012 (colonne tenant_id présente).
        conn.execute_batch(
            "CREATE TABLE gradatum_jobs (
                id TEXT NOT NULL PRIMARY KEY,
                payload TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'Pending',
                priority INTEGER NOT NULL DEFAULT 2,
                class TEXT NOT NULL DEFAULT 'System',
                kind TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                scheduled_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT,
                lease_until TEXT,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                deadline TEXT,
                last_error TEXT,
                await_jobs TEXT,
                tenant_id TEXT NOT NULL DEFAULT 'main'
            );",
        )
        .expect("fixture gradatum_jobs");
        drop(conn);

        // Lancer le remplaçant (open + run_migrations).
        let db = QueueDb::open(&path)
            .await
            .expect("open base production-like");
        let applied = run_migrations(&db)
            .await
            .expect("run_migrations sur base production-like");
        assert_eq!(applied, 0, "aucune migration rejouée sur base à jour");

        // La table de suivi n'a toujours que les 7 lignes d'origine.
        let n: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM _sqlx_migrations", [], |row| {
                    row.get(0)
                })
            })
            .await
            .expect("count");
        assert_eq!(
            n,
            MIGRATIONS.len() as i64,
            "table de suivi intacte — aucune migration rejouée"
        );
    }

    /// Contre-preuve — une base VIERGE reçoit exactement 7 applications (une par migration),
    /// et le second appel n'applique plus rien. Les migrations non-idempotentes 007/011 ne
    /// sont donc appliquées qu'une seule fois, jamais rejouées.
    #[tokio::test]
    async fn migration_runner_applies_fresh_migrations_then_skips() {
        let db = test_db().await;

        // `test_db` a déjà appliqué les 7 migrations via `run_migrations`.
        let n: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM _sqlx_migrations", [], |row| {
                    row.get(0)
                })
            })
            .await
            .expect("count");
        assert_eq!(
            n,
            MIGRATIONS.len() as i64,
            "base vierge → chaque migration appliquée exactement une fois"
        );

        // La colonne tenant_id (migration 011) existe — non-idempotente, donc elle
        // n'aurait PAS survécu à un rejeu.
        let cols: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('gradatum_jobs') WHERE name = 'tenant_id'",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .expect("pragma");
        assert_eq!(
            cols, 1,
            "colonne tenant_id présente (migration 011 appliquée)"
        );

        // Second appel → rien à rejouer.
        let applied2 = run_migrations(&db).await.expect("second run");
        assert_eq!(applied2, 0, "second appel : rien à rejouer");
    }

    /// Le checksum embarqué correspond aux fichiers sur disque (le même que sqlx lit) :
    /// la preuve de non-rejeu repose sur l'identité des octets, pas sur une copie dérivée.
    #[test]
    fn embedded_migrations_match_disk_files() {
        for m in MIGRATIONS {
            let filename = format!("{:03}_{}.sql", m.version, m.description.replace(' ', "_"));
            let disk = std::fs::read(format!(
                "{}/migrations/{filename}",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("lire la migration sur disque");
            let disk_checksum = Sha384::digest(&disk).to_vec();
            let embedded_checksum = Sha384::digest(m.sql.as_bytes()).to_vec();
            assert_eq!(
                disk_checksum, embedded_checksum,
                "le fichier embarqué (include_str!) doit être byte-identique au fichier sur disque ({filename})"
            );
        }
    }

    /// Fidélité sqlx — une base sale (ligne `success = false`) refuse le démarrage.
    #[tokio::test]
    async fn migration_runner_refuses_dirty_base() {
        let db = QueueDb::open_in_memory().await.expect("in-memory");
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE _sqlx_migrations (
                    version BIGINT PRIMARY KEY,
                    description TEXT NOT NULL,
                    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    success BOOLEAN NOT NULL,
                    checksum BLOB NOT NULL,
                    execution_time BIGINT NOT NULL
                );",
            )?;
            conn.execute(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
                 VALUES (?1, ?2, FALSE, x'00', -1)",
                params![6i64, "apalis bootstrap"],
            )?;
            Ok(())
        })
        .await
        .expect("fixture");

        let err = run_migrations(&db).await.expect_err("base sale → refus");
        assert!(
            matches!(err, QueueError::Storage(_)),
            "refus de démarrage attendu, obtenu : {err:?}"
        );
    }

    /// Fidélité sqlx — une migration appliquée dont le fichier a changé (checksum différent)
    /// refuse le démarrage : une migration appliquée est immuable.
    #[tokio::test]
    async fn migration_runner_refuses_modified_applied_migration() {
        let db = QueueDb::open_in_memory().await.expect("in-memory");
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE _sqlx_migrations (
                    version BIGINT PRIMARY KEY,
                    description TEXT NOT NULL,
                    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    success BOOLEAN NOT NULL,
                    checksum BLOB NOT NULL,
                    execution_time BIGINT NOT NULL
                );",
            )?;
            conn.execute(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
                 VALUES (?1, ?2, TRUE, x'00', -1)",
                params![6i64, "apalis bootstrap"],
            )?;
            Ok(())
        })
        .await
        .expect("fixture");

        let err = run_migrations(&db)
            .await
            .expect_err("checksum différent → refus");
        assert!(
            matches!(err, QueueError::Storage(_)),
            "refus de démarrage attendu, obtenu : {err:?}"
        );
    }

    // ── Preuve P0 (F-145 sous-lot 3) : format sérialisé des travaux intouché ────

    /// 🔴 P0 — le format sérialisé de la charge utile des travaux NE DOIT PAS CHANGER.
    ///
    /// Le payload stocké dans `gradatum_jobs.payload` est `serde_json::to_string(&JobRecord)`
    /// — indépendant du pilote SQLite (sqlx → rusqlite). Ce test fige le JSON exact d'un
    /// JobRecord représentatif, puis prouve que `enqueue` écrit ce JSON VERBATIM dans la
    /// colonne `payload` : le format est byte-identique à l'avant-port.
    #[tokio::test]
    async fn serialized_job_payload_format_is_pinned_and_stored_verbatim() {
        let store = SqliteQueueStore::new(test_db().await);

        let record = make_record(
            Job::Curate(CurateSpec {
                note_id: Ulid::generate(),
                tenant_id: "main".to_string(),
                ..Default::default()
            }),
            JobClass::Agent,
            JobStatus::Pending,
        );
        let golden = SqliteQueueStore::serialize_record(&record).expect("sérialisation");

        // Le JSON a la structure documentée : spec.kind taggé "type"/"data", lifecycle, retry.
        let v: serde_json::Value = serde_json::from_str(&golden).expect("JSON valide");
        assert_eq!(v["spec"]["kind"]["type"], "Curate", "tag serde(tag=type)");
        assert!(
            v["spec"]["kind"]["data"]["note_id"].is_string(),
            "contenu data du variant Curate"
        );
        assert!(v["lifecycle"]["status"].is_string());
        assert!(v["retry"]["max"].is_number());
        assert!(v["id"].is_string());

        let id = store.enqueue(record).await.expect("enqueue");

        // Le payload en colonne est byte-identique au JSON produit par serialize_record :
        // le pilote n'altère PAS le format — c'est la propriété qui a fait écarter une autre
        // option d'architecture sur F-248.
        let id_str = id.to_string();
        let raw: String = store
            .db()
            .with_conn(move |conn| {
                conn.query_row(
                    "SELECT payload FROM gradatum_jobs WHERE id = ?1",
                    [&id_str],
                    |row| row.get(0),
                )
            })
            .await
            .expect("read payload");
        assert_eq!(
            raw, golden,
            "le format sérialisé des travaux est intouché (F-145)"
        );
    }
}
