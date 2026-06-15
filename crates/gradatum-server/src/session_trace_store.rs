//! Session-log Tier 1 — append-only store for the `session_trace` table.
//!
//! ## Design
//!
//! Mirrors [`crate::event_log_store::EventLogStore`]: a dedicated
//! `rusqlite::Connection` on the same `index.db` file as `SqliteIndex`,
//! in WAL mode (multi-connection safe — reads non-blocking, writes serialised
//! by SQLite itself, `busy_timeout` 5000 ms).
//!
//! The `session_trace` table is created by migration `0015_session_trace.sql`,
//! executed by `SqliteIndex::open` (via `with_search_path` in `AppState`).
//!
//! ## Immutability guarantees (append-only)
//!
//! Only [`SessionTraceStore::insert_at`] writes rows — no UPDATE path per record.
//! The retention purge ([`SessionTraceStore::purge`]) deletes rows in bulk by age
//! (internal maintenance, outside ACL). Append-only by construction.
//!
//! ## Security invariants
//!
//! The store receives data already validated by the handler:
//! - `agent_id` = JWT `sub`, resolved server-side, never from the body.
//! - `tenant_id` = JWT (never from the body), passed as an explicit argument.
//! - Field bounds enforced on the handler side before insertion.
//!
//! ## Thread-safety
//!
//! `rusqlite::Connection` is neither `Send` nor `Sync` → wrapped in
//! `Arc<Mutex<Connection>>` (Tokio mutex). Locks held for the minimum scope.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};
use thiserror::Error;
use tokio::sync::Mutex;

/// Error type for the `session_trace` store.
#[derive(Debug, Error)]
pub enum SessionTraceError {
    /// Underlying SQLite error.
    #[error("session_trace SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Blocking thread failed (panic or cancellation) — cannot happen in practice.
    #[error("session_trace thread blocking échoué")]
    Blocking,
}

/// Row to insert into `session_trace` — write projection.
///
/// Built by the handler from `SessionTraceRequest` + JWT identity.
/// `agent_id` and `tenant_id` come from the JWT (never from the body).
#[derive(Debug, Clone)]
pub struct SessionTraceRow {
    /// Session ULID (server-generated if omitted by the client).
    pub session_id: String,
    /// Emitting agent identity = JWT `sub`.
    pub agent_id: String,
    /// Action timestamp in epoch ms (supplied by the client).
    pub ts_ms: i64,
    /// Action type (`plan` | `edit` | `tool-call` | `decision` | `verdict` | `deploy` | …).
    pub action_type: String,
    /// Action target (≤ 512 chars, bounded by the handler).
    pub target: Option<String>,
    /// Short intent (≤ 200 chars, bounded by the handler).
    pub intent: Option<String>,
    /// Outcome (`success` | `failure` | `partial`).
    pub outcome: Option<String>,
    /// Tier 2 marker — always `None` in Tier 1.
    pub marker: Option<String>,
    /// Reference (sha7 | ULID | section/ULID), validated by the handler.
    pub ref_: Option<String>,
}

/// Append-only store for the `session_trace` table.
///
/// Cloneable (inner `Arc`) — injected into `AppState` and shared between
/// the `POST /api/v1/session-log/trace` handler and the 90-day retention task.
#[derive(Clone)]
pub struct SessionTraceStore {
    /// Dedicated SQLite connection — separate from `SqliteIndex` to avoid deadlocks.
    ///
    /// Same `index.db` file (WAL) — SQLite guarantees multi-connection consistency.
    conn: Arc<Mutex<Connection>>,
}

impl SessionTraceStore {
    /// Opens a dedicated WAL connection at `path` for the `session_trace` table.
    ///
    /// WAL and `busy_timeout` PRAGMAs are applied immediately.
    /// Migration 0015 must already have been executed by `SqliteIndex::open`.
    ///
    /// # Errors
    ///
    /// Returns `SessionTraceError::Sqlite` if the file is inaccessible or PRAGMAs fail.
    pub async fn open(path: &Path) -> Result<Self, SessionTraceError> {
        let path = path.to_path_buf();
        // Ouvrir la connexion rusqlite dans un thread dédié — `Connection::open`
        // peut bloquer sur les locks OS (WAL checkpoint) et n'est pas async.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // PRAGMA alignés sur SqliteIndex/EventLogStore — nécessaires par connexion.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;
            conn.pragma_update(None, "foreign_keys", true)?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an in-memory connection for unit tests.
    ///
    /// Creates the `session_trace` table directly (without a migration runner).
    /// DDL is copied from `0015_session_trace.sql`.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self, SessionTraceError> {
        let conn = tokio::task::spawn_blocking(|| {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session_trace (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id   TEXT    NOT NULL,
                    agent_id     TEXT    NOT NULL,
                    tenant_id    TEXT    NOT NULL,
                    ts_ms        INTEGER NOT NULL,
                    action_type  TEXT    NOT NULL,
                    target       TEXT,
                    intent       TEXT,
                    outcome      TEXT,
                    marker       TEXT,
                    ref          TEXT,
                    created_at   INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_session_trace_session ON session_trace(session_id);
                CREATE INDEX IF NOT EXISTS idx_session_trace_created ON session_trace(created_at);
                CREATE INDEX IF NOT EXISTS idx_session_trace_agent   ON session_trace(tenant_id, agent_id);",
            )?;
            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Inserts a row with `created_at` set to the current server epoch ms.
    ///
    /// `tenant_id` comes from the JWT (never from the body). The insert is awaited
    /// synchronously (not fire-and-forget) for reliability on all action types.
    ///
    /// Returns the `rowid` of the inserted row.
    ///
    /// # Errors
    ///
    /// `SessionTraceError::Sqlite` on database error.
    #[must_use = "le rowid retourné fait partie du contrat de réponse de l'endpoint"]
    pub async fn insert_trace(
        &self,
        tenant_id: &str,
        r: &SessionTraceRow,
    ) -> Result<i64, SessionTraceError> {
        let now = system_now_ms();
        self.insert_at(tenant_id, r, now).await
    }

    /// Inserts a row with an explicit `created_at` (for testing).
    ///
    /// See [`SessionTraceStore::insert_trace`] for semantics. An explicit `created_at`
    /// allows tests to simulate old rows (age-based purge).
    #[must_use = "le rowid retourné fait partie du contrat de réponse de l'endpoint"]
    pub async fn insert_at(
        &self,
        tenant_id: &str,
        r: &SessionTraceRow,
        created_at: i64,
    ) -> Result<i64, SessionTraceError> {
        let conn = Arc::clone(&self.conn);
        let tenant = tenant_id.to_owned();
        let r = r.clone();

        let id = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO session_trace
                    (session_id, agent_id, tenant_id, ts_ms, action_type,
                     target, intent, outcome, marker, ref, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    r.session_id,
                    r.agent_id,
                    tenant,
                    r.ts_ms,
                    r.action_type,
                    r.target,
                    r.intent,
                    r.outcome,
                    r.marker,
                    r.ref_,
                    created_at,
                ],
            )?;
            Ok::<i64, rusqlite::Error>(conn.last_insert_rowid())
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(id)
    }

    /// Returns the total number of rows in `session_trace`.
    ///
    /// Used by store tests (post-insert/purge assertions).
    /// The binary does not wire this method in production
    /// (the retention task does not expose a `session_trace_rows` gauge).
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn count(&self) -> Result<i64, SessionTraceError> {
        let conn = Arc::clone(&self.conn);
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM session_trace", [], |r| r.get(0))?;
            Ok::<i64, rusqlite::Error>(count)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(count)
    }

    /// Purges stale rows according to two criteria.
    ///
    /// 1. **Age**: deletes rows where `created_at < retention_cutoff_ms`.
    /// 2. **Cap**: if the total count exceeds `max_rows`, deletes the oldest rows
    ///    until exactly `max_rows` remain.
    ///
    /// Returns the total number of deleted rows.
    ///
    /// Internal maintenance — never exposed via ACL. Does not affect the
    /// immutability of retained records (append-only).
    pub async fn purge(
        &self,
        retention_cutoff_ms: i64,
        max_rows: i64,
    ) -> Result<usize, SessionTraceError> {
        let conn = Arc::clone(&self.conn);

        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut total_deleted = 0usize;

            // Passe 1 : suppression par âge.
            let deleted_age = conn.execute(
                "DELETE FROM session_trace WHERE created_at < ?1",
                params![retention_cutoff_ms],
            )?;
            total_deleted += deleted_age;

            // Passe 2 : cap max_rows — supprimer les plus anciennes au-delà du cap.
            let current_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM session_trace", [], |r| r.get(0))?;

            if current_count > max_rows {
                let excess = current_count - max_rows;
                let deleted_cap = conn.execute(
                    "DELETE FROM session_trace WHERE id IN (
                        SELECT id FROM session_trace ORDER BY created_at ASC LIMIT ?1
                    )",
                    params![excess],
                )?;
                total_deleted += deleted_cap;
            }

            Ok::<usize, rusqlite::Error>(total_deleted)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(deleted)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Construit une `SessionTraceRow` minimale pour les tests.
    fn row() -> SessionTraceRow {
        SessionTraceRow {
            session_id: "01HQ0000000000000000000000".into(),
            agent_id: "claude-code".into(),
            ts_ms: 1000,
            action_type: "deploy".into(),
            target: Some("gradatum-server".into()),
            intent: Some("deploy vault_timeline".into()),
            outcome: Some("success".into()),
            marker: None,
            ref_: Some("a9982a8".into()),
        }
    }

    #[tokio::test]
    async fn insert_then_count() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        let id = s.insert_trace("main", &row()).await.expect("insert");
        assert!(id > 0, "rowid doit être > 0");
        assert_eq!(s.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn insert_two_appends() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.insert_trace("main", &row()).await.expect("insert 1");
        s.insert_trace("main", &row()).await.expect("insert 2");
        assert_eq!(s.count().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn insert_accepts_none_optionals() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        let mut r = row();
        r.target = None;
        r.intent = None;
        r.outcome = None;
        r.ref_ = None;
        let id = s.insert_trace("main", &r).await.expect("insert none");
        assert!(id > 0);
        assert_eq!(s.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn purge_removes_old_by_age() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.insert_at("main", &row(), 0).await.expect("insert old"); // created_at=0 (vieux)
        s.insert_trace("main", &row()).await.expect("insert now"); // created_at=now
                                                                   // cutoff_ms=1000 (la row created_at=0 est en-dessous), max_rows haut.
        let removed = s.purge(1_000, 1_000_000).await.expect("purge");
        assert_eq!(removed, 1, "la row created_at=0 < cutoff doit être purgée");
        assert_eq!(s.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn purge_caps_max_rows() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        // 4 lignes created_at=now ; cutoff=0 (rien par âge) ; cap=2 → 2 supprimées.
        for i in 0..4 {
            s.insert_at("main", &row(), 100 + i).await.expect("insert");
        }
        let removed = s.purge(0, 2).await.expect("purge cap");
        assert_eq!(removed, 2, "5-3 ... ici 4-2 = 2 lignes supprimées par cap");
        assert_eq!(s.count().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn purge_noop_when_recent_and_under_cap() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.insert_trace("main", &row()).await.expect("insert");
        // cutoff=0 (rien par âge car created_at=now) + cap haut → 0 supprimées.
        let removed = s.purge(0, 1_000_000).await.expect("purge noop");
        assert_eq!(removed, 0);
        assert_eq!(s.count().await.expect("count"), 1);
    }
}
