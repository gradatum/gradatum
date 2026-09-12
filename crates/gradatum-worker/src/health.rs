//! `/health` for the worker (F-156 P3) — the worker's first liveness/readiness surface.
//!
//! Until this module the worker exposed **no** `/health` at all: fleet checks could only
//! infer its liveness indirectly, from the leader-election lease it renews. That proxy is
//! misleading: the lease's `expires_at` measures the *renew* task,
//! not job consumption, so a frozen consumer with a live renew loop reads as alive.
//!
//! ## Design — probe writes, handlers read (mirrors `gradatum-server`)
//!
//! A detached Tokio task ([`run_probe_loop`]) probes the worker's dependencies off the
//! request path and publishes an **immutable snapshot** ([`HealthSnapshot`], an
//! [`arc_swap::ArcSwap`]) computed once per pass. Both handlers read that single atomic
//! value with **zero I/O** — `/health` can never block nor fail, and there is no torn read
//! across independent atomics. A dependency that hangs delays only the next probe pass.
//!
//! ## Two surfaces, one loopback listener (unconditional)
//!
//! Unlike the server (public `/health` on the main listener, `/health/detailed` on the
//! metrics side-channel), the worker is a background consumer with no public listener. Its
//! health listener is therefore **loopback-only** and serves both routes:
//!
//! - `liveness_handler` — `GET /health` → minimal [`LivenessReport`] (`status`, `version`,
//!   `build_sha`). Contract-identical to the other three services (same `gradatum-dto`).
//! - `detailed_handler` — `GET /health/detailed` → full [`HealthReport`] superset
//!   (capabilities, `dlq_by_kind`, queue, uptime, consumption verdict).
//!
//! The listener is started **unconditionally** ([`spawn_health_server`]): a worker with
//! `[apalis.metrics].enabled = false` still answers `/health`. Its bind is **separate** from
//! the gated `/metrics` scrape (`spawn_metrics_server`), and it is **bind-tolerant** for a
//! *loopback* address — a taken port logs a warning and the worker keeps running, never a `?`
//! that would abort boot over an observability port. A **non-loopback**
//! `bind`, however, is fail-closed: it would leak `/health/detailed` on the LAN, so it aborts
//! boot, mirroring the server's loopback-only metrics listener.
//!
//! ## Rollup (`status`) — worker criticality
//!
//! - **`unhealthy`** if any *critical* capability is [`CapabilityState::Unreachable`]:
//!   the `internal` server (all mutations flow through it), the `leadership` lease is stale,
//!   the configured `embed` endpoint is down, or a configured curator `llm` is down.
//! - **`degraded`** if the queue is too deep/old, a dead-letter job rotted past
//!   `DLQ_MAX_AGE_SECS`, the `consumption` signal went stale while jobs were pending, or a
//!   job kind's failure rate crossed `FAILURE_RATIO_DEGRADED`.
//! - **`starting`** until the first probe pass completes.
//! - **`ok`** otherwise.
//!
//! No secrets: the payload carries no tokens, full paths, or IPs; `build_sha` is a public
//! commit identifier.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Utc};
use gradatum_core::QueueStore;
use gradatum_core::job::{JobFilter, JobOrder, JobStatus};
use gradatum_dto::{CapabilityReport, CapabilityState, HealthReport, HealthStatus, LivenessReport};
use serde::{Deserialize, Serialize};

use crate::curator_loader::WorkerCuratorConfig;
use crate::metrics::WorkerMetrics;

/// Immutable `/health` snapshot published by the probe and read by the handlers.
///
/// `ArcSwap` gives lock-free reads and a single-writer atomic store — no lock is ever held
/// across a request (ADN 2). The public handler projects the held [`HealthReport`] to a
/// [`LivenessReport`].
pub type HealthSnapshot = arc_swap::ArcSwap<HealthReport>;

/// Queue depth past which `/health` flips to `degraded` (backlog building up).
const QUEUE_DEPTH_DEGRADED: u64 = 1_000;

/// Oldest-pending age (seconds) past which `/health` flips to `degraded`.
const QUEUE_AGE_DEGRADED_SECS: u64 = 300;

/// Age threshold (seconds) past which the **oldest** dead-letter job flips `/health` to
/// `degraded`. A dead job still in the DLQ after a full operating day is *forgotten*; 24 h
/// avoids flapping during active triage yet surfaces a forgotten job within a day (mirrors
/// the server's `DLQ_MAX_AGE_SECS`).
const DLQ_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Seconds without a consumed job, **while jobs are pending**, past which the `consumption`
/// capability flips to `unreachable` (consumer frozen although its lease keeps renewing).
///
/// Only triggers when `queue_depth > 0`: an idle worker with an empty queue legitimately
/// consumes nothing and stays healthy. This is the Caveat-1 signal the lease cannot give.
const CONSUMPTION_STALE_SECS: i64 = 300;

/// Per-kind failure ratio (`failed / total`) at or above which that kind's job-health
/// capability flips to `unreachable` (non-critical → `degraded`).
const FAILURE_RATIO_DEGRADED: f64 = 0.5;

/// Minimum jobs processed for a kind before its failure ratio is trusted — avoids flapping
/// to `degraded` on a single early failure (`1/1 = 100 %`).
const FAILURE_MIN_SAMPLES: u64 = 5;

/// Interval between probe passes.
const PROBE_INTERVAL_SECS: u64 = 15;

/// Per-probe timeout for each external dependency (TCP connect / lease read).
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

// ── Config ─────────────────────────────────────────────────────────────────────

/// Configuration of the worker's unconditional loopback health listener (`[apalis.health]`).
///
/// A **dedicated** port, separate from `[apalis.metrics]`: the metrics listener is gated and
/// defaults to `19091` (which the server already holds as its metrics side-channel), whereas
/// the health listener must bind unconditionally and must not collide. The default
/// `127.0.0.1:19094` sits clear of every other Gradatum loopback port (server public `19090`,
/// server metrics `19091`, internal API `19092`, worker metrics `19091`/`19093`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthServerConfig {
    /// Bind address. Loopback by default — the worker has no reason to expose `/health`
    /// beyond the host (fleet checks reach it over loopback, or the server proxies it).
    #[serde(default = "HealthServerConfig::default_bind")]
    pub bind: String,
    /// Listen port. Default `19094` (see the struct docs for the non-collision rationale).
    #[serde(default = "HealthServerConfig::default_port")]
    pub port: u16,
}

impl HealthServerConfig {
    /// Default bind address: loopback.
    fn default_bind() -> String {
        "127.0.0.1".to_string()
    }
    /// Default port: `19094`, clear of every other Gradatum loopback port.
    fn default_port() -> u16 {
        19094
    }
}

impl Default for HealthServerConfig {
    fn default() -> Self {
        Self {
            bind: Self::default_bind(),
            port: Self::default_port(),
        }
    }
}

// ── Probe dependencies ───────────────────────────────────────────────────────

/// Everything the probe reads. Owned/`Arc` handles so the probe loop is `'static`.
///
/// Grouped into a struct to keep the probe-loop spawn within the clippy argument limit and
/// to name each dependency the readiness verdict is built from.
pub struct ProbeDeps {
    /// Shared queue store — source of queue depth, DLQ, oldest-age and the leadership lease.
    pub store: Arc<dyn QueueStore + Send + Sync>,
    /// Shared metrics — source of the consumption signal and per-kind failure counts.
    pub metrics: WorkerMetrics,
    /// Base URL of the internal server API (`http://127.0.0.1:19092`) — critical dependency.
    pub internal_url: String,
    /// Whether the embedder is enabled (`[embed] enabled`).
    pub embed_enabled: bool,
    /// Embedder endpoint (`[embed] endpoint`) — probed only when enabled.
    pub embed_endpoint: String,
    /// Curator backend name (`heuristic` ⇒ offline, not a failure; else an LLM backend).
    pub curator_backend: String,
    /// Curator LLM base URL (`[curator.llm] base_url`), when an LLM backend is configured.
    pub curator_llm_base_url: Option<String>,
    /// Binary semantic version.
    pub version: String,
    /// Public build commit SHA.
    pub build_sha: String,
    /// Process start instant, for `uptime_secs`.
    pub started_at: Instant,
    /// Process start wall-clock, for the `started_at` RFC-3339 field.
    pub started_at_systime: SystemTime,
}

/// Reads the curator probe target from the `[curator]` section of the config, best-effort.
///
/// Returns `(backend, llm_base_url)`. Any read/parse failure (file absent, section absent,
/// malformed) yields the safe offline default `("heuristic", None)` — the authoritative
/// fallback logging is already done by
/// [`build_curator_pipeline`](crate::curator_loader::build_curator_pipeline), so this read
/// stays silent and does **not** double-record the section in `ConfigHealth`.
#[must_use]
pub fn curator_probe_target(config_path: &Path) -> (String, Option<String>) {
    use figment::{
        Figment,
        providers::{Format, Toml},
    };
    if !config_path.exists() {
        return ("heuristic".to_string(), None);
    }
    let cfg: WorkerCuratorConfig = Figment::new()
        .merge(Toml::file(config_path))
        .extract_inner("curator")
        .unwrap_or_default();
    let base_url = cfg.llm.map(|l| l.base_url);
    (cfg.backend, base_url)
}

// ── Initial snapshot ────────────────────────────────────────────────────────

/// Builds the initial `starting` snapshot — published before the first probe pass. A pinned
/// P1 consumer that only knows `ok`/`degraded` maps `starting` to `Unknown` (DTO catch-all),
/// never a hard error.
#[must_use]
pub fn starting_report(version: &str, build_sha: &str) -> HealthReport {
    HealthReport::new(HealthStatus::Starting, version.to_string())
        .with_build_sha(Some(build_sha.to_string()))
}

// ── Capability probes ─────────────────────────────────────────────────────────

/// Extracts `host:port` from an `http(s)://host:port/path` endpoint, owned so it can be held
/// across the `connect` await.
//
// ECON: duplicated from `gradatum-server::health` (2nd occurrence — Rule of Three not yet
// crossed). Sharing would need a new leaf crate both depend on; deferred. Upgrade → extract
// to a `gradatum-net`/`gradatum-health` leaf when a 3rd producer appears.
fn host_port_of(endpoint: &str) -> Option<String> {
    endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|s| s.split('/').next())
        .filter(|hp| !hp.is_empty())
        .map(str::to_owned)
}

/// Probes a TCP endpoint within `PROBE_TIMEOUT`. A malformed URL, refused connection, or
/// timeout all yield [`CapabilityState::Unreachable`]; success yields
/// [`CapabilityState::Reachable`]. The second element is the measured probe duration in
/// milliseconds (`None` only when the URL is malformed, so no probe ran).
async fn probe_tcp(endpoint: &str) -> (CapabilityState, Option<u64>) {
    let Some(host_port) = host_port_of(endpoint) else {
        return (CapabilityState::Unreachable, None);
    };
    let start = Instant::now();
    let res = tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(host_port)).await;
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    match res {
        Ok(Ok(_stream)) => (CapabilityState::Reachable, Some(ms)),
        Ok(Err(_)) | Err(_) => (CapabilityState::Unreachable, Some(ms)),
    }
}

/// Probes the internal server API (critical): every vault/index mutation the worker performs
/// flows through it, so an unreachable server means the worker cannot fulfil its contract.
async fn probe_internal(url: &str) -> CapabilityReport {
    let (state, ms) = probe_tcp(url).await;
    CapabilityReport::new("internal".into(), true, state, ms)
}

/// Probes the embedder (critical when enabled). Disabled by config ⇒ `not_configured`
/// (deliberate, not a failure); enabled ⇒ TCP probe of the endpoint.
async fn probe_embed(enabled: bool, endpoint: &str) -> CapabilityReport {
    if !enabled {
        return CapabilityReport::new("embed".into(), true, CapabilityState::NotConfigured, None);
    }
    let (state, ms) = probe_tcp(endpoint).await;
    CapabilityReport::new("embed".into(), true, state, ms)
}

/// Probes the curator backend.
///
/// A `heuristic` backend (or an LLM backend with no `base_url`) is a legitimate **offline**
/// mode for the worker — it still curates without a model — so it is reported
/// `not_configured` (non-critical), never driving the worker to `unhealthy`. A configured
/// LLM backend is TCP-probed and is **critical**: an operator who wired an LLM expects it up.
///
/// ## Authority divergence — do **not** alert on this verdict
///
/// The **server's** `/health` is the sole product authority for the curator: there an offline
/// heuristic curator reads as `unhealthy` (operator arbitrage). Here on the **worker** the same
/// heuristic/offline curator is `not_configured` and **non-critical** — a deliberate divergence.
/// No fleet/float alert must be built on the worker's curator verdict; the server's `/health`
/// carries the product-level curator judgment.
async fn probe_curator(backend: &str, llm_base_url: Option<&str>) -> CapabilityReport {
    match (backend, llm_base_url) {
        ("heuristic", _) | (_, None) => CapabilityReport::new(
            "curator".into(),
            false,
            CapabilityState::NotConfigured,
            None,
        ),
        (_, Some(base_url)) => {
            let (state, ms) = probe_tcp(base_url).await;
            CapabilityReport::new("curator".into(), true, state, ms)
        }
    }
}

/// Probes worker leadership (critical) via the shared leader-election lease.
///
/// `Some(true)` ⇒ fresh lease (`reachable`); `Some(false)`/DB error/timeout ⇒ stale or
/// unconfirmable (`unreachable`, fail-closed for a critical capability); `None` ⇒ the store
/// cannot observe leadership (`not_configured`).
async fn probe_leadership(
    store: &Arc<dyn QueueStore + Send + Sync>,
    now_ms: i64,
) -> CapabilityReport {
    let start = Instant::now();
    let res = tokio::time::timeout(PROBE_TIMEOUT, store.worker_lease_fresh(now_ms)).await;
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (state, observed) = match res {
        Ok(Ok(Some(true))) => (CapabilityState::Reachable, true),
        Ok(Ok(Some(false))) => (CapabilityState::Unreachable, true),
        Ok(Ok(None)) => (CapabilityState::NotConfigured, false),
        Ok(Err(_)) | Err(_) => (CapabilityState::Unreachable, true),
    };
    CapabilityReport::new("leadership".into(), true, state, observed.then_some(ms))
}

/// Derives the `consumption` capability (non-critical) — the real "is the Monitor consuming?"
/// signal, distinct from the renew lease.
///
/// - Empty queue (`queue_depth == 0`) ⇒ `reachable`: nothing to consume is not a failure.
/// - **Fresh** backlog (`queue_oldest_age_secs <= CONSUMPTION_STALE_SECS`) ⇒ `reachable`, even
///   if nothing was consumed recently: a legitimately long job behind a just-arrived backlog is
///   not a frozen consumer (gating on `queue_depth > 0` alone would false-flag).
/// - **Stale** backlog (oldest pending job older than [`CONSUMPTION_STALE_SECS`]) **and** the
///   last consumption older than the same window (or none ever, `last_consumed_ms == 0`) ⇒
///   `unreachable`: the consumer is frozen while its lease may still renew.
/// - Otherwise ⇒ `reachable`.
///
/// `last_probe_ms` carries the age of the last consumption in **milliseconds** (`None` when
/// nothing has been consumed yet), so the raw signal is visible in the report as well as in
/// the `gradatum_worker_last_job_consumed_timestamp_seconds` gauge.
fn derive_consumption(
    last_consumed_ms: i64,
    now_ms: i64,
    queue_depth: u64,
    queue_oldest_age_secs: u64,
) -> CapabilityReport {
    let age_ms = (last_consumed_ms > 0).then(|| (now_ms - last_consumed_ms).max(0));
    // Only a *stale backlog* can signal a frozen consumer: jobs pending AND the oldest of them
    // waiting longer than the stale window. A fresh backlog behind a long-running job is not
    // staleness. Compare in the i64 domain to avoid a lossy `as` cast.
    let oldest_secs = i64::try_from(queue_oldest_age_secs).unwrap_or(i64::MAX);
    let backlog_stale = queue_depth > 0 && oldest_secs > CONSUMPTION_STALE_SECS;
    let state = if !backlog_stale {
        CapabilityState::Reachable
    } else {
        let consumption_stale = match last_consumed_ms {
            0 => true, // stale backlog but nothing ever consumed
            _ => (now_ms - last_consumed_ms) / 1000 > CONSUMPTION_STALE_SECS,
        };
        if consumption_stale {
            CapabilityState::Unreachable
        } else {
            CapabilityState::Reachable
        }
    };
    CapabilityReport::new(
        "consumption".into(),
        false,
        state,
        age_ms.map(|ms| u64::try_from(ms).unwrap_or(u64::MAX)),
    )
}

/// Builds one job-health capability per kind that has processed at least one job, encoding
/// the `failed / total` verdict (non-critical): a kind at or above
/// [`FAILURE_RATIO_DEGRADED`] over at least [`FAILURE_MIN_SAMPLES`] jobs flips to
/// `unreachable` (→ `degraded`). The exact ratio stays available in
/// `gradatum_jobs_total{kind,status}` on `/metrics`; the report carries the derived verdict.
///
/// `last_probe_ms` carries the sampled `total` per kind, so the sample size backing the
/// verdict is visible without a `/metrics` round-trip.
fn job_health_capabilities(by_kind: &BTreeMap<String, (u64, u64)>) -> Vec<CapabilityReport> {
    by_kind
        .iter()
        .map(|(kind, &(failed, total))| {
            #[allow(clippy::cast_precision_loss)]
            let ratio = failed as f64 / total as f64;
            let state = if total >= FAILURE_MIN_SAMPLES && ratio >= FAILURE_RATIO_DEGRADED {
                CapabilityState::Unreachable
            } else {
                CapabilityState::Reachable
            };
            CapabilityReport::new(format!("jobs:{kind}"), false, state, Some(total))
        })
        .collect()
}

// ── Rollup ─────────────────────────────────────────────────────────────────────

/// Derives the overall `status` from the probed capabilities and queue signals.
///
/// Never returns [`HealthStatus::Starting`] (the pre-first-probe snapshot only) nor
/// [`HealthStatus::Unknown`] (a deserialization catch-all, never produced).
fn derive_status(
    caps: &[CapabilityReport],
    queue_depth: u64,
    queue_oldest_age_secs: u64,
    dlq_oldest_age_secs: u64,
) -> HealthStatus {
    if caps
        .iter()
        .any(|c| c.critical && c.state == CapabilityState::Unreachable)
    {
        return HealthStatus::Unhealthy;
    }
    let queue_degraded = queue_depth > QUEUE_DEPTH_DEGRADED
        || queue_oldest_age_secs > QUEUE_AGE_DEGRADED_SECS
        || dlq_oldest_age_secs > DLQ_MAX_AGE_SECS;
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
/// External capability probes (`internal`, `embed`, `curator`, `leadership`) run **in
/// parallel**, each bounded by `PROBE_TIMEOUT` — a hung dependency never delays the pass
/// past that bound. The queue reads are local SQLite (fast). Even a slow pass never affects
/// `/health`, which serves the last published snapshot.
///
/// # Side effects
/// Opens a TCP connection to each configured external dependency and reads the shared queue
/// DB (`count_jobs_by_status`, `dlq_counts_by_kind`, `worker_lease_fresh`, oldest-job
/// lookups). Reads the in-process metrics (no I/O).
pub async fn probe_once(deps: &ProbeDeps) -> HealthReport {
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX);

    // External probes concurrently, each timeout-bounded.
    let (internal_cap, embed_cap, curator_cap, leadership_cap) = tokio::join!(
        probe_internal(&deps.internal_url),
        probe_embed(deps.embed_enabled, &deps.embed_endpoint),
        probe_curator(&deps.curator_backend, deps.curator_llm_base_url.as_deref()),
        probe_leadership(&deps.store, now_ms),
    );

    // Queue depth + DLQ depth from a single GROUP BY status (no N+1); absent status ⇒ 0.
    let status_counts = deps
        .store
        .count_jobs_by_status(None)
        .await
        .unwrap_or_default();
    let queue_depth: u64 = status_counts.get(&JobStatus::Pending).copied().unwrap_or(0);
    let dlq_depth: u64 = status_counts.get(&JobStatus::DLQ).copied().unwrap_or(0);

    let dlq_by_kind = deps.store.dlq_counts_by_kind().await.unwrap_or_default();

    let queue_oldest_age_secs = oldest_age_secs(&deps.store, JobStatus::Pending).await;
    let dlq_oldest_age_secs = oldest_age_secs(&deps.store, JobStatus::DLQ).await;

    // Consumption signal + per-kind failure verdict from in-process metrics.
    let consumption_cap = derive_consumption(
        deps.metrics.last_consumed_ms(),
        now_ms,
        queue_depth,
        queue_oldest_age_secs,
    );
    let job_caps = job_health_capabilities(&deps.metrics.failure_counts_by_kind());

    let mut capabilities = vec![
        internal_cap,
        embed_cap,
        curator_cap,
        leadership_cap,
        consumption_cap,
    ];
    capabilities.extend(job_caps);

    let status = derive_status(
        &capabilities,
        queue_depth,
        queue_oldest_age_secs,
        dlq_oldest_age_secs,
    );

    let uptime_secs = deps.started_at.elapsed().as_secs();
    let started_at = deps
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

    HealthReport::new(status, deps.version.clone())
        .with_build_sha(Some(deps.build_sha.clone()))
        .with_uptime_secs(uptime_secs)
        .with_queue(queue_depth, queue_oldest_age_secs)
        .with_dlq(dlq_depth, dlq_oldest_age_secs, dlq_by_kind)
        .with_started_at(Some(started_at))
        .with_capabilities(capabilities)
}

/// Age in seconds of the oldest job in `status`, from `lifecycle.created_at` via
/// `list(order=CreatedAsc, limit=1)` (ULID monotone ≡ creation order). `0` if none/error.
async fn oldest_age_secs(store: &Arc<dyn QueueStore + Send + Sync>, status: JobStatus) -> u64 {
    let filter = JobFilter {
        status: Some(status),
        order: JobOrder::CreatedAsc,
        limit: 1,
        ..JobFilter::default()
    };
    store
        .list(filter)
        .await
        .ok()
        .and_then(|mut jobs| jobs.pop())
        .map(|job| {
            let age = Utc::now() - job.lifecycle.created_at;
            u64::try_from(age.num_seconds().max(0)).unwrap_or(0)
        })
        .unwrap_or(0)
}

// ── Probe loop ─────────────────────────────────────────────────────────────────

/// Background probe loop: every `PROBE_INTERVAL_SECS`, run [`probe_once`] and publish the
/// snapshot.
///
/// The first tick fires immediately, so the `starting` snapshot is replaced with a real
/// verdict within one probe pass of boot. `MissedTickBehavior::Skip` prevents catch-up bursts
/// if a pass runs long. The loop is a daemon task (ends at process exit).
pub async fn run_probe_loop(deps: ProbeDeps, snapshot: Arc<HealthSnapshot>) {
    use tokio::time::{MissedTickBehavior, interval};

    let mut ticker = interval(Duration::from_secs(PROBE_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let report = probe_once(&deps).await;
        snapshot.store(Arc::new(report));
    }
}

// ── HTTP server ─────────────────────────────────────────────────────────────────

/// Spawns the worker's unconditional loopback health listener as a detached Tokio task.
///
/// Serves `GET /health` (liveness) and `GET /health/detailed` (superset), both reading the
/// shared [`HealthSnapshot`]. The listener starts **regardless** of `[apalis.metrics]`.
///
/// **Loopback fail-closed**: `/health/detailed` exposes capabilities,
/// `dlq_by_kind` and queue depth — that detail must stay on the host (arbitrage « détail =
/// loopback seul »). A **non-loopback** `bind` (e.g. `0.0.0.0`) is an operator misconfiguration
/// that would leak the detailed surface on the LAN, so it is refused at boot (`Err`), mirroring
/// the server's metrics listener (`gradatum-server::metrics::spawn_metrics_listener` and
/// `gradatum-server::main`, caveat C7). This is the **only** error path.
///
/// **Bind-tolerant otherwise**: a *loopback* address that is malformed
/// or whose port is already taken logs a warning and returns `Ok(())` without spawning — the
/// worker keeps running. An observability port must not gate the consumer; the address itself
/// escaping loopback is a different, security-grade failure.
///
/// # Errors
/// Returns `Err` only when `bind` resolves to a non-loopback address (security fail-closed).
pub fn spawn_health_server(
    config: &HealthServerConfig,
    snapshot: Arc<HealthSnapshot>,
) -> anyhow::Result<()> {
    use std::net::SocketAddr;

    let addr: SocketAddr = match format!("{}:{}", config.bind, config.port).parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                bind = %config.bind,
                port = config.port,
                error = %e,
                "worker /health listener not started — invalid address (worker continues)"
            );
            return Ok(());
        }
    };

    // Fail-closed loopback guard (mirror gradatum-server metrics.rs:851 / main.rs:121, caveat
    // C7): the detailed surface must never escape the host. Unlike a taken port (tolerated
    // below), a non-loopback address is the violation itself — refuse it at boot.
    if !addr.ip().is_loopback() {
        anyhow::bail!(
            "worker /health bind must be loopback (detail = loopback only): address rejected = {addr}"
        );
    }

    let app = Router::new()
        .route("/health", get(liveness_handler))
        .route("/health/detailed", get(detailed_handler))
        .with_state(snapshot);

    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                // Port taken (e.g. co-located default) or refused: degrade gracefully.
                tracing::warn!(
                    addr = %addr,
                    error = %e,
                    "worker /health listener bind failed — /health unavailable, worker continues"
                );
                return;
            }
        };
        tracing::info!(addr = %addr, "worker /health listener started");
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "worker /health listener error");
        }
    });
    Ok(())
}

/// Handles `GET /health` — reads the snapshot with zero I/O and projects it to the minimal
/// [`LivenessReport`]. Always HTTP 200; the body carries the real `status`.
async fn liveness_handler(State(snapshot): State<Arc<HealthSnapshot>>) -> Json<LivenessReport> {
    let snap = snapshot.load_full();
    Json(LivenessReport::new(
        snap.status,
        snap.version.clone(),
        snap.build_sha.clone(),
    ))
}

/// Handles `GET /health/detailed` — reads the snapshot with zero I/O and returns the full
/// [`HealthReport`] superset. Always HTTP 200.
async fn detailed_handler(State(snapshot): State<Arc<HealthSnapshot>>) -> Json<HealthReport> {
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
            host_port_of("http://127.0.0.1:19092/internal").as_deref(),
            Some("127.0.0.1:19092")
        );
        assert_eq!(host_port_of("not-a-url"), None);
        assert_eq!(host_port_of("http://"), None);
    }

    #[test]
    fn health_config_default_port_is_dedicated() {
        // The health port must not collide with the metrics defaults (19091) nor the
        // server/internal ports; 19094 is the chosen dedicated loopback port.
        let cfg = HealthServerConfig::default();
        assert_eq!(cfg.bind, "127.0.0.1");
        assert_eq!(cfg.port, 19094);
    }

    #[test]
    fn health_listener_rejects_non_loopback_bind() {
        // A non-loopback bind would expose /health/detailed (capabilities, dlq_by_kind, queue)
        // on the LAN — fail-closed at boot, no listener spawned.
        let cfg = HealthServerConfig {
            bind: "0.0.0.0".to_string(),
            port: 19094,
        };
        let snapshot = Arc::new(HealthSnapshot::from_pointee(starting_report(
            "9.9.9", "cafebabe",
        )));
        let err = spawn_health_server(&cfg, snapshot).expect_err("non-loopback must fail closed");
        assert!(
            err.to_string().contains("loopback"),
            "expected a loopback fail-closed error, got: {err}"
        );
    }

    #[test]
    fn rollup_unhealthy_when_critical_capability_unreachable() {
        // internal server down (critical) ⇒ unhealthy, regardless of a calm queue.
        let caps = vec![
            cap("internal", true, CapabilityState::Unreachable),
            cap("leadership", true, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Unhealthy);
    }

    #[test]
    fn rollup_unhealthy_when_leadership_stale() {
        let caps = vec![
            cap("internal", true, CapabilityState::Reachable),
            cap("leadership", true, CapabilityState::Unreachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Unhealthy);
    }

    #[test]
    fn rollup_unhealthy_takes_precedence_over_degraded_signals() {
        let caps = vec![cap("internal", true, CapabilityState::Unreachable)];
        assert_eq!(
            derive_status(&caps, QUEUE_DEPTH_DEGRADED + 1, 0, 0),
            HealthStatus::Unhealthy
        );
    }

    #[test]
    fn rollup_degraded_on_consumption_unreachable_only() {
        // consumption is non-critical: a frozen consumer ⇒ degraded, not unhealthy.
        let caps = vec![
            cap("internal", true, CapabilityState::Reachable),
            cap("leadership", true, CapabilityState::Reachable),
            cap("consumption", false, CapabilityState::Unreachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Degraded);
    }

    #[test]
    fn rollup_degraded_on_queue_thresholds() {
        let healthy = vec![
            cap("internal", true, CapabilityState::Reachable),
            cap("leadership", true, CapabilityState::Reachable),
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
            cap("internal", true, CapabilityState::Reachable),
            cap("embed", true, CapabilityState::Reachable),
            cap("curator", false, CapabilityState::NotConfigured),
            cap("leadership", true, CapabilityState::Reachable),
            cap("consumption", false, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Ok);
    }

    #[test]
    fn embed_disabled_is_not_configured_not_a_failure() {
        // Building the report directly: embed disabled ⇒ not_configured ⇒ not unhealthy.
        let caps = vec![
            cap("internal", true, CapabilityState::Reachable),
            cap("embed", true, CapabilityState::NotConfigured),
            cap("leadership", true, CapabilityState::Reachable),
        ];
        assert_eq!(derive_status(&caps, 0, 0, 0), HealthStatus::Ok);
    }

    #[tokio::test]
    async fn probe_embed_disabled_reports_not_configured() {
        let report = probe_embed(false, "http://localhost:9999").await;
        assert_eq!(report.name, "embed");
        assert!(report.critical);
        assert_eq!(report.state, CapabilityState::NotConfigured);
    }

    #[tokio::test]
    async fn probe_curator_heuristic_is_not_configured_noncritical() {
        let report = probe_curator("heuristic", None).await;
        assert_eq!(report.name, "curator");
        assert!(!report.critical);
        assert_eq!(report.state, CapabilityState::NotConfigured);
    }

    #[tokio::test]
    async fn probe_curator_llm_without_url_is_not_configured() {
        let report = probe_curator("openai_compat", None).await;
        assert_eq!(report.state, CapabilityState::NotConfigured);
        assert!(!report.critical);
    }

    #[test]
    fn consumption_idle_empty_queue_is_reachable() {
        // Nothing consumed ever, but the queue is empty ⇒ not a failure.
        let c = derive_consumption(0, 1_000_000, 0, 0);
        assert_eq!(c.state, CapabilityState::Reachable);
        assert!(!c.critical);
        assert_eq!(c.last_probe_ms, None);
    }

    #[test]
    fn consumption_stale_backlog_and_frozen_is_unreachable() {
        // Stale backlog (oldest pending well past the window) AND last consumption far older
        // than the stale window ⇒ frozen consumer.
        let now = 10_000_000_i64;
        let last = now - (CONSUMPTION_STALE_SECS + 60) * 1000;
        let old_backlog = u64::try_from(CONSUMPTION_STALE_SECS + 60).unwrap();
        let c = derive_consumption(last, now, 5, old_backlog);
        assert_eq!(c.state, CapabilityState::Unreachable);
        assert!(c.last_probe_ms.is_some());
    }

    #[test]
    fn consumption_stale_backlog_never_consumed_is_unreachable() {
        // Old backlog, nothing ever consumed ⇒ frozen.
        let old_backlog = u64::try_from(CONSUMPTION_STALE_SECS + 60).unwrap();
        let c = derive_consumption(0, 10_000_000, 5, old_backlog);
        assert_eq!(c.state, CapabilityState::Unreachable);
    }

    #[test]
    fn consumption_stale_backlog_recently_consumed_is_reachable() {
        // Old backlog but a very recent consumption ⇒ still consuming, reachable.
        let now = 10_000_000_i64;
        let last = now - 1_000; // 1 s ago
        let old_backlog = u64::try_from(CONSUMPTION_STALE_SECS + 60).unwrap();
        let c = derive_consumption(last, now, 5, old_backlog);
        assert_eq!(c.state, CapabilityState::Reachable);
        assert_eq!(c.last_probe_ms, Some(1_000));
    }

    #[test]
    fn consumption_fresh_backlog_long_job_is_reachable() {
        // Consumption is stale (nothing consumed for well past the window) but
        // the oldest pending job is RECENT — a legitimately long job with a just-arrived
        // backlog, not a frozen consumer. Must NOT flip to unreachable.
        let now = 10_000_000_i64;
        let last = now - (CONSUMPTION_STALE_SECS + 120) * 1000; // consumption stale
        let c = derive_consumption(last, now, 5, 60); // backlog only 60 s old
        assert_eq!(c.state, CapabilityState::Reachable);
    }

    #[test]
    fn job_health_flags_high_failure_kind_above_min_samples() {
        let mut by_kind = BTreeMap::new();
        by_kind.insert("embed".to_string(), (6_u64, 10_u64)); // 60 % failure, 10 samples
        by_kind.insert("curate".to_string(), (1_u64, 10_u64)); // 10 % failure
        let caps = job_health_capabilities(&by_kind);
        let embed = caps.iter().find(|c| c.name == "jobs:embed").unwrap();
        let curate = caps.iter().find(|c| c.name == "jobs:curate").unwrap();
        assert_eq!(embed.state, CapabilityState::Unreachable);
        assert!(!embed.critical, "job-health capability is non-critical");
        assert_eq!(embed.last_probe_ms, Some(10)); // sample size exposed
        assert_eq!(curate.state, CapabilityState::Reachable);
    }

    #[test]
    fn job_health_does_not_flag_below_min_samples() {
        let mut by_kind = BTreeMap::new();
        by_kind.insert("purge".to_string(), (1_u64, 1_u64)); // 100 % but only 1 sample
        let caps = job_health_capabilities(&by_kind);
        let purge = caps.iter().find(|c| c.name == "jobs:purge").unwrap();
        assert_eq!(
            purge.state,
            CapabilityState::Reachable,
            "a single early failure must not flip to degraded"
        );
    }

    #[test]
    fn starting_report_carries_build_sha() {
        let r = starting_report("9.9.9", "cafebabe");
        assert_eq!(r.status, HealthStatus::Starting);
        assert_eq!(r.version, "9.9.9");
        assert_eq!(r.build_sha.as_deref(), Some("cafebabe"));
    }

    #[test]
    fn curator_probe_target_absent_file_is_offline_default() {
        let (backend, url) = curator_probe_target(Path::new("/nonexistent/server.toml"));
        assert_eq!(backend, "heuristic");
        assert_eq!(url, None);
    }
}
