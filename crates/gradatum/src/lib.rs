//! # gradatum
//!
//! Umbrella SDK facade. Re-exports curated subsets of focused crates via Cargo features for downstream ergonomics.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. All public traits are tagged
//! [`#[stability::unstable]`] or [`#[stability::experimental]`].
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! Scaffolding stub — implementation in Phase 1. See [`docs/PHASES.md`](https://github.com/gradatum/gradatum/blob/main/docs/PHASES.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

#[cfg(feature = "core")]
pub use gradatum_core as core;
#[cfg(feature = "client")]
pub use gradatum_sdk_rs as sdk;

/// Crate version (from `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
