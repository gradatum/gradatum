//! gradatum-worker — library API for integration tests.
//!
//! Exposes internal modules to allow testing from `tests/`.
//!
//! `curator_loader` and `leader` are internal service plumbing exposed only for this
//! crate's integration tests, but the active binary uses both: `main.rs` calls
//! `curator_loader::build_curator_pipeline` to build the curator, and
//! `leader::LeaderElection::try_acquire` is a **boot gate** (a non-leader exits
//! cleanly). Do not treat them as dead code.
//!
//! These modules are internal service plumbing, exposed only for this crate's
//! own integration tests. They are hidden from the rendered documentation and
//! are **not** a stable public API (this crate is a service binary, not a
//! reusable library).

// Modules exposed for this crate's integration tests
// (internal service plumbing — not a stable public API)
#[allow(dead_code)]
#[doc(hidden)]
pub mod curator_loader;
#[allow(dead_code)]
#[doc(hidden)]
pub mod leader;

#[doc(hidden)]
pub mod apalis_backend;
#[doc(hidden)]
pub mod apalis_handlers;
#[doc(hidden)]
pub mod config_health;
#[doc(hidden)]
pub mod distill_cluster;
#[doc(hidden)]
pub mod internal_client;
#[doc(hidden)]
pub mod metrics;
#[doc(hidden)]
pub mod monitor;
#[doc(hidden)]
pub mod quality_score;
#[doc(hidden)]
pub mod queue_path;
#[doc(hidden)]
pub mod schedules;
#[doc(hidden)]
pub mod wikilinks;

// ── Re-exports for integration tests ─────────────────────────────────────────

/// Builds the [`gradatum_curator::CuratorPipeline`] from the `[curator]` section
/// of the server TOML file. Exposed for integration tests.
///
/// Delegates to [`curator_loader::build_curator_pipeline`].
#[doc(hidden)]
pub use curator_loader::build_curator_pipeline;

/// Curator config deserialized from the server TOML.
/// Exposed for integration tests verifying gating-field propagation.
#[doc(hidden)]
pub use curator_loader::WorkerCuratorConfig;

/// Names the deprecated `[curator] llm_review_*` keys present in a config.
/// Exposed for integration tests asserting the deprecation contract.
#[doc(hidden)]
pub use curator_loader::deprecated_review_override_keys;

/// Apalis config (workers + schedules).
#[doc(hidden)]
pub use monitor::ApalisConfig;

/// Prometheus registry shared across workers.
#[doc(hidden)]
pub use metrics::WorkerMetrics;
