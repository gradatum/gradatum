//! Periodic cron schedules via `apalis-cron`.
//!
//! ## Schedules
//!
//! | Name | Cron expression | Action |
//! |---|---|---|
//! | `cleanup_dlq_daily` | `0 0 3 * * *` | Deletes DLQ jobs older than `retention_days` |
//!
//! Expressions use the `cron` crate's format: the **seconds field is mandatory** (6 or 7
//! fields). A standard 5-field crontab expression such as `0 3 * * *` is rejected by
//! `Schedule::from_str` at boot, and `build_monitor` then returns `Err` — the worker does
//! not start. Verified against `cron 0.16`.
//!
//! ## Architecture
//!
//! `apalis-cron` provides `CronStream`, which emits a `Tick` at each occurrence
//! of the cron expression. The handler receives the `Tick` and performs the SQL cleanup.

use std::future::Future;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use apalis::prelude::{BoxDynError, Data};
use apalis_cron::Tick;
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use gradatum_core::{
    DistillSource, Job, JobClass, JobFilter, JobLifecycle, JobLineage, JobMode, JobPriority,
    JobRecord, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, QueueError, QueueStore,
    TriggerSource,
};
use gradatum_db_sqlite::idempotency_cleanup;

use crate::internal_client::InternalClient;
use crate::metrics::WorkerMetrics;

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Cron schedule configuration read from TOML.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ScheduleConfig {
    /// Schedule name (e.g. `"cleanup_dlq_daily"`).
    pub name: String,
    /// Cron expression, `cron`-crate format — the seconds field is **mandatory**
    /// (6 or 7 fields), e.g. `"0 0 3 * * *"` for 03:00 daily.
    ///
    /// A 5-field crontab expression (`"0 3 * * *"`) is **rejected at boot**: the worker
    /// fails to start rather than silently mis-scheduling.
    pub cron: String,
    /// Retention in days for DLQ cleanup (default: 30).
    ///
    /// **Must be ≥ 1.** `0` is refused when the TOML is parsed: it would set the cleanup
    /// cutoff to "now" and purge the **entire** DLQ on the next tick, irreversibly. The
    /// 30-day default still applies — and only applies — when the key is absent.
    #[serde(
        default = "ScheduleConfig::default_retention",
        deserialize_with = "deserialize_retention_days"
    )]
    pub retention_days: u32,
}

impl ScheduleConfig {
    fn default_retention() -> u32 {
        30
    }
}

/// Deserializes [`ScheduleConfig::retention_days`], refusing `0`.
///
/// Fail-loud at **parse** time rather than in a `validate()` method: a validation method
/// is only as good as its call sites, and an asymmetry between a guard and the code path
/// that actually consumes the value is the very defect this rejects. Here no
/// [`ScheduleConfig`] carrying `retention_days = 0` can be produced by `serde` at all.
///
/// Note that `serde` does not invoke this function when the key is absent — the
/// `default` attribute supplies 30 — so an existing configuration is unaffected.
///
/// # Errors
///
/// `D::Error` if the value is not a `u32`, or if it is `0`.
fn deserialize_retention_days<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    let days = u32::deserialize(deserializer)?;
    if days == 0 {
        return Err(serde::de::Error::custom(
            "retention_days must be >= 1: 0 sets the DLQ cleanup cutoff to \"now\" and purges \
             the entire dead-letter queue irreversibly",
        ));
    }
    Ok(days)
}

// ─────────────────────────────────────────────────────────────────────────────
// Config — cron distill conditionnel (F-112)
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration of the conditional distill cron, read from the top-level
/// `[distill_cron]` TOML key (extracted by figment separately from `[apalis]`).
///
/// **Off by default**: with `enabled = false` the cron worker is not registered at all
/// (see `build_monitor`) AND the handler is a no-op — two independent defences. Turning it
/// on at runtime is a separate operator decision.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DistillCronConfig {
    /// Enables the cron. Defaults to `false`: no job emitted, no count call issued.
    #[serde(default)]
    pub enabled: bool,
    /// Cron expression in the `cron` crate's format: the seconds field is MANDATORY
    /// (6 or 7 fields), days of week are `1` = Sunday … `7` = Saturday, or the `Sun`-`Sat`
    /// names. Same format as the existing `cleanup_dlq_daily` cron. The standard 5-field
    /// crontab format is REJECTED at boot (fail-loud): its day-of-week numbering
    /// (`0` = Sunday) differs from the crate's, and translating it silently would shift
    /// the execution day.
    ///
    /// Defaults to `"0 0 4 * * Sun"` — weekly, Sunday at 04:00.
    #[serde(default = "DistillCronConfig::default_cron")]
    pub cron: String,
    /// Consolidatable sections, in consolidation priority order.
    #[serde(default = "DistillCronConfig::default_loci")]
    pub loci: Vec<String>,
    /// Minimum pressure (live, non-`processed` notes per locus) that triggers a
    /// `Job::Distill`. Must be 1 or more.
    #[serde(default = "DistillCronConfig::default_pressure_min")]
    pub pressure_min: u64,
    /// Maximum number of jobs enqueued per tick (hard cap). Must be 1 or more.
    #[serde(default = "DistillCronConfig::default_max_jobs_per_tick")]
    pub max_jobs_per_tick: usize,
}

impl DistillCronConfig {
    fn default_cron() -> String {
        "0 0 4 * * Sun".to_string()
    }

    fn default_loci() -> Vec<String> {
        vec![
            "debug".to_string(),
            "experiments".to_string(),
            "reference".to_string(),
        ]
    }

    fn default_pressure_min() -> u64 {
        20
    }

    fn default_max_jobs_per_tick() -> usize {
        2
    }

    /// Parses the cron expression into a [`cron::Schedule`] (the crate's native format).
    ///
    /// Used by [`Self::validate`] AND by the `build_monitor` registration, so that
    /// validation and registration can never diverge on the parse path.
    ///
    /// # Errors
    ///
    /// The `cron` parser's error message when the expression is invalid — including the
    /// rejected 5-field crontab format, see the `cron` field documentation.
    pub fn schedule(&self) -> Result<cron::Schedule, String> {
        cron::Schedule::from_str(&self.cron)
            .map_err(|e| format!("cron non parseable {:?} : {e}", self.cron))
    }

    /// Fail-loud configuration validation, called from `build_monitor` and propagated
    /// with `?` — an invalid TOML prevents the worker from booting.
    ///
    /// # Errors
    ///
    /// A message describing the first invalid field: unparseable cron,
    /// `pressure_min = 0`, `max_jobs_per_tick = 0`, or a `loci` list that is empty or
    /// contains a duplicate.
    pub fn validate(&self) -> Result<(), String> {
        self.schedule()?;
        if self.pressure_min == 0 {
            return Err("pressure_min must be ≥ 1".to_string());
        }
        if self.max_jobs_per_tick == 0 {
            return Err("max_jobs_per_tick must be ≥ 1".to_string());
        }
        if self.loci.is_empty() {
            return Err("loci must not be empty".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for locus in &self.loci {
            if !seen.insert(locus.as_str()) {
                return Err(format!("loci contient un doublon : {locus:?}"));
            }
        }
        Ok(())
    }
}

impl Default for DistillCronConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: Self::default_cron(),
            loci: Self::default_loci(),
            pressure_min: Self::default_pressure_min(),
            max_jobs_per_tick: Self::default_max_jobs_per_tick(),
        }
    }
}

/// Pure tick planning for the conditional distill cron.
///
/// `pressures` = measured pressure per locus in CONFIG order (`None` = count failed,
/// skipped best-effort). `busy_loci` = loci with a Distill job already queued/running.
/// Returns the loci to enqueue, config order, capped at `max_jobs_per_tick`.
#[must_use]
pub fn plan_distill_tick(
    pressures: &[(String, Option<u64>)],
    busy_loci: &[String],
    cfg: &DistillCronConfig,
) -> Vec<String> {
    pressures
        .iter()
        .filter(|(locus, _)| !busy_loci.contains(locus))
        .filter_map(|(locus, p)| p.filter(|&n| n >= cfg.pressure_min).map(|_| locus.clone()))
        .take(cfg.max_jobs_per_tick)
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared context for cron handlers
// ─────────────────────────────────────────────────────────────────────────────

/// Context injected into cron handlers.
///
/// Intended for manual injection outside the Monitor. Currently unused
/// — the Apalis Monitor injects pool and retention via `WorkerBuilder::data()`.
#[derive(Clone)]
#[allow(dead_code)]
pub struct CronHandlerCtx {
    /// SQLite pool for cleanup operations.
    pub pool: Arc<SqlitePool>,
    /// DLQ retention in days.
    pub dlq_retention_days: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — cleanup_dlq_daily
// ─────────────────────────────────────────────────────────────────────────────

/// Cron handler for daily DLQ cleanup.
///
/// Deletes jobs with `status = 'DLQ'` older than `retention_days` days (default: 30).
///
/// # Behaviour
///
/// - `completed_at < now - retention_days` → DELETE
/// - If `completed_at IS NULL` → uses `created_at` as fallback
/// - Returns `Ok(())` even if 0 rows were deleted
///
/// # Safety
///
/// Irreversible destructive operation — restricted to `status='DLQ'` only.
///
/// **Minimum retention floor: 1 day, enforced at parse time.** `retention_days = 0` is
/// refused when the TOML is deserialized (see [`ScheduleConfig::retention_days`]) — it
/// would set the cutoff to "now" and purge the **entire** DLQ on the next tick,
/// irreversibly and with no confirmation. 30 days remains a serde *default*, applied only
/// when the key is absent; a present value is otherwise used as-is, unclamped, all the way
/// to `Duration::days`. Treat this value as a destructive configuration knob.
pub async fn handle_cleanup_dlq(
    _tick: Tick<Utc>,
    pool: Data<Arc<SqlitePool>>,
    retention: Data<u32>,
) -> Result<(), BoxDynError> {
    let cutoff = Utc::now() - chrono::Duration::days(*retention as i64);
    let cutoff_str = cutoff.to_rfc3339();

    let result = sqlx::query(
        r#"
        DELETE FROM gradatum_jobs
        WHERE status = 'DLQ'
          AND COALESCE(completed_at, created_at) < ?
        "#,
    )
    .bind(&cutoff_str)
    .execute(pool.as_ref())
    .await;

    match result {
        Ok(res) => {
            let deleted = res.rows_affected();
            if deleted > 0 {
                info!(
                    deleted = deleted,
                    retention_days = *retention,
                    "cleanup_dlq_daily: {} DLQ jobs purged",
                    deleted
                );
            }
            Ok(())
        }
        Err(e) => {
            error!(error = %e, "cleanup_dlq_daily: SQL error");
            Err(BoxDynError::from(format!(
                "cleanup_dlq_daily SQL error: {e}"
            )))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler — cron distill conditionnel (F-112)
// ─────────────────────────────────────────────────────────────────────────────

/// Builds a [`JobRecord`] for a conditional distill cron enqueue.
///
/// - `Job::Distill(DistillSource { mode: Semantic, scope: Locus(locus), .. })` —
///   batch defaults (`batch_limit = 500`, `confidence_threshold = 0.75`) reused as-is.
/// - `JobClass::System` + `JobPriority::Low` — autonomous background cron that must
///   never block agent jobs.
/// - `JobSpec.scope = Locus(locus)` too: the `Locus`-only rule (never `VaultWide`) holds
///   on BOTH the internal `DistillSource.scope` and the outer `JobSpec.scope`.
/// - `JobMode::Batch`, `await_jobs` empty (no cascade dependency).
///
/// When `multi_tenant_enabled` is `true`, the OUTER `JobSpec.scope` instead carries
/// `Vault(tenant_id)`, since the distill handler resolves its vault from that scope; the
/// locus stays on `DistillSource.scope`, which is what both the handler and the
/// `read_busy_distill_loci` dedup read. When the flag is off, the outer scope remains
/// `Locus(locus)`, byte-for-byte identical to the single-vault behaviour.
#[must_use]
pub fn build_distill_job_record(
    locus: &str,
    tenant_id: &str,
    multi_tenant_enabled: bool,
) -> JobRecord {
    let now = Utc::now();
    let outer_scope = if multi_tenant_enabled {
        JobScope::Vault(tenant_id.to_string())
    } else {
        JobScope::Locus(locus.to_string())
    };
    JobRecord {
        id: ulid::Ulid::new(),
        spec: JobSpec {
            kind: Job::Distill(DistillSource {
                scope: JobScope::Locus(locus.to_string()),
                ..DistillSource::default()
            }),
            class: JobClass::System,
            mode: JobMode::Batch,
            scope: outer_scope,
            priority: JobPriority::Low,
        },
        scheduling: JobScheduling {
            trigger: TriggerSource::Demand,
            scheduled_at: now,
            // Empty — the cascade engine is not wired; a non-empty await_jobs would
            // strand the job in Waiting (mirrors build_embed/build_validate_job_record).
            await_jobs: vec![],
            deadline: None,
            cron_expr: None,
        },
        lifecycle: JobLifecycle {
            status: JobStatus::Pending,
            created_at: now,
            started_at: None,
            completed_at: None,
            lease_until: None,
            result: None,
        },
        retry: JobRetry::default(),
        lineage: JobLineage {
            triggered_by: None,
            parent_job: None,
            pipeline_id: None,
            pipeline_step: None,
            children: vec![],
            cost_usd: None,
        },
    }
}

/// Lists loci that already have an in-flight (`Pending`/`Running`/`Waiting`) `Distill`
/// job, for the distill cron dedup.
///
/// The locus is read from the INTERNAL `DistillSource.scope` (`Job::Distill(src) →
/// src.scope`) — the field the distill handler actually consumes — never
/// `JobSpec.scope`. Non-`Locus` scopes (e.g. `Notes`, `VaultWide`) carry no single
/// locus and are ignored for dedup.
///
/// # Errors
///
/// Propagates the first [`QueueStore::list`] failure so the caller can fail-closed
/// (abort the whole tick rather than risk duplicate enqueues).
async fn read_busy_distill_loci(store: &dyn QueueStore) -> Result<Vec<String>, QueueError> {
    let mut busy = Vec::new();
    // `JobFilter.status` is a single Option → one query per in-flight status.
    for status in [JobStatus::Pending, JobStatus::Running, JobStatus::Waiting] {
        let filter = JobFilter {
            kind: Some("Distill".to_string()),
            status: Some(status),
            limit: 500,
            ..Default::default()
        };
        for record in store.list(filter).await? {
            if let Job::Distill(src) = &record.spec.kind
                && let JobScope::Locus(locus) = &src.scope
            {
                busy.push(locus.clone());
            }
        }
    }
    Ok(busy)
}

/// Core logic of the distill cron tick, decoupled from apalis `Data` injection
/// for testability (I/O provided as closures).
///
/// Flow: `enabled=false` → no-op · read busy loci (**fail-CLOSED**: read error aborts
/// the whole tick) · measure pressure per locus (best-effort: a `None` count skips the
/// locus) · [`plan_distill_tick`] decides · enqueue each retained locus · `on_enqueued`
/// per successful enqueue.
///
/// The `enabled=false` no-op is defence in depth — the worker is not even registered
/// when disabled (see `build_monitor`).
async fn distill_cron_tick<FB, RB, FC, RC, FE, RE, FM>(
    cfg: &DistillCronConfig,
    read_busy: FB,
    count_locus: FC,
    enqueue_locus: FE,
    on_enqueued: FM,
) where
    FB: FnOnce() -> RB,
    RB: Future<Output = Result<Vec<String>, ()>>,
    FC: Fn(String) -> RC,
    RC: Future<Output = Option<u64>>,
    FE: Fn(String) -> RE,
    RE: Future<Output = Result<(), ()>>,
    FM: Fn(),
{
    if !cfg.enabled {
        tracing::debug!("distill_pressure cron: disabled (enabled=false) — no-op");
        return;
    }

    // 1. Dédup fail-CLOSED (P1-2) : échec de lecture des loci occupés ⇒ tick abandonné.
    let busy_loci = match read_busy().await {
        Ok(loci) => loci,
        Err(()) => {
            warn!("distill_pressure cron: busy-loci read failed — tick aborted (fail-closed)");
            return;
        }
    };

    // 2. Pression par locus (best-effort : comptage en échec ⇒ None ⇒ locus ignoré).
    let mut pressures: Vec<(String, Option<u64>)> = Vec::with_capacity(cfg.loci.len());
    for locus in &cfg.loci {
        let pressure = count_locus(locus.clone()).await;
        pressures.push((locus.clone(), pressure));
    }

    // 3. Décision pure (seuil, cap, dédup) puis enqueue.
    for locus in plan_distill_tick(&pressures, &busy_loci, cfg) {
        if enqueue_locus(locus).await.is_ok() {
            on_enqueued();
        }
    }
}

/// Cron handler for the conditional distill pressure sweep.
///
/// Wires `distill_cron_tick` to the injected [`InternalClient`] (per-locus count),
/// [`QueueStore`] (busy-loci read + enqueue) and [`WorkerMetrics`] (enqueue counter).
///
/// Vault iteration follows the multi-vault flag: when it is off, the single vault
/// `"main"` is swept with no network call; when it is on, the handler iterates over the
/// vaults returned by [`InternalClient::list_active_vaults`] and aborts the whole tick if
/// that listing fails — it never falls back to an implicit cross-vault scan.
///
/// Always returns `Ok(())` — the cron is best-effort and never fails the worker.
pub async fn handle_distill_cron(
    _tick: Tick<Utc>,
    cfg: Data<DistillCronConfig>,
    client: Data<Arc<dyn InternalClient>>,
    queue: Data<Arc<dyn QueueStore + Send + Sync>>,
    metrics: Data<WorkerMetrics>,
    mt: Data<crate::apalis_handlers::MultiTenantCfg>,
) -> Result<(), BoxDynError> {
    let cfg_ref: &DistillCronConfig = &cfg;
    let client_ref: &dyn InternalClient = &**client;
    let queue_ref: &dyn QueueStore = &**queue;
    let metrics_ref: &WorkerMetrics = &metrics;

    // C2 (EX-C2-3, INV-JOB-SCOPE) : à OFF, un seul vault "main" SANS appel réseau
    // (byte-identical) ; à ON, itération explicite PAR vault actif — échec de listing
    // ⇒ tick abandonné (fail-closed, jamais de scan cross-vault implicite).
    let multi_enabled = mt.enabled;
    let vaults: Vec<String> = if multi_enabled {
        match client_ref.list_active_vaults().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e,
                    "distill_pressure cron: list_active_vaults failed — tick aborted (fail-closed)");
                return Ok(());
            }
        }
    } else {
        vec!["main".to_string()]
    };

    for vault in &vaults {
        let vault_id = vault.as_str();
        distill_cron_tick(
            cfg_ref,
            || async {
                read_busy_distill_loci(queue_ref).await.map_err(|e| {
                    warn!(error = %e, "distill_pressure cron: busy-loci read failed");
                })
            },
            |locus| async move {
                match client_ref
                    .count_unprocessed(vault_id, &locus, cfg_ref.pressure_min)
                    .await
                {
                    Ok(n) => Some(n),
                    Err(e) => {
                        warn!(locus = %locus, error = %e,
                        "distill_pressure cron: count failed — skipping locus (best-effort)");
                        None
                    }
                }
            },
            |locus| async move {
                match queue_ref
                    .enqueue(build_distill_job_record(&locus, vault_id, multi_enabled))
                    .await
                {
                    Ok(job_id) => {
                        info!(locus = %locus, vault_id = %vault_id, job_id = %job_id,
                        "distill_pressure cron: Distill job enqueued");
                        Ok(())
                    }
                    Err(e) => {
                        warn!(locus = %locus, error = %e,
                        "distill_pressure cron: enqueue failed (best-effort)");
                        Err(())
                    }
                }
            },
            || metrics_ref.inc_distill_cron_enqueued(),
        )
        .await;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Periodic sweep (recover_stale_leases, cancel_expired_deadlines, promote_retries)
// ─────────────────────────────────────────────────────────────────────────────

/// Runs one periodic sweep of the job store.
///
/// Calls the 5 queue maintenance operations:
/// 1. [`QueueStore::recover_stale_leases`] — returns expired Running jobs to Pending
/// 2. [`QueueStore::cancel_expired_deadlines`] — cancels jobs past their deadline
/// 3. [`QueueStore::promote_retries`] — moves Failed jobs to Pending (or DLQ at max retries)
/// 4. [`QueueStore::promote_stranded_waiting_jobs`] — DAG recovery: promotes stranded
///    `Waiting` jobs whose all dependencies are `Done` but whose post-commit cascade failed
///    (worker crash or storage error). No-op when no stranded jobs exist.
/// 5. [`idempotency_cleanup`] — purges idempotency entries older than 24 hours (TTL)
///
/// The `pool` is required to clean the `gradatum_idempotency` table (migration 008).
/// If `pool` is `None`, operation 5 is skipped with a WARN (the table may grow
/// unboundedly — acceptable only in unit tests).
///
/// Invoked every 30 s by the worker loop via `tokio::spawn`.
/// Does not panic — errors are logged.
pub async fn run_sweep_once(
    store: &(impl QueueStore + ?Sized),
    lease_ttl: Duration,
    pool: Option<&SqlitePool>,
) {
    let now = Utc::now();

    // 1. Recover expired leases
    match store.recover_stale_leases(lease_ttl).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: expired leases recovered");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: recover_stale_leases failed"),
    }

    // 2. Cancel expired deadlines
    match store.cancel_expired_deadlines(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: expired deadlines cancelled");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: cancel_expired_deadlines failed"),
    }

    // 3. Promote retries (Failed → Pending or DLQ at max retries)
    match store.promote_retries(now).await {
        Ok(ids) if !ids.is_empty() => {
            info!(count = ids.len(), "sweep: retries promus en Pending");
        }
        Ok(_) => {}
        Err(e) => warn!(error = %e, "sweep: promote_retries failed"),
    }

    // 4. DAG recovery sweep : promote Waiting jobs whose all deps are Done
    //    but whose post-commit cascade was missed (crash or storage error).
    //    No-op if no stranded jobs exist (common case — await_jobs unused in prod v0.6.x).
    match store.promote_stranded_waiting_jobs().await {
        Ok(promoted) if promoted > 0 => {
            tracing::info!(promoted, "dag_recovery_sweep: jobs Waiting rattrapes");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "dag_recovery_sweep: promote_stranded_waiting_jobs failed")
        }
    }

    // 5. Idempotency cleanup (TTL 24h).
    // Purges `gradatum_idempotency` entries older than now - 24h.
    // Without this cleanup the table grows unboundedly (one row per POST /api/v1/jobs).
    match pool {
        Some(p) => {
            let cutoff_ms = (now - chrono::Duration::hours(24)).timestamp_millis();
            if let Err(e) = idempotency_cleanup(p, cutoff_ms).await {
                warn!(error = %e, "sweep: idempotency_cleanup failed — table may grow");
            }
        }
        None => {
            warn!("sweep: pool unavailable — idempotency_cleanup skipped (table may grow)");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use gradatum_core::{JobFilter, JobRecord, JobResult, QueueError, QueueEvent};
    use std::sync::Mutex;
    use tokio::sync::broadcast::Receiver;
    use ulid::Ulid;

    /// Mock store for testing `sweep_once`.
    struct MockStore {
        stale_calls: Mutex<u32>,
        deadline_calls: Mutex<u32>,
        retry_calls: Mutex<u32>,
    }

    impl MockStore {
        fn new() -> Self {
            Self {
                stale_calls: Mutex::new(0),
                deadline_calls: Mutex::new(0),
                retry_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl QueueStore for MockStore {
        async fn enqueue(&self, _: JobRecord) -> Result<Ulid, QueueError> {
            unimplemented!()
        }
        async fn dequeue(
            &self,
            _tenant_filter: Option<&str>,
        ) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn get(&self, _: Ulid, _: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn complete(&self, _: Ulid, _: JobResult) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail(&self, _: Ulid, _: &str, _: u32) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn cancel(&self, _: Ulid, _: Option<&str>) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn fail_dlq(&self, _: Ulid, _: &str) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn find_awaiting(&self, _: Ulid) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        async fn set_pending(&self, _: Ulid) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn recover_stale_leases(&self, _: Duration) -> Result<Vec<Ulid>, QueueError> {
            *self.stale_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn cancel_expired_deadlines(
            &self,
            _: DateTime<Utc>,
        ) -> Result<Vec<Ulid>, QueueError> {
            *self.deadline_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn promote_retries(&self, _: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
            *self.retry_calls.lock().unwrap() += 1;
            Ok(vec![])
        }
        async fn schedule_retry(&self, _: Ulid, _: DateTime<Utc>) -> Result<(), QueueError> {
            unimplemented!()
        }
        async fn list(&self, _: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
            unimplemented!()
        }
        fn subscribe(&self) -> Receiver<QueueEvent> {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            drop(tx);
            rx
        }
    }

    #[tokio::test]
    async fn sweep_once_calls_all_three_methods() {
        let store = MockStore::new();
        // pool=None : idempotency_cleanup ignoré en test unitaire (table non disponible).
        run_sweep_once(&store, Duration::from_secs(300), None).await;
        assert_eq!(*store.stale_calls.lock().unwrap(), 1);
        assert_eq!(*store.deadline_calls.lock().unwrap(), 1);
        assert_eq!(*store.retry_calls.lock().unwrap(), 1);
    }

    // ── Tests F-112 — DistillCronConfig + plan_distill_tick ──────────────────

    #[test]
    fn distill_cron_config_defaults_off_with_spec_values() {
        let c = DistillCronConfig::default();
        assert!(!c.enabled);
        // Écart plan assumé : format natif crate `cron` (secondes + DOW 1=Sun),
        // même sémantique que le "0 4 * * 0" crontab de la spec (dimanche 04:00).
        // Le format 5 champs ne parse PAS avec cron 0.16 (cf. validation fail-loud).
        assert_eq!(c.cron, "0 0 4 * * Sun");
        assert_eq!(c.loci, vec!["debug", "experiments", "reference"]);
        assert_eq!(c.pressure_min, 20);
        assert_eq!(c.max_jobs_per_tick, 2);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn distill_cron_config_validation_fail_loud() {
        assert!(
            DistillCronConfig {
                cron: "pas-un-cron".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        // Crontab standard 5 champs (spec §2 littérale) : REFUSÉ fail-loud —
        // la numérotation DOW crontab (0=Sun) diverge du crate cron (1=Sun).
        assert!(
            DistillCronConfig {
                cron: "0 4 * * 0".into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DistillCronConfig {
                pressure_min: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DistillCronConfig {
                max_jobs_per_tick: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DistillCronConfig {
                loci: vec![],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            DistillCronConfig {
                loci: vec!["debug".into(), "debug".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn plan_tick_below_threshold_emits_nothing() {
        let cfg = DistillCronConfig::default();
        let out = plan_distill_tick(&[("debug".into(), Some(19))], &[], &cfg);
        assert!(out.is_empty());
    }

    #[test]
    fn plan_tick_at_threshold_emits_locus() {
        let cfg = DistillCronConfig::default();
        let out = plan_distill_tick(&[("debug".into(), Some(20))], &[], &cfg);
        assert_eq!(out, vec!["debug"]);
    }

    #[test]
    fn plan_tick_caps_and_keeps_config_order() {
        let cfg = DistillCronConfig::default(); // cap 2
        let p = vec![
            ("debug".into(), Some(50)),
            ("experiments".into(), Some(50)),
            ("reference".into(), Some(50)),
        ];
        assert_eq!(
            plan_distill_tick(&p, &[], &cfg),
            vec!["debug", "experiments"]
        );
    }

    #[test]
    fn plan_tick_skips_busy_and_failed_counts() {
        let cfg = DistillCronConfig::default();
        let p = vec![
            ("debug".into(), Some(50)),     // busy → skip
            ("experiments".into(), None),   // count KO → skip (best-effort)
            ("reference".into(), Some(50)), // émis
        ];
        assert_eq!(
            plan_distill_tick(&p, &["debug".to_string()], &cfg),
            vec!["reference"]
        );
    }

    // ── Tests F-112 Task 3 — builder + handler (no-op / fail-closed / happy) ──

    #[test]
    fn build_distill_job_record_is_batch_locus_semantic() {
        use gradatum_core::DistillMode;
        let rec = build_distill_job_record("debug", "main", false);
        match &rec.spec.kind {
            Job::Distill(src) => {
                assert_eq!(src.mode, DistillMode::Semantic);
                assert_eq!(src.batch_limit, 500);
                match &src.scope {
                    JobScope::Locus(l) => assert_eq!(l, "debug"),
                    other => panic!("DistillSource.scope doit être Locus, obtenu {other:?}"),
                }
            }
            other => panic!("kind doit être Job::Distill, obtenu {other:?}"),
        }
        assert_eq!(rec.spec.mode, JobMode::Batch);
        assert_eq!(rec.spec.class, JobClass::System);
        assert_eq!(rec.spec.priority, JobPriority::Low);
        match &rec.spec.scope {
            JobScope::Locus(l) => assert_eq!(l, "debug"),
            other => panic!("JobSpec.scope doit être Locus, obtenu {other:?}"),
        }
        assert!(rec.scheduling.await_jobs.is_empty());
    }

    #[tokio::test]
    async fn distill_cron_disabled_is_noop() {
        use std::cell::Cell;
        let cfg = DistillCronConfig {
            enabled: false,
            ..Default::default()
        };
        let busy = Cell::new(false);
        let counted = Cell::new(false);
        let enqueued = Cell::new(false);
        distill_cron_tick(
            &cfg,
            || async {
                busy.set(true);
                Ok::<Vec<String>, ()>(vec![])
            },
            |_locus: String| async {
                counted.set(true);
                Some(0u64)
            },
            |_locus: String| async {
                enqueued.set(true);
                Ok::<(), ()>(())
            },
            || {},
        )
        .await;
        assert!(!busy.get(), "enabled=false ⇒ lecture busy_loci interdite");
        assert!(!counted.get(), "enabled=false ⇒ comptage interdit");
        assert!(!enqueued.get(), "enabled=false ⇒ enqueue interdit");
    }

    #[tokio::test]
    async fn distill_cron_dedup_read_failure_aborts_tick() {
        use std::cell::Cell;
        let cfg = DistillCronConfig {
            enabled: true,
            ..Default::default()
        };
        let counted = Cell::new(false);
        let enqueued = Cell::new(false);
        distill_cron_tick(
            &cfg,
            || async { Err::<Vec<String>, ()>(()) }, // lecture busy_loci en échec
            |_locus: String| async {
                counted.set(true);
                Some(50u64)
            },
            |_locus: String| async {
                enqueued.set(true);
                Ok::<(), ()>(())
            },
            || {},
        )
        .await;
        assert!(
            !counted.get(),
            "échec busy_loci ⇒ fail-closed, aucun comptage"
        );
        assert!(
            !enqueued.get(),
            "échec busy_loci ⇒ fail-closed, aucun enqueue"
        );
    }

    #[tokio::test]
    async fn distill_cron_enqueues_planned_loci_and_increments_metric() {
        use std::cell::{Cell, RefCell};
        let cfg = DistillCronConfig {
            enabled: true,
            pressure_min: 20,
            max_jobs_per_tick: 2,
            loci: vec!["debug".to_string(), "experiments".to_string()],
            ..Default::default()
        };
        let enqueued: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let metric = Cell::new(0u32);
        let enq = &enqueued;
        let met = &metric;
        distill_cron_tick(
            &cfg,
            || async { Ok::<Vec<String>, ()>(vec![]) }, // aucun locus occupé
            // debug ≥ seuil (20) → émis ; experiments < seuil → ignoré.
            |locus: String| async move { Some(if locus == "debug" { 50u64 } else { 10 }) },
            move |locus: String| async move {
                enq.borrow_mut().push(locus);
                Ok::<(), ()>(())
            },
            move || met.set(met.get() + 1),
        )
        .await;
        assert_eq!(*enqueued.borrow(), vec!["debug".to_string()]);
        assert_eq!(metric.get(), 1, "un seul enqueue réussi ⇒ métrique = 1");
    }
}
