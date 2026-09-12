//! `/health` — background-probed liveness/detail surfaces (F-154 P2).
//!
//! ## Design — probe writes, handlers read
//!
//! A detached Tokio task ([`run_probe_loop`]) probes the service's dependencies off the
//! request path and publishes an **immutable snapshot** ([`HealthSnapshot`], an
//! [`arc_swap::ArcSwap`]) computed **once per pass**. Both handlers read that single atomic
//! value with **zero I/O** — they can never block nor fail `/health`, and there is no
//! torn read across independent atomics. A dependency that hangs delays only
//! the next probe pass, never a request.
//!
//! ## Two surfaces
//!
//! - [`liveness_handler`] — public, unauthenticated `GET /health` on the main listener.
//!   Returns the minimal [`LivenessReport`] (`status`, `version`, `build_sha`). `build_sha`
//!   stays public (fleet-check parity). HTTP status is **always 200** — the body carries the
//!   real state, including `unhealthy`.
//! - [`detailed_handler`] — loopback-only `GET /health/detailed` on the metrics side-channel
//!   (`:19091`, fail-closed to loopback). Returns the full [`HealthReport`] superset
//!   (capabilities, `dlq_by_kind`, queue, uptime, WAL…).
//!
//! ## Rollup (`status`) — computed by the probe, per operator criticality
//!
//! - **`unhealthy`** if any *critical* capability is [`CapabilityState::Unreachable`]:
//!   `embed` down, `curator` LLM absent/unreachable (heuristic-only counts as absent), or
//!   `worker` lease stale.
//! - **`degraded`** if the queue is too deep/old, a dead-letter job rotted past
//!   `DLQ_MAX_AGE_SECS`, or a *non-critical* capability (`studio`) is missing.
//! - **`starting`** until the first probe pass completes (the initial snapshot).
//! - **`ok`** otherwise.
//!
//! No PII: the payload carries no full paths, tokens, or IPs; `build_sha` is a public commit
//! identifier.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use gradatum_core::job::{JobFilter, JobOrder, JobStatus};
use gradatum_dto::{CapabilityReport, CapabilityState, HealthReport, HealthStatus, LivenessReport};

use crate::config::ServerConfig;
use crate::state::AppState;

/// Immutable `/health` snapshot published by the probe and read by the handlers.
///
/// Holds the full [`HealthReport`]; the public handler projects it to a [`LivenessReport`].
/// `ArcSwap` gives lock-free reads and a single-writer atomic store — no lock is ever held
/// across a request (ADN 2).
pub type HealthSnapshot = arc_swap::ArcSwap<HealthReport>;

/// Queue depth past which `/health` flips to `degraded` (backlog building up).
const QUEUE_DEPTH_DEGRADED: u64 = 1_000;

/// Oldest-pending age (seconds) past which `/health` flips to `degraded`.
const QUEUE_AGE_DEGRADED_SECS: u64 = 300;

/// Age threshold, in seconds, past which the **oldest** dead-letter job flips `/health`
/// to `degraded`.
///
/// A DLQ entry is a terminal failure (retries exhausted) awaiting operator triage. A fresh
/// dead job is expected transiently and must not raise the alarm on every poll; a job still
/// in the DLQ after a full operating day is *forgotten* — exactly the F-204/F-206 defect.
/// 24 h avoids flapping during active triage yet surfaces a forgotten job within a day.
const DLQ_MAX_AGE_SECS: u64 = 24 * 60 * 60;

// ── Initial snapshot ────────────────────────────────────────────────────────

/// Builds the initial `starting` snapshot — published before the first probe pass.
///
/// A pinned P1 consumer that only knows `ok`/`degraded` maps `starting` to `Unknown`
/// (DTO catch-all), never a hard error.
#[must_use]
pub fn starting_report(version: &str, build_sha: &str) -> HealthReport {
    HealthReport::new(HealthStatus::Starting, version.to_string())
        .with_build_sha(Some(build_sha.to_string()))
}

// ── Capability probes ─────────────────────────────────────────────────────────

/// Extracts `host:port` from an `http(s)://host:port/path` endpoint, owned so it can be
/// held across the `connect` await.
fn host_port_of(endpoint: &str) -> Option<String> {
    endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|s| s.split('/').next())
        .filter(|hp| !hp.is_empty())
        .map(str::to_owned)
}

/// Probes a TCP endpoint within `timeout`. A malformed URL, refused connection, or timeout
/// all yield [`CapabilityState::Unreachable`]; success yields [`CapabilityState::Reachable`].
/// The second element is the measured probe duration in milliseconds (`None` only when the
/// URL is malformed, so no probe ran).
async fn probe_tcp(endpoint: &str, timeout: Duration) -> (CapabilityState, Option<u64>) {
    let Some(host_port) = host_port_of(endpoint) else {
        // Malformed endpoint: cannot be reached, but no probe was performed.
        return (CapabilityState::Unreachable, None);
    };
    let start = Instant::now();
    let res = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(host_port)).await;
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    match res {
        Ok(Ok(_stream)) => (CapabilityState::Reachable, Some(ms)),
        // Connection error or timeout — the dependency did not answer in time.
        Ok(Err(_)) | Err(_) => (CapabilityState::Unreachable, Some(ms)),
    }
}

/// Probes the embedder (critical). Disabled by config ⇒ `not_configured` (deliberate,
/// not a failure); enabled ⇒ TCP probe of the endpoint.
async fn probe_embed(cfg: &ServerConfig, timeout: Duration) -> CapabilityReport {
    if !cfg.embed.enabled {
        return CapabilityReport::new("embed".into(), true, CapabilityState::NotConfigured, None);
    }
    let (state, ms) = probe_tcp(&cfg.embed.endpoint, timeout).await;
    CapabilityReport::new("embed".into(), true, state, ms)
}

/// Probes the curation backend (critical).
///
/// Operator criticality (F-154): a **heuristic-only** curator means the curation LLM is
/// absent — treated as a down critical capability (`unreachable`), which drives the rollup
/// to `unhealthy`. A configured LLM backend is TCP-probed; a non-heuristic backend with no
/// `llm` block is a misconfiguration and is likewise reported `unreachable`.
async fn probe_curator(cfg: &ServerConfig, timeout: Duration) -> CapabilityReport {
    if cfg.curator.backend == "heuristic" {
        return CapabilityReport::new("curator".into(), true, CapabilityState::Unreachable, None);
    }
    match &cfg.curator.llm {
        Some(llm) => {
            let (state, ms) = probe_tcp(&llm.base_url, timeout).await;
            CapabilityReport::new("curator".into(), true, state, ms)
        }
        None => CapabilityReport::new("curator".into(), true, CapabilityState::Unreachable, None),
    }
}

/// Probes worker liveness (critical) via the shared leader-election lease.
///
/// `Some(true)` ⇒ fresh lease (`reachable`); `Some(false)`/DB error/timeout ⇒ stale or
/// unconfirmable (`unreachable`, fail-closed for a critical capability); `None` ⇒ the store
/// cannot observe leadership (`not_configured`, e.g. no queue DB wired in tests).
async fn probe_worker(state: &AppState, now_ms: i64, timeout: Duration) -> CapabilityReport {
    let start = Instant::now();
    let res = tokio::time::timeout(timeout, state.job_store.worker_lease_fresh(now_ms)).await;
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (cap_state, observed) = match res {
        Ok(Ok(Some(true))) => (CapabilityState::Reachable, true),
        Ok(Ok(Some(false))) => (CapabilityState::Unreachable, true),
        Ok(Ok(None)) => (CapabilityState::NotConfigured, false),
        // Store error or timeout: cannot confirm the worker is alive → down (fail-closed).
        Ok(Err(_)) | Err(_) => (CapabilityState::Unreachable, true),
    };
    CapabilityReport::new("worker".into(), true, cap_state, observed.then_some(ms))
}

/// Probes studio asset availability (non-critical) — a local directory stat.
///
/// Missing directory ⇒ `unreachable`, which contributes at most to `degraded` (operator:
/// studio absent = degraded/mention, never KO).
fn probe_studio(cfg: &ServerConfig) -> CapabilityReport {
    let start = Instant::now();
    let present = std::fs::metadata(&cfg.studio.ui_dir)
        .map(|m| m.is_dir())
        .unwrap_or(false);
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let state = if present {
        CapabilityState::Reachable
    } else {
        CapabilityState::Unreachable
    };
    CapabilityReport::new("studio".into(), false, state, Some(ms))
}

// ── Rollup ─────────────────────────────────────────────────────────────────────

/// Derives the overall `status` from the probed capabilities and queue signals.
///
/// Never returns [`HealthStatus::Starting`] (that is the pre-first-probe snapshot only) nor
/// [`HealthStatus::Unknown`] (a deserialization catch-all, never produced).
fn derive_status(
    caps: &[CapabilityReport],
    queue_depth: u64,
    queue_oldest_age_secs: u64,
    dlq_oldest_age_secs: u64,
) -> HealthStatus {
    // A critical capability that is unreachable ⇒ the service cannot fulfill its contract.
    if caps
        .iter()
        .any(|c| c.critical && c.state == CapabilityState::Unreachable)
    {
        return HealthStatus::Unhealthy;
    }
    let queue_degraded = queue_depth > QUEUE_DEPTH_DEGRADED
        || queue_oldest_age_secs > QUEUE_AGE_DEGRADED_SECS
        || dlq_oldest_age_secs > DLQ_MAX_AGE_SECS;
    // A non-critical capability that is unreachable (e.g. studio missing) ⇒ degraded.
    let cap_degraded = caps
        .iter()
        .any(|c| !c.critical && c.state == CapabilityState::Unreachable);
    if queue_degraded || cap_degraded {
        return HealthStatus::Degraded;
    }
    HealthStatus::Ok
}

// ── Probe pass ─────────────────────────────────────────────────────────────────

/// Runs one full probe pass and returns the resulting [`HealthReport`].
///
/// External capability probes (`embed`, `curator`, `worker`) run **in parallel**, each
/// bounded by `timeout` — a hung dependency never delays the pass past that bound. The
/// queue/vault/WAL reads are local SQLite (fast) and are not individually bounded; even so,
/// a slow pass never affects `/health`, which serves the last published snapshot.
///
/// # Side effects
/// Opens a TCP connection to each configured external dependency and reads the shared
/// queue DB (`count_jobs_by_status`, `dlq_counts_by_kind`, `worker_lease_fresh`, oldest-job
/// lookups) plus the WAL file size and vault registry counts.
pub async fn probe_once(state: &AppState, timeout: Duration) -> HealthReport {
    let cfg = state.server_config.as_ref();
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);

    // Capability probes — external ones concurrently, each timeout-bounded.
    let (embed_cap, curator_cap, worker_cap) = tokio::join!(
        probe_embed(cfg, timeout),
        probe_curator(cfg, timeout),
        probe_worker(state, now_ms, timeout),
    );
    let studio_cap = probe_studio(cfg);
    let capabilities = vec![embed_cap, curator_cap, worker_cap, studio_cap];

    // Queue depth + DLQ depth from a single GROUP BY status (no N+1); absent status ⇒ 0.
    let status_counts = state
        .job_store
        .count_jobs_by_status(None)
        .await
        .unwrap_or_default();
    let queue_depth: u64 = status_counts.get(&JobStatus::Pending).copied().unwrap_or(0);
    let dlq_depth: u64 = status_counts.get(&JobStatus::DLQ).copied().unwrap_or(0);

    // Per-kind DLQ breakdown (F-154 P2 observability) — a single GROUP BY kind.
    let dlq_by_kind = state
        .job_store
        .dlq_counts_by_kind()
        .await
        .unwrap_or_default();

    let queue_oldest_age_secs = oldest_age_secs(state, JobStatus::Pending).await;
    let dlq_oldest_age_secs = oldest_age_secs(state, JobStatus::DLQ).await;

    let status = derive_status(
        &capabilities,
        queue_depth,
        queue_oldest_age_secs,
        dlq_oldest_age_secs,
    );

    let uptime_secs = state.started_at.elapsed().as_secs();

    // WAL size — real measurement; missing/checkpointed file ⇒ 0 (not a lie: WAL empty).
    let sqlite_wal_size_bytes: u64 = state
        .wal_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    let started_at = state
        .started_at_systime
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| {
            DateTime::<Utc>::from_timestamp(
                i64::try_from(d.as_secs()).unwrap_or(0),
                d.subsec_nanos(),
            )
            .map(|dt| dt.to_rfc3339())
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

    let tenant_count = state.vault.tenant_count().await.unwrap_or(0);
    let locus_count = state.vault.locus_count().await.unwrap_or(0);

    HealthReport::new(status, state.version.to_string())
        .with_build_sha(Some(state.build_sha.to_string()))
        .with_uptime_secs(uptime_secs)
        .with_counts(tenant_count, locus_count)
        .with_queue(queue_depth, queue_oldest_age_secs)
        .with_dlq(dlq_depth, dlq_oldest_age_secs, dlq_by_kind)
        .with_sqlite_wal_size_bytes(sqlite_wal_size_bytes)
        .with_started_at(Some(started_at))
        .with_capabilities(capabilities)
}

/// Age in seconds of the oldest job in `status`, from `lifecycle.created_at` via
/// `list(order=CreatedAsc, limit=1)` (ULID monotone ≡ creation order). 0 if none/error.
async fn oldest_age_secs(state: &AppState, status: JobStatus) -> u64 {
    let filter = JobFilter {
        status: Some(status),
        order: JobOrder::CreatedAsc,
        limit: 1,
        ..JobFilter::default()
    };
    state
        .job_store
        .list(filter)
        .await
        .ok()
        .and_then(|mut jobs| jobs.pop())
        .map(|job| {
            let age = Utc::now() - job.lifecycle.created_at;
            // Negative (clock skew) bounded to 0.
            u64::try_from(age.num_seconds().max(0)).unwrap_or(0)
        })
        .unwrap_or(0)
}

// ── Probe loop ─────────────────────────────────────────────────────────────────

/// Background probe loop: every `interval_secs`, run [`probe_once`] and publish the snapshot.
///
/// The first tick fires immediately, so the `starting` snapshot is replaced with a real
/// verdict within one probe pass of boot. `MissedTickBehavior::Skip` prevents catch-up
/// bursts if a pass runs long. The loop is a daemon task (ends at process exit), consistent
/// with the other periodic server tasks.
pub async fn run_probe_loop(
    state: AppState,
    snapshot: Arc<HealthSnapshot>,
    interval_secs: u64,
    timeout: Duration,
) {
    use tokio::time::{MissedTickBehavior, interval};

    let mut ticker = interval(Duration::from_secs(interval_secs));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let report = probe_once(&state, timeout).await;
        snapshot.store(Arc::new(report));
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// Handles `GET /health` — public, unauthenticated. Reads the snapshot with zero I/O and
/// projects it to the minimal [`LivenessReport`]. Always HTTP 200; the body carries the
/// real `status` (including `unhealthy`/`starting`).
pub async fn liveness_handler(State(s): State<AppState>) -> Json<LivenessReport> {
    let snap = s.health_snapshot.load_full();
    Json(LivenessReport::new(
        snap.status,
        snap.version.clone(),
        snap.build_sha.clone(),
    ))
}

/// Handles `GET /health/detailed` — loopback side-channel only. Reads the snapshot with zero
/// I/O and returns the full [`HealthReport`] superset. Always HTTP 200.
pub async fn detailed_handler(State(snapshot): State<Arc<HealthSnapshot>>) -> Json<HealthReport> {
    Json((*snapshot.load_full()).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str, critical: bool, state: CapabilityState) -> CapabilityReport {
        CapabilityReport::new(name.into(), critical, state, None)
    }

    #[test]
    fn host_port_extracts_from_url() {
        assert_eq!(
            host_port_of("http://localhost:8436/v1/embeddings").as_deref(),
            Some("localhost:8436")
        );
        assert_eq!(
            host_port_of("https://10.0.0.1:443").as_deref(),
            Some("10.0.0.1:443")
        );
        assert_eq!(host_port_of("not-a-url"), None);
        assert_eq!(host_port_of("http://"), None);
    }

    #[test]
    fn rollup_unhealthy_when_critical_capability_unreachable() {
        // curator down (critical) ⇒ unhealthy, regardless of a healthy queue.
        let caps = vec![
            cap("embed", true, CapabilityState::Reachable),
            cap("curator", true, CapabilityState::Unreachable),
            cap("worker", true, CapabilityState::Reachable),
            cap("studio", false, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Unhealthy);
    }

    #[test]
    fn rollup_unhealthy_takes_precedence_over_degraded_signals() {
        // Even with a deep queue (degraded signal), a critical down forces unhealthy.
        let caps = vec![
            cap("embed", true, CapabilityState::Unreachable),
            cap("worker", true, CapabilityState::Reachable),
        ];
        assert_eq!(
            derive_status(&caps, QUEUE_DEPTH_DEGRADED + 1, 0, 0),
            HealthStatus::Unhealthy
        );
    }

    #[test]
    fn rollup_degraded_on_studio_missing_only() {
        // studio is non-critical: missing ⇒ degraded, not unhealthy.
        let caps = vec![
            cap("embed", true, CapabilityState::Reachable),
            cap("curator", true, CapabilityState::Reachable),
            cap("worker", true, CapabilityState::Reachable),
            cap("studio", false, CapabilityState::Unreachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Degraded);
    }

    #[test]
    fn rollup_degraded_on_queue_thresholds() {
        let healthy = vec![
            cap("embed", true, CapabilityState::Reachable),
            cap("worker", true, CapabilityState::Reachable),
        ];
        assert_eq!(
            derive_status(&healthy, QUEUE_DEPTH_DEGRADED + 1, 0, 0),
            HealthStatus::Degraded
        );
        assert_eq!(
            derive_status(&healthy, 0, QUEUE_AGE_DEGRADED_SECS + 1, 0),
            HealthStatus::Degraded
        );
        assert_eq!(
            derive_status(&healthy, 0, 0, DLQ_MAX_AGE_SECS + 1),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn rollup_ok_when_all_healthy_and_queue_calm() {
        let caps = vec![
            cap("embed", true, CapabilityState::Reachable),
            cap("curator", true, CapabilityState::Reachable),
            cap("worker", true, CapabilityState::Reachable),
            cap("studio", false, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Ok);
    }

    #[test]
    fn rollup_not_configured_critical_does_not_force_unhealthy() {
        // embed disabled by config ⇒ not_configured (deliberate) ⇒ not unhealthy.
        let caps = vec![
            cap("embed", true, CapabilityState::NotConfigured),
            cap("curator", true, CapabilityState::Reachable),
            cap("worker", true, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Ok);
    }

    #[test]
    fn starting_report_carries_build_sha() {
        let r = starting_report("9.9.9", "cafebabe");
        assert_eq!(r.status, HealthStatus::Starting);
        assert_eq!(r.version, "9.9.9");
        assert_eq!(r.build_sha.as_deref(), Some("cafebabe"));
    }
}
