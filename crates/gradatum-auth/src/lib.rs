//! # gradatum-auth
//!
//! External identity verification: JWT (Ed25519, audience-scoped), OIDC, API-key.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. All public traits are annotated
//! [`#[stability::unstable]`] or [`#[stability::experimental]`].
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).
//!
//! ## Status
//!
//! `RevocationStore` and JWT Ed25519 are implemented and active.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

/// Version of this crate, inherited from `workspace.package.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod jwt;
pub mod revocation;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
