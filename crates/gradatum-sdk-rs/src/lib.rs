//! # gradatum-sdk-rs
//!
//! Rust SDK client for the gradatum-server HTTP API.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. All public traits are tagged
//! [`#[stability::unstable]`] or [`#[stability::experimental]`].
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! Scaffolding stub — implementation in Phase 2. See [`docs/PHASES.md`](https://github.com/gradatum/gradatum/blob/main/docs/PHASES.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

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
