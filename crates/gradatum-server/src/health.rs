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
//! - `queue_oldest_age_secs`: 0 (deferred — no dedicated method on `QueueStore`).
//! - `sqlite_wal_size_bytes`: real size of the WAL file (`AppState.wal_path`),
//!   0 if WAL absent/checkpointed (real measurement). The dashboard surfaces "n/a".
//!
//! # No PII
//!
//! The payload contains no full paths, tokens, IPs, or personal data.
//! `build_sha` is a commit identifier (public), not sensitive data.

use std::time::UNIX_EPOCH;

use axum::{extract::State, Json};
use chrono::{DateTime, Utc};
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
    // F-37 S1.3 / T12 — queue_depth réel = jobs `Pending` (GROUP BY status).
    // Dégrade gracieusement à 0 si le store n'est pas câblé (placeholder dev/test).
    let queue_depth: u64 = s
        .job_store
        .count_jobs_by_status()
        .await
        .map(|m| {
            m.get(&gradatum_core::job::JobStatus::Pending)
                .copied()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    // queue_oldest_age_secs : pas de méthode dédiée sur QueueStore (trait F-16) →
    // conservé à 0 (différé : nécessiterait une requête MIN(created_at) sur la file).
    let queue_oldest_age_secs: u64 = 0;

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
