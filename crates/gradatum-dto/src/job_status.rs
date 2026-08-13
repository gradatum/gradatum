//! DTO for the `job_status` MCP tool.
//!
//! Wire contract: the request ([`JobStatusRequest`] — a single `job_id`) is used to
//! auto-derive the `inputSchema` of the `job_status` MCP tool (SSOT `mcp_tool_schema`).
//!
//! The **response** shape (`JobStatusView`) is defined server-side in `gradatum-server`,
//! because it is distilled from the internal [`gradatum_core::JobRecord`] — the DTO crate
//! deliberately carries only the request contract, mirroring `JobListResponse` /
//! `CreateJobResponse` which also live server-side.

use serde::Deserialize;

/// Request for the `job_status` MCP tool.
///
/// Carries the ULID of a job previously enqueued by an async endpoint (e.g. `vault_write`,
/// which replies `202 { job_id, poll_url }`). The server validates that `job_id` is a
/// well-formed ULID (invalid → 400) before any lookup — parse-don't-validate at the
/// boundary.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct JobStatusRequest {
    /// ULID of the job to inspect — the `job_id` returned by the enqueuing endpoint.
    ///
    /// Crockford base32, 26 chars (e.g. `01KYX5FBTXQSG37BQEYP0RF47B`).
    pub job_id: String,
}

impl JobStatusRequest {
    /// Constructs a job-status request for the given `job_id`.
    #[must_use]
    pub fn new(job_id: String) -> Self {
        Self { job_id }
    }
}
