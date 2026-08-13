//! # gradatum-engine
//!
//! Rust supervisor for a native `llama-server` subprocess — axum OpenAI-compatible.
//!
//! ## Architecture
//!
//! `gradatum-engine` is a supervisor that:
//! 1. **spawns** `llama-server` via `tokio::process::Command` (never via a shell).
//! 2. **waits until ready**: polls `GET /health` on the child until HTTP 200.
//! 3. **proxies**: `/v1/chat/completions` and `/v1/embeddings` handlers → reqwest to the child.
//! 4. **supervises**: bounded restart-on-failure + graceful shutdown.
//!
//! The engine no longer loads any model itself (zero VRAM/RAM duplication).
//!
//! ## Stability
//!
//! `2.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible additions
//! only within `2.x`, breaking changes deferred to the next major. See
//! [RELEASE-POLICY.md](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Feature gates
//!
//! - `serve`: compiles the axum server and the `llama-server` supervisor.
//!
//! Without the feature: stub crate (only `VERSION` is exposed).
//!
//! ## Anti-cycle invariant
//!
//! `gradatum-engine` may depend on `gradatum-core` and `gradatum-dto`.
//! `gradatum-core` and `gradatum-dto` must NEVER depend on `gradatum-engine`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(feature = "serve")]
pub mod config;

#[cfg(feature = "serve")]
pub mod error;

#[cfg(feature = "serve")]
pub mod health;

#[cfg(feature = "serve")]
pub mod metrics;

#[cfg(feature = "serve")]
pub mod runtime;

#[cfg(feature = "serve")]
pub mod server;

#[cfg(feature = "serve")]
pub mod sink;

#[cfg(feature = "serve")]
pub mod supervisor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
