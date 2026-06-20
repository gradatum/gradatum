//! gradatum-worker — library API for integration tests.
//!
//! Exposes internal modules to allow testing from `tests/`.
//!
//! The `dispatch`, `curator_loader`, and `leader` modules are retained for
//! compatibility with existing integration tests. They are no longer used
//! directly by the active binary, which uses the Apalis Monitor.

// Modules retained for integration-test backward compatibility
// (not used by the active binary — dispatcher replaced by Apalis Monitor)
#[allow(dead_code)]
pub mod curator_loader;
#[allow(dead_code)]
pub mod dispatch;
#[allow(dead_code)]
pub mod leader;

pub mod apalis_backend;
pub mod apalis_handlers;
pub mod distill_cluster;
pub mod internal_client;
pub mod metrics;
pub mod monitor;
pub mod schedules;
pub mod wikilinks;

// ── Re-exports for integration tests ─────────────────────────────────────────

/// Builds the [`gradatum_curator::CuratorPipeline`] from the `[curator]` section
/// of the server TOML file. Exposed for integration tests.
///
/// Delegates to [`curator_loader::build_curator_pipeline`].
pub use curator_loader::build_curator_pipeline;

/// Curator config deserialized from the server TOML.
/// Exposed for integration tests verifying gating-field propagation.
pub use curator_loader::WorkerCuratorConfig;

/// Apalis config (workers + schedules).
pub use monitor::ApalisConfig;

/// Prometheus registry shared across workers.
pub use metrics::WorkerMetrics;
