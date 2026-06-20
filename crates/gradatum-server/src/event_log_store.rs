//! Append-only store for the `event_log` table.
//!
//! ## Design
//!
//! `EventLogStore` opens its own `rusqlite::Connection` on the same database
//! as `SqliteIndex` (`index.db`). The database is in WAL mode — multiple connections
//! are safe: reads are non-blocking, writes are serialised by SQLite itself
//! (`busy_timeout` 5000 ms).
//!
//! The `event_log` table is created by migration `0006_event_log.sql`, executed
//! when `SqliteIndex` is opened in `AppState::with_search_path`.
//!
//! ## Immutability guarantees
//!
//! Only `insert_batch` writes rows — no UPDATE path per record.
//! The retention purge deletes rows in bulk by age (internal maintenance).
//! This design guarantees append-only semantics without an `AclOp::Append` check.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` is neither `Send` nor `Sync`. It is wrapped in
//! `Arc<Mutex<Connection>>` (Tokio mutex) — the same pattern as `SqliteIndex`.
//! Locks are held for the minimum scope (dropped before any `.await`).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tokio::sync::Mutex;

use gradatum_dto::QaEventDto;

/// Error type for the `event_log` store.
#[derive(Debug, Error)]
pub enum EventLogError {
    /// Underlying SQLite error.
    #[error("event_log SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// RFC 3339 timestamp from a `QaEvent` could not be parsed.
    #[error("event_log timestamp invalide : {0}")]
    BadTimestamp(String),
    /// Mutex poisoned — cannot happen with a Tokio mutex.
    #[error("event_log mutex poisonné")]
    Poisoned,
}

/// Derives the `outcome` on a best-effort basis from an HTTP status code.
///
/// Mapping:
/// - `2xx` → `"Success"`
/// - `4xx` → `"Rejected"` (client error — request served but rejected)
/// - `5xx` → `"Error"` (server/model error)
/// - other (1xx/3xx) → `None` (indeterminate, stored as NULL)
///
/// Best-effort: a missing outcome (`None`) is valid — the column is nullable.
fn outcome_from_status(status_code: u16) -> Option<&'static str> {
    match status_code {
        200..=299 => Some("Success"),
        400..=499 => Some("Rejected"),
        500..=599 => Some("Error"),
        _ => None,
    }
}

/// Pending event projection from the `event_log` table.
///
/// Returned by [`EventLogStore::fetch_pending`]. Contains the fields required
/// for downstream consumption (distillation). Never exposes prompt/response content
/// (metadata-only schema, confidentiality invariant).
// API publique (lib) consommée par F-22 (lot B v0.4.4) + tests d'intégration.
// Le bin gradatum-server ne la câble pas encore → invisible au dead_code lint binaire
// (même convention que state.rs:577). Sera utilisée par handle_distill au lot B.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEvent {
    /// Internal identifier (rowid) — key passed to `mark_processed`.
    pub id: i64,
    /// Request timestamp in epoch ms (parsed from RFC 3339 at insertion time).
    pub ts: i64,
    /// HTTP route served (e.g. `/v1/embeddings`).
    pub route: String,
    /// Model alias used by the client.
    pub model_alias: String,
    /// Resolved effective provider.
    pub provider: String,
    /// Derived feature (e.g. `embed`, `chat`) — nullable.
    pub feature_id: Option<String>,
    /// HTTP response code.
    pub status_code: u16,
    /// Emitting agent identifier — nullable.
    pub agent_id: Option<String>,
    /// Best-effort outcome (`Success`/`Rejected`/`Error`) — nullable.
    pub outcome: Option<String>,
}

/// Append-only store for the `event_log` table.
///
/// Cloneable (inner `Arc`) — injected into `AppState` and shared between
/// handlers and the retention task.
#[derive(Clone)]
pub struct EventLogStore {
    /// Dedicated SQLite connection — separate from `SqliteIndex` to avoid deadlocks.
    ///
    /// Same `index.db` file (WAL) — SQLite guarantees multi-connection consistency.
    conn: Arc<Mutex<Connection>>,
}

impl EventLogStore {
    /// Opens a dedicated WAL connection at `path` for the `event_log` table.
    ///
    /// WAL and `busy_timeout` PRAGMAs are applied immediately.
    /// Migration 0006 must already have been executed by `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Returns `EventLogError::Sqlite` if the file is inaccessible or PRAGMAs fail.
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

    /// Opens an in-memory connection for unit tests.
    ///
    /// Creates the `event_log` table directly (without a migration runner).
    /// Must NOT be used in production.
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
                    agent_id     TEXT,
                    outcome      TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_event_log_created   ON event_log(created_at);
                CREATE INDEX IF NOT EXISTS idx_event_log_tenant    ON event_log(tenant_id);
                CREATE INDEX IF NOT EXISTS idx_event_log_feature   ON event_log(feature_id);
                CREATE INDEX IF NOT EXISTS idx_event_log_processed ON event_log(processed);
                CREATE INDEX IF NOT EXISTS idx_event_log_agent     ON event_log(agent_id);
                CREATE INDEX IF NOT EXISTS idx_event_log_outcome   ON event_log(outcome);",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Inserts a batch of `QaEventDto` in a single transaction.
    ///
    /// Returns the number of inserted rows.
    ///
    /// `tenant_id` is extracted from the JWT (`TrustContext`) — never from the request body.
    /// `created_at` is the server epoch ms at call time.
    /// `ts` is parsed from `dto.timestamp` (RFC 3339) → epoch ms.
    ///
    /// # Errors
    ///
    /// - `EventLogError::BadTimestamp` if a timestamp cannot be parsed.
    /// - `EventLogError::Sqlite` on database error.
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
                // F-19 M6 : outcome dérivé best-effort du status_code à l'écriture.
                let outcome = outcome_from_status(dto.status_code);
                tx.execute(
                    "INSERT INTO event_log
                        (ts, tenant_id, route, model_alias, model_used, provider,
                         feature_id, status_code, latency_ms,
                         tokens_input, tokens_output, cost_usd,
                         processed, created_at, agent_id, outcome)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0,?13,?14,?15)",
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
                        outcome,
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

    /// Purges stale rows according to two criteria.
    ///
    /// 1. **Age**: deletes rows where `created_at < retention_cutoff_ms`.
    /// 2. **Cap**: if the total row count exceeds `max_rows`, deletes the oldest
    ///    rows until exactly `max_rows` remain.
    ///
    /// Returns the total number of deleted rows.
    ///
    /// This is internal maintenance — never exposed via ACL.
    /// It does not affect the immutability of retained records.
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

    /// Returns the total number of rows in `event_log`.
    ///
    /// Used to update the `gradatum_event_log_rows` Prometheus gauge.
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

    /// Reads up to `limit` unprocessed events (`processed = 0`) for the tenant,
    /// oldest first (FIFO by `created_at` then `id`).
    ///
    /// Read-only: does NOT modify `processed`. Marking is explicit via
    /// [`EventLogStore::mark_processed`] AFTER successful processing. This
    /// read/mark decoupling lets the consumer mark only what it actually processed —
    /// a crash between `fetch` and `mark` re-delivers the same events (at-least-once),
    /// never losing them.
    ///
    /// ## Double-processing prevention
    ///
    /// Idempotence relies on a **single worker** (concurrency=1): one consumer reads
    /// then marks sequentially. `mark_processed` is transactional; two concurrent
    /// `fetch_pending` calls could return the same rows, hence the single-worker
    /// constraint must be preserved.
    ///
    /// `limit = 0` returns an empty vec without querying.
    ///
    /// # Errors
    ///
    /// `EventLogError::Sqlite` on database error; `EventLogError::Poisoned` if the
    /// blocking thread fails.
    // API lib consommée par F-22 (lot B) + tests — non câblée par le bin actuellement.
    #[allow(dead_code)]
    pub async fn fetch_pending(
        &self,
        tenant_id: &str,
        limit: u32,
    ) -> Result<Vec<PendingEvent>, EventLogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = Arc::clone(&self.conn);
        let tenant_id = tenant_id.to_owned();

        let rows = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, ts, route, model_alias, provider, feature_id,
                        status_code, agent_id, outcome
                 FROM event_log
                 WHERE processed = 0 AND tenant_id = ?1
                 ORDER BY created_at ASC, id ASC
                 LIMIT ?2",
            )?;
            let mapped = stmt
                .query_map(params![tenant_id, limit as i64], |r| {
                    let status_i64: i64 = r.get(6)?;
                    Ok(PendingEvent {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        route: r.get(2)?,
                        model_alias: r.get(3)?,
                        provider: r.get(4)?,
                        feature_id: r.get(5)?,
                        // status_code stocké en INTEGER — borné [0, u16::MAX] par construction
                        // (toujours un code HTTP valide à l'insertion). Clamp défensif.
                        status_code: status_i64.clamp(0, u16::MAX as i64) as u16,
                        agent_id: r.get(7)?,
                        outcome: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<Vec<PendingEvent>, rusqlite::Error>(mapped)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(rows)
    }

    /// Marks events identified by `ids` as processed (`processed = 1`).
    ///
    /// Transactional: either all `ids` are marked or none (atomicity).
    /// Returns the number of rows actually updated (may be less than `ids.len()`
    /// if some ids do not exist or were already `processed = 1`).
    ///
    /// An empty `ids` slice returns `Ok(0)` without querying.
    ///
    /// ## Append-only preserved
    ///
    /// `processed` is an internal consumption flag, not a mutation of telemetry content
    /// (route/latency/status remain immutable). This flag is outside the immutability
    /// scope (consumption, not data rewrite).
    ///
    /// # Errors
    ///
    /// `EventLogError::Sqlite` on database error; `EventLogError::Poisoned` if the
    /// blocking thread fails.
    // API lib consommée par F-22 (lot B) + tests — non câblée par le bin actuellement.
    #[allow(dead_code)]
    pub async fn mark_processed(&self, ids: &[i64]) -> Result<usize, EventLogError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = Arc::clone(&self.conn);
        let ids: Vec<i64> = ids.to_vec();

        let updated = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let tx = conn.unchecked_transaction()?;
            let mut total = 0usize;
            {
                let mut stmt = tx.prepare(
                    "UPDATE event_log SET processed = 1 WHERE id = ?1 AND processed = 0",
                )?;
                for id in &ids {
                    total += stmt.execute(params![id])?;
                }
            }
            tx.commit()?;
            Ok::<usize, rusqlite::Error>(total)
        })
        .await
        .map_err(|_| EventLogError::Poisoned)??;

        Ok(updated)
    }
}

/// Returns the current system time as epoch milliseconds.
///
/// Panics only if the system clock is before the Unix epoch — impossible in practice.
fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("horloge système avant epoch UNIX — invariant système")
        .as_millis() as i64
}

/// Parses an RFC 3339 timestamp into epoch milliseconds.
///
/// # Errors
///
/// Returns `EventLogError::BadTimestamp` if the format is invalid.
/// The timestamp stored in the error is sanitised (control characters filtered,
/// max 64 chars) to prevent log injection (CR/LF/ANSI).
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
        dto.agent_id = Some("example-agent".to_owned());

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

    // ── Tests outcome (F-19 M6) ─────────────────────────────────────────────────

    #[test]
    fn outcome_from_status_maps_ranges() {
        assert_eq!(outcome_from_status(200), Some("Success"));
        assert_eq!(outcome_from_status(204), Some("Success"));
        assert_eq!(outcome_from_status(400), Some("Rejected"));
        assert_eq!(outcome_from_status(404), Some("Rejected"));
        assert_eq!(outcome_from_status(500), Some("Error"));
        assert_eq!(outcome_from_status(503), Some("Error"));
        assert_eq!(outcome_from_status(301), None, "3xx non discriminé");
        assert_eq!(outcome_from_status(100), None, "1xx non discriminé");
    }

    /// insert_batch remplit outcome best-effort depuis status_code (fetch_pending le lit).
    #[tokio::test]
    async fn insert_fills_outcome_from_status() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");

        let mut ok = make_dto("/v1/chat", false);
        ok.status_code = 200;
        let mut rejected = make_dto("/v1/embed", false);
        rejected.status_code = 400;
        let mut err = make_dto("/v1/chat", false);
        err.status_code = 503;

        store
            .insert_batch("main", &[ok, rejected, err])
            .await
            .expect("insert");

        let pending = store
            .fetch_pending("main", 100)
            .await
            .expect("fetch_pending");
        assert_eq!(pending.len(), 3);
        // FIFO : ordre d'insertion préservé (created_at identique → ORDER BY id ASC).
        assert_eq!(pending[0].outcome.as_deref(), Some("Success"));
        assert_eq!(pending[1].outcome.as_deref(), Some("Rejected"));
        assert_eq!(pending[2].outcome.as_deref(), Some("Error"));
    }

    // ── Tests fetch_pending + mark_processed (F-19 M5) ──────────────────────────

    #[tokio::test]
    async fn fetch_pending_returns_unprocessed_only() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        let batch = vec![make_dto("/v1/chat", false), make_dto("/v1/embed", false)];
        store.insert_batch("main", &batch).await.expect("insert");

        let pending = store.fetch_pending("main", 10).await.expect("fetch");
        assert_eq!(pending.len(), 2, "2 events pending au départ");

        // Marquer le premier comme traité.
        let marked = store.mark_processed(&[pending[0].id]).await.expect("mark");
        assert_eq!(marked, 1, "1 row marquée");

        let after = store.fetch_pending("main", 10).await.expect("fetch 2");
        assert_eq!(after.len(), 1, "1 event pending restant après marquage");
        assert_eq!(after[0].id, pending[1].id, "le restant est le 2e event");
    }

    #[tokio::test]
    async fn fetch_pending_respects_limit_and_tenant() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        // 3 events tenant 'main', 1 event tenant 'other'.
        store
            .insert_batch(
                "main",
                &[
                    make_dto("/a", false),
                    make_dto("/b", false),
                    make_dto("/c", false),
                ],
            )
            .await
            .expect("insert main");
        store
            .insert_batch("other", &[make_dto("/x", false)])
            .await
            .expect("insert other");

        // limit=2 sur 'main' → 2 rows max.
        let limited = store.fetch_pending("main", 2).await.expect("fetch limit");
        assert_eq!(limited.len(), 2, "limit respecté");

        // tenant 'other' isolé → 1 row, jamais les events de 'main'.
        let other = store.fetch_pending("other", 10).await.expect("fetch other");
        assert_eq!(other.len(), 1, "isolation tenant");
        assert_eq!(other[0].route, "/x");

        // limit=0 → vec vide sans requête.
        let zero = store.fetch_pending("main", 0).await.expect("fetch 0");
        assert!(zero.is_empty(), "limit=0 retourne vide");
    }

    #[tokio::test]
    async fn mark_processed_is_idempotent_and_atomic() {
        let store = EventLogStore::open_in_memory()
            .await
            .expect("open in-memory");
        store
            .insert_batch("main", &[make_dto("/a", false), make_dto("/b", false)])
            .await
            .expect("insert");
        let pending = store.fetch_pending("main", 10).await.expect("fetch");
        let ids: Vec<i64> = pending.iter().map(|e| e.id).collect();

        // Premier marquage : 2 rows.
        let n1 = store.mark_processed(&ids).await.expect("mark 1");
        assert_eq!(n1, 2);

        // Second marquage (mêmes ids) : 0 row (déjà processed=1 → anti double-traitement).
        let n2 = store.mark_processed(&ids).await.expect("mark 2");
        assert_eq!(
            n2, 0,
            "re-marquage = 0 (WHERE processed=0 exclut déjà traités)"
        );

        // ids vide → 0 sans erreur.
        let n3 = store.mark_processed(&[]).await.expect("mark empty");
        assert_eq!(n3, 0);

        // ids inexistants → 0 sans erreur.
        let n4 = store.mark_processed(&[999_999]).await.expect("mark ghost");
        assert_eq!(n4, 0, "id inexistant ne marque rien");
    }
}
