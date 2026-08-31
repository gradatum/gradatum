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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gradatum_db_sqlite::QueueDb;
use rusqlite::params;

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
    db: QueueDb,
    /// Unique identifier for this worker (ULID generated at creation).
    holder: String,
    cfg: LeaderConfig,
}

impl LeaderElection {
    /// Creates a new election handle.
    ///
    /// The `db` must already have the `worker_leadership` schema applied
    /// (via [`gradatum_queue::schema::SCHEMA_V1`]).
    pub async fn new(db: QueueDb, cfg: LeaderConfig) -> anyhow::Result<Self> {
        let holder = ulid::Ulid::generate().to_string();
        Ok(Self { db, holder, cfg })
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
            .expect("system clock valid (post-epoch)")
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
        let holder = self.holder.clone();
        let db = self.db.clone();

        // Les deux tentatives s'exécutent sur un fil bloquant sous le verrou unique
        // (motif de pont F-145) — pas d'attente async sous verrou.
        let acquired = db
            .with_conn(move |conn| {
                // Attempt 1: INSERT OR IGNORE (slot absent → this worker wins).
                let inserted = conn.execute(
                    "INSERT OR IGNORE INTO worker_leadership (slot, holder, expires_at) VALUES (0, ?1, ?2)",
                    params![holder, expires],
                )?;
                if inserted == 1 {
                    return Ok(true);
                }

                // Attempt 2: CAS on the existing slot (leader expired OR already held by us).
                let updated = conn.execute(
                    "UPDATE worker_leadership
                     SET holder = ?1, expires_at = ?2
                     WHERE slot = 0 AND (expires_at < ?3 OR holder = ?4)",
                    params![holder, expires, now, holder],
                )?;
                Ok(updated == 1)
            })
            .await
            .map_err(|e| anyhow::anyhow!("leader election try_acquire failed: {e}"))?;

        Ok(acquired)
    }

    /// Renews the current leader's lease.
    ///
    /// Returns `true` if renewal succeeded (this worker is still leader),
    /// `false` if the slot was claimed by another worker in the meantime.
    pub async fn renew(&self) -> anyhow::Result<bool> {
        let now = Self::now_ms();
        let expires = now + self.cfg.expires_after.as_millis() as i64;
        let holder = self.holder.clone();
        let db = self.db.clone();

        let updated = db
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE worker_leadership SET expires_at = ?1 WHERE slot = 0 AND holder = ?2",
                    params![expires, holder],
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("leader election renew failed: {e}"))?;

        Ok(updated == 1)
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
    /// - If the DB is unreachable (database locked), the error propagates —
    ///   the caller decides whether it is critical (`.ok()` for best-effort).
    pub async fn release(&self) -> anyhow::Result<()> {
        let holder = self.holder.clone();
        let db = self.db.clone();

        db.with_conn(move |conn| {
            conn.execute("DELETE FROM worker_leadership WHERE holder = ?1", [holder])?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("leader election release failed: {e}"))?;
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
                        tracing::debug!(holder = %self.holder, "leadership renewed");
                    }
                    Ok(false) => {
                        tracing::warn!(
                            holder = %self.holder,
                            "leadership lost — stopping renewal loop"
                        );
                        break;
                    }
                    Err(e) => {
                        tracing::error!(
                            holder = %self.holder,
                            error = %e,
                            "error during leadership renewal"
                        );
                        break;
                    }
                }
            }
        })
    }
}
