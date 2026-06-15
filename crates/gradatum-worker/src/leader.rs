//! Leader election via SQLite CAS on the `worker_leadership` table.
//!
//! Mechanism: single slot (slot=0). Each worker attempts:
//! 1. `INSERT OR IGNORE` to create the slot if it does not yet exist.
//! 2. `UPDATE WHERE expires_at < now OR holder = self` to claim the slot
//!    if the current leader has expired or if this worker already holds it.
//!
//! `expires_at` timestamps are stored in **Unix milliseconds** to allow
//! tests with sub-second durations (< 1 second).
//!
//! The renewal loop runs in a separate Tokio task and keeps the lease alive.
//! If renewal fails (DB unreachable, lost lock), the task terminates
//! silently — the main worker must monitor the `JoinHandle`.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;

/// Configuration for leader election.
#[derive(Debug, Clone)]
pub struct LeaderConfig {
    /// Lease renewal frequency.
    pub renew_every: Duration,
    /// Lease validity duration (must be > `renew_every` to avoid starvation).
    pub expires_after: Duration,
}

impl Default for LeaderConfig {
    fn default() -> Self {
        Self {
            renew_every: Duration::from_secs(30),
            expires_after: Duration::from_secs(60),
        }
    }
}

/// Leader election handle for a worker.
///
/// Cloneable: the same handle can be passed to the renewal loop and to
/// dispatch code without duplicating the DB connection.
#[derive(Clone)]
pub struct LeaderElection {
    pool: Arc<SqlitePool>,
    /// Unique identifier for this worker (ULID generated at creation).
    holder: String,
    cfg: LeaderConfig,
}

impl LeaderElection {
    /// Creates a new election handle.
    ///
    /// The `pool` must already have the `worker_leadership` schema applied
    /// (via [`gradatum_queue::schema::SCHEMA_V1`]).
    pub async fn new(pool: Arc<SqlitePool>, cfg: LeaderConfig) -> anyhow::Result<Self> {
        let holder = ulid::Ulid::new().to_string();
        Ok(Self { pool, holder, cfg })
    }

    /// Returns the current timestamp in Unix milliseconds.
    ///
    /// Uses milliseconds to allow tests with sub-second durations
    /// (e.g. `expires_after = Duration::from_millis(300)`).
    ///
    /// # Panics
    ///
    /// Unreachable in practice: panics only if the system clock is
    /// earlier than the Unix epoch, which cannot occur on a correctly
    /// configured system.
    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("horloge système valide (post-epoch)")
            .as_millis() as i64
    }

    /// Attempts to acquire (or re-confirm) leadership.
    ///
    /// Returns `true` if this worker is now the leader, `false` otherwise.
    ///
    /// # Algorithm
    ///
    /// 1. `INSERT OR IGNORE`: if the slot does not exist, this worker becomes leader.
    /// 2. `UPDATE WHERE expires_at < now OR holder = self`: if the current leader
    ///    has expired OR this worker already holds the slot, the slot is claimed/renewed.
    pub async fn try_acquire(&self) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let expires = now + self.cfg.expires_after.as_millis() as i64;

        // Attempt 1: INSERT OR IGNORE (slot absent → this worker wins).
        let r = sqlx::query(
            "INSERT OR IGNORE INTO worker_leadership (slot, holder, expires_at) VALUES (0, ?, ?)",
        )
        .bind(&self.holder)
        .bind(expires)
        .execute(self.pool.as_ref())
        .await?;

        if r.rows_affected() == 1 {
            return Ok(true);
        }

        // Attempt 2: CAS on the existing slot (leader expired OR already held by us).
        let r = sqlx::query(
            "UPDATE worker_leadership
             SET holder = ?, expires_at = ?
             WHERE slot = 0 AND (expires_at < ? OR holder = ?)",
        )
        .bind(&self.holder)
        .bind(expires)
        .bind(now)
        .bind(&self.holder)
        .execute(self.pool.as_ref())
        .await?;

        Ok(r.rows_affected() == 1)
    }

    /// Renews the current leader's lease.
    ///
    /// Returns `true` if renewal succeeded (this worker is still leader),
    /// `false` if the slot was claimed by another worker in the meantime.
    pub async fn renew(&self) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let expires = now + self.cfg.expires_after.as_millis() as i64;
        let r = sqlx::query(
            "UPDATE worker_leadership SET expires_at = ? WHERE slot = 0 AND holder = ?",
        )
        .bind(expires)
        .bind(&self.holder)
        .execute(self.pool.as_ref())
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Releases this worker's leadership lease.
    ///
    /// Deletes the `worker_leadership` row only when `holder = self.holder`
    /// to avoid releasing a lease claimed by another worker in the meantime
    /// (race-safe).
    ///
    /// ## Side effects
    ///
    /// - If the row does not exist (already expired or claimed), this is a no-op.
    /// - If the DB is unreachable (pool closed, locked), the error propagates —
    ///   the caller decides whether it is critical (`.ok()` for best-effort).
    pub async fn release(&self) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM worker_leadership WHERE holder = ?")
            .bind(&self.holder)
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// Spawns the renewal loop as a background task.
    ///
    /// The task terminates automatically if:
    /// - Renewal returns `false` (leadership lost).
    /// - A DB error occurs.
    ///
    /// The returned `JoinHandle` allows `abort()`ing the task during shutdown.
    pub fn spawn_renewal(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.cfg.renew_every);
            // Consume the immediate first tick.
            interval.tick().await;
            loop {
                interval.tick().await;
                match self.renew().await {
                    Ok(true) => {
                        tracing::debug!(holder = %self.holder, "leadership renouvelé");
                    }
                    Ok(false) => {
                        tracing::warn!(
                            holder = %self.holder,
                            "leadership perdu — arrêt de la boucle de renouvellement"
                        );
                        break;
                    }
                    Err(e) => {
                        tracing::error!(
                            holder = %self.holder,
                            error = %e,
                            "erreur lors du renouvellement du leadership"
                        );
                        break;
                    }
                }
            }
        })
    }
}
