//! # gradatum-cache
//!
//! In-process moka LRU cache with checksum validation on hit.
//!
//! ## Modules
//!
//! - [`effective_note`] : `EffectiveNoteCache` — stores `(Arc<EffectiveNote>, ContentHash)`.
//!   On cache hit, the caller provides an async `validator` closure that returns the current
//!   hash from SQLite. Match → returns cached value. Mismatch → invalidates entry + returns `None`.
//!   Implements a hash-before-serve validation strategy for safe cache invalidation.
//!
//! ## Stability
//!
//! `1.0.0` — public API under [SemVer 2.0.0](https://semver.org): backward-compatible
//! additions only within `1.x`, breaking changes deferred to the next major. See the
//! [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod effective_note;

pub use effective_note::{CacheKey, EffectiveNoteCache, EffectiveNoteCacheConfig};

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
