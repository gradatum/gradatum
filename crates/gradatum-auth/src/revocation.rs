//! `RevocationStore` — storage and lookup of revoked JWT tokens (by `jti`).
//!
//! Trait with two implementations:
//! - [`InMemoryRevocationStore`]: dev-only, emits a WARN at boot, no persistence.
//! - [`SqliteRevocationStore`]: production, WAL, on-demand GC via `gc()` — no background
//!   task ships in `1.0.0`, so expired `jti` rows accumulate until an operator calls it.
//!
//! [`boot_guard_check`] rejects startup if the bind address is non-loopback and the store is `"memory"`.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dashmap::DashMap;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use tokio::sync::Mutex;

/// Error variants for `RevocationStore` operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RevocationError {
    /// SQLite error (via rusqlite).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The blocking thread failed (panic or cancellation) — impossible in practice.
    #[error("revocation blocking thread failed")]
    Blocking,
    /// System time computation error.
    #[error("system time error: {0}")]
    Time(#[from] std::time::SystemTimeError),
}

/// Trait for JWT token revocation (by `jti`).
///
/// Implementations: [`InMemoryRevocationStore`] (dev) and [`SqliteRevocationStore`] (production).
/// Always used behind an `Arc<dyn RevocationStore>` in `AppState`.
///
/// # Multi-tenant isolation (P0 #2)
///
/// All methods accept `tenant_id` — a revocation is scoped to the tenant. A token
/// of tenant A cannot be revoked or queried by tenant B.
#[async_trait]
pub trait RevocationStore: Send + Sync + 'static {
    /// Returns `true` if the `jti` is revoked (for this `tenant_id`) and not yet expired.
    async fn is_revoked(&self, jti: &str, tenant_id: &str) -> Result<bool, RevocationError>;

    /// Revokes the `jti` for `tenant_id` until `exp`.
    ///
    /// Idempotent: a second call for the same `(jti, tenant_id)` updates `exp`.
    async fn revoke(
        &self,
        jti: &str,
        tenant_id: &str,
        exp: SystemTime,
    ) -> Result<(), RevocationError>;

    /// Removes expired entries for `tenant_id`. Returns the number of deleted rows.
    async fn gc(&self, tenant_id: &str) -> Result<usize, RevocationError>;
}

// ─── InMemoryRevocationStore ─────────────────────────────────────────────────

/// In-memory revocation store — DEV ONLY.
///
/// Emits a tracing WARN at boot. No persistence: a restart clears all revocations.
///
/// # Reaching this store requires an explicit opt-in
///
/// [`boot_guard_check`] rejects only the **exact** string `"memory"` on a non-loopback
/// bind, and takes a `&str` — it cannot, on its own, discriminate a typo. What closes
/// that gap is upstream: `gradatum-server` types its `revocation_store` configuration
/// field as an enum (`RevocationStoreKind`), so a third value — `"Memory"`, `"mem"` — is
/// a deserialization error and the server never starts. No typo can therefore land here
/// silently; only an explicit `revocation_store = "memory"` does.
///
/// Callers outside `gradatum-server` that pass an arbitrary `&str` to
/// [`boot_guard_check`] do not get that protection and must validate their own values.
pub struct InMemoryRevocationStore {
    /// Key: `(tenant_id, jti)` — scoped per tenant (P0 #2).
    inner: DashMap<(String, String), SystemTime>,
}

impl InMemoryRevocationStore {
    /// Creates a new in-memory store. Emits a tracing WARN.
    pub fn new() -> Self {
        tracing::warn!(
            "InMemoryRevocationStore enabled — DEV ONLY. \
             A restart clears all revocations. \
             Use SqliteRevocationStore in production."
        );
        Self {
            inner: DashMap::new(),
        }
    }
}

impl Default for InMemoryRevocationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RevocationStore for InMemoryRevocationStore {
    async fn is_revoked(&self, jti: &str, tenant_id: &str) -> Result<bool, RevocationError> {
        let key = (tenant_id.to_string(), jti.to_string());
        match self.inner.get(&key) {
            Some(exp) => Ok(*exp > SystemTime::now()),
            None => Ok(false),
        }
    }

    async fn revoke(
        &self,
        jti: &str,
        tenant_id: &str,
        exp: SystemTime,
    ) -> Result<(), RevocationError> {
        let key = (tenant_id.to_string(), jti.to_string());
        self.inner.insert(key, exp);
        Ok(())
    }

    async fn gc(&self, tenant_id: &str) -> Result<usize, RevocationError> {
        let now = SystemTime::now();
        let tid = tenant_id.to_string();
        // Collecter d'abord pour éviter de tenir une référence DashMap pendant remove.
        let to_remove: Vec<String> = self
            .inner
            .iter()
            .filter(|kv| kv.key().0 == tid && *kv.value() <= now)
            .map(|kv| kv.key().1.clone())
            .collect();
        let count = to_remove.len();
        for jti in &to_remove {
            self.inner.remove(&(tid.clone(), jti.clone()));
        }
        Ok(count)
    }
}

// ─── SqliteRevocationStore ────────────────────────────────────────────────────

/// SQLite-backed revocation store — production.
///
/// Schema: `revoked(jti TEXT, tenant_id TEXT NOT NULL DEFAULT 'main', exp INTEGER NOT NULL,
/// revoked_at INTEGER NOT NULL, PRIMARY KEY (tenant_id, jti))`.
/// WAL enabled. Connection pool capped at 4 (low concurrent read volume expected).
///
/// # Multi-tenant isolation (P0 #2)
///
/// The primary key is `(tenant_id, jti)` — a JTI of tenant A is independent from
/// a JTI of tenant B. All queries filter by `tenant_id`.
pub struct SqliteRevocationStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteRevocationStore {
    /// Opens or creates the SQLite database at `db_path`.
    ///
    /// Creates the `revoked` table (with `tenant_id`) if it does not exist.
    /// For existing installations, migrates by adding the column if absent.
    /// Enables WAL mode.
    pub async fn new(db_path: &Path) -> Result<Self, RevocationError> {
        let path = db_path.to_path_buf();
        // Connexion rusqlite dédiée — motif de pont synchrone/asynchrone repris des
        // magasins du serveur (proactive_recall_store / note_usage_store / read_usage_store) :
        // ouverture sur fil bloquant, connexion unique sous verrou `tokio::sync::Mutex`.
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            // WAL + busy_timeout 5 s, alignés sur les réglages sqlx d'origine
            // (journal_mode=WAL explicite, busy_timeout=5000 ms). `synchronous`
            // reste au défaut SQLite (FULL) comme avec sqlx — la durabilité des
            // révocations est préservée.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "busy_timeout", 5000i32)?;

            // Schéma idempotent avec tenant_id (P0 #2).
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS revoked (
                    jti        TEXT    NOT NULL,
                    tenant_id  TEXT    NOT NULL DEFAULT 'main',
                    exp        INTEGER NOT NULL,
                    revoked_at INTEGER NOT NULL,
                    PRIMARY KEY (tenant_id, jti)
                )
                "#,
            )?;

            // Migration pour les installations existantes : ajoute la colonne
            // `tenant_id` si elle n'existe pas encore. SQLite ne supporte pas
            // `ADD COLUMN IF NOT EXISTS` — on utilise `pragma_table_info`.
            let has_tenant_id: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('revoked') WHERE name = 'tenant_id'",
                [],
                |row| row.get(0),
            )?;

            if !has_tenant_id {
                // Migration additive : ajoute tenant_id avec DEFAULT 'main'.
                // L'ancienne PK (jti seul) est conservée (SQLite ne permet pas de
                // modifier une PK via ALTER TABLE). Le filtrage par tenant_id dans
                // les requêtes assure l'isolation.
                conn.execute(
                    "ALTER TABLE revoked ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'main'",
                    [],
                )?;

                tracing::info!(
                    "revocation store: added tenant_id column (migration for multi-tenant isolation P0 #2)"
                );
            }

            Ok::<Connection, rusqlite::Error>(conn)
        })
        .await
        .map_err(|_| RevocationError::Blocking)??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl RevocationStore for SqliteRevocationStore {
    async fn is_revoked(&self, jti: &str, tenant_id: &str) -> Result<bool, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let jti = jti.to_owned();
        let tenant_id = tenant_id.to_owned();
        let conn = Arc::clone(&self.conn);

        let exp = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let row = conn
                .query_row(
                    "SELECT exp FROM revoked WHERE jti = ?1 AND tenant_id = ?2",
                    params![jti, tenant_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok::<Option<i64>, rusqlite::Error>(row)
        })
        .await
        .map_err(|_| RevocationError::Blocking)??;

        Ok(exp.is_some_and(|exp| exp > now_secs))
    }

    async fn revoke(
        &self,
        jti: &str,
        tenant_id: &str,
        exp: SystemTime,
    ) -> Result<(), RevocationError> {
        let exp_secs = exp.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let jti = jti.to_owned();
        let tenant_id = tenant_id.to_owned();
        let conn = Arc::clone(&self.conn);

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR REPLACE INTO revoked (jti, tenant_id, exp, revoked_at) VALUES (?1, ?2, ?3, ?4)",
                params![jti, tenant_id, exp_secs, now_secs],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .map_err(|_| RevocationError::Blocking)??;
        Ok(())
    }

    async fn gc(&self, tenant_id: &str) -> Result<usize, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let tenant_id = tenant_id.to_owned();
        let conn = Arc::clone(&self.conn);

        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let rows = conn.execute(
                "DELETE FROM revoked WHERE exp <= ?1 AND tenant_id = ?2",
                params![now_secs, tenant_id],
            )?;
            Ok::<usize, rusqlite::Error>(rows)
        })
        .await
        .map_err(|_| RevocationError::Blocking)??;
        Ok(deleted)
    }
}

// ─── Boot guard ──────────────────────────────────────────────────────────────

/// Validates the bind address / revocation store combination at startup.
///
/// Rejects: non-loopback bind address combined with `revocation_store == "memory"`.
/// Rationale: an in-memory store loses all revocations on restart and must not be
/// exposed on a public or LAN network interface.
///
/// # Arguments
///
/// - `bind_is_loopback`: `true` if `ServerConfig.server.bind.ip().is_loopback()`
/// - `revocation_store`: value of the `ServerConfig.auth.revocation_store` field
///
/// # Errors
///
/// Returns `Err(&'static str)` with an explanatory message — the caller should `eprintln!` and exit(1).
pub fn boot_guard_check(
    bind_is_loopback: bool,
    revocation_store: &str,
) -> Result<(), &'static str> {
    if !bind_is_loopback && revocation_store == "memory" {
        Err(
            "revocation_store=memory is forbidden when bind is non-loopback (caveat C2). \
             Use revocation_store=sqlite in production.",
        )
    } else {
        Ok(())
    }
}
