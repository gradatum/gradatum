//! # gradatum-sdk-rs
//!
//! Rust SDK client for the gradatum-server HTTP API.
//!
//! ## Status
//!
//! Not yet implemented — planned for a future release.

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
