//! # gradatum
//!
//! Umbrella SDK facade. Re-exports curated subsets of focused crates via Cargo features for downstream ergonomics.
//!
//! ## Stability
//!
//! `2.1.1` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `2.x`, breaking changes deferred to the next major. Traits still
//! tagged `#[stability::unstable]` or `#[stability::experimental]` carry a finer-grained
//! tier per RELEASE-POLICY.md AM1.
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Contents
//!
//! Re-exports curated subsets of focused crates:
//! - `core` feature — **enabled by default** → `gradatum-core` (shared primitives,
//!   traits, types), reachable as `gradatum::core`
//! - `client` feature — opt-in → `gradatum-sdk-rs` (HTTP client SDK — planned; the
//!   crate is currently a placeholder with no client surface, hence not in `default`)
//!
//! A plain `cargo add gradatum` therefore exposes `gradatum::core` in addition to
//! [`VERSION`]. Use `default-features = false` to opt out of the `core` re-export.
//!
//! Downstream crates that need the full workspace surface can depend on `gradatum`
//! directly and enable the relevant features, rather than listing individual crates.

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
