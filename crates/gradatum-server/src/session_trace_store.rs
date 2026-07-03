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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};
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

/// Curseur keyset opaque `(created_at, id)` — pagination stable `created_at DESC, id DESC`.
///
/// Encodé en HTTP comme `"<created_at>_<id>"` par le handler.
#[derive(Debug, Clone, Copy)]
pub struct TraceCursor {
    /// Valeur `created_at` de la dernière ligne de la page précédente.
    pub created_at: i64,
    /// Valeur `id` (rowid) de la dernière ligne de la page précédente.
    pub id: i64,
}

/// Filtre de requête pour [`SessionTraceStore::query_traces`].
///
/// `tenant_id` provient du JWT (jamais du body). Tous les autres champs sont optionnels.
/// `limit` est déjà validé/cappé (≤ 200, ≥ 1) par le handler avant d'être passé ici.
#[derive(Debug, Clone)]
pub struct TraceQuery {
    /// Tenant scopé par le JWT — jamais du body/query HTTP.
    pub tenant_id: String,
    /// Filtre optionnel sur `action_type` (ex. `"plan"`, `"decision"`).
    pub action_type: Option<String>,
    /// Filtre optionnel sur `agent_id`.
    pub agent_id: Option<String>,
    /// Filtre optionnel sur `session_id`.
    pub session_id: Option<String>,
    /// Borne inférieure inclusive sur `ts_ms`.
    pub from_ms: Option<i64>,
    /// Borne supérieure inclusive sur `ts_ms`.
    pub to_ms: Option<i64>,
    /// Curseur keyset pour la pagination (page suivante).
    pub cursor: Option<TraceCursor>,
    /// Nombre maximum de lignes à retourner. Le store renvoie jusqu'à `limit + 1`
    /// pour permettre au handler de détecter l'existence d'une page suivante.
    pub limit: u32,
}

/// Ligne projetée en lecture depuis `session_trace`.
///
/// `marker` is omitted (reserved for future use, NULL in the current schema).
#[derive(Debug, Clone)]
pub struct TraceRow {
    /// Rowid SQLite de la ligne.
    pub id: i64,
    /// Session ULID.
    pub session_id: String,
    /// Identité de l'agent émetteur (JWT `sub`).
    pub agent_id: String,
    /// Horodatage epoch ms fourni par le client.
    pub ts_ms: i64,
    /// Type d'action (`plan` | `edit` | `tool-call` | etc.).
    pub action_type: String,
    /// Cible de l'action (≤ 512 chars).
    pub target: Option<String>,
    /// Intention courte (≤ 200 chars).
    pub intent: Option<String>,
    /// Résultat (`success` | `failure` | `partial`).
    pub outcome: Option<String>,
    /// Référence (sha7 | ULID | section/ULID).
    pub ref_: Option<String>,
    /// Horodatage d'insertion serveur en epoch ms.
    pub created_at: i64,
}

/// Identifiant sentinelle pour les traces internes `context-sent`.
///
/// `assemble_context` does not carry a JWT `sub` (internal path without a caller identity).
/// This constant is used as the fixed `agent_id` for all rows inserted by
/// [`SessionTraceStore::mark_sent`].
///
/// Wired in production by `context/mod.rs`.
#[cfg_attr(not(test), allow(dead_code))]
pub const AGENT_CONTEXT_WINDOW: &str = "context-window";

/// Longueur maximale (en caractères Unicode) du snippet stocké dans `marker`.
#[cfg_attr(not(test), allow(dead_code))]
const SENT_SNIPPET_MAX_CHARS: usize = 512;

/// Entrée retournée par [`SessionTraceStore::get_sent`].
///
/// Représente le snippet et l'horodatage du **1er** [`SessionTraceStore::mark_sent`]
/// for a given ULID in a session. `ts_ms` is exposed for use by `fold_score` (`age_ms`).
///
/// Wired in production by `context/mod.rs`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct SentEntry {
    /// Snippet figé au 1er `mark_sent` — jamais ré-extrait du body courant.
    pub snippet: String,
    /// Horodatage epoch ms du 1er envoi dans la session.
    pub ts_ms: i64,
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

    /// Opens an in-memory connection for tests (unit and integration).
    ///
    /// Creates the `session_trace` table directly (without a migration runner).
    /// DDL is copied from `0015_session_trace.sql`.
    ///
    /// **Do not use in production** — use [`SessionTraceStore::open`].
    /// Available without `#[cfg(test)]` gate to allow usage from external integration
    /// tests (`tests/`) which compile the crate without the test cfg flag.
    #[allow(dead_code)]
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

    /// Marque un ULID comme « déjà envoyé dans la session courante ».
    ///
    /// Insère une ligne append-only `action_type="context-sent"` dans `session_trace`
    /// avec `agent_id=`[`AGENT_CONTEXT_WINDOW`], `target=ulid`, `marker=snippet` borné
    /// à [`SENT_SNIPPET_MAX_CHARS`] caractères Unicode, et `ts_ms=now_ms`.
    ///
    /// L'appel est sûr si invoqué plusieurs fois sur le même ULID : [`get_sent`] conserve
    /// le snippet du 1er mark (snippet figé, contrainte cache v0.7.2).
    ///
    /// # Errors
    ///
    /// `SessionTraceError::Sqlite` si l'insertion échoue.
    ///
    /// [`get_sent`]: SessionTraceStore::get_sent
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn mark_sent(
        &self,
        tenant: &str,
        session_id: &str,
        ulid: &str,
        snippet: &str,
        now_ms: i64,
    ) -> Result<(), SessionTraceError> {
        let bounded = truncate_to_chars(snippet, SENT_SNIPPET_MAX_CHARS);
        let row = SessionTraceRow {
            session_id: session_id.to_owned(),
            agent_id: AGENT_CONTEXT_WINDOW.to_owned(),
            ts_ms: now_ms,
            action_type: "context-sent".to_owned(),
            target: Some(ulid.to_owned()),
            intent: None,
            outcome: None,
            marker: Some(bounded),
            ref_: None,
        };
        self.insert_at(tenant, &row, now_ms).await?;
        Ok(())
    }

    /// Retourne la carte `ulid → SentEntry` des notes déjà envoyées dans `session_id`.
    ///
    /// Pour chaque ULID, seul le **1er** `mark_sent` (plus ancien `ts_ms`, puis `id` en
    /// tiebreaker) est conservé — snippet figé, conforme à la contrainte cache v0.7.2
    /// (Global Constraint 5).
    ///
    /// La requête filtre sur `tenant_id` **et** `session_id` (audit P2-1 : isolation
    /// multi-tenant stricte).
    ///
    /// # Errors
    ///
    /// `SessionTraceError::Sqlite` sur erreur de base de données.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn get_sent(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<HashMap<String, SentEntry>, SessionTraceError> {
        let conn = Arc::clone(&self.conn);
        let tenant = tenant.to_owned();
        let session_id = session_id.to_owned();

        let entries = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT target, marker, ts_ms
                 FROM session_trace
                 WHERE tenant_id = ?1
                   AND session_id = ?2
                   AND action_type = 'context-sent'
                 ORDER BY ts_ms ASC, id ASC",
            )?;
            let mut map: HashMap<String, SentEntry> = HashMap::new();
            let rows = stmt.query_map(params![tenant, session_id], |row| {
                let target: Option<String> = row.get(0)?;
                let marker: Option<String> = row.get(1)?;
                let ts_ms: i64 = row.get(2)?;
                Ok((target, marker, ts_ms))
            })?;
            for row in rows {
                let (target, marker, ts_ms) = row?;
                if let (Some(ulid), Some(snippet)) = (target, marker) {
                    // Garder uniquement le 1er mark par ULID (ORDER BY ts_ms ASC, id ASC).
                    map.entry(ulid).or_insert(SentEntry { snippet, ts_ms });
                }
            }
            Ok::<HashMap<String, SentEntry>, rusqlite::Error>(map)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(entries)
    }

    /// Lit les traces `session_trace` filtrées, triées `created_at DESC, id DESC`.
    ///
    /// Always applies the `action_type != 'context-sent'` filter —
    /// a **hard unconditional SQL clause**, never omitted.
    /// Tenant-scopé par `q.tenant_id` (provient du JWT, jamais du body).
    ///
    /// Renvoie jusqu'à `q.limit + 1` lignes. Le handler tronque à `q.limit` et
    /// calcule `next_cursor` si la ligne supplémentaire est présente.
    ///
    /// # Errors
    ///
    /// `SessionTraceError::Sqlite` sur erreur de base de données.
    /// `SessionTraceError::Blocking` si le thread bloquant panique.
    pub async fn query_traces(&self, q: &TraceQuery) -> Result<Vec<TraceRow>, SessionTraceError> {
        let conn = Arc::clone(&self.conn);
        let q = q.clone();

        let rows = tokio::task::spawn_blocking(move || {
            // Clauses fixes : tenant + exclusion context-sent (prérequis dur F-85).
            let mut sql = String::from(
                "SELECT id, session_id, agent_id, ts_ms, action_type, target, intent, \
                        outcome, ref, created_at \
                 FROM session_trace \
                 WHERE tenant_id = ?1 AND action_type != 'context-sent'",
            );
            // Paramètres dynamiques liés par position (?1, ?2, …).
            // binds[0] = ?1 (tenant_id), binds[1] = ?2, etc.
            let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> =
                vec![Box::new(q.tenant_id.clone())];
            let mut next = 2u8;

            if let Some(ref a) = q.action_type {
                sql.push_str(&format!(" AND action_type = ?{next}"));
                binds.push(Box::new(a.clone()));
                next += 1;
            }
            if let Some(ref a) = q.agent_id {
                sql.push_str(&format!(" AND agent_id = ?{next}"));
                binds.push(Box::new(a.clone()));
                next += 1;
            }
            if let Some(ref s) = q.session_id {
                sql.push_str(&format!(" AND session_id = ?{next}"));
                binds.push(Box::new(s.clone()));
                next += 1;
            }
            if let Some(f) = q.from_ms {
                sql.push_str(&format!(" AND ts_ms >= ?{next}"));
                binds.push(Box::new(f));
                next += 1;
            }
            if let Some(t) = q.to_ms {
                sql.push_str(&format!(" AND ts_ms <= ?{next}"));
                binds.push(Box::new(t));
                next += 1;
            }
            if let Some(cur) = q.cursor {
                // Keyset pagination : exclut la page précédente.
                sql.push_str(&format!(
                    " AND (created_at < ?{a} OR (created_at = ?{a} AND id < ?{b}))",
                    a = next,
                    b = next + 1
                ));
                binds.push(Box::new(cur.created_at));
                binds.push(Box::new(cur.id));
                next += 2;
            }
            // ORDER + LIMIT : renvoie limit+1 pour détecter la page suivante.
            sql.push_str(&format!(" ORDER BY created_at DESC, id DESC LIMIT ?{next}"));
            binds.push(Box::new(i64::from(q.limit) + 1));

            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                binds.iter().map(|b| b.as_ref()).collect();
            let rows = stmt
                .query_map(params_ref.as_slice(), |row| {
                    Ok(TraceRow {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        agent_id: row.get(2)?,
                        ts_ms: row.get(3)?,
                        action_type: row.get(4)?,
                        target: row.get(5)?,
                        intent: row.get(6)?,
                        outcome: row.get(7)?,
                        ref_: row.get(8)?,
                        created_at: row.get(9)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<Vec<TraceRow>, rusqlite::Error>(rows)
        })
        .await
        .map_err(|_| SessionTraceError::Blocking)??;

        Ok(rows)
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

/// Tronque `s` à `max_chars` caractères Unicode sans couper au milieu d'un codepoint.
///
/// Retourne une `String` possédée — coût O(`max_chars`) sur la tranche tronquée.
#[cfg_attr(not(test), allow(dead_code))]
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_owned(),
        None => s.to_owned(),
    }
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

    // --- Tests sent-tracker (Task 5) ---

    #[tokio::test]
    async fn mark_then_get_sent_returns_ulid_snippet_ts() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.mark_sent("tenant-a", "sess-1", "ulid-A", "snippet text", 100)
            .await
            .expect("mark_sent");
        let sent = s.get_sent("tenant-a", "sess-1").await.expect("get_sent");
        let entry = sent
            .get("ulid-A")
            .expect("ulid-A doit être présent dans la map");
        assert_eq!(entry.snippet, "snippet text");
        assert_eq!(entry.ts_ms, 100);
    }

    #[tokio::test]
    async fn get_sent_snippet_is_frozen_first() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.mark_sent("tenant-a", "sess-1", "ulid-A", "snip1", 100)
            .await
            .expect("mark 1");
        s.mark_sent("tenant-a", "sess-1", "ulid-A", "snip2", 200)
            .await
            .expect("mark 2 (même ulid, snippet différent)");
        let sent = s.get_sent("tenant-a", "sess-1").await.expect("get_sent");
        let entry = sent.get("ulid-A").expect("ulid-A présent");
        assert_eq!(
            entry.snippet, "snip1",
            "le 1er snippet doit être figé au mark initial"
        );
        assert_eq!(entry.ts_ms, 100, "ts_ms du 1er mark conservé");
    }

    #[tokio::test]
    async fn get_sent_scoped_by_tenant_and_session() {
        let s = SessionTraceStore::open_in_memory()
            .await
            .expect("open in-memory");
        s.mark_sent("tenant-a", "sess-1", "ulid-A", "snip-a1", 100)
            .await
            .expect("mark tenant-a/sess-1");
        s.mark_sent("tenant-b", "sess-1", "ulid-B", "snip-b1", 100)
            .await
            .expect("mark tenant-b/sess-1 (autre tenant)");
        s.mark_sent("tenant-a", "sess-2", "ulid-C", "snip-a2", 100)
            .await
            .expect("mark tenant-a/sess-2 (autre session)");
        let sent = s.get_sent("tenant-a", "sess-1").await.expect("get_sent");
        assert!(
            sent.contains_key("ulid-A"),
            "ulid-A présent — même tenant + même session"
        );
        assert!(
            !sent.contains_key("ulid-B"),
            "ulid-B absent — tenant-b isolé de tenant-a"
        );
        assert!(
            !sent.contains_key("ulid-C"),
            "ulid-C absent — sess-2 isolée de sess-1"
        );
        assert_eq!(sent.len(), 1, "exactement 1 entrée pour tenant-a/sess-1");
    }

    // --- Tests query_traces (Task 1 Slice 3) ---

    /// Insère une trace « normale » avec `created_at` explicite — retourne l'id.
    async fn insert_normal(
        store: &SessionTraceStore,
        tenant: &str,
        session: &str,
        agent: &str,
        action: &str,
        ts: i64,
        created: i64,
    ) -> i64 {
        let row = SessionTraceRow {
            session_id: session.to_owned(),
            agent_id: agent.to_owned(),
            ts_ms: ts,
            action_type: action.to_owned(),
            target: Some("t".into()),
            intent: Some("i".into()),
            outcome: Some("success".into()),
            marker: None,
            ref_: None,
        };
        store
            .insert_at(tenant, &row, created)
            .await
            .expect("insert_normal")
    }

    /// `context-sent` est toujours exclu, même quand aucun autre filtre n'est posé.
    #[tokio::test]
    async fn query_traces_excludes_context_sent() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        store
            .mark_sent("main", "S1", "01ABC", "snippet", 1000)
            .await
            .unwrap();
        insert_normal(&store, "main", "S1", "agentX", "decision", 2000, 2000).await;
        let q = TraceQuery {
            tenant_id: "main".into(),
            action_type: None,
            agent_id: None,
            session_id: None,
            from_ms: None,
            to_ms: None,
            cursor: None,
            limit: 50,
        };
        let rows = store.query_traces(&q).await.unwrap();
        assert_eq!(rows.len(), 1, "context-sent doit être exclu");
        assert_eq!(rows[0].action_type, "decision");
    }

    /// Même si `action_type = "context-sent"` est passé en filtre, le prérequis
    /// hard filter neutralizes the request — no rows should be returned.
    #[tokio::test]
    async fn query_traces_filter_context_sent_value_is_neutralized() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        store
            .mark_sent("main", "S1", "01ABC", "snippet", 1000)
            .await
            .unwrap();
        let q = TraceQuery {
            tenant_id: "main".into(),
            action_type: Some("context-sent".into()),
            agent_id: None,
            session_id: None,
            from_ms: None,
            to_ms: None,
            cursor: None,
            limit: 50,
        };
        let rows = store.query_traces(&q).await.unwrap();
        assert!(
            rows.is_empty(),
            "demander context-sent reste neutralisé par la clause fixe"
        );
    }

    /// Ordre `created_at DESC` et filtres `action_type` / `agent_id` + `session_id`.
    #[tokio::test]
    async fn query_traces_orders_created_desc_and_filters() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        insert_normal(&store, "main", "S1", "a", "plan", 100, 100).await;
        insert_normal(&store, "main", "S1", "a", "decision", 200, 200).await;
        insert_normal(&store, "main", "S2", "b", "plan", 300, 300).await;

        // Tri created_at DESC.
        let all = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: None,
                session_id: None,
                from_ms: None,
                to_ms: None,
                cursor: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].created_at > all[1].created_at, "ordre DESC attendu");

        // Filtre action_type.
        let plans = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: Some("plan".into()),
                agent_id: None,
                session_id: None,
                from_ms: None,
                to_ms: None,
                cursor: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(plans.len(), 2);
        assert!(plans.iter().all(|r| r.action_type == "plan"));

        // Filtre agent_id + session_id.
        let f = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: Some("b".into()),
                session_id: Some("S2".into()),
                from_ms: None,
                to_ms: None,
                cursor: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].agent_id, "b");
    }

    /// Bornes temporelles `from_ms` et `to_ms` inclusives sur `ts_ms`.
    #[tokio::test]
    async fn query_traces_time_bounds_inclusive_on_ts_ms() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        insert_normal(&store, "main", "S", "a", "plan", 100, 1).await;
        insert_normal(&store, "main", "S", "a", "plan", 200, 2).await;
        insert_normal(&store, "main", "S", "a", "plan", 300, 3).await;
        let rows = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: None,
                session_id: None,
                from_ms: Some(200),
                to_ms: Some(300),
                cursor: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.ts_ms >= 200 && r.ts_ms <= 300));
    }

    /// Isolation tenant : les traces d'un autre tenant ne doivent pas apparaître.
    #[tokio::test]
    async fn query_traces_tenant_isolation() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        insert_normal(&store, "main", "S", "a", "plan", 100, 1).await;
        insert_normal(&store, "other", "S", "a", "plan", 100, 2).await;
        let rows = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: None,
                session_id: None,
                from_ms: None,
                to_ms: None,
                cursor: None,
                limit: 50,
            })
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "seule la trace du tenant main doit être retournée"
        );
    }

    /// Pagination keyset : la page 2 ne chevauche pas la page 1 et ne laisse pas de trou.
    #[tokio::test]
    async fn query_traces_keyset_pagination_no_gap_no_overlap() {
        let store = SessionTraceStore::open_in_memory().await.unwrap();
        for i in 1i64..=5 {
            insert_normal(&store, "main", "S", "a", "plan", i * 10, i).await;
        }
        // Page 1 : limit=2 → query_traces renvoie limit+1 = 3 (le handler tronquera).
        let p1 = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: None,
                session_id: None,
                from_ms: None,
                to_ms: None,
                cursor: None,
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(p1.len(), 3, "limit+1 pour détecter la page suivante");

        // Cursor = dernier élément retenu (index limit-1 = 1).
        let last = &p1[1];
        let cur = TraceCursor {
            created_at: last.created_at,
            id: last.id,
        };

        let p2 = store
            .query_traces(&TraceQuery {
                tenant_id: "main".into(),
                action_type: None,
                agent_id: None,
                session_id: None,
                from_ms: None,
                to_ms: None,
                cursor: Some(cur),
                limit: 2,
            })
            .await
            .unwrap();
        // Aucun chevauchement : tous les ids de p2 sont < id du cursor.
        assert!(
            p2.iter().all(|r| r.id < last.id),
            "p2 ne doit pas contenir des lignes de p1"
        );
    }
}
