//! Shared wire contract for the fleet service registry.
//!
//! Defined **once** here so the heartbeat producers (each binary of the fleet — server,
//! worker, gateway, engines) and the read consumers (fleet dashboards, `/registry`) agree
//! on a single shape.
//!
//! - [`HeartbeatRequest`] — what a fleet member POSTs to announce it is alive. Consumed by
//!   the ingress handler (P6b) and validated at the store boundary (allowlist + field caps).
//! - [`RegistryEntry`] / [`RegistryReport`] — the read projection served by `GET /registry`
//!   on the loopback side-channel: one entry per **living** service (expired ones omitted).
//!
//! ## Forward/backward compatibility
//!
//! `#[non_exhaustive]` on the public structs lets later phases add fields without a SemVer
//! break; the constructors below exist because a `#[non_exhaustive]` struct cannot be built
//! with literal syntax from another crate. Additive fields carry `#[serde(default)]`, so an
//! older or partial payload deserializes cleanly (missing field → default).
//!
//! The `service` name is kept a bare `String` on the wire (the allowlist that closes the set
//! of valid names is a **server-side** concern, enforced at the store boundary — the DTO
//! carries the value, it does not police it).

use serde::{Deserialize, Serialize};

/// Heartbeat announced by one fleet member to the registry.
///
/// A member re-POSTs this at a fixed cadence (~15 s); the store refreshes the `last_seen`
/// clock on each receipt and lazily drops entries older than its TTL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HeartbeatRequest {
    /// Fleet service name (e.g. `"server"`, `"worker"`, `"engine-agent-main"`). The set of
    /// valid names is closed and enforced by the receiving store, not by this DTO.
    pub service: String,
    /// Binary version of the announcing member (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Build commit SHA of the announcing member (public). `None` when built without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// OS process id of the announcing member (`std::process::id()`).
    pub pid: u32,
}

impl HeartbeatRequest {
    /// Builds a heartbeat request.
    #[must_use]
    pub fn new(service: String, version: String, build_sha: Option<String>, pid: u32) -> Self {
        Self {
            service,
            version,
            build_sha,
            pid,
        }
    }
}

/// One living service in the registry read projection.
///
/// Only members whose last heartbeat is within the store TTL appear here; expired ones are
/// omitted by the read path (lazy expiration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistryEntry {
    /// Fleet service name (allowlisted at registration).
    pub service: String,
    /// Binary version last announced by this member.
    pub version: String,
    /// Build commit SHA last announced (public). `None` when the member reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// OS process id last announced by this member.
    pub pid: u32,
    /// Wall-clock instant of the last heartbeat, epoch milliseconds. Informational only —
    /// liveness is decided by the store's monotonic TTL, not by this field.
    pub last_seen_ms: i64,
}

impl RegistryEntry {
    /// Builds a registry entry.
    #[must_use]
    pub fn new(
        service: String,
        version: String,
        build_sha: Option<String>,
        pid: u32,
        last_seen_ms: i64,
    ) -> Self {
        Self {
            service,
            version,
            build_sha,
            pid,
            last_seen_ms,
        }
    }
}

/// The registry read projection served by `GET /registry` (loopback side-channel).
///
/// Carries the living entries only, sorted by service name for a deterministic body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegistryReport {
    /// Living services, one entry each, ordered by service name.
    #[serde(default)]
    pub entries: Vec<RegistryEntry>,
}

impl RegistryReport {
    /// Builds a registry report.
    #[must_use]
    pub fn new(entries: Vec<RegistryEntry>) -> Self {
        Self { entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_roundtrips_without_build_sha() {
        let hb = HeartbeatRequest::new("worker".into(), "2.3.0".into(), None, 4242);
        let json = serde_json::to_string(&hb).expect("serialize");
        // `build_sha: None` is omitted from the wire.
        assert!(
            !json.contains("build_sha"),
            "None build_sha must be omitted: {json}"
        );
        let back: HeartbeatRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hb);
    }

    #[test]
    fn report_deserializes_from_empty_object() {
        // A payload missing `entries` must default to an empty list (additive-field guard).
        let r: RegistryReport = serde_json::from_str("{}").expect("deserialize empty");
        assert!(r.entries.is_empty());
    }
}
