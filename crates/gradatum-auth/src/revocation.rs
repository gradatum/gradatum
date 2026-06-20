//! `RevocationStore` — storage and lookup of revoked JWT tokens (by `jti`).
//!
//! Trait with two implementations:
//! - [`InMemoryRevocationStore`]: dev-only, emits a WARN at boot, no persistence.
//! - [`SqliteRevocationStore`]: production, WAL, periodic GC.
//!
//! [`boot_guard_check`] rejects startup if the bind address is non-loopback and the store is `"memory"`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dashmap::DashMap;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

/// Error variants for `RevocationStore` operations.
#[derive(Debug, thiserror::Error)]
pub enum RevocationError {
    /// SQLite error (via sqlx).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),
    /// System time computation error.
    #[error("system time error: {0}")]
    Time(#[from] std::time::SystemTimeError),
}

/// Trait for JWT token revocation (by `jti`).
///
/// Implementations: [`InMemoryRevocationStore`] (dev) and [`SqliteRevocationStore`] (production).
/// Always used behind an `Arc<dyn RevocationStore>` in `AppState`.
#[async_trait]
pub trait RevocationStore: Send + Sync + 'static {
    /// Returns `true` if the `jti` is revoked and not yet expired.
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError>;

    /// Revokes the `jti` until `exp`.
    ///
    /// Idempotent: a second call for the same `jti` updates `exp`.
    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError>;

    /// Removes expired entries. Returns the number of deleted rows.
    async fn gc(&self) -> Result<usize, RevocationError>;
}

// ─── InMemoryRevocationStore ─────────────────────────────────────────────────

/// In-memory revocation store — DEV ONLY.
///
/// Emits a tracing WARN at boot. No persistence: a restart clears all revocations.
/// Forbidden when the bind address is non-loopback (see [`boot_guard_check`]).
pub struct InMemoryRevocationStore {
    inner: DashMap<String, SystemTime>,
}

impl InMemoryRevocationStore {
    /// Creates a new in-memory store. Emits a tracing WARN.
    pub fn new() -> Self {
        tracing::warn!(
            "InMemoryRevocationStore activé — DEV ONLY. \
             Un redémarrage efface toutes les révocations. \
             Utiliser SqliteRevocationStore en production."
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
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError> {
        match self.inner.get(jti) {
            Some(exp) => Ok(*exp > SystemTime::now()),
            None => Ok(false),
        }
    }

    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError> {
        self.inner.insert(jti.to_string(), exp);
        Ok(())
    }

    async fn gc(&self) -> Result<usize, RevocationError> {
        let now = SystemTime::now();
        // Collecter d'abord pour éviter de tenir une référence DashMap pendant remove.
        let to_remove: Vec<String> = self
            .inner
            .iter()
            .filter(|kv| *kv.value() <= now)
            .map(|kv| kv.key().clone())
            .collect();
        let count = to_remove.len();
        for k in &to_remove {
            self.inner.remove(k);
        }
        Ok(count)
    }
}

// ─── SqliteRevocationStore ────────────────────────────────────────────────────

/// SQLite-backed revocation store — production.
///
/// Schema: `revoked(jti TEXT PK, exp INTEGER NOT NULL, revoked_at INTEGER NOT NULL)`.
/// WAL enabled. Connection pool capped at 4 (low concurrent read volume expected).
pub struct SqliteRevocationStore {
    pool: SqlitePool,
}

impl SqliteRevocationStore {
    /// Opens or creates the SQLite database at `db_path`.
    ///
    /// Creates the `revoked` table if it does not exist. Enables WAL mode.
    pub async fn new(db_path: &Path) -> Result<Self, RevocationError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        // Schéma idempotent.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS revoked (
                jti        TEXT    PRIMARY KEY,
                exp        INTEGER NOT NULL,
                revoked_at INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl RevocationStore for SqliteRevocationStore {
    async fn is_revoked(&self, jti: &str) -> Result<bool, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let row: Option<(i64,)> = sqlx::query_as("SELECT exp FROM revoked WHERE jti = ?1")
            .bind(jti)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some_and(|(exp,)| exp > now_secs))
    }

    async fn revoke(&self, jti: &str, exp: SystemTime) -> Result<(), RevocationError> {
        let exp_secs = exp.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        sqlx::query("INSERT OR REPLACE INTO revoked (jti, exp, revoked_at) VALUES (?1, ?2, ?3)")
            .bind(jti)
            .bind(exp_secs)
            .bind(now_secs)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn gc(&self) -> Result<usize, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let result = sqlx::query("DELETE FROM revoked WHERE exp <= ?1")
            .bind(now_secs)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() as usize)
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
            "revocation_store=memory est interdit quand bind est non-loopback (caveat C2). \
             Utilisez revocation_store=sqlite en production.",
        )
    } else {
        Ok(())
    }
}
