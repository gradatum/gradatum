//! Types for recurring-task health monitoring (`scheduled_task_health`).
//!
//! Consumed by:
//! - `gradatum-index::SqliteIndex` (implementation) via the `IndexStore` trait
//! - `gradatum-server` (background scheduler + `/api/v1/system/scheduled` endpoint)

use serde::{Deserialize, Serialize};

/// Outcome of a single recurring-task tick.
///
/// Passed to `IndexStore::record_task_run` after each execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The tick completed without error.
    Ok,
    /// The tick produced an error (non-fatal — the task continues).
    Error,
}

/// Aggregated health state of a recurring task.
///
/// Returned by `IndexStore::list_scheduled_health`.
/// The `errors_24h` field is a sliding window computed at query time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTaskHealth {
    /// Stable task name (constant, e.g. `"telemetry-flush"`).
    pub task_name: String,
    /// Epoch-ms timestamp of the last run (`None` if the task has never run).
    pub last_run_ms: Option<i64>,
    /// Outcome of the last tick: `"ok"` | `"error"` | `None` if never run.
    pub last_outcome: Option<String>,
    /// Duration of the last tick in milliseconds.
    pub last_duration_ms: Option<i64>,
    /// Error message of the last failing tick (`None` if the last tick was `Ok`).
    pub last_error: Option<String>,
    /// Total number of ticks since process boot.
    pub run_count: i64,
    /// Number of errors in the last 24 hours (sliding window: `now − 86_400_000 ms`).
    pub errors_24h: i64,
}
