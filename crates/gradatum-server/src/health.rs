//! `GET /health` — unauthenticated diagnostic endpoint.
//!
//! Returns a JSON payload with 10 diagnostic fields.
//! Accessible without authentication.
//!
//! # Status
//!
//! - `"ok"`: nominal state.
//! - `"degraded"`: queue too deep (`depth > 1000`) or too old (`oldest_age_secs > 300`).
//!   HTTP status remains 200 in both cases — operations decides the action.
//!
//! # Field state
//!
//! - `tenant_count` / `locus_count`: real values from the vault registry.
//! - `queue_depth`: real count of `Pending` jobs (`count_jobs_by_status`), 0 if store not wired.
//! - `queue_oldest_age_secs`: age in seconds of the oldest `Pending` job, sourced from
//!   `AppState.job_store` via `QueueStore::list(status=Pending, order=CreatedAsc, limit=1)`.
//!   Both fields share the **same store** (`job_store` → `gradatum_jobs`) — coherent by
//!   construction. Returns 0 if the queue is empty or the store is not wired.
//! - `sqlite_wal_size_bytes`: real size of the WAL file (`AppState.wal_path`),
//!   0 if WAL absent/checkpointed (real measurement). The dashboard surfaces "n/a".
//!
//! # No PII
//!
//! The payload contains no full paths, tokens, IPs, or personal data.
//! `build_sha` is a commit identifier (public), not sensitive data.

use std::time::UNIX_EPOCH;

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use gradatum_core::job::{JobFilter, JobOrder, JobStatus};
use serde::Serialize;

use crate::state::AppState;

/// Payload returned by `GET /health`.
#[derive(Debug, Serialize)]
pub struct HealthPayload {
    /// Service status: `"ok"` | `"degraded"`.
    pub status: &'static str,
    /// Binary version (`CARGO_PKG_VERSION`).
    pub version: &'static str,
    /// Build commit SHA (`BUILD_SHA` env var, or `"unknown"`).
    pub build_sha: &'static str,
    /// Seconds elapsed since process start.
    pub uptime_secs: u64,
    /// Number of known tenants.
    pub tenant_count: u32,
    /// Number of known loci.
    pub locus_count: u32,
    /// Processing queue depth.
    pub queue_depth: u64,
    /// Age of the oldest queued entry in seconds.
    pub queue_oldest_age_secs: u64,
    /// SQLite WAL file size in bytes (0 if absent or inaccessible).
    pub sqlite_wal_size_bytes: u64,
    /// Process start timestamp in RFC 3339 format.
    pub started_at: String,
}

/// Handles `GET /health` — unauthenticated.
///
/// No `TrustContext` check — called directly from the router BEFORE the auth middleware.
///
/// # Side effects
/// Reads the WAL file size (`metadata` syscall) — non-blocking on NVMe/tmpfs.
pub async fn handler(State(s): State<AppState>) -> Json<HealthPayload> {
    // DT-OBS-1 — source unifiée pour queue_depth et queue_oldest_age_secs.
    //
    // Les deux champs lisent maintenant `AppState.job_store` → `gradatum_jobs`
    // (même table, même store). L'ancienne implémentation lisait `queue_depth`
    // depuis `job_store` et `queue_oldest_age_secs` depuis `AppState.queue`
    // (trait `Queue` legacy → table `jobs_v2` drainée par migration 009) —
    // incohérence de sources garantissant un `oldest_age_secs` toujours nul.
    //
    // Cohérence par construction : un seul `COUNT` + un seul `LIST(limit=1)` sur
    // `gradatum_jobs`. Chaque champ dégrade gracieusement à 0 si le store
    // n'est pas câblé (NoopQueueStore).

    // F-37 S1.3 / T12 — queue_depth réel = jobs `Pending` (GROUP BY status).
    let queue_depth: u64 = s
        .job_store
        .count_jobs_by_status()
        .await
        .map(|m| m.get(&JobStatus::Pending).copied().unwrap_or(0))
        .unwrap_or(0);

    // DT-OBS-1 — oldest_age_secs depuis job_store (même source que queue_depth).
    //
    // `list(status=Pending, order=CreatedAsc, limit=1)` retourne le job Pending
    // le plus ancien via `ORDER BY id ASC LIMIT 1` (ULID monotone ≡ ordre temporel).
    // `lifecycle.created_at` est l'horodatage d'insertion — l'âge est la différence
    // avec `Utc::now()`. Retourne 0 si la file est vide ou en cas d'erreur.
    let queue_oldest_age_secs: u64 = {
        let filter = JobFilter {
            status: Some(JobStatus::Pending),
            order: JobOrder::CreatedAsc,
            limit: 1,
            ..JobFilter::default()
        };
        s.job_store
            .list(filter)
            .await
            .ok()
            .and_then(|mut jobs| jobs.pop())
            .map(|job| {
                let age = Utc::now() - job.lifecycle.created_at;
                // `num_seconds()` retourne 0 si négatif (clock skew) — correct.
                u64::try_from(age.num_seconds().max(0)).unwrap_or(0)
            })
            .unwrap_or(0)
    };

    let status = if queue_depth > 1_000 || queue_oldest_age_secs > 300 {
        "degraded"
    } else {
        "ok"
    };

    let uptime_secs = s.started_at.elapsed().as_secs();

    // F-37 S1.3 / T12 — taille WAL réelle depuis `AppState.wal_path` (câblé par
    // `with_search_path`). `metadata` échoue si WAL absent (checkpoint) → 0 (qui
    // signifie ici "WAL vide/checkpointé", mesure réelle, plus un stub menteur).
    // Le champ `/health` reste `u64` (rétrocompat wire). La distinction "n/a"
    // honnête (`Option`) est portée par le dashboard `/api/v1/dashboard`.
    let sqlite_wal_size_bytes: u64 = s
        .wal_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    // Conversion SystemTime → RFC3339 sans panic.
    // `duration_since(UNIX_EPOCH)` échoue uniquement si la clock système est avant 1970 — impossible.
    let started_at = s
        .started_at_systime
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| {
            DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                .map(|dt| dt.to_rfc3339())
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());

    // T2 P2.0c : vrais comptages depuis le registry vault (méthodes async).
    // Fallback à 0 si le vault n'est pas encore initialisé ou inaccessible.
    let tenant_count = s.vault.tenant_count().await.unwrap_or(0);
    let locus_count = s.vault.locus_count().await.unwrap_or(0);

    Json(HealthPayload {
        status,
        version: s.version,
        build_sha: s.build_sha,
        uptime_secs,
        tenant_count,
        locus_count,
        queue_depth,
        queue_oldest_age_secs,
        sqlite_wal_size_bytes,
        started_at,
    })
}
