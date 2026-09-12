//! Shared wire contract for `GET /health` — the typed foundation
//! consumed by every `/health` surface.
//!
//! Defined **once** here so producers (server, worker, engine) and consumers (fleet
//! checks, dashboards, the service registry) agree on a single shape. Two report levels:
//!
//! - [`LivenessReport`] — the minimal, always-public liveness block (`status`, `version`,
//!   `build_sha`). Suitable for an unauthenticated probe.
//! - [`HealthReport`] — the detailed, loopback/privileged superset. Mirrors the fields of
//!   the current `gradatum-server` `/health` payload and adds the forward blocks
//!   (`capabilities`, per-kind DLQ breakdown).
//!
//! ## Forward/backward compatibility — two independent mechanisms
//!
//! 1. **Deserialization catch-all** ([`HealthStatus::Unknown`], [`CapabilityState::Unknown`]):
//!    a pinned external consumer parsing a `status` string it does not know **must not** fail
//!    to deserialize. `#[serde(other)]` maps any unknown wire value to `Unknown`. This is the
//!    load-bearing guard: `#[non_exhaustive]` protects the **constructor** site (internal, new
//!    variants) but does **nothing** for deserialization — a producer emitting `"unhealthy"` to
//!    a consumer that only knew `"ok"`/`"degraded"` would hard-error without the catch-all.
//! 2. **Additive fields** (`#[serde(default)]` on the numeric/optional/collection fields):
//!    an older payload missing a field deserializes with that field at its default, so
//!    [`HealthReport`] parses the current `2.2.0` server payload unchanged.
//!
//! `#[non_exhaustive]` on the public structs/enums lets later phases add variants/fields
//! without a SemVer break; the constructors below exist because a `#[non_exhaustive]` struct
//! cannot be built with literal syntax from another crate (the P2 producer lives in
//! `gradatum-server`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Overall service health, serialized as a stable lowercase string.
///
/// Wire values: `"starting"`, `"ok"`, `"degraded"`, `"unhealthy"`. Any other string
/// deserializes to [`HealthStatus::Unknown`] instead of failing — a pinned consumer
/// survives a producer that emits a status it has never seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// Process is initializing and not yet ready to serve.
    Starting,
    /// Nominal — the service is healthy.
    Ok,
    /// Serving, but a threshold was crossed (deep/old queue, forgotten DLQ job…).
    /// Operations decides the action; the HTTP status typically stays `200`.
    Degraded,
    /// The service cannot fulfill its contract (a critical capability is down).
    Unhealthy,
    /// Catch-all for any wire value not known to this build. Never produced
    /// intentionally — it exists so deserialization of a newer producer's status
    /// cannot hard-fail on a pinned consumer.
    #[serde(other)]
    Unknown,
}

/// Reachability of a single declared capability, serialized as a stable lowercase string.
///
/// Wire values: `"reachable"`, `"unreachable"`, `"not_configured"`. Any other string
/// deserializes to [`CapabilityState::Unknown`] (same forward-compat guarantee as
/// [`HealthStatus`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilityState {
    /// The dependency responded to its probe within the deadline.
    Reachable,
    /// The dependency was probed but did not respond (down, timed out, refused).
    Unreachable,
    /// The dependency is not configured for this instance — absence is expected,
    /// not a failure.
    NotConfigured,
    /// Catch-all for any wire value not known to this build.
    #[serde(other)]
    Unknown,
}

/// Health of one declared dependency of the service (a "capability").
///
/// A `critical` capability that is [`CapabilityState::Unreachable`] is what drives the
/// overall status toward [`HealthStatus::Unhealthy`]; a non-critical one contributes at
/// most to [`HealthStatus::Degraded`]. The status-derivation logic lives with the producer
/// (P2+), not in this DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CapabilityReport {
    /// Stable identifier of the capability (e.g. `"vault"`, `"queue"`, `"engine"`).
    /// Kept a free `String` — the set of capability names is producer-defined and open.
    pub name: String,
    /// Whether this capability is required for the service to be considered healthy.
    pub critical: bool,
    /// Result of the last probe of this capability.
    pub state: CapabilityState,
    /// Duration of the last probe in milliseconds, when one was performed.
    /// `None` when the capability was never probed (e.g. `not_configured`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_ms: Option<u64>,
}

impl CapabilityReport {
    /// Builds a capability report.
    #[must_use]
    pub fn new(
        name: String,
        critical: bool,
        state: CapabilityState,
        last_probe_ms: Option<u64>,
    ) -> Self {
        Self {
            name,
            critical,
            state,
            last_probe_ms,
        }
    }
}

/// Minimal liveness block — the always-public subset of `/health`.
///
/// Safe to return unauthenticated: it carries no queue/DLQ internals, only the coarse
/// `status`, the running `version`, and the (public) `build_sha`.
///
/// Deserializes from the full detailed payload too: unknown fields are ignored by serde,
/// so parsing a complete `/health` body into a `LivenessReport` keeps just these three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LivenessReport {
    /// Coarse-grained service status.
    pub status: HealthStatus,
    /// Binary version (`CARGO_PKG_VERSION` of the producer).
    pub version: String,
    /// Build commit SHA — a public commit identifier, not sensitive data. `None` when
    /// the producer was built without a `BUILD_SHA`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
}

impl LivenessReport {
    /// Builds a liveness report.
    #[must_use]
    pub fn new(status: HealthStatus, version: String, build_sha: Option<String>) -> Self {
        Self {
            status,
            version,
            build_sha,
        }
    }
}

/// Detailed health block — the loopback/privileged superset of `/health`.
///
/// Mirrors the fields of the current `gradatum-server` `/health` payload (so it round-trips
/// the `2.2.0` wire) and adds the forward blocks: the per-capability [`CapabilityReport`]
/// list and the per-kind DLQ breakdown (`dlq_by_kind`).
///
/// Every additive field carries `#[serde(default)]`, so an older or partial payload
/// deserializes cleanly (missing field → default). Only `status` and `version` are required.
///
/// Being `#[non_exhaustive]`, this struct is constructed from other crates via
/// [`HealthReport::new`] plus the `with_*` builder methods, never with literal syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HealthReport {
    /// Overall service status.
    pub status: HealthStatus,
    /// Binary version (`CARGO_PKG_VERSION` of the producer).
    pub version: String,
    /// Build commit SHA (public). `None` when built without a `BUILD_SHA`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// Seconds elapsed since process start.
    #[serde(default)]
    pub uptime_secs: u64,
    /// Number of known tenants in the vault registry.
    #[serde(default)]
    pub tenant_count: u32,
    /// Number of known loci in the vault registry.
    #[serde(default)]
    pub locus_count: u32,
    /// Processing queue depth (`Pending` jobs).
    #[serde(default)]
    pub queue_depth: u64,
    /// Age in seconds of the oldest queued entry.
    #[serde(default)]
    pub queue_oldest_age_secs: u64,
    /// Dead-letter queue depth — jobs whose retries were exhausted (terminal, silent
    /// failures).
    #[serde(default)]
    pub dlq_depth: u64,
    /// Age in seconds of the oldest dead-letter job. Staleness (not count) is the signal
    /// that a dead job has been forgotten.
    #[serde(default)]
    pub dlq_oldest_age_secs: u64,
    /// Per-kind dead-letter breakdown (`job kind` → count). Additive forward field; empty
    /// when the producer does not (yet) expose it. `BTreeMap` for deterministic ordering
    /// of the serialized output.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dlq_by_kind: BTreeMap<String, u64>,
    /// SQLite WAL file size in bytes (`0` if absent or checkpointed).
    #[serde(default)]
    pub sqlite_wal_size_bytes: u64,
    /// Process start timestamp in RFC 3339 format. `None` when the producer does not
    /// expose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Per-capability health of the service's declared dependencies. Additive forward
    /// field; empty when the producer does not (yet) expose capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityReport>,
}

impl HealthReport {
    /// Builds a detailed report with only the two required fields set; every other field
    /// defaults (zero counts, empty collections, no timestamp). Populate the rest with the
    /// `with_*` methods.
    #[must_use]
    pub fn new(status: HealthStatus, version: String) -> Self {
        Self {
            status,
            version,
            build_sha: None,
            uptime_secs: 0,
            tenant_count: 0,
            locus_count: 0,
            queue_depth: 0,
            queue_oldest_age_secs: 0,
            dlq_depth: 0,
            dlq_oldest_age_secs: 0,
            dlq_by_kind: BTreeMap::new(),
            sqlite_wal_size_bytes: 0,
            started_at: None,
            capabilities: Vec::new(),
        }
    }

    /// Sets the (public) build commit SHA.
    #[must_use]
    pub fn with_build_sha(mut self, build_sha: Option<String>) -> Self {
        self.build_sha = build_sha;
        self
    }

    /// Sets the process uptime in seconds.
    #[must_use]
    pub fn with_uptime_secs(mut self, uptime_secs: u64) -> Self {
        self.uptime_secs = uptime_secs;
        self
    }

    /// Sets the vault registry counts (tenants, loci) together — they share a source.
    #[must_use]
    pub fn with_counts(mut self, tenant_count: u32, locus_count: u32) -> Self {
        self.tenant_count = tenant_count;
        self.locus_count = locus_count;
        self
    }

    /// Sets the processing-queue block (depth + oldest age) — computed from one source.
    #[must_use]
    pub fn with_queue(mut self, depth: u64, oldest_age_secs: u64) -> Self {
        self.queue_depth = depth;
        self.queue_oldest_age_secs = oldest_age_secs;
        self
    }

    /// Sets the dead-letter block (depth + oldest age + per-kind breakdown) together.
    #[must_use]
    pub fn with_dlq(
        mut self,
        depth: u64,
        oldest_age_secs: u64,
        by_kind: BTreeMap<String, u64>,
    ) -> Self {
        self.dlq_depth = depth;
        self.dlq_oldest_age_secs = oldest_age_secs;
        self.dlq_by_kind = by_kind;
        self
    }

    /// Sets the SQLite WAL file size in bytes.
    #[must_use]
    pub fn with_sqlite_wal_size_bytes(mut self, bytes: u64) -> Self {
        self.sqlite_wal_size_bytes = bytes;
        self
    }

    /// Sets the process start timestamp (RFC 3339).
    #[must_use]
    pub fn with_started_at(mut self, started_at: Option<String>) -> Self {
        self.started_at = started_at;
        self
    }

    /// Sets the per-capability reports.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<CapabilityReport>) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_status_serializes_to_stable_lowercase() {
        assert_eq!(
            serde_json::to_string(&HealthStatus::Starting).unwrap(),
            "\"starting\""
        );
        assert_eq!(serde_json::to_string(&HealthStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&HealthStatus::Degraded).unwrap(),
            "\"degraded\""
        );
        assert_eq!(
            serde_json::to_string(&HealthStatus::Unhealthy).unwrap(),
            "\"unhealthy\""
        );
    }

    #[test]
    fn health_status_round_trips_each_known_variant() {
        for (wire, variant) in [
            ("starting", HealthStatus::Starting),
            ("ok", HealthStatus::Ok),
            ("degraded", HealthStatus::Degraded),
            ("unhealthy", HealthStatus::Unhealthy),
        ] {
            let json = format!("\"{wire}\"");
            let parsed: HealthStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant, "wire {wire} must map to its variant");
        }
    }

    #[test]
    fn health_status_unknown_literal_maps_to_unknown() {
        // A pinned consumer must not hard-fail on a status it never saw.
        let parsed: HealthStatus = serde_json::from_str("\"quantum_flux\"").unwrap();
        assert_eq!(parsed, HealthStatus::Unknown);
    }

    #[test]
    fn capability_state_unknown_literal_maps_to_unknown() {
        let parsed: CapabilityState = serde_json::from_str("\"exploding\"").unwrap();
        assert_eq!(parsed, CapabilityState::Unknown);
    }

    #[test]
    fn capability_report_round_trips() {
        let report = CapabilityReport::new(
            "engine".to_string(),
            true,
            CapabilityState::Reachable,
            Some(12),
        );
        let json = serde_json::to_string(&report).unwrap();
        let back: CapabilityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn capability_report_omits_absent_probe_duration() {
        let report = CapabilityReport::new(
            "smtp".to_string(),
            false,
            CapabilityState::NotConfigured,
            None,
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("last_probe_ms"),
            "None probe must be skipped"
        );
    }

    /// A real `GET /health` body as served by `gradatum-server` at `2.2.0`.
    const HEALTH_PAYLOAD_2_2_0: &str = r#"{
        "status": "ok",
        "version": "2.2.0",
        "build_sha": "5aaecc02",
        "uptime_secs": 86400,
        "tenant_count": 3,
        "locus_count": 7,
        "queue_depth": 0,
        "queue_oldest_age_secs": 0,
        "dlq_depth": 0,
        "dlq_oldest_age_secs": 0,
        "sqlite_wal_size_bytes": 4096,
        "started_at": "2026-09-05T00:00:00+00:00"
    }"#;

    #[test]
    fn detailed_report_deserializes_current_2_2_0_payload() {
        let report: HealthReport = serde_json::from_str(HEALTH_PAYLOAD_2_2_0).unwrap();
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(report.version, "2.2.0");
        assert_eq!(report.build_sha.as_deref(), Some("5aaecc02"));
        assert_eq!(report.uptime_secs, 86_400);
        assert_eq!(report.tenant_count, 3);
        assert_eq!(report.locus_count, 7);
        assert_eq!(report.dlq_depth, 0);
        assert_eq!(report.sqlite_wal_size_bytes, 4096);
        assert_eq!(
            report.started_at.as_deref(),
            Some("2026-09-05T00:00:00+00:00")
        );
        // Additive blocks absent from the 2.2.0 wire → empty, no deserialization error.
        assert!(report.dlq_by_kind.is_empty());
        assert!(report.capabilities.is_empty());
    }

    #[test]
    fn liveness_report_deserializes_full_payload_ignoring_extras() {
        // Unknown fields are ignored: parsing the full body keeps just the liveness subset.
        let report: LivenessReport = serde_json::from_str(HEALTH_PAYLOAD_2_2_0).unwrap();
        assert_eq!(report.status, HealthStatus::Ok);
        assert_eq!(report.version, "2.2.0");
        assert_eq!(report.build_sha.as_deref(), Some("5aaecc02"));
    }

    #[test]
    fn detailed_report_builder_populates_forward_blocks() {
        let mut by_kind = BTreeMap::new();
        by_kind.insert("embed".to_string(), 2_u64);
        let report = HealthReport::new(HealthStatus::Degraded, "2.3.0".to_string())
            .with_build_sha(Some("deadbeef".to_string()))
            .with_uptime_secs(10)
            .with_counts(1, 2)
            .with_queue(5, 30)
            .with_dlq(2, 90, by_kind.clone())
            .with_sqlite_wal_size_bytes(8192)
            .with_started_at(Some("2026-09-06T00:00:00+00:00".to_string()))
            .with_capabilities(vec![CapabilityReport::new(
                "vault".to_string(),
                true,
                CapabilityState::Reachable,
                Some(3),
            )]);

        let json = serde_json::to_string(&report).unwrap();
        let back: HealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
        assert_eq!(back.dlq_by_kind, by_kind);
        assert_eq!(back.capabilities.len(), 1);
        assert_eq!(back.status, HealthStatus::Degraded);
    }

    #[test]
    fn detailed_report_unknown_status_survives_round_trip() {
        // A future producer status the consumer does not know must parse as Unknown.
        let json = r#"{"status":"maintenance","version":"9.9.9"}"#;
        let report: HealthReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.status, HealthStatus::Unknown);
        assert_eq!(report.version, "9.9.9");
    }
}
