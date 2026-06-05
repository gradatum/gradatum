//! Registry SQLite — journalisation des requêtes et statut providers.
//!
//! Deux tables :
//! - `request_log`     : chaque requête traitée (alias, provider, latence, status, route)
//! - `provider_status` : état courant d'un provider (ok / rate_limited / down / degraded)
//!
//! TTL dégradation : `purge_stale_statuses(ttl)` supprime les entrées non-ok
//! dont `last_update` est plus ancien que `now - ttl`. Appelé toutes les 5 min
//! par un background task dans `main.rs`.
//!
//! Thread-safety : `Connection` rusqlite est `!Send` — elle est encapsulée dans
//! un `Mutex<Connection>` et toutes les opérations en mode `spawn_blocking` pour
//! ne jamais bloquer le runtime tokio.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

/// Erreurs du registre.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Erreur rusqlite (connexion, requête, migration).
    #[error("erreur SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Verrou Mutex empoisonné (panique dans un thread précédent).
    #[error("verrou du registre empoisonné")]
    PoisonedLock,

    /// Erreur runtime tokio spawn_blocking.
    #[error("erreur join spawn_blocking: {0}")]
    JoinError(#[from] tokio::task::JoinError),
}

impl<T> From<std::sync::PoisonError<T>> for RegistryError {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        RegistryError::PoisonedLock
    }
}

/// Statut d'un provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    RateLimited,
    Down,
    Degraded,
}

impl ProviderStatus {
    /// Sérialise le statut en string pour stockage SQL.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderStatus::Ok => "ok",
            ProviderStatus::RateLimited => "rate_limited",
            ProviderStatus::Down => "down",
            ProviderStatus::Degraded => "degraded",
        }
    }

    /// Parse depuis string SQL.
    fn from_str(s: &str) -> Self {
        match s {
            "ok" => ProviderStatus::Ok,
            "rate_limited" => ProviderStatus::RateLimited,
            "down" => ProviderStatus::Down,
            "degraded" => ProviderStatus::Degraded,
            // Valeur inconnue → dégradé par précaution.
            _ => ProviderStatus::Degraded,
        }
    }
}

/// Entrée à journaliser après chaque requête traitée.
#[derive(Debug, Clone)]
pub struct RequestLogEntry {
    /// Alias modèle tel que reçu dans la requête client.
    pub model_alias: String,
    /// Nom du provider réel résolu par l'alias.
    pub provider_real: String,
    /// Identifiant de modèle réel transmis au backend.
    pub real_model: String,
    /// Route HTTP de la requête (ex: "/v1/chat/completions").
    pub route: String,
    /// Latence mesurée end-to-end en millisecondes.
    pub latency_ms: Option<u64>,
    /// Code HTTP retourné au client.
    pub status_code: u16,
    /// true si la requête était en mode streaming.
    pub streamed: bool,
    /// Message d'erreur si la requête a échoué.
    pub error_message: Option<String>,
}

/// Statut stocké d'un provider (retourné par `get_provider_status`).
#[derive(Debug, Clone)]
pub struct StoredProviderStatus {
    pub provider_name: String,
    pub status: ProviderStatus,
    /// Timestamp unix millisecondes de la dernière mise à jour.
    pub last_update: i64,
    /// Timestamp unix millisecondes de la prochaine tentative (None si status=ok).
    pub next_retry: Option<i64>,
    /// Raison textuelle (optionnelle).
    pub reason: Option<String>,
}

/// Registre partagé — wrappé dans `Arc` pour clonage léger entre handlers.
#[derive(Clone)]
pub struct Registry {
    /// Connexion SQLite protégée par Mutex (rusqlite Connection est !Send).
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl Registry {
    /// Ouvre (ou crée) la base SQLite au chemin donné et exécute les migrations.
    ///
    /// Retourne une erreur si le fichier est inaccessible ou si la migration échoue.
    pub fn new(path: &Path) -> Result<Self, RegistryError> {
        let conn = rusqlite::Connection::open(path)?;

        // WAL mode — améliore les performances concurrentes lecture/écriture.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        // Migrations inline — idempotentes grâce aux IF NOT EXISTS.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS request_log (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                ts              INTEGER NOT NULL,
                model_alias     TEXT NOT NULL,
                provider_real   TEXT NOT NULL,
                real_model      TEXT NOT NULL,
                route           TEXT NOT NULL,
                latency_ms      INTEGER,
                status_code     INTEGER,
                streamed        INTEGER NOT NULL DEFAULT 0,
                error_message   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_request_log_ts    ON request_log(ts);
            CREATE INDEX IF NOT EXISTS idx_request_log_alias ON request_log(model_alias);

            CREATE TABLE IF NOT EXISTS provider_status (
                provider_name TEXT PRIMARY KEY,
                status        TEXT NOT NULL,
                last_update   INTEGER NOT NULL,
                next_retry    INTEGER,
                reason        TEXT
            );
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Journalise une requête traitée.
    ///
    /// Opération non-bloquante : exécutée dans `spawn_blocking`.
    pub async fn log_request(&self, entry: RequestLogEntry) -> Result<(), RegistryError> {
        let conn = Arc::clone(&self.conn);
        let ts_now = now_millis();

        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| RegistryError::PoisonedLock)?;
            guard.execute(
                "INSERT INTO request_log
                    (ts, model_alias, provider_real, real_model, route, latency_ms, status_code, streamed, error_message)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    ts_now,
                    entry.model_alias,
                    entry.provider_real,
                    entry.real_model,
                    entry.route,
                    entry.latency_ms.map(|v| v as i64),
                    entry.status_code as i64,
                    if entry.streamed { 1i64 } else { 0i64 },
                    entry.error_message,
                ],
            )?;
            Ok::<(), RegistryError>(())
        })
        .await??;

        Ok(())
    }

    /// Met à jour ou insère le statut d'un provider.
    ///
    /// Opération non-bloquante : exécutée dans `spawn_blocking`.
    pub async fn set_provider_status(
        &self,
        provider: &str,
        status: ProviderStatus,
        reason: Option<&str>,
        retry_in: Option<Duration>,
    ) -> Result<(), RegistryError> {
        let conn = Arc::clone(&self.conn);
        let now = now_millis();
        let next_retry = retry_in.map(|d| now + d.as_millis() as i64);
        let provider = provider.to_owned();
        let reason = reason.map(|s| s.to_owned());
        let status_str = status.as_str().to_owned();

        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| RegistryError::PoisonedLock)?;
            guard.execute(
                "INSERT INTO provider_status (provider_name, status, last_update, next_retry, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(provider_name) DO UPDATE SET
                     status      = excluded.status,
                     last_update = excluded.last_update,
                     next_retry  = excluded.next_retry,
                     reason      = excluded.reason",
                rusqlite::params![provider, status_str, now, next_retry, reason],
            )?;
            Ok::<(), RegistryError>(())
        })
        .await??;

        Ok(())
    }

    /// Retourne le statut stocké d'un provider, ou `None` s'il n'est pas enregistré.
    ///
    /// Opération non-bloquante : exécutée dans `spawn_blocking`.
    pub async fn get_provider_status(
        &self,
        provider: &str,
    ) -> Result<Option<StoredProviderStatus>, RegistryError> {
        let conn = Arc::clone(&self.conn);
        let provider = provider.to_owned();

        let result = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| RegistryError::PoisonedLock)?;
            let mut stmt = guard.prepare(
                "SELECT provider_name, status, last_update, next_retry, reason
                 FROM provider_status WHERE provider_name = ?1",
            )?;

            let mut rows = stmt.query(rusqlite::params![provider])?;
            match rows.next()? {
                Some(row) => {
                    let stored = StoredProviderStatus {
                        provider_name: row.get(0)?,
                        status: ProviderStatus::from_str(&row.get::<_, String>(1)?),
                        last_update: row.get(2)?,
                        next_retry: row.get(3)?,
                        reason: row.get(4)?,
                    };
                    Ok::<Option<StoredProviderStatus>, RegistryError>(Some(stored))
                }
                None => Ok(None),
            }
        })
        .await??;

        Ok(result)
    }

    /// Supprime les entrées `provider_status` non-ok dont `last_update < now - ttl`.
    ///
    /// Les statuts rate_limited/down/degraded qui ne sont plus mis à jour
    /// s'accumulent indéfiniment sans cette purge. Elle évite la fausse image
    /// d'un provider perpétuellement dégradé.
    ///
    /// Les entrées `status = 'ok'` ne sont jamais purgées.
    pub async fn purge_stale_statuses(&self, ttl: Duration) -> Result<u64, RegistryError> {
        let conn = Arc::clone(&self.conn);
        let cutoff = now_millis() - ttl.as_millis() as i64;

        let deleted = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| RegistryError::PoisonedLock)?;
            let n = guard.execute(
                "DELETE FROM provider_status
                 WHERE status != 'ok' AND last_update < ?1",
                rusqlite::params![cutoff],
            )?;
            Ok::<usize, RegistryError>(n)
        })
        .await??;

        Ok(deleted as u64)
    }
}

/// Retourne le timestamp unix courant en millisecondes.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_registry() -> (Registry, NamedTempFile) {
        let f = NamedTempFile::new().expect("impossible de créer fichier temporaire");
        let reg = Registry::new(f.path()).expect("ouverture registry échouée");
        (reg, f)
    }

    fn sample_entry(alias: &str) -> RequestLogEntry {
        RequestLogEntry {
            model_alias: alias.to_owned(),
            provider_real: "test-backend".to_owned(),
            real_model: "model-v1".to_owned(),
            route: "/v1/chat/completions".to_owned(),
            latency_ms: Some(42),
            status_code: 200,
            streamed: false,
            error_message: None,
        }
    }

    #[tokio::test]
    async fn test_log_request_round_trip() {
        let (reg, _f) = test_registry();
        reg.log_request(sample_entry("my-model")).await.unwrap();

        let conn = reg.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM request_log WHERE model_alias = 'my-model'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_provider_status_insert_and_get() {
        let (reg, _f) = test_registry();

        reg.set_provider_status(
            "test-provider",
            ProviderStatus::RateLimited,
            Some("trop de requetes"),
            Some(Duration::from_secs(60)),
        )
        .await
        .unwrap();

        let stored = reg.get_provider_status("test-provider").await.unwrap();
        assert!(stored.is_some());
        let s = stored.unwrap();
        assert_eq!(s.status, ProviderStatus::RateLimited);
        assert_eq!(s.reason.as_deref(), Some("trop de requetes"));
        assert!(s.next_retry.is_some());
    }

    #[tokio::test]
    async fn test_provider_status_unknown_returns_none() {
        let (reg, _f) = test_registry();
        let stored = reg.get_provider_status("inexistant").await.unwrap();
        assert!(stored.is_none());
    }

    #[tokio::test]
    async fn test_purge_removes_stale_non_ok() {
        let (reg, _f) = test_registry();

        reg.set_provider_status("bad-provider", ProviderStatus::Down, None, None)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;
        let deleted = reg
            .purge_stale_statuses(Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(deleted, 1, "un statut stale attendu supprimé");
    }

    #[tokio::test]
    async fn test_purge_preserves_ok_status() {
        let (reg, _f) = test_registry();

        reg.set_provider_status("good-provider", ProviderStatus::Ok, None, None)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;
        let deleted = reg
            .purge_stale_statuses(Duration::from_millis(1))
            .await
            .unwrap();
        assert_eq!(deleted, 0, "les statuts ok ne doivent pas être purgés");

        let stored = reg.get_provider_status("good-provider").await.unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().status, ProviderStatus::Ok);
    }

    #[tokio::test]
    async fn test_provider_status_upsert() {
        let (reg, _f) = test_registry();

        reg.set_provider_status("p1", ProviderStatus::Down, Some("timeout"), None)
            .await
            .unwrap();

        reg.set_provider_status("p1", ProviderStatus::Ok, None, None)
            .await
            .unwrap();

        let stored = reg.get_provider_status("p1").await.unwrap().unwrap();
        assert_eq!(stored.status, ProviderStatus::Ok);
        assert!(stored.reason.is_none());
    }
}
