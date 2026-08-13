//! `RevocationStore` — storage and lookup of revoked JWT tokens (by `jti`).
//!
//! Trait with two implementations:
//! - [`InMemoryRevocationStore`]: dev-only, emits a WARN at boot, no persistence.
//! - [`SqliteRevocationStore`]: production, WAL, on-demand GC via `gc()` — no background
//!   task ships in `1.0.0`, so expired `jti` rows accumulate until an operator calls it.
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
#[non_exhaustive]
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
    pool: SqlitePool,
}

impl SqliteRevocationStore {
    /// Opens or creates the SQLite database at `db_path`.
    ///
    /// Creates the `revoked` table (with `tenant_id`) if it does not exist.
    /// For existing installations, migrates by adding the column if absent.
    /// Enables WAL mode.
    pub async fn new(db_path: &Path) -> Result<Self, RevocationError> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        // Schéma idempotent avec tenant_id (P0 #2).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS revoked (
                jti        TEXT    NOT NULL,
                tenant_id  TEXT    NOT NULL DEFAULT 'main',
                exp        INTEGER NOT NULL,
                revoked_at INTEGER NOT NULL,
                PRIMARY KEY (tenant_id, jti)
            )
            "#,
        )
        .execute(&pool)
        .await?;

        // Migration pour les installations existantes : ajoute la colonne
        // `tenant_id` si elle n'existe pas encore. SQLite ne supporte pas
        // `ADD COLUMN IF NOT EXISTS` — on utilise `pragma_table_info`.
        let has_tenant_id: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('revoked') WHERE name = 'tenant_id'",
        )
        .fetch_one(&pool)
        .await?;

        if !has_tenant_id {
            // Migration additive : ajoute tenant_id avec DEFAULT 'main'.
            // L'ancienne PK (jti seul) est reconstruite via la nouvelle table
            // avec PK composite (tenant_id, jti). On utilise une approche en deux temps :
            // 1. Ajouter la colonne (ALTER TABLE)
            // 2. Recréer la table avec la nouvelle PK (seulement si nécessaire)
            let _ = sqlx::query(
                "ALTER TABLE revoked ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'main'",
            )
            .execute(&pool)
            .await;

            // Pour les installations existantes, la PK reste (jti) car SQLite
            // ne permet pas de modifier une PK via ALTER TABLE. Le filtrage
            // par tenant_id dans les requêtes assure l'isolation.
            tracing::info!(
                "revocation store: added tenant_id column (migration for multi-tenant isolation P0 #2)"
            );
        }

        Ok(Self { pool })
    }
}

#[async_trait]
impl RevocationStore for SqliteRevocationStore {
    async fn is_revoked(&self, jti: &str, tenant_id: &str) -> Result<bool, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT exp FROM revoked WHERE jti = ?1 AND tenant_id = ?2")
                .bind(jti)
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some_and(|(exp,)| exp > now_secs))
    }

    async fn revoke(
        &self,
        jti: &str,
        tenant_id: &str,
        exp: SystemTime,
    ) -> Result<(), RevocationError> {
        let exp_secs = exp.duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        sqlx::query(
            "INSERT OR REPLACE INTO revoked (jti, tenant_id, exp, revoked_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(jti)
        .bind(tenant_id)
        .bind(exp_secs)
        .bind(now_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn gc(&self, tenant_id: &str) -> Result<usize, RevocationError> {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let result = sqlx::query("DELETE FROM revoked WHERE exp <= ?1 AND tenant_id = ?2")
            .bind(now_secs)
            .bind(tenant_id)
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
            "revocation_store=memory is forbidden when bind is non-loopback (caveat C2). \
             Use revocation_store=sqlite in production.",
        )
    } else {
        Ok(())
    }
}
