//! `Queue` trait async + `SqliteQueue` impl avec `UPDATE...RETURNING` atomic lease (P2.0b).
//!
//! ## Garanties P2.0b
//!
//! - **Atomic claim** : `UPDATE...RETURNING` SQLite atomique garantit qu'un seul
//!   worker obtient un job meme en cas de contention multi-processus (WAL mode).
//! - **Lease recovery** : un job dont la lease a expire (`lease_until < now`) est
//!   re-claimable automatiquement ; `attempts` est incremente a chaque re-claim.
//! - **Dead-letter** : quand `attempts >= max_attempts`, le job passe en `dead`.
//! - **WAL mode** : pool sqlx 8 connexions, journal WAL, synchronous NORMAL.
//! - **Timestamps en millisecondes** : precision sub-second pour les tests + prod.
//!
//! ## Differences avec la Phase 1 rusqlite
//!
//! - ID : `i64` AUTOINCREMENT (vs ULID TEXT en Phase 1)
//! - Table : `jobs_v2` (coexistence avec `jobs` Phase 1 dans la meme base)
//! - Payload : `BLOB` opaque (vs TEXT JSON en Phase 1)
//! - API : trait async + multi-kind filter (vs struct synchrone + kind unique)
//! - Timestamps : millisecondes (precision sub-second, compatible Duration::from_millis)

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::schema::SCHEMA_V1;

/// Erreurs retournees par les operations de queue sqlx-based.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Erreur sqlx sous-jacente (SQLite, pool, requete).
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Horloge systeme invalide (anterieure a UNIX_EPOCH).
    #[error("system time: {0}")]
    Time(#[from] std::time::SystemTimeError),
}

/// Identifiant opaque d'un job (AUTOINCREMENT i64, stable dans la DB).
pub type JobId = i64;

/// Donnees necessaires pour enqueue un nouveau job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewJob {
    /// Tenant isole (actuellement toujours `"main"` en P2.0).
    pub tenant_id: String,
    /// Type de job, ex. `"curate"`, `"embed"`.
    pub kind: String,
    /// Payload binaire opaque (encode bincode par l'appelant).
    pub payload: Vec<u8>,
    /// Nombre maximal de tentatives avant passage en `dead`.
    pub max_attempts: i32,
}

/// Job avec lease active retourne par [`Queue::lease`].
#[derive(Debug, Clone)]
pub struct LeasedJob {
    /// Identifiant du job dans `jobs_v2`.
    pub id: JobId,
    /// Tenant du job.
    pub tenant_id: String,
    /// Type de job.
    pub kind: String,
    /// Payload binaire opaque.
    pub payload: Vec<u8>,
    /// Nombre de tentatives (incremente a chaque lease, y compris re-lease).
    pub attempts: i32,
}

/// Vue lecture d'un job sans claim (résultat de [`Queue::get`]).
///
/// Distinct de [`LeasedJob`] : pas de `payload` (lecture meta seule),
/// pas de mutation côté DB. Utilisé par `GET /api/v1/jobs/:id`.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub status: JobStatus,
    pub attempts: i32,
    pub last_error: Option<String>,
}

/// Etat d'un job dans la queue P2.0b.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    /// En attente de traitement.
    Pending,
    /// Lease active — un worker est en train de traiter ce job.
    Leased,
    /// Traitement termine avec succes.
    Done,
    /// `attempts >= max_attempts` — dead-letter, pas de retry automatique.
    Dead,
}

impl JobStatus {
    /// Convertit le statut en sa représentation string SQLite (`"pending"`, `"leased"`, etc.).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::Done => "done",
            Self::Dead => "dead",
        }
    }

    /// Reconstruit un `JobStatus` depuis une string SQLite. Retourne `None` si inconnu.
    /// Signature custom `Option<Self>` — pas un parse public, pas de trait `FromStr` requis.
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
}

/// Trait async pour la queue de jobs gradatum (P2.0b).
///
/// Toutes les methodes sont async et `Send + Sync + 'static` pour usage
/// dans des handlers Axum/tokio multi-threaded.
#[async_trait]
pub trait Queue: Send + Sync + 'static {
    /// Lit l'état d'un job sans le claimer. Retourne `None` si l'id n'existe pas.
    ///
    /// Utilisé par `GET /api/v1/jobs/:id` (poll status).
    async fn get(&self, id: JobId) -> Result<Option<JobInfo>, QueueError>;

    /// Insere un nouveau job `pending` et retourne son `JobId`.
    async fn enqueue(&self, job: NewJob) -> Result<JobId, QueueError>;

    /// Claim atomique du premier job disponible (pending ou lease expiree).
    ///
    /// Filtre sur `kinds` (OR) ; retourne `None` si la queue est vide.
    /// `duration` definit la duree de validite de la lease (precision milliseconde).
    async fn lease(
        &self,
        kinds: &[&str],
        duration: Duration,
    ) -> Result<Option<LeasedJob>, QueueError>;

    /// Marque un job comme `done` (terminal — ne peut plus etre re-clame).
    async fn complete(&self, id: JobId) -> Result<(), QueueError>;

    /// Marque un job comme echoue avec un message d'erreur.
    ///
    /// Si `attempts < max_attempts` : remet en `pending` pour retry.
    /// Si `attempts >= max_attempts` : passe en `dead`.
    async fn fail(&self, id: JobId, err: &str) -> Result<(), QueueError>;

    /// Prolonge la lease d'un job actif.
    async fn extend_lease(&self, id: JobId, dur: Duration) -> Result<(), QueueError>;

    /// Nombre de jobs en etat `pending`.
    async fn depth(&self) -> Result<u64, QueueError>;

    /// Age du plus vieux job `pending` en secondes (0 si queue vide).
    async fn oldest_age_secs(&self) -> Result<u64, QueueError>;
}

/// Implementation sqlx-based de [`Queue`] avec SQLite WAL.
///
/// Pool de 8 connexions max ; `UPDATE...RETURNING` pour le claim atomique.
/// Table cible : `jobs_v2` (coexiste avec table `jobs` Phase 1 rusqlite).
/// Timestamps en millisecondes Unix pour precision sub-second.
pub struct SqliteQueue {
    pool: SqlitePool,
}

impl SqliteQueue {
    /// Ouvre (ou cree) une base SQLite au `db_path` donne.
    ///
    /// Cree le schema `jobs_v2` + `worker_leadership` si absent (idempotent).
    ///
    /// # Erreurs
    ///
    /// Retourne [`QueueError::Sqlx`] si l'ouverture, le pool ou la migration echouent.
    pub async fn new(db_path: &Path) -> Result<Self, QueueError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // 5s busy_timeout : sans ce réglage, SQLite renvoie SQLITE_BUSY
            // immédiatement si un autre writer tient le verrou WAL. Avec busy_timeout,
            // SQLite réessaie jusqu'à 5s avant d'échouer — évite l'erreur d'ack
            // instantanée qui laissait les jobs coincés en Running.
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        // Executer le DDL complet (idempotent via IF NOT EXISTS).
        // SCHEMA_V1 contient plusieurs statements separes par `;`.
        sqlx::query(SCHEMA_V1).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Ouvre une base SQLite en mémoire (`:memory:`) pour les tests.
    ///
    /// Connexion unique (max_connections=1) — la base disparaît à la fermeture du pool.
    /// WAL désactivé sur `:memory:` (sans effet sur le comportement).
    ///
    /// # Usage
    ///
    /// ```rust,no_run
    /// use gradatum_queue::SqliteQueue;
    /// // Dans un contexte tokio async :
    /// // let queue = SqliteQueue::in_memory().await.expect("in-memory queue");
    /// ```
    pub async fn in_memory() -> Result<Self, QueueError> {
        let opts = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Memory);
        // max_connections=1 : connexion unique obligatoire pour les DBs :memory: sqlx.
        // Plusieurs connexions ouvrent des bases distinctes (non partagées) en mémoire.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::query(SCHEMA_V1).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Retourne le timestamp courant en millisecondes Unix.
    ///
    /// Utilise des millisecondes (et non secondes) pour garantir la precision
    /// sub-second necessaire aux tests de lease courte (< 1 seconde).
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

        Ok(row.map(|r| {
            let status_str: String = r.get("status");
            let status = JobStatus::from_str(&status_str).unwrap_or(JobStatus::Pending);
            JobInfo {
                id: r.get("id"),
                status,
                attempts: r.get("attempts"),
                last_error: r.get("last_error"),
            }
        }))
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
        // Convertir la duree en millisecondes pour la precision sub-second.
        let lease_until = now + duration.as_millis() as i64;
        let leased_by = ulid::Ulid::new().to_string();

        // Construire dynamiquement les placeholders pour le filtre IN.
        // `kinds` provient du code appelant (jamais d'une entree externe),
        // donc pas de risque d'injection ; les valeurs sont bindees separement.
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

        // Bind dans l'ordre : lease_until, leased_by, now (UPDATE SET), now (WHERE expiry)
        // puis les kinds du filtre IN.
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
        sqlx::query(
            "UPDATE jobs_v2
             SET status = 'done', lease_until = NULL, leased_by = NULL, updated_at = ?
             WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn fail(&self, id: JobId, err: &str) -> Result<(), QueueError> {
        let now = Self::now_ms()?;
        // Si attempts >= max_attempts -> dead ; sinon retour a pending pour retry.
        sqlx::query(
            "UPDATE jobs_v2
             SET status     = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'pending' END,
                 last_error = ?,
                 updated_at  = ?,
                 lease_until = NULL,
                 leased_by   = NULL
             WHERE id = ?",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await?;
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
        // created_at est en ms ; on convertit la difference en secondes.
        Ok(row
            .map(|(c,)| ((now - c).max(0) as u64) / 1000)
            .unwrap_or(0))
    }
}
