//! # gradatum-auth
//!
//! External identity verification: JWT (Ed25519, audience-scoped, mandatory `kid`),
//! key store, and JTI revocation.
//!
//! ## Stability
//!
//! `1.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `1.x`, breaking changes deferred to the next major. Items still
//! annotated `#[stability::unstable]` or `#[stability::experimental]` carry a finer-grained
//! tier per RELEASE-POLICY.md AM1.
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! JWT Ed25519 is active; `RevocationStore` is read on every request
//! (fail-closed — revoked tokens are rejected), but nothing writes to it in 1.0.0.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Version of this crate, inherited from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod jwt;
pub mod key_store;
pub mod revocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
