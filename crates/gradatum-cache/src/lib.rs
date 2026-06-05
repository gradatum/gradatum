//! # gradatum-cache
//!
//! Cache moka LRU in-process avec validation de checksum sur hit.
//!
//! ## Modules
//!
//! - [`effective_note`] : `EffectiveNoteCache` — stocke `(Arc<EffectiveNote>, ContentHash)`.
//!   Sur cache hit, le caller fournit un async validator qui retourne le hash courant
//!   depuis SQLite. Si match → retour cache. Si mismatch → invalidation + `None`.
//!   Implémente D-perf-2 / B22 spec §6.1 risque #5.
//!
//! ## Stability
//!
//! `0.x` — no API stability guarantee. All public traits are tagged
//! [`#[stability::unstable]`] or [`#[stability::experimental]`].
//! See the [versioning policy](https://github.com/gradatum/gradatum/blob/main/RELEASE-POLICY.md).

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
