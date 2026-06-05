//! Queue Phase 1 — SQLite-backed avec atomic claim via `UPDATE…RETURNING`.
//!
//! Implémentation rusqlite synchrone préservée pour rétrocompatibilité des
//! tests Phase 1. Pour le code P2.0b, utiliser [`crate::SqliteQueue`].
//!
//! ## Garanties Phase 1
//!
//! - Une seule connexion par `LegacyQueue`, protégée par `tokio::sync::Mutex`.
//!   Deux `claim_one` concurrents sont sérialisés : le second voit la queue vide.
//! - Lease expirée = job re-claimable. `attempts` est incrémenté à chaque claim.
//! - Phase 1 : pas de retry automatique sur `failed`. `fail()` est terminal.
//! - `claim_one` utilise `UPDATE…RETURNING` (SQLite ≥ 3.35, bundled ≥ 3.47).
//!
//! ## 4 PRAGMA C12 — appliquées à l'ouverture
//!
//! ```sql
//! PRAGMA journal_mode  = WAL;
//! PRAGMA synchronous   = NORMAL;
//! PRAGMA busy_timeout  = 5000;
//! PRAGMA foreign_keys  = ON;
//! ```

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::job::{Job, JobStatus};
use crate::schema::{CREATE_IDX_JOBS_STATUS_LEASE, CREATE_JOBS_TABLE};

/// Erreurs retournées par [`LegacyQueue`] (rusqlite-based, Phase 1).
#[derive(Debug, thiserror::Error)]
pub enum LegacyQueueError {
    /// Erreur SQLite sous-jacente.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Erreur I/O (ouverture fichier, permissions).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Échec de décodage d'un ULID stocké en base.
    #[error("ulid parse: {0}")]
    UlidParse(#[from] ulid::DecodeError),
}

/// Queue SQLite-backed Phase 1 (rusqlite, sync, ULID-based).
///
/// Préservée pour rétrocompatibilité des tests Phase 1.
/// Utiliser [`LegacyQueue::open`] pour une base persistante ou
/// [`LegacyQueue::open_in_memory`] pour les tests.
///
/// Pour le code P2.0b, préférer [`crate::SqliteQueue`] (sqlx-based, async trait).
pub struct LegacyQueue {
    /// Connexion unique sérialisée par Mutex tokio.
    /// Maintenir le lock le moins longtemps possible (jamais à travers un `.await`).
    conn: Mutex<Connection>,
}

impl LegacyQueue {
    /// Ouvre (ou crée) une base de données SQLite persistante au `path` donné.
    ///
    /// Applique les 4 PRAGMA C12 et crée le schéma si absent.
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`] si l'ouverture ou les PRAGMA échouent.
    pub async fn open(path: &Path) -> Result<Self, LegacyQueueError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Ouvre une base de données en mémoire (utile pour les tests).
    ///
    /// Note : WAL n'est pas supporté sur `:memory:`, SQLite revient à `MEMORY`.
    /// Le test `pragmas.rs` accepte les deux valeurs (`wal` ou `memory`).
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`] si l'initialisation échoue.
    pub async fn open_in_memory() -> Result<Self, LegacyQueueError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Initialise la connexion : 4 PRAGMA C12 + DDL schéma.
    fn init(conn: Connection) -> Result<Self, LegacyQueueError> {
        // PRAGMA C12 — obligatoires, appliqués dans cet ordre précis.
        conn.execute_batch(
            "PRAGMA journal_mode  = WAL;
             PRAGMA synchronous   = NORMAL;
             PRAGMA busy_timeout  = 5000;
             PRAGMA foreign_keys  = ON;",
        )?;

        // DDL — idempotent via IF NOT EXISTS.
        conn.execute_batch(CREATE_JOBS_TABLE)?;
        conn.execute_batch(CREATE_IDX_JOBS_STATUS_LEASE)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insère un nouveau job `pending` dans la queue.
    ///
    /// Retourne l'[`Ulid`] du job créé.
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`] en cas d'échec d'insertion.
    pub async fn enqueue(&self, kind: &str, payload_json: &str) -> Result<Ulid, LegacyQueueError> {
        let id = Ulid::new();
        let now_ms = now_ms();

        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "INSERT INTO jobs (id, kind, payload_json, status, lease_until, created_at, updated_at, attempts, last_error)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, 0, NULL)",
        )?
        .execute(params![
            id.to_string(),
            kind,
            payload_json,
            JobStatus::Pending,
            now_ms,
        ])?;

        Ok(id)
    }

    /// Claim le premier job disponible (pending ou lease expirée) de manière atomique.
    ///
    /// Utilise `UPDATE…RETURNING` pour garantir qu'une seule tâche concurrente
    /// obtient le job, même sans verrou explicite au niveau applicatif.
    ///
    /// `lease_duration_ms` définit la durée de validité du claim en millisecondes
    /// (standard Phase 1 : 300_000 ms = 5 min).
    ///
    /// Retourne `Ok(None)` si aucun job n'est disponible.
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`] ou [`LegacyQueueError::UlidParse`].
    pub async fn claim_one(&self, lease_duration_ms: i64) -> Result<Option<Job>, LegacyQueueError> {
        let now_ms = now_ms();
        let lease_until = now_ms + lease_duration_ms;

        let conn = self.conn.lock().await;
        // UPDATE…RETURNING atomique — SQLite ≥ 3.35 (bundled 3.47).
        // Le sous-SELECT identifie le premier job éligible :
        //   - pending  : jamais claim
        //   - claimed  : lease expirée (re-claimable)
        // ORDER BY created_at garantit FIFO.
        let mut stmt = conn.prepare_cached(
            "UPDATE jobs
             SET status = 'claimed',
                 lease_until = ?1,
                 updated_at  = ?2,
                 attempts    = attempts + 1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE status = 'pending'
                    OR (status = 'claimed' AND lease_until < ?2)
                 ORDER BY created_at ASC
                 LIMIT 1
             )
             RETURNING id, kind, payload_json, status, lease_until,
                       created_at, updated_at, attempts, last_error",
        )?;

        let job = stmt
            .query_row(params![lease_until, now_ms], row_to_job)
            .optional()?;

        Ok(job)
    }

    /// Marque un job comme `done`.
    ///
    /// Efface `lease_until` et `last_error`. Idempotent si appelé plusieurs fois
    /// sur le même job (seul `updated_at` change).
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`].
    pub async fn complete(&self, id: Ulid) -> Result<(), LegacyQueueError> {
        let now_ms = now_ms();
        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "UPDATE jobs
             SET status = 'done', lease_until = NULL, updated_at = ?1
             WHERE id = ?2",
        )?
        .execute(params![now_ms, id.to_string()])?;
        Ok(())
    }

    /// Marque un job comme `failed` avec un message d'erreur.
    ///
    /// Phase 1 : `failed` est terminal (pas de retry automatique).
    /// Efface `lease_until`.
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`].
    pub async fn fail(&self, id: Ulid, reason: &str) -> Result<(), LegacyQueueError> {
        let now_ms = now_ms();
        let conn = self.conn.lock().await;
        conn.prepare_cached(
            "UPDATE jobs
             SET status = 'failed', lease_until = NULL,
                 updated_at = ?1, last_error = ?2
             WHERE id = ?3",
        )?
        .execute(params![now_ms, reason, id.to_string()])?;
        Ok(())
    }

    /// Lit la valeur d'un PRAGMA SQLite par son nom.
    ///
    /// Utilisé principalement dans les tests pour vérifier les 4 PRAGMA C12.
    ///
    /// # Erreurs
    ///
    /// Retourne [`LegacyQueueError::Sqlite`] si le PRAGMA n'existe pas ou si le type
    /// de retour `T` ne correspond pas.
    pub async fn pragma_value<T: rusqlite::types::FromSql>(
        &self,
        name: &str,
    ) -> Result<T, LegacyQueueError> {
        let conn = self.conn.lock().await;
        // Construire la requête PRAGMA dynamiquement.
        // Seuls des noms alphanumériques + underscore sont acceptés en pratique.
        // Pas de risque d'injection car name provient toujours du code appelant,
        // jamais d'une entrée utilisateur externe.
        let sql = format!("PRAGMA {name}");
        let value = conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(value)
    }
}

/// Retourne le timestamp courant en millisecondes Unix.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // L'horloge système ne peut pas être antérieure à UNIX_EPOCH dans
        // un environnement de prod standard.
        .expect("horloge système antérieure à UNIX_EPOCH — environnement invalide")
        .as_millis() as i64
}

/// Mappe une ligne SQLite vers un [`Job`].
///
/// L'ordre des colonnes DOIT correspondre exactement au SELECT du RETURNING
/// et aux queries de lecture.
fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let id_str: String = row.get(0)?;
    let id = id_str.parse::<Ulid>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(Job {
        id,
        kind: row.get(1)?,
        payload_json: row.get(2)?,
        status: row.get(3)?,
        lease_until: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        attempts: row.get::<_, i64>(7)? as u32,
        last_error: row.get(8)?,
    })
}
