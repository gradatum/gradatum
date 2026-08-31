//! `GET /health` — unauthenticated diagnostic endpoint.
//!
//! Returns a JSON payload with 12 diagnostic fields.
//! Accessible without authentication.
//!
//! # Status
//!
//! - `"ok"`: nominal state.
//! - `"degraded"`: queue too deep (`depth > 1000`), too old (`oldest_age_secs > 300`),
//!   or a dead-letter job left unattended too long (`dlq_oldest_age_secs > `
//!   `DLQ_MAX_AGE_SECS`). HTTP status remains 200 in every case — operations decides
//!   the action.
//!
//! # Field state
//!
//! - `tenant_count` / `locus_count`: real values from the vault registry.
//! - `queue_depth`: real count of `Pending` jobs (`count_jobs_by_status`), 0 if store not wired.
//! - `queue_oldest_age_secs`: age in seconds of the oldest `Pending` job, sourced from
//!   `AppState.job_store` via `QueueStore::list(status=Pending, order=CreatedAsc, limit=1)`.
//!   Both fields share the **same store** (`job_store` → `gradatum_jobs`) — coherent by
//!   construction. Returns 0 if the queue is empty or the store is not wired.
//! - `dlq_depth`: real count of `DLQ` (dead-letter) jobs — read from the **same**
//!   `count_jobs_by_status` map as `queue_depth` (one query, no extra round-trip). A DLQ job
//!   is a job whose retries were exhausted: a terminal, silent failure. Exposing the count
//!   makes it observable (F-204/F-206). 0 if the store is not wired.
//! - `dlq_oldest_age_secs`: age in seconds of the oldest `DLQ` job, computed from
//!   `lifecycle.created_at` via `QueueStore::list(status=DLQ, order=CreatedAsc, limit=1)` —
//!   the same proven path as `queue_oldest_age_secs`. Age is measured from creation (not from
//!   the moment of DLQ entry): for a job that dies seconds after enqueue the two are within
//!   seconds, and creation age is what the F-204/F-206 audit itself measured ("57 days").
//!   This age — not the raw count — drives the `degraded` status, because a slowly rising
//!   counter is invisible while a threshold on staleness is not. 0 if there is no DLQ job.
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

/// Age threshold, in seconds, past which the **oldest** dead-letter job flips
/// `/health` to `"degraded"`.
///
/// A DLQ entry is a terminal failure (retries exhausted) awaiting operator triage
/// (replay or prune). A freshly dead job is expected transiently and must not raise
/// the alarm on every poll; a job still in the DLQ after a full operating day is
/// *forgotten* — which is exactly the F-204/F-206 defect (jobs rotted 10 and 57 days
/// unnoticed). 24 h is long enough to avoid flapping during active triage, short enough
/// that a forgotten job surfaces within a day.
///
/// This is a safety cap, not a per-caller runtime parameter (no caller passes a
/// different value today), so it is a documented constant rather than a config field —
/// consistent with the existing `queue_depth`/`queue_oldest_age_secs` thresholds.
const DLQ_MAX_AGE_SECS: u64 = 24 * 60 * 60;

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
    /// Dead-letter queue depth — jobs whose retries were exhausted (terminal, silent
    /// failures). Exposed so a rising backlog of dead work is observable (F-204/F-206).
    pub dlq_depth: u64,
    /// Age in seconds of the oldest dead-letter job (from `lifecycle.created_at`).
    /// Drives the `degraded` status past `DLQ_MAX_AGE_SECS` — staleness, not count,
    /// is the signal that a dead job has been forgotten.
    pub dlq_oldest_age_secs: u64,
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
    // Les deux champs lisent `AppState.job_store` → `gradatum_jobs` (même table,
    // même store). L'ancien second bras lisait `queue_oldest_age_secs` depuis
    // `AppState.queue` (trait `Queue` legacy → table `jobs_v2` drainée par
    // migration 009) — incohérence de sources garantissant un `oldest_age_secs`
    // toujours nul. Le champ `queue` et la table `jobs_v2` sont supprimés en
    // 2.1.0 (F-177) : il ne reste qu'une seule source.
    //
    // Cohérence par construction : un seul `COUNT` + un seul `LIST(limit=1)` sur
    // `gradatum_jobs`. Chaque champ dégrade gracieusement à 0 si le store
    // n'est pas câblé (NoopQueueStore).

    // F-37 S1.3 / T12 — queue_depth réel = jobs `Pending` (GROUP BY status).
    // F-204/F-206 — un SEUL appel `count_jobs_by_status` alimente `queue_depth` ET
    // `dlq_depth` : le `GROUP BY status` renvoie tous les statuts présents (DLQ inclus),
    // donc aucune requête supplémentaire (pas de N+1). Statut absent de la map ⇒ 0.
    let status_counts = s
        .job_store
        .count_jobs_by_status(None)
        .await
        .unwrap_or_default();
    let queue_depth: u64 = status_counts.get(&JobStatus::Pending).copied().unwrap_or(0);
    let dlq_depth: u64 = status_counts.get(&JobStatus::DLQ).copied().unwrap_or(0);

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

    // F-204/F-206 — âge du plus vieux job DLQ, même chemin prouvé que
    // `queue_oldest_age_secs` : `list(status=DLQ, order=CreatedAsc, limit=1)` renvoie le
    // plus ancien via `ORDER BY id ASC LIMIT 1` (ULID monotone ≡ ordre de création).
    // `lifecycle.created_at` est l'horodatage d'insertion ; l'âge est la différence avec
    // `Utc::now()`. 0 si la DLQ est vide, si la file n'est pas câblée, ou en cas d'erreur.
    let dlq_oldest_age_secs: u64 = {
        let filter = JobFilter {
            status: Some(JobStatus::DLQ),
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
                // `num_seconds()` négatif (clock skew) borné à 0 — correct.
                u64::try_from(age.num_seconds().max(0)).unwrap_or(0)
            })
            .unwrap_or(0)
    };

    // F-204/F-206 — la DLQ contribue au statut par l'ANCIENNETÉ, pas par le compte : un
    // job mort frais est attendu transitoirement (triage à venir), un job mort depuis
    // > DLQ_MAX_AGE_SECS est oublié. `dlq_oldest_age_secs = 0` (DLQ vide) ne déclenche
    // jamais, le seuil étant strictement positif.
    let status = if queue_depth > 1_000
        || queue_oldest_age_secs > 300
        || dlq_oldest_age_secs > DLQ_MAX_AGE_SECS
    {
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
        dlq_depth,
        dlq_oldest_age_secs,
        sqlite_wal_size_bytes,
        started_at,
    })
}
