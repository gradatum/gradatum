//! `MetricSamplePoint` — a time-series metric sample point.
//!
//! Lives in `gradatum-core` because it is a return type of the `IndexStore` trait
//! (parallel to [`crate::scheduled_health::ScheduledTaskHealth`]).

/// A time-series sample read from the `metric_sample` table.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSamplePoint {
    /// Curated series key (e.g. `"mcp_tool_calls.vault_write"`).
    pub series: String,
    /// Epoch-ms timestamp (raw point, or lower bound `MIN(ts_ms)` of the bucket when downsampled).
    pub ts_ms: i64,
    /// Value (raw cumulative, or bucket average when downsampled).
    pub value: f64,
}
