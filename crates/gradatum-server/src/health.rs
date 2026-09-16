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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use gradatum_core::job::{
    JOB_KIND_MESSAGE, JobFilter, JobOrder, JobStatus, QueueStore, job_kind_str,
};
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

/// Page size of the probe's `Pending` scans (bounded by `JobFilter::limit`).
const QUEUE_SCAN_PAGE: usize = 200;

/// Hard cap on the number of pages scanned per probe pass (⇒ 5 000 rows).
///
/// A scan that hits this cap **never** reports an age of `0` — it degrades to the
/// oldest `Pending` of any kind, because `0` would silently hide a real backlog.
const QUEUE_SCAN_MAX_PAGES: usize = 25;

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

// ── Queue aggregates (F-277) ───────────────────────────────────────────────────

/// Queue figures published by `/health`, message deliveries set apart (F-277).
///
/// A `Job::Message` row is **state-carrying**: it stays `Pending` until its recipient
/// acknowledges it, and no worker ever leases it. Counting it as queued work made
/// `/health` report `degraded` for a full mailbox — a signal about the recipient, not
/// about the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueAggregates {
    /// `Pending` processing jobs — message deliveries excluded.
    depth: u64,
    /// Age in seconds of the oldest `Pending` **processing** job (`0` when there is none).
    oldest_age_secs: u64,
    /// Message deliveries awaiting acknowledgement.
    mailbox_pending: u64,
}

/// Age in seconds of `created_at` relative to `now`; clock skew is bounded to `0`.
fn age_secs(now: DateTime<Utc>, created_at: DateTime<Utc>) -> u64 {
    u64::try_from((now - created_at).num_seconds().max(0)).unwrap_or(0)
}

/// Computes the queue aggregates, excluding message deliveries.
///
/// `pending_total` comes from the single `count_jobs_by_status` already issued by the
/// caller — it is not re-queried here.
///
/// # Side effects
/// Issues up to `2 × QUEUE_SCAN_MAX_PAGES` bounded `list` reads on the queue DB.
async fn queue_aggregates(
    store: &dyn QueueStore,
    pending_total: u64,
    now: DateTime<Utc>,
) -> QueueAggregates {
    let mailbox_pending = count_pending_message_deliveries(store).await;
    QueueAggregates {
        // Saturating: `mailbox_pending` is a lower bound once the scan window is
        // exhausted, so `depth` can only ever be over-estimated — never negative,
        // never under-reported (an over-estimate cannot hide a backlog).
        depth: pending_total.saturating_sub(mailbox_pending),
        oldest_age_secs: oldest_processing_age_secs(store, JobStatus::Pending, now).await,
        mailbox_pending,
    }
}

/// Counts `Pending` message deliveries, scanning at most `QUEUE_SCAN_MAX_PAGES` pages.
///
/// Beyond that window the result is a **lower bound** (and `0` on a store error) —
/// which keeps `queue_depth` conservative rather than optimistic.
async fn count_pending_message_deliveries(store: &dyn QueueStore) -> u64 {
    let mut count: u64 = 0;
    let mut cursor = None;
    for _ in 0..QUEUE_SCAN_MAX_PAGES {
        let filter = JobFilter {
            status: Some(JobStatus::Pending),
            kind: Some(JOB_KIND_MESSAGE.to_string()),
            order: JobOrder::CreatedAsc,
            limit: QUEUE_SCAN_PAGE,
            cursor,
            ..JobFilter::default()
        };
        let Ok(page) = store.list(filter).await else {
            break;
        };
        count = count.saturating_add(u64::try_from(page.len()).unwrap_or(u64::MAX));
        if page.len() < QUEUE_SCAN_PAGE {
            break;
        }
        cursor = page.last().map(|job| job.id);
        if cursor.is_none() {
            break;
        }
    }
    count
}

/// Age of the oldest job in `status` whose kind is **not** a message delivery.
///
/// Scans `status` oldest-first and stops at the first processing job. Returns `0` only
/// when that status really holds no such job (a page shorter than `QUEUE_SCAN_PAGE` proves
/// exhaustion). If the scan window runs out first, it falls back to the age of the oldest
/// job of that status, whatever its kind: reporting `0` there would turn an unreadable
/// queue into a healthy-looking one, so the fallback is a **lower bound** — never an
/// optimistic zero.
///
/// Used for the `Pending` queue, where no per-kind breakdown exists, and as the
/// conservative fail-safe of [`oldest_dead_letter_age_secs`] when the dead-letter
/// breakdown cannot be read.
async fn oldest_processing_age_secs(
    store: &dyn QueueStore,
    status: JobStatus,
    now: DateTime<Utc>,
) -> u64 {
    let mut cursor = None;
    let mut oldest_any: Option<DateTime<Utc>> = None;
    for _ in 0..QUEUE_SCAN_MAX_PAGES {
        let filter = JobFilter {
            status: Some(status),
            order: JobOrder::CreatedAsc,
            limit: QUEUE_SCAN_PAGE,
            cursor,
            ..JobFilter::default()
        };
        let Ok(page) = store.list(filter).await else {
            break;
        };
        if oldest_any.is_none() {
            oldest_any = page.first().map(|job| job.lifecycle.created_at);
        }
        if let Some(job) = page
            .iter()
            .find(|job| job_kind_str(&job.spec.kind) != JOB_KIND_MESSAGE)
        {
            return age_secs(now, job.lifecycle.created_at);
        }
        if page.len() < QUEUE_SCAN_PAGE {
            // Locus exhausted: no processing job is in this status at all.
            return 0;
        }
        cursor = page.last().map(|job| job.id);
        if cursor.is_none() {
            return 0;
        }
    }
    oldest_any.map_or(0, |created_at| age_secs(now, created_at))
}

/// Age in seconds of the oldest dead-lettered **processing** job.
///
/// Derived from `by_kind`, the per-kind dead-letter breakdown already computed for this
/// pass by the single `GROUP BY kind` — an exact, unbounded aggregate. [`job_kind_str`] is
/// the single source of truth for both the `kind` column written at enqueue time and the
/// [`JOB_KIND_MESSAGE`] guard, so the breakdown names every non-delivery kind present in
/// the DLQ; a kind it does not name cannot age the queue.
///
/// Two consequences, both wanted:
///
/// - a DLQ holding nothing but message deliveries (a mailbox that filled up) is answered
///   `0` **exactly**, without a single row read — the population this field sets apart cannot
///   make a healthy service look degraded, however large it grows;
/// - when a non-delivery kind is present, its oldest entry is read directly (`limit 1`
///   per kind) and the oldest of those answers. This is **exact**: neither the number of
///   dead-lettered deliveries nor the size of the DLQ can push a processing entry out of
///   reach, so there is no bounded window and no fallback to the age of a delivery.
///
/// # Failure behaviour
/// An unreadable queue must not look healthy. When the breakdown is unavailable (`None`)
/// or any per-kind read fails, this degrades to [`oldest_processing_age_secs`] and its
/// conservative fallback (the oldest entry of the status, whatever its kind) — never to an
/// optimistic `0`.
async fn oldest_dead_letter_age_secs(
    store: &dyn QueueStore,
    by_kind: Option<&BTreeMap<String, u64>>,
    now: DateTime<Utc>,
) -> u64 {
    let Some(by_kind) = by_kind else {
        return oldest_processing_age_secs(store, JobStatus::DLQ, now).await;
    };
    let processing_kinds: Vec<&String> = by_kind
        .iter()
        .filter(|(kind, count)| kind.as_str() != JOB_KIND_MESSAGE && **count > 0)
        .map(|(kind, _)| kind)
        .collect();
    if processing_kinds.is_empty() {
        return 0;
    }
    let mut oldest_created: Option<DateTime<Utc>> = None;
    for kind in processing_kinds {
        let filter = JobFilter {
            status: Some(JobStatus::DLQ),
            kind: Some(kind.clone()),
            order: JobOrder::CreatedAsc,
            limit: 1,
            cursor: None,
            ..JobFilter::default()
        };
        let Ok(page) = store.list(filter).await else {
            return oldest_processing_age_secs(store, JobStatus::DLQ, now).await;
        };
        if let Some(job) = page.first() {
            let created_at = job.lifecycle.created_at;
            oldest_created = Some(oldest_created.map_or(created_at, |c| c.min(created_at)));
        }
    }
    oldest_created.map_or(0, |created_at| age_secs(now, created_at))
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
    let pending_total: u64 = status_counts.get(&JobStatus::Pending).copied().unwrap_or(0);
    let dlq_depth: u64 = status_counts.get(&JobStatus::DLQ).copied().unwrap_or(0);

    // F-277: message deliveries are state-carrying rows waiting on their recipient,
    // never queued work — they are counted apart, not as queue depth.
    let aggregates = queue_aggregates(state.job_store.as_ref(), pending_total, Utc::now()).await;
    let queue_depth = aggregates.depth;
    // Le compteur de boîte est publié par le rapport DÉTAILLÉ (listener loopback), et
    // journalisé ici en `debug!` — jamais en `info!` : à 10 s de cadence, une boîte non
    // vide (état parfaitement normal) produirait ~8 600 lignes par jour sur le canal
    // opérateur, exactement le bruit que F-277 retire de `/health`.
    tracing::debug!(
        mailbox_pending = aggregates.mailbox_pending,
        queue_depth,
        "queue aggregates (Message deliveries set apart)"
    );

    // Per-kind DLQ breakdown (F-154 P2 observability) — a single GROUP BY kind. Kept as a
    // `Result` rather than defaulted here: an unavailable breakdown must not be read as
    // "no processing job is dead" (a `0` that says "healthy" about a queue nobody could read).
    let dlq_by_kind = state.job_store.dlq_counts_by_kind().await;

    let queue_oldest_age_secs = aggregates.oldest_age_secs;
    // Un job `Message` en DLQ attend un acquittement, pas un worker : son âge ne dit rien
    // de l'abandon d'un travail mort (le compte, lui, reste dans `dlq_depth`). L'âge est
    // lu par kind sur l'agrégat ci-dessus — exact, sans balayage borné.
    let dlq_oldest_age_secs = oldest_dead_letter_age_secs(
        state.job_store.as_ref(),
        dlq_by_kind.as_ref().ok(),
        Utc::now(),
    )
    .await;

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
        .with_mailbox_pending(aggregates.mailbox_pending)
        .with_dlq(
            dlq_depth,
            dlq_oldest_age_secs,
            dlq_by_kind.unwrap_or_default(),
        )
        .with_sqlite_wal_size_bytes(sqlite_wal_size_bytes)
        .with_started_at(Some(started_at))
        .with_capabilities(capabilities)
        // F-160 L-D.2 — the personal-data frontier, appended last so the published shape
        // grows at its end. The two figures are the live registry's, not a re-count: the
        // observer increments them on the writing task, so a probe reads what the passes
        // actually did rather than re-walking a corpus it has no business walking.
        //
        // `with_privacy` may raise an `Ok` report to `Degraded` when the corpus holds a
        // note no pass judged. That is the intended direction and the only one: an
        // unfinished scan is a loss of coverage, not a service outage.
        .with_privacy(
            state.privacy.mode.as_str(),
            state.metrics.privacy_scanned.get(),
            state.metrics.privacy_unscanned.get(),
        )
        .with_privacy_detector(state.mounted_detector_name())
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

#[cfg(test)]
mod f277_queue_aggregates_tests {
    use std::collections::HashMap;
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use chrono::{DateTime, Duration, Utc};
    use gradatum_core::job::{
        Job, JobClass, JobFilter, JobLifecycle, JobLineage, JobMode, JobPriority, JobRecord,
        JobResult, JobRetry, JobScheduling, JobScope, JobSpec, JobStatus, MessageDeliverySpec,
        QueueError, QueueStore, TriggerSource, job_kind_str,
    };
    use ulid::Ulid;

    use super::{
        QUEUE_SCAN_MAX_PAGES, QUEUE_SCAN_PAGE, QueueAggregates, derive_status,
        oldest_dead_letter_age_secs, queue_aggregates,
    };
    use gradatum_dto::{CapabilityReport, CapabilityState, HealthStatus};

    /// File en mémoire : seuls `list` et `count_jobs_by_status` sont significatifs.
    struct FakeQueue {
        jobs: Vec<JobRecord>,
    }

    impl FakeQueue {
        fn new(jobs: Vec<JobRecord>) -> Self {
            Self { jobs }
        }
    }

    #[async_trait]
    impl QueueStore for FakeQueue {
        async fn enqueue(&self, _job: JobRecord) -> Result<Ulid, QueueError> {
            Err(QueueError::Storage("fake".into()))
        }
        async fn dequeue(&self, _t: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
            Ok(None)
        }
        async fn get(&self, _id: Ulid, _t: Option<&str>) -> Result<Option<JobRecord>, QueueError> {
            Ok(None)
        }
        async fn complete(&self, _id: Ulid, _r: JobResult) -> Result<(), QueueError> {
            Ok(())
        }
        async fn fail(&self, _id: Ulid, _e: &str, _a: u32) -> Result<(), QueueError> {
            Ok(())
        }
        async fn cancel(&self, _id: Ulid, _t: Option<&str>) -> Result<(), QueueError> {
            Ok(())
        }
        async fn fail_dlq(&self, _id: Ulid, _e: &str) -> Result<(), QueueError> {
            Ok(())
        }
        async fn find_awaiting(&self, _id: Ulid) -> Result<Vec<JobRecord>, QueueError> {
            Ok(vec![])
        }
        async fn set_pending(&self, _id: Ulid) -> Result<(), QueueError> {
            Ok(())
        }
        async fn recover_stale_leases(&self, _ttl: StdDuration) -> Result<Vec<Ulid>, QueueError> {
            Ok(vec![])
        }
        async fn cancel_expired_deadlines(
            &self,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Ulid>, QueueError> {
            Ok(vec![])
        }
        async fn promote_retries(&self, _now: DateTime<Utc>) -> Result<Vec<Ulid>, QueueError> {
            Ok(vec![])
        }
        async fn schedule_retry(&self, _id: Ulid, _at: DateTime<Utc>) -> Result<(), QueueError> {
            Ok(())
        }
        fn subscribe(&self) -> tokio::sync::broadcast::Receiver<gradatum_core::job::QueueEvent> {
            tokio::sync::broadcast::channel(1).1
        }

        /// Applique `status`, `kind`, l'ordre `CreatedAsc`, le curseur et la limite —
        /// le sous-ensemble de `JobFilter` dont dépend le probe.
        async fn list(&self, filter: JobFilter) -> Result<Vec<JobRecord>, QueueError> {
            let mut out: Vec<JobRecord> = self
                .jobs
                .iter()
                .filter(|j| filter.status.is_none_or(|s| s == j.lifecycle.status))
                .filter(|j| {
                    filter
                        .kind
                        .as_deref()
                        .is_none_or(|k| k == job_kind_str(&j.spec.kind))
                })
                .filter(|j| filter.cursor.is_none_or(|c| j.id > c))
                .cloned()
                .collect();
            out.sort_by_key(|j| j.id);
            out.truncate(filter.limit);
            Ok(out)
        }

        async fn count_jobs_by_status(
            &self,
            _t: Option<&str>,
        ) -> Result<HashMap<JobStatus, u64>, QueueError> {
            let mut m: HashMap<JobStatus, u64> = HashMap::new();
            for j in &self.jobs {
                *m.entry(j.lifecycle.status).or_insert(0) += 1;
            }
            Ok(m)
        }
    }

    /// Construit un job `Pending` du kind demandé, créé `age_secs` avant `now`.
    fn pending(job: Job, now: DateTime<Utc>, age_secs: i64) -> JobRecord {
        let created = now - Duration::seconds(age_secs);
        JobRecord {
            id: Ulid::from_datetime(created.into()),
            spec: JobSpec {
                kind: job,
                class: JobClass::System,
                mode: JobMode::Batch,
                scope: JobScope::VaultWide,
                priority: JobPriority::Low,
            },
            scheduling: JobScheduling {
                trigger: TriggerSource::Demand,
                scheduled_at: created,
                await_jobs: vec![],
                deadline: None,
                cron_expr: None,
            },
            lifecycle: JobLifecycle {
                status: JobStatus::Pending,
                created_at: created,
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

    fn message_job() -> Job {
        Job::Message(MessageDeliverySpec {
            message_id: Ulid::generate(),
            recipient: "identity/agent/backend".to_string(),
        })
    }

    /// Construit un job en file de rebut (DLQ), créé `age_secs` avant `now`.
    ///
    /// L'ULID reste dérivé de `created_at` (comme [`pending`]) : l'ordre de lecture
    /// par identifiant est donc l'ordre des dates, celui des balayages bornés.
    fn dead(job: Job, now: DateTime<Utc>, age_secs: i64) -> JobRecord {
        let mut record = pending(job, now, age_secs);
        record.lifecycle.status = JobStatus::DLQ;
        record
    }
    fn embed_job() -> Job {
        Job::Embed(gradatum_core::job::EmbedSpec {
            note_id: Ulid::generate(),
            tenant_id: "main".to_string(),
            force_regenerate: false,
        })
    }

    async fn aggregates_of(jobs: Vec<JobRecord>, now: DateTime<Utc>) -> QueueAggregates {
        let store = FakeQueue::new(jobs);
        let pending_total = store
            .count_jobs_by_status(None)
            .await
            .unwrap_or_default()
            .get(&JobStatus::Pending)
            .copied()
            .unwrap_or(0);
        queue_aggregates(&store, pending_total, now).await
    }

    fn healthy_caps() -> Vec<CapabilityReport> {
        vec![
            CapabilityReport::new("embed".into(), true, CapabilityState::Reachable, None),
            CapabilityReport::new("worker".into(), true, CapabilityState::Reachable, None),
            CapabilityReport::new("studio".into(), false, CapabilityState::Reachable, None),
        ]
    }

    #[tokio::test]
    async fn pending_message_delivery_alone_keeps_the_queue_healthy() {
        let now = Utc::now();
        let agg = aggregates_of(vec![pending(message_job(), now, 400)], now).await;
        assert_eq!(
            agg.depth, 0,
            "une livraison en attente n'est pas un travail"
        );
        assert_eq!(agg.oldest_age_secs, 0);
        assert_eq!(agg.mailbox_pending, 1);
        assert_eq!(
            derive_status(&healthy_caps(), agg.depth, agg.oldest_age_secs, 0),
            HealthStatus::Ok
        );
    }

    #[tokio::test]
    async fn pending_processing_job_still_degrades() {
        let now = Utc::now();
        let agg = aggregates_of(vec![pending(embed_job(), now, 400)], now).await;
        assert_eq!(agg.depth, 1);
        assert!(agg.oldest_age_secs >= 400, "âge = {}", agg.oldest_age_secs);
        assert_eq!(
            derive_status(&healthy_caps(), agg.depth, agg.oldest_age_secs, 0),
            HealthStatus::Degraded
        );
    }

    #[tokio::test]
    async fn depth_counts_processing_jobs_and_mailbox_counts_deliveries() {
        let now = Utc::now();
        let jobs = vec![
            pending(message_job(), now, 10),
            pending(message_job(), now, 11),
            pending(message_job(), now, 12),
            pending(embed_job(), now, 13),
            pending(embed_job(), now, 14),
        ];
        let agg = aggregates_of(jobs, now).await;
        assert_eq!(agg.depth, 2);
        assert_eq!(agg.mailbox_pending, 3);
    }

    #[tokio::test]
    async fn older_message_backlog_does_not_hide_a_stuck_processing_job() {
        let now = Utc::now();
        let mut jobs: Vec<JobRecord> = (0..60)
            .map(|i| pending(message_job(), now, 1_000 + i))
            .collect();
        jobs.push(pending(embed_job(), now, 400));
        let agg = aggregates_of(jobs, now).await;
        assert_eq!(agg.depth, 1);
        assert!(agg.oldest_age_secs >= 400, "âge = {}", agg.oldest_age_secs);
        assert_eq!(agg.mailbox_pending, 60);
    }

    /// Scan window exhausted ⇒ never `0`: degrade to the age of the oldest `Pending` of
    /// any kind, because a `0` here would make a real outage look like a healthy queue.
    #[tokio::test]
    async fn exhausted_scan_window_falls_back_to_oldest_pending_never_zero() {
        let now = Utc::now();
        let total = QUEUE_SCAN_PAGE * QUEUE_SCAN_MAX_PAGES + 1;
        let jobs: Vec<JobRecord> = (0..total)
            .map(|i| pending(message_job(), now, 10_000 - i64::try_from(i).unwrap_or(0)))
            .collect();
        let agg = aggregates_of(jobs, now).await;
        assert!(
            agg.oldest_age_secs > 0,
            "fenêtre épuisée : l'âge doit retomber sur le plus ancien Pending, pas sur 0"
        );
    }

    /// Agrégat par kind indisponible (lecture en échec) ⇒ repli conservateur.
    ///
    /// Le kind ne peut plus être consulté pour trancher la population ; l'âge revient au
    /// balayage borné et à son repli, jamais à un `0` optimiste qui blanchirait une file
    /// de rebut illisible.
    #[tokio::test]
    async fn unreadable_breakdown_falls_back_to_the_bounded_scan() {
        let now = Utc::now();
        let store = FakeQueue::new(vec![dead(embed_job(), now, 400)]);

        // `None` = l'agrégat `GROUP BY kind` n'a pas pu être lu pendant cette passe.
        let age = oldest_dead_letter_age_secs(&store, None, now).await;

        assert!(
            age >= 400,
            "sans agrégat, le job de traitement mort doit quand même vieillir la DLQ (obtenu {age})"
        );
    }
}
