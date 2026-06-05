//! Store append-only pour la table `event_log` (B1 tranche v0.3.0).
//!
//! ## Design
//!
//! `EventLogStore` ouvre sa propre connexion `rusqlite::Connection` sur la même DB
//! que `SqliteIndex` (`index.db`). La DB est en mode WAL — les connexions multiples
//! sont safe : les lectures sont non-bloquantes, les écritures sont sérialisées par
//! SQLite lui-même (busy_timeout 5000ms).
//!
//! La table `event_log` est créée par la migration `0006_event_log.sql` exécutée
//! lors de l'ouverture du `SqliteIndex` dans `AppState::with_search_path`.
//!
//! ## Garanties d'immuabilité
//!
//! Seul `insert_batch` écrit des lignes — aucun chemin UPDATE par record.
//! La purge de rétention supprime des lignes EN MASSE par âge (maintenance interne).
//! Cette construction garantit l'append-only D-06 v81 sans `AclOp::Append`.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` n'est ni `Send` ni `Sync`. On l'encapsule dans
//! `Arc<Mutex<Connection>>` (tokio Mutex) — même pattern que `SqliteIndex`.
//! Les locks sont scopés au minimum (droppés avant tout `.await`).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use rusqlite::{params, Connection, OpenFlags};
use thiserror::Error;
use tokio::sync::Mutex;

use gradatum_dto::QaEventDto;

/// Erreur du store event_log.
#[derive(Debug, Error)]
pub enum EventLogError {
    /// Erreur SQLite sous-jacente.
    #[error("event_log SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Timestamp RFC3339 non parsable depuis QaEvent.
    #[error("event_log timestamp invalide : {0}")]
    BadTimestamp(String),
    /// Mutex poisonné — ne peut pas arriver avec tokio Mutex.
    #[error("event_log mutex poisonné")]
    Poisoned,
}

/// Store append-only pour la table `event_log`.
///
/// Cloneable (`Arc` intérieur) — injecté dans `AppState` et partagé entre
/// les handlers et la tâche de rétention.
#[derive(Clone)]
pub struct EventLogStore {
    /// Connexion SQLite dédiée — separée du `SqliteIndex` pour éviter les deadlocks.
    ///
    /// Même fichier `index.db` (WAL) — SQLite garantit la cohérence multi-connexion.
    conn: Arc<Mutex<Connection>>,
}

impl EventLogStore {
    /// Ouvre une connexion dédiée sur `path` (en WAL) pour la table `event_log`.
    ///
    /// Les PRAGMA WAL + busy_timeout sont appliqués immédiatement.
    /// La migration 0006 doit déjà avoir été exécutée par `SqliteIndex::open`.
    ///
    /// # Erreurs
    ///
    /// Retourne `EventLogError::Sqlite` si le fichier est inaccessible ou
    /// si les PRAGMA échouent.
    pub async fn open(path: &Path) -> Result<Self, EventLogError> {
        let path = path.to_path_buf();
        // Ouvrir la connexion rusqlite dans un thread dédié — `Connection::open`
        // peut bloquer sur les locks OS (WAL checkpoint) et n'est pas async.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // PRAGMA C12 alignés sur SqliteIndex — nécessaires sur chaque connexion SQLite.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Ouvre une connexion in-memory pour les tests unitaires.
    ///
    /// Crée la table `event_log` directement (sans migration runner).
    /// Ne doit PAS être utilisé en production.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, EventLogError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS event_log (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts           INTEGER NOT NULL,
                    tenant_id    TEXT    NOT NULL,
                    route        TEXT    NOT NULL,
                    model_alias  TEXT    NOT NULL,
                    model_used   TEXT,
                    provider     TEXT    NOT NULL,
                    feature_id   TEXT,
                    status_code  INTEGER NOT NULL,
                    latency_ms   INTEGER NOT NULL,
                    tokens_input  INTEGER,
                    tokens_output INTEGER,
                    cost_usd     REAL,
                    processed    INTEGER NOT NULL DEFAULT 0,
                    created_at   INTEGER NOT NULL,
                    agent_id     TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
                CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
                CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
                CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);
                CREATE INDEX IF NOT EXISTS idx_event_log_agent     ON event_log(agent_id);",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insère un batch de `QaEventDto` en une seule transaction.
    ///
    /// Retourne le nombre de lignes insérées.
    ///
    /// `tenant_id` est extrait du JWT (TrustContext) — jamais du corps de requête.
    /// `created_at` est l'epoch ms serveur au moment de l'appel.
    /// `ts` est parsé depuis `dto.timestamp` (RFC3339) → epoch ms.
    ///
    /// # Erreurs
    ///
    /// - `EventLogError::BadTimestamp` si un timestamp ne se parse pas.
    /// - `EventLogError::Sqlite` sur erreur DB.
    pub async fn insert_batch(
        &self,
        tenant_id: &str,
        events: &[QaEventDto],
    ) -> Result<usize, EventLogError> {
        if events.is_empty() {
            return Ok(0);
        }

        // Pré-calculer les epoch ms hors du lock.
        let now_ms = system_now_ms();
        let tenant_id = tenant_id.to_owned();

        // Parser les timestamps RFC3339 → epoch ms (peut échouer → erreur avant le lock).
        let ts_vec: Vec<i64> = events
            .iter()
            .map(|e| parse_rfc3339_ms(&e.timestamp))
            .collect::<Result<Vec<_>, _>>()?;

        let events: Vec<QaEventDto> = events.to_vec();

        // Opération SQLite dans spawn_blocking — rusqlite n'est pas async.
        let conn = Arc::clone(&self.conn);
        let inserted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();

            let tx = conn.unchecked_transaction()?;
            let mut count = 0usize;

            for (dto, ts) in events.iter().zip(ts_vec.iter()) {
                tx.execute(
                    "INSERT INTO event_log
                        (ts, tenant_id, route, model_alias, model_used, provider,
                         feature_id, status_code, latency_ms,
                         tokens_input, tokens_output, cost_usd,
                         processed, created_at, agent_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0,?13,?14)",
                    params![
                        ts,
                        tenant_id,
                        dto.route,
                        dto.model_alias,
                        dto.model_used,
                        dto.provider,
                        dto.feature_id,
                        dto.status_code as i64,
                        dto.latency_ms as i64,
                        dto.tokens_input.map(|v| v as i64),
                        dto.tokens_output.map(|v| v as i64),
                        dto.cost_usd,
                        now_ms,
                        dto.agent_id,
                    ],
                )?;
                count += 1;
            }

            tx.commit()?;
            Ok::<usize, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(inserted)
    }

    /// Purge les lignes obsolètes selon deux critères :
    ///
    /// 1. **Âge** : supprime les lignes avec `created_at < retention_cutoff_ms`.
    /// 2. **Cap** : si le nombre total de lignes dépasse `max_rows`, supprime
    ///    les plus anciennes jusqu'à atteindre `max_rows`.
    ///
    /// Retourne le nombre total de lignes supprimées.
    ///
    /// Cette opération est de la maintenance interne — jamais exposée via ACL.
    /// Elle n'altère pas l'immuabilité des records retenus (D-06 v81).
    pub async fn purge(
        &self,
        retention_cutoff_ms: i64,
        max_rows: u64,
    ) -> Result<u64, EventLogError> {
        let conn = Arc::clone(&self.conn);

        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut total_deleted = 0u64;

            // Passe 1 : suppression par âge.
            let deleted_age = conn.execute(
                "DELETE FROM event_log WHERE created_at < ?1",
                params![retention_cutoff_ms],
            )?;
            total_deleted += deleted_age as u64;

            // Passe 2 : cap max_rows — supprimer les plus anciens au-delà du cap.
            // COUNT(*) — sûr sur une petite table de télémétrie (pas de full-scan coûteux
            // en prod car `created_at` est indexé et COUNT sur index row-id est O(1) sur SQLite).
            let current_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM event_log", [], |r| r.get(0))?;

            if current_count > max_rows as i64 {
                let excess = current_count - max_rows as i64;
                // DELETE les `excess` lignes les plus anciennes (ORDER BY created_at ASC LIMIT).
                // SQLite supporte DELETE ... WHERE id IN (SELECT ... LIMIT ...).
                let deleted_cap = conn.execute(
                    "DELETE FROM event_log WHERE id IN (
                        SELECT id FROM event_log ORDER BY created_at ASC LIMIT ?1
                    )",
                    params![excess],
                )?;
                total_deleted += deleted_cap as u64;
            }

            Ok::<u64, rusqlite::Error>(total_deleted)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(deleted)
    }

    /// Retourne le nombre total de lignes dans `event_log`.
    ///
    /// Utilisé pour la mise à jour de la gauge Prometheus `gradatum_event_log_rows`.
    pub async fn count(&self) -> Result<u64, EventLogError> {
        let conn = Arc::clone(&self.conn);

        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM event_log", [], |r| r.get(0))?;
            Ok::<u64, rusqlite::Error>(count as u64)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(count)
    }
}

/// Retourne l'epoch ms courant (horloge système).
///
/// Panique uniquement si l'horloge système est avant l'epoch UNIX (impossible en pratique).
fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("horloge système avant epoch UNIX — invariant système")
        .as_millis() as i64
}

/// Parse un timestamp RFC3339 en epoch milliseconds.
///
/// # Erreurs
///
/// Retourne `EventLogError::BadTimestamp` si le format est invalide.
/// Le timestamp stocké dans l'erreur est sanitisé (caractères de contrôle filtrés,
/// max 64 chars) pour prévenir toute injection dans les logs (CR/LF/ANSI — F4).
fn parse_rfc3339_ms(ts: &str) -> Result<i64, EventLogError> {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis())
        .map_err(|_| {
            // F4 : sanitiser le timestamp avant de l'embarquer dans le message d'erreur.
            // Filtre les caractères de contrôle (CR, LF, ESC, séquences ANSI…) et tronque à 64 chars.
            let safe_ts: String = ts.chars().filter(|c| !c.is_control()).take(64).collect();
            EventLogError::BadTimestamp(safe_ts)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit un `QaEventDto` minimal pour les tests.
    fn make_dto(route: &str, has_tokens: bool) -> QaEventDto {
        QaEventDto {
            route: route.to_owned(),
            model_alias: "alias-test".to_owned(),
            provider: "test-provider".to_owned(),
            status_code: 200,
            latency_ms: 42,
            timestamp: "2026-06-01T12:00:00Z".to_owned(),
            feature_id: Some("feat-1".to_owned()),
            model_used: None,
            tokens_input: if has_tokens { Some(100) } else { None },
            tokens_output: if has_tokens { Some(50) } else { None },
            cost_usd: None,
            agent_id: None,
        }
    }

    #[tokio::test]
    async fn insert_batch_append_two_batches() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let batch1 = vec![make_dto("/v1/chat", true), make_dto("/v1/embed", false)];
        let n1 = store
            .insert_batch("main", &batch1)
            .await
            .expect("insert batch1");
        assert_eq!(n1, 2, "batch1 doit insérer 2 lignes");

        let batch2 = vec![make_dto("/v1/chat", true)];
        let n2 = store
            .insert_batch("main", &batch2)
            .await
            .expect("insert batch2");
        assert_eq!(n2, 1, "batch2 doit insérer 1 ligne");

        let total = store.count().await.expect("count");
        assert_eq!(total, 3, "total doit être 3 (2 + 1 lignes)");
    }

    #[tokio::test]
    async fn insert_batch_empty_returns_zero() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        let n = store.insert_batch("main", &[]).await.expect("insert empty");
        assert_eq!(n, 0);
        assert_eq!(store.count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn insert_batch_accepts_none_tokens() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        let batch = vec![make_dto("/v1/embed", false)];
        let n = store
            .insert_batch("main", &batch)
            .await
            .expect("insert with None tokens");
        assert_eq!(n, 1);
        assert_eq!(store.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn purge_by_age_removes_old_rows_and_keeps_recent() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Insérer 3 lignes (created_at = now).
        let batch = vec![
            make_dto("/v1/chat", false),
            make_dto("/v1/chat", false),
            make_dto("/v1/chat", false),
        ];
        store.insert_batch("main", &batch).await.expect("insert");
        assert_eq!(store.count().await.expect("count"), 3);

        // Purger avec cutoff = maintenant + 1 minute (supprime tout).
        let cutoff_future = system_now_ms() + 60_000;
        let deleted = store
            .purge(cutoff_future, 1_000_000)
            .await
            .expect("purge age");
        assert_eq!(
            deleted, 3,
            "doit supprimer les 3 lignes (toutes 'anciennes')"
        );
        assert_eq!(store.count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn purge_by_age_keeps_recent_rows() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let batch = vec![make_dto("/v1/chat", false), make_dto("/v1/embed", false)];
        store.insert_batch("main", &batch).await.expect("insert");

        // Purger avec cutoff = epoch 0 (rien à supprimer car created_at = now >> 0).
        let deleted = store.purge(0, 1_000_000).await.expect("purge no-op");
        assert_eq!(deleted, 0, "cutoff=0 ne doit rien supprimer");
        assert_eq!(store.count().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn purge_cap_max_rows() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Insérer 5 lignes.
        let batch: Vec<QaEventDto> = (0..5)
            .map(|i| make_dto(&format!("/r/{i}"), false))
            .collect();
        store.insert_batch("main", &batch).await.expect("insert");
        assert_eq!(store.count().await.expect("count"), 5);

        // Cap à 3 (cutoff = 0 → rien supprimé par âge, mais cap déclenche).
        let deleted = store.purge(0, 3).await.expect("purge cap");
        assert_eq!(deleted, 2, "doit supprimer 2 lignes (5 - 3 = 2 excès)");
        assert_eq!(
            store.count().await.expect("count"),
            3,
            "il doit rester exactement 3 lignes"
        );
    }

    #[tokio::test]
    async fn purge_cap_combined_with_age() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        // Insérer 4 lignes.
        let batch: Vec<QaEventDto> = (0..4)
            .map(|i| make_dto(&format!("/r/{i}"), false))
            .collect();
        store.insert_batch("main", &batch).await.expect("insert");

        // Purge : cutoff = futur (supprime tout par âge) + cap = 2 (irrelevant car tout supprimé).
        let cutoff = system_now_ms() + 60_000;
        let deleted = store.purge(cutoff, 2).await.expect("purge combined");
        assert_eq!(deleted, 4, "doit supprimer les 4 lignes par âge");
        assert_eq!(store.count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn count_returns_zero_on_empty() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        assert_eq!(store.count().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn bad_timestamp_returns_error() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        let bad = vec![QaEventDto {
            timestamp: "NOT-A-DATE".to_owned(),
            route: "/v1/chat".to_owned(),
            model_alias: "a".to_owned(),
            provider: "p".to_owned(),
            status_code: 200,
            latency_ms: 1,
            feature_id: None,
            model_used: None,
            tokens_input: None,
            tokens_output: None,
            cost_usd: None,
            agent_id: None,
        }];
        let result = store.insert_batch("main", &bad).await;
        assert!(
            matches!(result, Err(EventLogError::BadTimestamp(_))),
            "timestamp invalide doit retourner BadTimestamp"
        );
    }

    // ── Tests agent_id ────────────────────────────────────────────────────────

    /// insert_batch : event avec agent_id présent → insertion correcte (count=1).
    #[tokio::test]
    async fn insert_batch_with_agent_id_present() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let mut dto = make_dto("/v1/chat", false);
        dto.agent_id = Some("claude-backend-agent".to_owned());

        let n = store
            .insert_batch("main", &[dto])
            .await
            .expect("insert avec agent_id");
        assert_eq!(n, 1, "doit insérer 1 ligne avec agent_id");
        assert_eq!(store.count().await.expect("count"), 1);
    }

    /// insert_batch : event avec agent_id=None → colonne NULL, insertion correcte.
    #[tokio::test]
    async fn insert_batch_with_agent_id_none() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let dto = make_dto("/v1/embed", false); // agent_id = None par défaut dans make_dto

        let n = store
            .insert_batch("main", &[dto])
            .await
            .expect("insert avec agent_id None");
        assert_eq!(n, 1, "doit insérer 1 ligne avec agent_id NULL");
        assert_eq!(store.count().await.expect("count"), 1);
    }

    /// insert_batch : batch mixte (agent_id présent + None) → toutes insérées.
    #[tokio::test]
    async fn insert_batch_mixed_agent_id() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let mut dto_with = make_dto("/v1/chat", true);
        dto_with.agent_id = Some("some-agent".to_owned());
        let dto_without = make_dto("/v1/embed", false); // agent_id = None

        let n = store
            .insert_batch("main", &[dto_with, dto_without])
            .await
            .expect("insert batch mixte agent_id");
        assert_eq!(n, 2, "doit insérer 2 lignes (agent_id présent + None)");
        assert_eq!(store.count().await.expect("count"), 2);
    }
}
